//! The asset system: typed constructors, the discovery side-channel, and the
//! resolver that drives asset URLs to convergence inside the compile loop.
//!
//! Mechanism (proven in the spike, jj `rtnknlwo`): an asset's URL is resolved
//! through the *realization* fixed-point loop, split into two halves with
//! opposite comemo requirements.
//!
//! - **Resolved-map — tracked/hashed (data IN, drives convergence).**
//!   [`ResolvedAssets`] (`AssetSpec -> url`) rides the relayout style chain via
//!   [`TwylaAssetMap`]. The `StyleChain` is hashed *by value* into typst's
//!   `realize` memo key, so injecting a more-complete map is a comemo miss for
//!   every `asset.*` consumer — which is what re-resolves them. (It must be
//!   `Hash`; `#[track]` can't help — the map crosses typst's by-value realize
//!   boundary, which we don't own.) The `Hash` is *order-independent* so the
//!   same set of entries hashes identically regardless of discovery order,
//!   which is what lets a warm rebuild reuse the previous compile's cache.
//! - **Discovery sink — hash-excluded (specs OUT, drives discovery).**
//!   [`AssetSink`] wraps a write-only [`crossbeam_channel::Sender`]; its `Hash`
//!   is constant and `PartialEq` always-true, so it rides the chain via
//!   [`TwylaAssetSink`] without ever moving the comemo key. The contextual
//!   `.url` method `send`s its [`AssetRequest`] on a miss and never reads back.
//!
//! Why the side-channel is sound: the set of asset *inputs* is immutable going
//! into a compile; the sink only reports *which* fixed inputs were referenced,
//! and processing `(source, transform) -> bytes` is a pure function computed
//! off to the side by [`AssetResolver`]. comemo's at-least-once-per-unique-input
//! is all discovery needs (dup call sites collapse; we dedup by [`AssetSpec`]).

use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use comemo::Tracked;
use crossbeam_channel::{Receiver, Sender, unbounded};
use ecow::{EcoString, eco_vec};
use typst::World;
use typst::diag::{At, HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{
    Binding, Bytes, Context, Module, PathOrStr, Repr, Scope, Str, Style, Value, elem, func, scope,
    ty,
};
use typst::syntax::{FileId, Span, Spanned};
use typst::utils::LazyHash;
use typst_utils::hash128;

use crate::project::TwylaContext;

// ---------------------------------------------------------------------------
// Keys: AssetSpec -> AssetRequest -> Asset
// ---------------------------------------------------------------------------

/// What produces an asset's bytes — the cache/store key. An asset's output is
/// a pure function of its spec, so this is what every map keys on.
///
/// FileId-based on purpose: `FileId` is `Eq + Hash` (usable as a `HashMap`
/// key), whereas typst's `Bytes`/`DataSource` are neither `Eq` nor `Ord`. When
/// `asset.raw` lands we'll unify the source behind `loading::DataSource` and
/// hand-impl `Eq` (it's a reflexive marker) to keep this keyable.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum AssetSpec {
    /// Copy a project file verbatim (fingerprinted).
    File { file: FileId },
    /// Compile a Sass/SCSS file to CSS.
    Sass { file: FileId },
}

impl AssetSpec {
    /// The source file this spec reads from.
    fn file(&self) -> FileId {
        match self {
            AssetSpec::File { file } | AssetSpec::Sass { file } => *file,
        }
    }

    /// The output extension after transforming (empty if the source has none).
    fn out_ext(&self) -> String {
        match self {
            AssetSpec::Sass { .. } => "css".to_string(),
            AssetSpec::File { file } => Path::new(file.vpath().get_without_slash())
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default(),
        }
    }
}

/// A spec plus its (future) output policy. The `output: auto | str | info => str`
/// argument will live here; keeping the wrapper now makes adding it
/// non-breaking. For today it carries nothing beyond the spec.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AssetRequest {
    pub spec: AssetSpec,
}

/// A handle to an asset, returned by `asset.file(..)` / `asset.sass(..)`.
///
/// Pure and eval-time: it just captures the [`AssetRequest`]. Resolve its URL
/// with `.url` **inside a `#context` block** — that's where the injected
/// resolved-map is readable.
#[ty(scope)]
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Asset {
    request: AssetRequest,
}

#[scope]
impl Asset {
    /// The resolved, fingerprinted URL of this asset (e.g.
    /// `/assets/main-<hash>.css`).
    ///
    /// Contextual — call it inside `#context`:
    ///
    /// ```typ
    /// #context html.elem("link", attrs: (
    ///   rel: "stylesheet",
    ///   href: asset.sass("main.scss").url,
    /// ))
    /// ```
    #[func(contextual)]
    fn url(&self, context: Tracked<Context>) -> HintedStrResult<Str> {
        let styles = context.styles()?;

        // Tracked read: a hit here is what makes the page converge.
        if let Value::Dyn(dynamic) = styles.get_cloned(TwylaAssetMap::map)
            && let Some(map) = dynamic.downcast::<ResolvedAssets>()
            && let Some(url) = map.get(&self.request.spec)
        {
            return Ok(Str::from(url.as_str()));
        }

        // Miss: report the request on the write-only sink (invisible to comemo)
        // and return a placeholder. A later relayout iteration — once the
        // resolver has processed it and injected the URL — re-runs this (comemo
        // miss on the changed map) and returns the real URL.
        if let Value::Dyn(dynamic) = styles.get_cloned(TwylaAssetSink::sink)
            && let Some(sink) = dynamic.downcast::<AssetSink>()
        {
            let _ = sink.0.send(self.request.clone());
        }
        Ok(Str::from(ASSET_PENDING))
    }
}

impl Repr for Asset {
    fn repr(&self) -> EcoString {
        EcoString::inline("asset(..)")
    }
}

/// Placeholder URL returned for an as-yet-unresolved asset. By the time the
/// loop converges every consumer has re-run against a populated map, so this
/// never survives into output (a leak means non-convergence — a bug).
const ASSET_PENDING: &str = "/__twyla-asset-pending__";

// ---------------------------------------------------------------------------
// Constructors: the `asset` module
// ---------------------------------------------------------------------------

/// Reference a project file as an asset, copied verbatim and fingerprinted.
#[func]
fn file(
    /// Path to the file, relative to the calling file.
    path: Spanned<PathOrStr>,
) -> SourceResult<Asset> {
    Ok(Asset {
        request: AssetRequest {
            spec: AssetSpec::File { file: resolve(path)? },
        },
    })
}

/// Compile a Sass/SCSS file to a fingerprinted CSS asset.
#[func]
fn sass(
    /// Path to the `.sass`/`.scss` file, relative to the calling file.
    path: Spanned<PathOrStr>,
) -> SourceResult<Asset> {
    Ok(Asset {
        request: AssetRequest {
            spec: AssetSpec::Sass { file: resolve(path)? },
        },
    })
}

/// Resolve a path argument to the `FileId` it names, relative to the calling
/// file (mirrors how `read`/`image` resolve their paths).
fn resolve(path: Spanned<PathOrStr>) -> SourceResult<FileId> {
    Ok(path.v.resolve_if_some(path.span.id()).at(path.span)?.intern())
}

/// Build the `asset` module (`asset.file`, `asset.sass`) for the global scope.
pub fn module() -> Module {
    let mut scope = Scope::new();
    scope.define_func::<file>();
    scope.define_func::<sass>();
    Module::new("asset", scope)
}

/// Bind the `asset` module into a global scope. Called from [`crate::prelude`].
pub fn install(global: &mut Scope) {
    global.bind("asset".into(), Binding::detached(Value::Module(module())));
}

// ---------------------------------------------------------------------------
// Style-chain carriers
// ---------------------------------------------------------------------------

/// Host element carrying the resolved-URL map (the tracked half).
#[elem]
pub struct TwylaAssetMap {
    /// A [`Value::Dyn`] wrapping [`ResolvedAssets`]. Content-hashed, so it
    /// drives re-realization as assets resolve.
    #[default(Value::None)]
    pub map: Value,
}

/// Host element carrying the discovery sink (the hash-excluded half).
#[elem]
pub struct TwylaAssetSink {
    /// A [`Value::Dyn`] wrapping [`AssetSink`]. Hashes constant, so injecting
    /// it never moves the realization cache key.
    #[default(Value::None)]
    pub sink: Value,
}

/// The resolved-URL map carried on the style chain: `AssetSpec -> url`.
///
/// `Hash` is **order-independent** (the per-entry hashes are sorted before
/// hashing) so the same set of entries hashes identically regardless of the
/// order rayon discovered them in — which is what lets a warm rebuild hit the
/// previous compile's realization cache. `BTreeMap` would give this for free,
/// but no component of `AssetSpec` is `Ord`, so we do it by hand over a
/// `HashMap`.
#[ty(name = "twyla-resolved-assets")]
#[derive(Clone, PartialEq, Debug)]
pub struct ResolvedAssets(HashMap<AssetSpec, EcoString>);

impl ResolvedAssets {
    fn get(&self, spec: &AssetSpec) -> Option<&EcoString> {
        self.0.get(spec)
    }
}

impl Hash for ResolvedAssets {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut entries: Vec<u128> = self.0.iter().map(|kv| hash128(&kv)).collect();
        entries.sort_unstable();
        entries.hash(state);
    }
}

impl Repr for ResolvedAssets {
    fn repr(&self) -> EcoString {
        EcoString::inline("twyla-resolved-assets(..)")
    }
}

/// Write-only channel for reporting discovered asset requests out of the
/// otherwise-pure realization pass. Hash/Eq are deliberately **constant** —
/// the channel identity is invisible to comemo (see module docs).
#[ty(name = "twyla-asset-sink")]
#[derive(Clone)]
pub struct AssetSink(Sender<AssetRequest>);

impl Debug for AssetSink {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str("AssetSink(..)")
    }
}

impl Repr for AssetSink {
    fn repr(&self) -> EcoString {
        EcoString::inline("twyla-asset-sink")
    }
}

impl PartialEq for AssetSink {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Hash for AssetSink {
    fn hash<H: Hasher>(&self, _: &mut H) {}
}

// ---------------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------------

/// How a resolved asset's bytes reach the output. `Copy` keeps only the source
/// path (stream-copied at emit time — never the whole file in memory);
/// `Bytes` holds small transformed output (e.g. compiled CSS).
#[derive(Clone, Debug)]
pub enum Emit {
    /// Stream-copy from this on-disk source path.
    Copy(PathBuf),
    /// Write these transformed bytes.
    Bytes(Bytes),
}

/// One processed asset: where it lives and how to emit it.
#[derive(Clone, Debug)]
pub struct ResolvedAsset {
    /// The spec that produced it (its key).
    pub spec: AssetSpec,
    /// Root-relative (or base-url-prefixed) URL.
    pub url: EcoString,
    /// Bundle-relative output path, e.g. `assets/main-<hash>.css`.
    pub output_path: PathBuf,
    /// How to emit the bytes.
    pub emit: Emit,
}

/// Owns the discovery channel and the accumulated resolved-asset store, and
/// processes discovered requests between relayout iterations.
pub struct AssetResolver {
    ctx: TwylaContext,
    tx: Sender<AssetRequest>,
    rx: Receiver<AssetRequest>,
    resolved: HashMap<AssetSpec, ResolvedAsset>,
}

impl AssetResolver {
    /// Create a resolver, seeded with already-known assets (e.g. from a warm
    /// `serve` session). An empty seed is a cold build.
    pub fn new(ctx: &TwylaContext, seed: HashMap<AssetSpec, ResolvedAsset>) -> Self {
        let (tx, rx) = unbounded();
        Self {
            ctx: ctx.clone(),
            tx,
            rx,
            resolved: seed,
        }
    }

    /// The (constant-hashing) sink style — build once and chain every iteration.
    pub fn sink_style(&self) -> LazyHash<Style> {
        TwylaAssetSink::sink
            .set(Value::dynamic(AssetSink(self.tx.clone())))
            .wrap()
    }

    /// The resolved-map style for the current state — rebuilt each iteration as
    /// the store grows.
    pub fn map_style(&self) -> LazyHash<Style> {
        let map: HashMap<AssetSpec, EcoString> = self
            .resolved
            .iter()
            .map(|(spec, asset)| (spec.clone(), asset.url.clone()))
            .collect();
        TwylaAssetMap::map
            .set(Value::dynamic(ResolvedAssets(map)))
            .wrap()
    }

    /// Drain the discovery channel and process each newly-seen request.
    /// Returns `true` if nothing new was resolved (assets are *settled*).
    pub fn drain_and_process(&mut self, world: Tracked<dyn World + '_>) -> SourceResult<bool> {
        let mut settled = true;
        // Collect first so we don't hold the receiver across processing.
        let requests: Vec<AssetRequest> = self.rx.try_iter().collect();
        for request in requests {
            if self.resolved.contains_key(&request.spec) {
                continue;
            }
            let asset = self.process(world, &request.spec)?;
            self.resolved.insert(request.spec, asset);
            settled = false;
        }
        Ok(settled)
    }

    /// Process one spec into a [`ResolvedAsset`]: read the source bytes
    /// (tracked, so the watcher sees them), transform, fingerprint, name.
    fn process(
        &self,
        world: Tracked<dyn World + '_>,
        spec: &AssetSpec,
    ) -> SourceResult<ResolvedAsset> {
        let file = spec.file();
        let bytes = world.file(file).map_err(|err| {
            eco_vec![SourceDiagnostic::error(Span::detached(), EcoString::from(err))]
        })?;

        // Transform. Phase 0: identity for both (grass for Sass lands in
        // Phase 1). The fingerprint is of the *output* bytes, like zola/hugo.
        let output: Emit = match spec {
            AssetSpec::File { .. } => Emit::Copy(self.ctx.root.join(file.vpath().get_without_slash())),
            AssetSpec::Sass { .. } => Emit::Bytes(bytes.clone()),
        };
        let hash = match &output {
            Emit::Copy(_) => hash128(&bytes),
            Emit::Bytes(b) => hash128(b),
        };

        let name = match source_stem(file) {
            Some(stem) => format!("{stem}-{hash:032x}.{}", spec.out_ext()),
            None => format!("{hash:032x}.{}", spec.out_ext()),
        };
        let output_path = self.ctx.default_asset_dir().join(&name);
        let url = EcoString::from(self.ctx.asset_url(&output_path));

        Ok(ResolvedAsset {
            spec: spec.clone(),
            url,
            output_path,
            emit: output,
        })
    }

    /// Consume the resolver, yielding every resolved asset (for emission).
    pub fn into_resolved(self) -> Vec<ResolvedAsset> {
        self.resolved.into_values().collect()
    }
}

/// File stem of a project file id, for naming (`main.scss` -> `main`).
fn source_stem(file: FileId) -> Option<EcoString> {
    Path::new(file.vpath().get_without_slash())
        .file_stem()
        .map(|s| EcoString::from(s.to_string_lossy().as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::TwylaContext;
    use crate::render::RenderWorld;
    use crossbeam_channel::unbounded;
    use typst::foundations::Value;
    use typst::syntax::{RootedPath, VirtualPath, VirtualRoot};
    use typst::utils::LazyHash;
    use typst_bundle::{BundleDocument, BundleFile};
    use typst_utils::hash128;

    fn site(files: &[(&str, &str)]) -> (TwylaContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("content")).unwrap();
        for (rel, contents) in files {
            std::fs::write(dir.path().join(rel), contents).unwrap();
        }
        let ctx = TwylaContext::new(dir.path(), Some("https://example.com".into())).unwrap();
        (ctx, dir)
    }

    /// Compile the whole site through the real loop; return (first-page HTML,
    /// resolved assets).
    fn compile(ctx: &TwylaContext) -> (String, Vec<ResolvedAsset>) {
        let world = RenderWorld::new(ctx).unwrap();
        let sources: Vec<FileId> = ctx
            .scan_pages()
            .unwrap()
            .into_iter()
            .map(|p| {
                FileId::new(RootedPath::new(
                    VirtualRoot::Project,
                    VirtualPath::virtualize(&ctx.root, &p).unwrap(),
                ))
            })
            .collect();
        let warned = crate::compile::compile_bundle(ctx, &world, &sources);
        let (bundle, _harvested, assets) = warned.output.unwrap();
        let mut html = String::new();
        for (_, file) in bundle.files.iter() {
            if let BundleFile::Document(BundleDocument::Html(doc)) = file {
                html = typst_html::html(doc).unwrap();
                break;
            }
        }
        (html, assets)
    }

    fn fid(path: &str) -> FileId {
        FileId::new(RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new(path).unwrap(),
        ))
    }

    /// The whole point of Phase 0: an `asset.*().url()` call resolves to a
    /// fingerprinted URL through the relayout loop, with no placeholder left
    /// behind — proving discovery → process → re-inject → re-realize works in
    /// the *real* compile path.
    #[test]
    fn cold_build_resolves_asset_url_through_the_loop() {
        let (ctx, _dir) = site(&[
            ("content/main.typ", "#context asset.file(\"logo.svg\").url()"),
            ("content/logo.svg", "<svg/>"),
        ]);
        let (html, assets) = compile(&ctx);

        assert_eq!(assets.len(), 1, "expected one resolved asset, got {assets:?}");
        let url = assets[0].url.as_str();
        assert!(
            url.contains("/assets/logo-") && url.ends_with(".svg"),
            "unexpected auto-named url: {url}",
        );
        assert!(html.contains(url), "page missing resolved url {url}:\n{html}");
        assert!(
            !html.contains("__twyla-asset-pending__"),
            "placeholder leaked into output:\n{html}",
        );
    }

    /// Two assets on one page both resolve (loop converges with several
    /// outstanding placeholders), and `sass` fingerprints to a `.css` name.
    #[test]
    fn multiple_assets_resolve_and_sass_renames_to_css() {
        let (ctx, _dir) = site(&[
            (
                "content/main.typ",
                "#context asset.file(\"a.txt\").url() #context asset.sass(\"b.scss\").url()",
            ),
            ("content/a.txt", "hello"),
            ("content/b.scss", "a{b:c}"),
        ]);
        let (html, assets) = compile(&ctx);

        assert_eq!(assets.len(), 2, "got {assets:?}");
        assert!(
            assets.iter().any(|a| a.url.contains("/assets/a-") && a.url.ends_with(".txt")),
            "missing a.txt asset: {assets:?}",
        );
        assert!(
            assets.iter().any(|a| a.url.contains("/assets/b-") && a.url.ends_with(".css")),
            "sass asset not renamed to .css: {assets:?}",
        );
        assert!(!html.contains("__twyla-asset-pending__"), "placeholder leaked:\n{html}");
    }

    /// Hash discipline (the comemo split): the resolved-map hashes by *content*
    /// and is order-independent; the sink hashes *constant*.
    #[test]
    fn map_hashes_by_content_sink_is_excluded() {
        let a = AssetSpec::File { file: fid("content/a.css") };
        let b = AssetSpec::Sass { file: fid("content/b.scss") };

        let make = |pairs: &[(&AssetSpec, &str)]| {
            let mut m = HashMap::new();
            for (k, v) in pairs {
                m.insert((*k).clone(), EcoString::from(*v));
            }
            ResolvedAssets(m)
        };

        // Same entries, opposite insertion order → identical hash.
        let m1 = make(&[(&a, "ua"), (&b, "ub")]);
        let m2 = make(&[(&b, "ub"), (&a, "ua")]);
        assert_eq!(
            hash128(&m1),
            hash128(&m2),
            "resolved-map hash must be order-independent (enables warm-cache hits)",
        );

        // Different URL → different hash (this is what drives re-realization).
        let m3 = make(&[(&a, "CHANGED"), (&b, "ub")]);
        assert_ne!(hash128(&m1), hash128(&m3), "map must track its contents");

        // The sink: two distinct channels must hash identically as style fields.
        let (tx1, _r1) = unbounded::<AssetRequest>();
        let (tx2, _r2) = unbounded::<AssetRequest>();
        let s1: LazyHash<Style> = TwylaAssetSink::sink.set(Value::dynamic(AssetSink(tx1))).wrap();
        let s2: LazyHash<Style> = TwylaAssetSink::sink.set(Value::dynamic(AssetSink(tx2))).wrap();
        assert_eq!(
            hash128(&s1),
            hash128(&s2),
            "sink identity must be invisible to comemo",
        );
    }
}
