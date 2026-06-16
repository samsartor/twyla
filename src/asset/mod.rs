// `AssetSpec::Raw` carries `Bytes`, whose `Arc` refcount reads to clippy as
// interior mutability — but the spec's `Hash`/`Eq` are purely content-based, so
// it's a sound `HashMap` key (the same reason typst hashes `Bytes` freely).
#![allow(clippy::mutable_key_type)]

pub(crate) mod file;
pub(crate) mod image;
mod raw;
pub(crate) mod sass;
pub(crate) mod typst_doc;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use std::{fs, io};

use comemo::Tracked;
use crossbeam_channel::{Receiver, Sender, unbounded};
use ecow::{EcoString, eco_format, eco_vec};
use iddqd::{IdHashItem, IdHashMap};
use typst::World;
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{
    Binding, Bytes, Content, Module, PathOrStr, Repr, Scope, Str, Style, StyleChain, Value, elem,
    ty,
};
use typst::introspection::{History, Introspect, Introspector};
use typst::loading::{Encoding, Readable};
use typst::syntax::{FileId, Span, VirtualRoot};
use typst::utils::LazyHash;
use typst_utils::hash128;

use crate::document::{DiscoveredDoc, DocumentSink, ResolvedDocument, TwylaDocumentSink};
use crate::project::TwylaContext;
use crate::render::Emit;

// ---------------------------------------------------------------------------
// Keys: AssetSpec -> AssetRequest -> Asset
// ---------------------------------------------------------------------------

/// What produces an asset's bytes — the cache/store key. An asset's output is
/// a pure function of its spec, so this is what every map keys on.
#[derive(Clone, PartialEq, Hash, Debug)]
pub enum AssetSpec {
    File {
        file: FileId,
    },
    Sass {
        file: FileId,
        minify: bool,
    },
    Raw {
        bytes: Bytes,
        /// Output extension (drives the static server's Content-Type), sniffed
        /// by the caller; `None` → `bin`.
        ext: Option<EcoString>,
    },
    Image {
        source: ImageSource,
        width: Option<u32>,
        height: Option<u32>,
        fit: image::Fit,
        filter: image::Filter,
        /// `None` keeps the source format.
        format: Option<image::Format>,
        quality: u8,
    },
    Typst {
        input: typst_doc::TypstInput,
        format: typst_doc::Format,
        /// `png` resolution in pixels per inch. Normalized to `0` for the other
        /// formats, so it never fragments their output.
        ppi: i64,
    },
}

/// Where a processed image's source bytes come from.
#[derive(Clone, PartialEq, Hash, Debug)]
pub enum ImageSource {
    /// A project file, read from disk (and watched). Fingerprinted by content.
    File(FileId),
    /// In-memory bytes (an inline image), content-addressed by the bytes.
    Bytes(Bytes),
}

impl Eq for AssetSpec {}

#[derive(Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum OutputReq {
    /// The user wants a URL, but any URL will do.
    Url,
    /// The user wants to read the asset bytes directly.
    Read,
}

/// A spec plus the call site that requested it, and its future output policy.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AssetReq {
    pub spec: AssetSpec,
    ///  carried so a failed [`build`](file::build) (missing file, sass error)
    /// can blame a real source location instead of `<detached>`
    pub span: Span,
    /// How the asset is going to be used.
    pub outputs: BTreeSet<OutputReq>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AssetReqIntrospect(pub AssetReq);

pub fn hash_spec(spec: &AssetSpec) -> u128 {
    hash128(&("__twyla_asset__", &spec))
}

impl Introspect for AssetReqIntrospect {
    type Output = Option<ResolvedAsset>;

    fn introspect(
        &self,
        _engine: &mut typst::engine::Engine,
        introspector: Tracked<dyn Introspector + '_>,
    ) -> Self::Output {
        let Some(Value::Dyn(value)) = Introspector::value(&*introspector, hash_spec(&self.0.spec))
        else {
            return None;
        };
        value.downcast().cloned()
    }

    fn diagnose(&self, history: &History<Self::Output>) -> SourceDiagnostic {
        todo!()
    }
}

// Assets are *elements* ([`file::FileAsset`], [`sass::SassAsset`]), not a
// single handle type. Each element captures its source fields (so
// `#set asset.sass(minify: false)` works) and exposes the same two contextual
// scope methods — `.url()` and `.read()` — that build an [`AssetSpec`] from
// those fields and resolve it off the style chain. The element→spec→resolve
// machinery they share lives here: [`resolve_or_request`] / [`read_or_request`]
// (the placeholder protocol) and [`unresolved_asset_show`] (the default show
// rule, which refuses to render a bare asset). Discovery is *lazy*: it happens
// only when `.url()`/`.read()` actually run, so an asset that's never used is
// never built. See the module docs and [[twyla_element_scope_methods_spike]].

/// The placeholder protocol every `asset.*` consumer obeys, in one place: look
/// the spec up in the injected resolved-map ([`TwylaAssetMap`]) and either
/// answer from the hit or request it on a miss.
///
/// - **Hit** — the map already has this spec's [`ResolvedAsset`]; `hit` reads
///   what it needs off it (URL, bytes, …). This tracked read is what makes the
///   page *converge*.
/// - **Miss** — report the spec on the write-only discovery sink
///   ([`TwylaAssetSink`]; its epoch is in the comemo key, the channel isn't) and
///   return `miss`. A later relayout iteration re-runs this against the
///   now-populated map.
///
/// Used off `context.styles()` (the contextual methods) or a show rule's live
/// `styles` directly (the native image rule — no `#context` needed).
fn resolve_with<T>(
    styles: StyleChain,
    spec: &AssetSpec,
    span: Span,
    hit: impl FnOnce(&ResolvedAsset) -> T,
    miss: impl FnOnce() -> T,
) -> T {
    if let Value::Dyn(dynamic) = styles.get_cloned(TwylaAssetMap::map)
        && let Some(map) = dynamic.downcast::<ResolvedAssets>()
        && let Some(asset) = map.get(spec)
    {
        return hit(asset);
    }

    if let Value::Dyn(dynamic) = styles.get_cloned(TwylaAssetSink::sink)
        && let Some(sink) = dynamic.downcast::<AssetSink>()
    {
        let _ = sink.tx.send(AssetReq {
            spec: spec.clone(),
            span,
        });
    }
    miss()
}

/// Resolve a spec's URL off the style chain, or request it (returns
/// [`ASSET_PENDING`] on a miss). Shared by the asset elements' `.url()` and the
/// native image rule's vector path.
pub(crate) fn resolve_or_request(styles: StyleChain, spec: &AssetSpec, span: Span) -> Str {
    resolve_with(
        styles,
        spec,
        span,
        |asset| Str::from(asset.url.as_str()),
        || Str::from(ASSET_PENDING),
    )
}

/// A resolved image: its URL plus the output's intrinsic pixel dimensions (when
/// known). Returned by [`resolve_image_or_request`] for the native image rule.
pub(crate) struct ResolvedImage {
    pub url: Str,
    pub dimensions: Option<(u32, u32)>,
}

/// Resolve a raster-image spec to its URL *and* output dimensions off the style
/// chain, or request it on a miss. Like [`resolve_or_request`] but also surfaces
/// `built.dimensions` so the native rule can emit `<img width height>`.
pub(crate) fn resolve_image_or_request(
    styles: StyleChain,
    spec: &AssetSpec,
    span: Span,
) -> ResolvedImage {
    resolve_with(
        styles,
        spec,
        span,
        |asset| ResolvedImage {
            url: Str::from(asset.url.as_str()),
            dimensions: asset.built.dimensions,
        },
        || ResolvedImage {
            url: Str::from(ASSET_PENDING),
            dimensions: None,
        },
    )
}

/// Resolve a spec's output off the style chain, or request it (returns an empty
/// value on a miss — the placeholder, discarded before convergence). Backs
/// [`Asset::read`]; `encoding` selects `str` (UTF-8) vs raw `bytes`, mirroring
/// the native `read`. The bytes come from `built.emit`, so a path-backed asset
/// is read from disk here rather than held in RAM.
fn read_or_request(
    styles: StyleChain,
    spec: &AssetSpec,
    span: Span,
    encoding: Option<Encoding>,
) -> HintedStrResult<Readable> {
    resolve_with(
        styles,
        spec,
        span,
        |asset| {
            let bytes = asset
                .built
                .emit
                .read()
                .map_err(|err| eco_format!("failed to read asset bytes: {err}"))?;
            decode(bytes, encoding)
        },
        || Ok(decode_empty(encoding)),
    )
}

/// Apply `read`'s `encoding` to resolved bytes: `none` → raw bytes, UTF-8 →
/// decode to a string (erroring on invalid UTF-8, like the native `read`).
fn decode(bytes: Bytes, encoding: Option<Encoding>) -> HintedStrResult<Readable> {
    match encoding {
        None => Ok(Readable::Bytes(bytes)),
        Some(Encoding::Utf8) => {
            Ok(Readable::Str(bytes.to_str().map_err(|err| {
                eco_format!("asset is not valid UTF-8: {err}")
            })?))
        }
    }
}

/// The empty placeholder returned on a miss, of the type `encoding` selects.
fn decode_empty(encoding: Option<Encoding>) -> Readable {
    match encoding {
        None => Readable::Bytes(Bytes::new(Vec::<u8>::new())),
        Some(Encoding::Utf8) => Readable::Str(Str::from("")),
    }
}

/// Placeholder URL returned for an as-yet-unresolved asset. By convergence
/// every consumer has re-run against a populated map, so this never survives
/// into output (a leak means non-convergence — a bug).
const ASSET_PENDING: &str = "/__twyla-asset-pending__";

/// Resolve an asset element's `path` field to the `FileId` it names, relative
/// to `span`'s file (the file the asset call was written in — mirrors how
/// `read`/`image` resolve their paths). Shared by the element `.url()`/`.read()`
/// methods, which resolve lazily off the element's own span.
pub(crate) fn resolve_path(path: &PathOrStr, span: Span) -> HintedStrResult<FileId> {
    Ok(path.resolve_if_some(span.id())?.intern())
}

/// Default show for a bare asset element: refuse to render. An asset exists to
/// hand back a `url`/bytes; one that reaches realization was never resolved
/// (`.url()`/`.read()` consume the element before it can be shown), so there is
/// nothing meaningful to render. Per-element `ShowFn`s delegate here.
pub(crate) fn show_unresolved(span: Span, name: &str) -> SourceResult<Content> {
    Err(eco_vec![
        SourceDiagnostic::error(span, eco_format!("`asset.{name}` cannot be shown directly"))
            .with_hint(eco_format!(
                "resolve it inside a `#context` block — `.url()` to link it, `.read()` to inline it"
            ))
    ])
}

/// Build the `asset` module (`asset.file`, `asset.sass`) for the global scope.
/// Each name binds an *element* so `#set asset.sass(..)` works (see
/// [`file::FileAsset`] / [`sass::SassAsset`]).
pub fn module() -> Module {
    let mut scope = Scope::new();
    scope.define_elem::<file::FileAsset>();
    scope.define_elem::<sass::SassAsset>();
    scope.define_elem::<image::ImageAsset>();
    scope.define_elem::<typst_doc::TypstAsset>();
    Module::new("asset", scope)
}

/// Bind the `asset` module into a global scope. Called from [`crate::prelude`].
pub fn install(global: &mut Scope) {
    global.bind("asset".into(), Binding::detached(Value::Module(module())));
}

// ---------------------------------------------------------------------------
// Per-type build output + resolved record
// ---------------------------------------------------------------------------

/// What a per-type `build` ([`file::build`], [`sass::build`]) produces. The
/// resolver turns it into a [`ResolvedAsset`] by adding the fingerprinted name,
/// output path, and URL (all shared logic).
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    /// How the bytes reach the output.
    pub emit: Emit,
    /// Every on-disk file the build read (entry + transitive imports). Drives
    /// invalidation and the watcher's dependency set.
    pub upstream: Vec<Upstream>,
    /// Content hash of the *output* bytes (the fingerprint).
    pub content_hash: u128,
    /// Output extension (`css` for sass, the source ext for a copy)
    pub ext: Option<String>,
    /// The name of the original asset file, if any.
    pub stem: Option<String>,
    /// Intrinsic pixel dimensions of the output, when it's a raster image
    /// ([`image::build`]) — lets the native image rule emit `<img width height>`
    /// for aspect-ratio reservation. `None` for non-image assets.
    pub dimensions: Option<(u32, u32)>,
}

impl Hash for Built {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.content_hash.hash(state);
        self.stem.hash(state);
        if let Emit::Copy(path) = &self.emit {
            path.hash(state);
        }
    }
}

/// One on-disk file an asset depends on, with its mtime at resolve time (for
/// [`AssetResolver::revalidate`]).
#[derive(Clone, Debug, PartialEq, PartialOrd, Ord, Eq)]
pub struct Upstream {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
}

impl Upstream {
    pub fn new_lazy(path: PathBuf) -> Self {
        Upstream {
            mtime: mtime(&path).ok(),
            path,
        }
    }

    pub fn new_read_string(path: PathBuf) -> io::Result<(Self, String)> {
        let mut f = fs::File::open(&path).map_err(|e| annotate(&path, e))?;
        let mtime = f
            .metadata()
            .map_err(|e| annotate(&path, e))?
            .modified()
            .ok();
        let mut text = String::new();
        f.read_to_string(&mut text)
            .map_err(|e| annotate(&path, e))?;
        Ok((Upstream { path, mtime }, text))
    }

    pub fn new_read_bytes(path: PathBuf) -> io::Result<(Self, Vec<u8>)> {
        let mut f = fs::File::open(&path).map_err(|e| annotate(&path, e))?;
        let mtime = f
            .metadata()
            .map_err(|e| annotate(&path, e))?
            .modified()
            .ok();
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).map_err(|e| annotate(&path, e))?;
        Ok((Upstream { path, mtime }, bytes))
    }
}

/// Annotate an I/O error with the absolute path it concerns, so a failed asset
/// read reports *which* file (`/abs/path: No such file or directory`) instead of
/// a bare OS error. `std::path::absolute` resolves the path against the cwd
/// without touching the filesystem (so it works even when the file is missing).
fn annotate(path: &Path, err: io::Error) -> io::Error {
    let shown = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    io::Error::new(err.kind(), format!("{}: {err}", shown.display()))
}

/// One processed asset: where it lives, how to emit it, and what it depends on.
///
/// This is the value carried on the style chain (inside [`ResolvedAssets`]), so
/// `asset.*().url()` / `.read()` read straight off it. Its `built.emit` is
/// either in-memory bytes (Arc-shared, cheap to clone onto the chain) or a path
/// the bytes are read from on demand — so injecting the whole record never pulls
/// asset bytes into RAM.
#[ty]
#[derive(Clone, Debug, PartialEq, Hash)]
pub struct ResolvedAsset {
    /// The spec that produced it (its key).
    pub spec: AssetSpec,
    /// The built asset
    pub built: Built,
    /// Bundle-relative output path, e.g. `assets/main-<hash>.css`.
    pub output_path: String,
    /// The public, root-relative URL `.url()` returns (`output_path` resolved
    /// through [`TwylaContext::asset_url`], so it folds in any `base_url`).
    pub url: String,
}

impl Repr for ResolvedAsset {
    fn repr(&self) -> EcoString {
        todo!()
    }
}

impl IdHashItem for ResolvedAsset {
    type Key<'a> = &'a AssetSpec;

    fn key(&self) -> &AssetSpec {
        &self.spec
    }

    iddqd::id_upcast!();
}

impl ResolvedAsset {
    /// The on-disk source files this asset depends on — for the serve watcher.
    pub fn upstream_paths(&self) -> impl Iterator<Item = &Path> {
        self.built.upstream.iter().map(|u| u.path.as_path())
    }
}

// ---------------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------------

pub struct AssetResolver {
    epoch: u64,
    ctx: TwylaContext,
    tx: Sender<AssetReq>,
    rx: Receiver<AssetReq>,
    pub resolved: IdHashMap<ResolvedAsset>,
    pub documents: IdHashMap<ResolvedDocument>,
}

impl AssetResolver {
    /// A fresh, empty resolver. The asset and document stores fill as they're
    /// discovered and persist for the resolver's lifetime.
    pub fn new(ctx: &TwylaContext) -> Self {
        let (tx, rx) = unbounded();
        let (doc_tx, doc_rx) = unbounded();
        Self {
            epoch: EPOCH.fetch_add(1, Ordering::Relaxed),
            ctx: ctx.clone(),
            tx,
            rx,
            resolved: HashMap::new(),
            doc_tx,
            doc_rx,
            documents: BTreeMap::new(),
        }
    }

    /// Resolve a discovered document's source file to an on-disk path + mtime
    /// for invalidation. Only project files have a watched path; package files
    /// are immutable, so they need none (`None`).
    fn doc_source(&self, doc: &DiscoveredDoc) -> (Option<PathBuf>, Option<SystemTime>) {
        let Some(id) = doc.source else {
            return (None, None);
        };
        match id.root() {
            VirtualRoot::Project => {
                let path = self.ctx.root.join(id.vpath().get_without_slash());
                let mtime = mtime(&path).ok();
                (Some(path), mtime)
            }
            VirtualRoot::Package(_) => (None, None),
        }
    }

    /// Every collected document, in deterministic (`output`-keyed) order.
    pub fn documents(&self) -> impl Iterator<Item = &DiscoveredDoc> {
        self.documents.values().map(|stored| &stored.doc)
    }

    /// Process one spec into a [`ResolvedAsset`]: dispatch to the per-type
    /// build, then add the shared fingerprinted name / output path / URL.
    fn process(
        &self,
        world: Tracked<dyn World + '_>,
        spec: &AssetSpec,
        span: Span,
    ) -> SourceResult<ResolvedAsset> {
        let built = match spec {
            AssetSpec::File { file } => file::build(world, *file, &self.ctx, span)?,
            AssetSpec::Sass { file, minify } => {
                sass::build(world, *file, *minify, &self.ctx, span)?
            }
            // `Raw` is in-memory bytes — it can't fail, so it needs no span.
            AssetSpec::Raw { bytes, ext } => raw::build(bytes.clone(), ext.clone()),
            AssetSpec::Image {
                source,
                width,
                height,
                fit,
                filter,
                format,
                quality,
            } => image::build(
                source, *width, *height, *fit, *filter, *format, *quality, &self.ctx, span,
            )?,
            AssetSpec::Typst { input, format, ppi } => {
                typst_doc::build(world, input, *format, *ppi, &self.ctx, span)?
            }
        };

        let output_path = self.ctx.default_asset_output(&built);
        let url = self.ctx.asset_url(&output_path);

        Ok(ResolvedAsset {
            spec: spec.clone(),
            built,
            output_path,
            url,
        })
    }

    /// Bump the discovery epoch, invalidating the comemo discovery generation so
    /// the next compile re-discovers (re-sends on) both sinks cleanly. One epoch
    /// covers assets and documents, so a change to either re-discovers both (see
    /// [`AssetSink`] / [`DocumentSink`](crate::document::DocumentSink)).
    fn bump_epoch(&mut self) {
        self.epoch = EPOCH.fetch_add(1, Ordering::Relaxed);
    }

    /// Evict every resolved asset matching `pred`. Returns whether anything was
    /// removed (the caller bumps the epoch). The path-free invalidation seam:
    /// when a path-aware watcher lands, add an `invalidate(&changed_paths)` that
    /// calls this with an intersection predicate instead of stat'ing every
    /// upstream.
    fn evict_assets_where(&mut self, mut pred: impl FnMut(&ResolvedAsset) -> bool) -> bool {
        let before = self.resolved.len();
        self.resolved.retain(|_, asset| !pred(asset));
        self.resolved.len() != before
    }

    /// Evict every collected document matching `pred`. Returns whether anything
    /// was removed (the caller bumps the epoch).
    fn evict_docs_where(&mut self, mut pred: impl FnMut(&StoredDoc) -> bool) -> bool {
        let before = self.documents.len();
        self.documents.retain(|_, stored| !pred(stored));
        self.documents.len() != before
    }

    /// Evict assets *and* documents whose on-disk sources changed since they
    /// were resolved (mtime check), bumping the discovery epoch once if anything
    /// went. Called at the top of each compile; a no-op on the first compile
    /// (empty stores). Returns whether anything was evicted.
    pub fn revalidate(&mut self) -> bool {
        let assets = self.evict_assets_where(|asset| {
            asset
                .built
                .upstream
                .iter()
                .any(|u| mtime(&u.path).ok() != u.mtime)
        });
        let docs = self.evict_docs_where(|stored| match &stored.source {
            Some(path) => mtime(path).ok() != stored.mtime,
            None => false,
        });
        if assets || docs {
            self.bump_epoch();
        }
        assets || docs
    }

    /// A snapshot of every currently-resolved asset (for emission).
    pub fn resolved_assets(&self) -> impl Iterator<Item = &ResolvedAsset> {
        self.resolved.iter()
    }
}

/// Last-modified time of an on-disk path
fn mtime(path: &Path) -> io::Result<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified())
}

#[cfg(test)]
mod tests;
