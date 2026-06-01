//! SPIKE — the asset side-channel, proven in isolation.
//!
//! Validates the mechanism for `asset.*` builtins before wiring up real
//! transforms (grass, image resize) or folding it into [`crate::compile`].
//! The three claims under test (see `#[cfg(test)]` below):
//!
//! 1. **Converges in one cold round.** A page that calls `asset.file("x")`
//!    when the URL is unknown gets a placeholder on the first realization,
//!    twyla processes the discovered spec, injects the resolved URL, and the
//!    *next* realization returns the real URL.
//! 2. **No-op when seeded.** If twyla seeds the resolved-URL map from its
//!    persistent store before compiling, the very first realization returns
//!    the real URL, the discovery channel stays empty, and the loop converges
//!    in typst's normal iteration count — zero extra asset rounds.
//! 3. **The sink doesn't perturb the comemo hash.** Discovery rides a
//!    write-only [`crossbeam_channel::Sender`]; the value carrying it onto the
//!    style chain hashes to a constant and compares equal regardless of which
//!    channel it holds, so injecting it never busts the realization cache.
//!    Only the resolved-map (the *tracked* driver) moves the hash.
//!
//! The split is the whole point: **map = tracked/hashed (drives convergence),
//! sink = hash-excluded (drives discovery).** Get it backwards and you either
//! never converge or re-realize the whole site every compile.

use comemo::{Track, Tracked, TrackedMut};
use crossbeam_channel::{Sender, unbounded};
use ecow::{EcoString, eco_vec};
use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};
use typst::World;
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{
    Binding, Context, Dict, IntoValue, Module, NativeElement, Output, Repr, Scope, Str, Style,
    StyleChain, TargetElem, Value, elem, func, ty,
};
use typst::syntax::{FileId, Span, VirtualPath};
use typst::utils::LazyHash;
use typst_bundle::{Bundle, BundleDocument, BundleFile};
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{EmptyIntrospector, MAX_ITERS};
use typst_library::model::DocumentElem;
use typst_utils::Protected;

// ---------------------------------------------------------------------------
// Carriers on the style chain
// ---------------------------------------------------------------------------

/// Host element carrying the **resolved-URL map** — the *tracked* half.
///
/// Maps an asset spec key (here just the path string) to its final URL. Twyla
/// injects it onto the relayout style chain each iteration; because it's
/// content-hashed, replacing it with a more-complete map is a comemo *miss*
/// for every `asset.*` consumer, which is exactly what re-resolves them. This
/// is the same trick `documents()` uses (see [`crate::document`]).
#[elem]
pub struct TwylaAssetMap {
    /// `spec-key -> url` for every asset twyla has resolved so far.
    #[default(Dict::new())]
    pub map: Dict,
}

/// Host element carrying the **discovery sink** — the *hash-excluded* half.
#[elem]
pub struct TwylaAssetSink {
    /// A [`Value::Dyn`] wrapping an [`AssetSink`]. Hashes constant (see
    /// [`AssetSink`]), so injecting it never moves the realization cache key.
    #[default(Value::None)]
    pub sink: Value,
}

/// Write-only channel for reporting discovered asset specs out of the
/// otherwise-pure realization pass.
///
/// The `Hash`/`PartialEq` impls are deliberately **constant**: any two
/// `AssetSink`s hash equal and compare equal regardless of the channel they
/// hold. That is what keeps the sink out of comemo's view — it can ride the
/// style chain alongside the resolved-map without ever perturbing the hash
/// comemo keys realization on. (`dyn_hash` also folds in the `TypeId`, so the
/// constant is per-type, not globally zero — irrelevant here, just noted.)
#[ty]
#[derive(Clone)]
pub struct AssetSink(pub Sender<EcoString>);

impl Debug for AssetSink {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str("AssetSink(..)")
    }
}

impl Repr for AssetSink {
    fn repr(&self) -> EcoString {
        EcoString::inline("asset-sink")
    }
}

impl PartialEq for AssetSink {
    /// All sinks are equal — the channel identity is invisible to comemo.
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Hash for AssetSink {
    /// Hash nothing — the channel identity is invisible to comemo.
    fn hash<H: Hasher>(&self, _: &mut H) {}
}

// ---------------------------------------------------------------------------
// The contextual builtin: `asset.file(path)`
// ---------------------------------------------------------------------------

/// Resolve an asset path to its final URL (spike: path input only).
///
/// Contextual: it reads the injected resolved-map off the style chain. On a
/// hit it returns the real URL. On a miss it reports the spec on the discovery
/// sink and returns a placeholder; a later relayout iteration — once twyla has
/// processed the spec and injected its URL — re-runs this (comemo miss on the
/// changed map) and returns the resolved URL.
#[func(contextual)]
pub fn file(
    /// The context to read the resolved-asset map from.
    context: Tracked<Context>,
    /// Project-relative path of the asset.
    path: Str,
) -> HintedStrResult<Str> {
    let styles = context.styles()?;
    let key = path.as_str();

    // Tracked read: a hit here is what makes the page converge.
    let map = styles.get_cloned(TwylaAssetMap::map);
    if let Ok(Value::Str(url)) = map.get(key) {
        return Ok(url.clone());
    }

    // Miss: report on the write-only sink (invisible to comemo) and return a
    // placeholder for this round.
    if let Value::Dyn(dynamic) = styles.get_cloned(TwylaAssetSink::sink)
        && let Some(sink) = dynamic.downcast::<AssetSink>()
    {
        let _ = sink.0.send(EcoString::from(key));
    }
    Ok(Str::from(format!("/__twyla-pending__/{key}")))
}

/// Build the `asset` module (`asset.file`, …) for the global scope.
pub fn module() -> Module {
    let mut scope = Scope::new();
    scope.define_func::<file>();
    Module::new("asset", scope)
}

/// Bind the `asset` module into a global scope. Called from [`crate::prelude`].
pub fn install(global: &mut Scope) {
    global.bind("asset".into(), Binding::detached(Value::Module(module())));
}

// ---------------------------------------------------------------------------
// Spike driver: a relayout loop that drains the sink and re-injects
// ---------------------------------------------------------------------------

/// What the spike compile observed — enough to assert all three claims.
#[derive(Debug)]
pub struct SpikeOutcome {
    /// Final serialized HTML (placeholders should be gone on success).
    pub html: String,
    /// Specs the sink received across the whole compile, in order.
    pub discovered: Vec<EcoString>,
    /// How many realization iterations the loop ran (1 ⇒ converged cold).
    pub iterations: usize,
}

/// Evaluate `source` once, then run the relayout fixed-point with asset
/// discovery folded into the *same* loop (no nesting). `seed` pre-populates
/// the resolved-URL map — pass an empty dict for a cold build, or a warm map
/// to exercise the seeded no-op path.
///
/// This deliberately mirrors [`crate::compile`]'s `relayout`, with two added
/// lines of behavior: (1) the resolved-map is recomputed every iteration as
/// the sink drains, and (2) the break is gated on assets being *settled* as
/// well as typst's own introspection constraint — otherwise typst could
/// declare convergence on a round where a freshly-discovered asset is still
/// showing its placeholder.
pub fn spike_resolve(
    world: &dyn World,
    source: FileId,
    seed: Dict,
) -> SourceResult<SpikeOutcome> {
    let mut sink = Sink::new();
    let traced = Traced::default();
    let world = world.track();

    // Evaluate the source into a body and wrap it as a routed document.
    let body = {
        let src = world.source(source).map_err(|err| {
            eco_vec![SourceDiagnostic::error(Span::detached(), EcoString::from(err))]
        })?;
        let mut tracked_sink = sink.track_mut();
        let module = typst_eval::eval(
            world,
            world.library(),
            traced.track(),
            TrackedMut::reborrow_mut(&mut tracked_sink),
            Route::default().track(),
            &src,
        )?;
        module.content()
    };
    let path = typst::foundations::BundlePath::new(VirtualPath::new("index.html").unwrap()).unwrap();
    let content = DocumentElem::new(path, body).pack();

    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(Bundle::target()).wrap();
    let base_target = base.chain(&target);

    let (tx, rx) = unbounded::<EcoString>();
    let sink_style: LazyHash<Style> =
        TwylaAssetSink::sink.set(Value::dynamic(AssetSink(tx))).wrap();

    let mut resolved = seed;
    let mut discovered = Vec::new();
    let empty_introspector = EmptyIntrospector;
    let mut history: Vec<Bundle> = Vec::new();
    let mut iterations = 0usize;
    let bundle: Bundle;

    loop {
        iterations += 1;

        // Recompute the injected styles every iteration — the resolved-map
        // grows as the sink drains. The sink style is constant (and hashes
        // constant), so only the map ever moves the comemo key.
        let map_style: LazyHash<Style> = TwylaAssetMap::map.set(resolved.clone()).wrap();
        let styles = base_target.chain(&map_style);
        let styles = styles.chain(&sink_style);

        let introspector = history
            .last()
            .map(|doc| doc.introspector())
            .unwrap_or(&empty_introspector);
        let constraint = comemo::Constraint::new();

        let mut subsink = Sink::new();
        let mut engine = Engine {
            library,
            world,
            introspector: Protected::new(introspector.track_with(&constraint)),
            traced: traced.track(),
            sink: subsink.track_mut(),
            route: Route::default(),
        };

        let document = Bundle::create(&mut engine, &content, styles)?;

        // Drain the discovery sink and "process" each new spec. Real twyla
        // hashes bytes / runs grass / encodes here, on rayon, off to the side;
        // the spike just synthesizes a deterministic URL.
        let mut newly_resolved = 0;
        for spec in rx.try_iter() {
            discovered.push(spec.clone());
            if !resolved.contains(spec.as_str()) {
                resolved.insert(spec.as_str().into(), process(&spec).into_value());
                newly_resolved += 1;
            }
        }
        let assets_settled = newly_resolved == 0;

        // Break only when typst's introspection AND twyla's assets agree.
        if constraint.validate(document.introspector()) && assets_settled {
            sink.extend_from_sink(subsink);
            bundle = document;
            break;
        }

        if history.len() >= MAX_ITERS - 1 {
            sink.extend_from_sink(subsink);
            bundle = document;
            break;
        }

        // If only assets moved (typst was otherwise stable), still re-run with
        // the enriched map; push history so the introspector carries forward.
        history.push(document);
    }

    let delayed = sink.delayed();
    if !delayed.is_empty() {
        return Err(delayed);
    }

    let html = render_bundle(&bundle)?;
    Ok(SpikeOutcome {
        html,
        discovered,
        iterations,
    })
}

/// Stand-in for the real transform pipeline: deterministic content-addressed
/// URL from the spec. Real impl resizes/transcodes/hashes bytes on rayon.
fn process(spec: &str) -> String {
    let digest = {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in spec.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{:08x}", h & 0xffff_ffff)
    };
    let (stem, ext) = spec.rsplit_once('.').unwrap_or((spec, ""));
    if ext.is_empty() {
        format!("/assets/{stem}.{digest}")
    } else {
        format!("/assets/{stem}.{digest}.{ext}")
    }
}

/// Serialize the first HTML document in the bundle.
fn render_bundle(bundle: &Bundle) -> SourceResult<String> {
    for (_, file) in bundle.files.iter() {
        if let BundleFile::Document(BundleDocument::Html(doc)) = file {
            return typst_html::html(doc);
        }
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::TwylaContext;
    use crate::render::RenderWorld;
    use typst::syntax::{RootedPath, VirtualRoot};

    /// A page that reads one asset URL inside `#context` (the only place a
    /// contextual builtin resolves) and shows it as text.
    const PAGE: &str = "#context asset.file(\"style.css\")";

    fn world_with(content: &str) -> (RenderWorld, FileId, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("content")).unwrap();
        std::fs::write(root.join("content/main.typ"), content).unwrap();
        let ctx = TwylaContext::new(root, Some("https://example.com".into())).unwrap();
        let world = RenderWorld::new(&ctx).unwrap();
        let id = FileId::new(RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new("content/main.typ").unwrap(),
        ));
        (world, id, dir)
    }

    /// Iterations typst needs for an equivalent `#context` page that touches
    /// no assets — the baseline the asset machinery must not exceed.
    fn typst_baseline_iterations() -> usize {
        let (world, id, _dir) = world_with("#context \"warm\"");
        spike_resolve(&world, id, Dict::new()).unwrap().iterations
    }

    /// Claim 1 — a cold build (empty seed) discovers the asset and resolves it
    /// without leaking a placeholder, costing at most one extra realization
    /// over the typst baseline (the discovery round, which often piggybacks on
    /// an iteration typst was doing anyway).
    #[test]
    fn cold_build_discovers_and_resolves() {
        let (world, id, _dir) = world_with(PAGE);
        let out = spike_resolve(&world, id, Dict::new()).unwrap();

        assert_eq!(out.discovered, vec![EcoString::from("style.css")]);
        assert!(
            out.html.contains("/assets/style.") && out.html.contains(".css"),
            "URL not resolved into the output:\n{}",
            out.html,
        );
        assert!(
            !out.html.contains("__twyla-pending__"),
            "placeholder leaked into the output:\n{}",
            out.html,
        );
        assert!(
            out.iterations <= typst_baseline_iterations() + 1,
            "cold build added more than one asset round (got {})",
            out.iterations,
        );
    }

    /// Claim 2 — seeding the resolved-map (as twyla would from its persistent
    /// store) makes the first realization return the real URL: discovery never
    /// fires and the loop converges in exactly typst's baseline count — zero
    /// extra asset rounds.
    #[test]
    fn seeded_build_is_a_noop() {
        let (world, id, _dir) = world_with(PAGE);
        let mut seed = Dict::new();
        seed.insert("style.css".into(), "/assets/style.deadbeef.css".into_value());

        let out = spike_resolve(&world, id, seed).unwrap();

        assert!(
            out.discovered.is_empty(),
            "discovery fired despite a warm seed: {:?}",
            out.discovered,
        );
        assert!(
            out.html.contains("/assets/style.deadbeef.css"),
            "seeded URL not used:\n{}",
            out.html,
        );
        assert_eq!(
            out.iterations,
            typst_baseline_iterations(),
            "seeded build must add no realizations over the typst baseline",
        );
    }

    /// Claim 3 — the sink is invisible to comemo. Two sinks holding different
    /// channels produce an identical style-chain hash, while changing the
    /// resolved-map (the tracked half) does move the hash.
    #[test]
    fn sink_does_not_perturb_the_comemo_hash() {
        let (tx1, _r1) = unbounded::<EcoString>();
        let (tx2, _r2) = unbounded::<EcoString>();
        let s1: LazyHash<Style> = TwylaAssetSink::sink.set(Value::dynamic(AssetSink(tx1))).wrap();
        let s2: LazyHash<Style> = TwylaAssetSink::sink.set(Value::dynamic(AssetSink(tx2))).wrap();
        assert_eq!(
            typst_utils::hash128(&s1),
            typst_utils::hash128(&s2),
            "different channels must hash identically (sink hash-excluded)",
        );

        let mut m1 = Dict::new();
        m1.insert("a".into(), "x".into_value());
        let mut m2 = Dict::new();
        m2.insert("a".into(), "y".into_value());
        let ms1: LazyHash<Style> = TwylaAssetMap::map.set(m1).wrap();
        let ms2: LazyHash<Style> = TwylaAssetMap::map.set(m2).wrap();
        assert_ne!(
            typst_utils::hash128(&ms1),
            typst_utils::hash128(&ms2),
            "the resolved-map must move the hash (it drives convergence)",
        );
    }
}
