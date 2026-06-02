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
//! - **Discovery sink — per-resolver epoch (specs OUT, drives discovery).**
//!   [`AssetSink`] wraps a write-only [`crossbeam_channel::Sender`] and rides
//!   the chain via [`TwylaAssetSink`]. Its `Hash` keys on the resolver's
//!   *epoch*: constant within one resolver's lifetime (so across iterations
//!   only the map moves the comemo key), but distinct per resolver. A
//!   *globally*-constant hash is unsound — comemo would reuse a previous
//!   compile's cached `placeholder + send` for a fresh resolver, and the
//!   discovery `send` (a side effect inside memoized code) would never re-fire,
//!   starving it. The contextual `.url` method `send`s its [`AssetRequest`] on a
//!   miss and never reads back.
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
use std::sync::atomic::{AtomicU64, Ordering};

use comemo::Tracked;
use crossbeam_channel::{Receiver, Sender, unbounded};
use ecow::{EcoString, eco_format, eco_vec};
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
            let _ = sink.tx.send(self.request.clone());
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
/// otherwise-pure realization pass.
///
/// Hash/Eq key on the resolver's `epoch`, **not** the channel: constant within
/// one resolver's lifetime (so across relayout iterations only the resolved-map
/// moves the comemo key — that's what drives convergence), but **distinct per
/// resolver** (so a fresh resolver's empty-map realization is a comemo *miss*,
/// not a hit on a previous compile's cached placeholder). A globally-constant
/// hash here is unsound: comemo would reuse the cached `placeholder + send`
/// across resolver instances, and the discovery `send` — a side effect inside
/// memoized code — would never re-fire, starving the new resolver. The
/// channel identity itself stays out of the hash (it's not observable output).
#[ty(name = "twyla-asset-sink")]
#[derive(Clone)]
pub struct AssetSink {
    epoch: u64,
    tx: Sender<AssetRequest>,
}

impl Debug for AssetSink {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "AssetSink(epoch={})", self.epoch)
    }
}

impl Repr for AssetSink {
    fn repr(&self) -> EcoString {
        EcoString::inline("twyla-asset-sink")
    }
}

impl PartialEq for AssetSink {
    fn eq(&self, other: &Self) -> bool {
        self.epoch == other.epoch
    }
}

impl Hash for AssetSink {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.epoch.hash(state);
    }
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

/// Hands out a distinct discovery epoch per [`AssetResolver`]. See
/// [`AssetSink`] for why a fresh epoch (rather than a constant) is required for
/// correctness across compiles that share comemo's process-global cache.
static RESOLVER_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Owns the discovery channel and the accumulated resolved-asset store, and
/// processes discovered requests between relayout iterations.
pub struct AssetResolver {
    epoch: u64,
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
            epoch: RESOLVER_EPOCH.fetch_add(1, Ordering::Relaxed),
            ctx: ctx.clone(),
            tx,
            rx,
            resolved: seed,
        }
    }

    /// The sink style — build once and chain every iteration. Hashes on this
    /// resolver's `epoch` (constant for its lifetime, distinct per resolver).
    pub fn sink_style(&self) -> LazyHash<Style> {
        let sink = AssetSink {
            epoch: self.epoch,
            tx: self.tx.clone(),
        };
        TwylaAssetSink::sink.set(Value::dynamic(sink)).wrap()
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
        let on_disk = self.ctx.root.join(file.vpath().get_without_slash());

        // Transform, then fingerprint the *output* bytes (like zola/hugo). A
        // `File` keeps only the source path — stream-copied at emit time, never
        // held in memory; `Sass` holds its (small) compiled CSS.
        let (emit, hash) = match spec {
            AssetSpec::File { .. } => (Emit::Copy(on_disk), hash128(&bytes)),
            AssetSpec::Sass { .. } => {
                let css = compile_sass(&bytes, &on_disk)?;
                let css = Bytes::new(css.into_bytes());
                let hash = hash128(&css);
                (Emit::Bytes(css), hash)
            }
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
            emit,
        })
    }

    /// Consume the resolver, yielding every resolved asset (for emission).
    pub fn into_resolved(self) -> Vec<ResolvedAsset> {
        self.resolved.into_values().collect()
    }
}

/// Compile Sass/SCSS source to CSS via grass. `@use`/`@import` resolve against
/// the source file's directory on disk — those imports aren't yet seen by the
/// watcher (that lands with dependency tracking); the entry file is, since it's
/// read through the tracked World.
fn compile_sass(source: &Bytes, on_disk: &Path) -> SourceResult<String> {
    let src = std::str::from_utf8(source).map_err(|e| {
        eco_vec![SourceDiagnostic::error(
            Span::detached(),
            eco_format!("sass source is not valid UTF-8: {e}"),
        )]
    })?;
    // Pick syntax by extension: `.sass` is the indented syntax, everything
    // else (`.scss`) is SCSS — grass's `from_string` would otherwise assume
    // SCSS for both.
    let syntax = match on_disk.extension().and_then(|e| e.to_str()) {
        Some("sass") => grass::InputSyntax::Sass,
        _ => grass::InputSyntax::Scss,
    };
    let mut options = grass::Options::default().input_syntax(syntax);
    if let Some(parent) = on_disk.parent() {
        options = options.load_path(parent);
    }
    grass::from_string(src.to_string(), &options).map_err(|e| {
        eco_vec![SourceDiagnostic::error(Span::detached(), eco_format!("sass: {e}"))]
    })
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

    /// Two *separate* compiles in one process (sharing comemo's global cache)
    /// must each resolve independently. This is the regression that the
    /// per-resolver epoch fixes: with a globally-constant sink hash, the second
    /// compile would hit the first's cached placeholder and never re-fire its
    /// discovery `send`, leaking the placeholder.
    #[test]
    fn separate_compiles_each_resolve() {
        let files: &[(&str, &str)] = &[
            ("content/index.typ", "#context link(asset.file(\"logo.svg\").url())[logo]"),
            ("content/logo.svg", "<svg/>"),
        ];
        let (ctx_a, _a) = site(files);
        let (html_a, _) = compile(&ctx_a);
        assert!(!html_a.contains("__twyla-asset-pending__"), "compile A leaked:\n{html_a}");

        let (ctx_b, _b) = site(files);
        let (html_b, _) = compile(&ctx_b);
        assert!(!html_b.contains("__twyla-asset-pending__"), "compile B leaked:\n{html_b}");
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
    fn map_hashes_by_content_sink_keys_on_epoch() {
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

        // The sink keys on epoch, not channel: same epoch → equal hash (stable
        // across a resolver's iterations); different epoch → different hash
        // (a fresh resolver must miss the cache, never reuse a stale send).
        let sink = |epoch, tx| TwylaAssetSink::sink.set(Value::dynamic(AssetSink { epoch, tx })).wrap();
        let (tx1, _r1) = unbounded::<AssetRequest>();
        let (tx2, _r2) = unbounded::<AssetRequest>();
        let (tx3, _r3) = unbounded::<AssetRequest>();
        let same_a: LazyHash<Style> = sink(7, tx1);
        let same_b: LazyHash<Style> = sink(7, tx2);
        let other: LazyHash<Style> = sink(8, tx3);
        assert_eq!(hash128(&same_a), hash128(&same_b), "same epoch must hash equal regardless of channel");
        assert_ne!(hash128(&same_a), hash128(&other), "distinct epochs must hash differently");
    }
}
