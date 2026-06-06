//! The asset system: typed constructors, the discovery side-channel, and the
//! resolver that drives asset URLs to convergence inside the compile loop.
//!
//! Per-asset-type logic lives in submodules ([`file`], [`sass`]); this module
//! owns the shared machinery: the [`AssetSpec`] key, the [`Asset`] handle and
//! its contextual `.url()`, the style-chain carriers, and the [`AssetResolver`]
//! (discovery channel + persistent store + naming/fingerprint/URL assembly).
//!
//! Mechanism (proven in the spike, jj `rtnknlwo`): an asset's URL is resolved
//! through the *realization* fixed-point loop, split into two halves with
//! opposite comemo requirements.
//!
//! - **Resolved-map — tracked/hashed (data IN, drives convergence).**
//!   [`ResolvedAssets`] (`AssetSpec -> ResolvedAsset`) rides the relayout style
//!   chain via [`TwylaAssetMap`]. The full record rides the chain so `.url()`
//!   *and* `.read()` answer off it; its `Hash`/`Eq` key only on each entry's
//!   `(spec, content_hash, url)`, never the bytes (the `content_hash`
//!   fingerprint stands in for them — see [`ResolvedAssets`]). The `StyleChain`
//!   is hashed *by value* into typst's `realize` memo key, so injecting a
//!   more-complete map is a comemo miss for every `asset.*` consumer — which is
//!   what re-resolves them. Its `Hash` is *order-independent* so the same entry
//!   set hashes identically regardless of discovery order (lets a warm rebuild
//!   reuse the cache).
//! - **Discovery sink — per-generation epoch (specs OUT, drives discovery).**
//!   [`AssetSink`] wraps a write-only [`crossbeam_channel::Sender`] and rides
//!   the chain via [`TwylaAssetSink`]. Its `Hash` keys on the resolver's
//!   *epoch* — a discovery generation: constant while the resolved set only
//!   grows (so across iterations only the map moves the comemo key), but
//!   **bumped on every eviction** ([`AssetResolver::evict_where`]). That matters
//!   for a *persistent* resolver: evicting an asset can return the map to a
//!   previously-cached state (e.g. editing the only stylesheet → empty map),
//!   and without a fresh epoch comemo would replay the old cached
//!   `placeholder + send`, skipping the discovery `send` (a side effect inside
//!   memoized code) and starving re-discovery.

// `AssetSpec::Raw` carries `Bytes`, whose `Arc` refcount reads to clippy as
// interior mutability — but the spec's `Hash`/`Eq` are purely content-based, so
// it's a sound `HashMap` key (the same reason typst hashes `Bytes` freely).
#![allow(clippy::mutable_key_type)]

mod file;
mod raw;
mod sass;

use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use std::{fs, io};

use comemo::Tracked;
use crossbeam_channel::{Receiver, Sender, unbounded};
use ecow::{EcoString, eco_format};
use typst::World;
use typst::diag::{At, HintedStrResult, SourceResult};
use typst::foundations::{
    Binding, Bytes, Context, Module, PathOrStr, Repr, Scope, Str, Style, StyleChain, Value, elem,
    func, scope, ty,
};
use typst::loading::{Encoding, Readable};
use typst::syntax::{FileId, Spanned};
use typst::utils::LazyHash;
use typst_utils::hash128;

use crate::render::Emit;
use crate::project::TwylaContext;

// ---------------------------------------------------------------------------
// Keys: AssetSpec -> AssetRequest -> Asset
// ---------------------------------------------------------------------------

/// What produces an asset's bytes — the cache/store key. An asset's output is
/// a pure function of its spec, so this is what every map keys on.
///
/// `File`/`Sass` are FileId-based on purpose: `FileId` is `Eq + Hash` (usable
/// as a `HashMap` key). `Raw` instead carries `Bytes` directly — typst's
/// `Bytes` is `Hash + PartialEq` but **not `Eq`**, so the derive of `Eq` is
/// replaced by a hand-written marker impl (sound because `Bytes`' `PartialEq`
/// is reflexive; see [`impl Eq`](#impl-Eq)). When the user-facing `asset.raw`
/// lands we'll likely unify the on-disk variants behind `loading::DataSource`;
/// likely to grow into a trait once there are more variants, an enum for now.
#[derive(Clone, PartialEq, Hash, Debug)]
pub enum AssetSpec {
    /// Copy a project file verbatim (fingerprinted). See [`file`].
    File { file: FileId },
    /// Compile a Sass/SCSS file to CSS. See [`sass`]. `minify` selects grass's
    /// compressed output style; it's part of the key so the minified and
    /// expanded builds of the same file resolve to distinct assets.
    Sass { file: FileId, minify: bool },
    /// Emit in-memory bytes verbatim (fingerprinted), content-addressed by the
    /// bytes themselves. Used today by the image rule for inline/byte-source
    /// images that have no project `FileId`. See [`raw`].
    Raw {
        bytes: Bytes,
        /// Output extension (drives the static server's Content-Type), sniffed
        /// by the caller; `None` → `bin`.
        ext: Option<EcoString>,
    },
}

/// Marker `Eq` for the `Raw` variant's non-`Eq` `Bytes` field. Sound because
/// every variant's `PartialEq` is reflexive (`Bytes` compares by content, so
/// `b == b`), which is all `Eq` asserts beyond `PartialEq`.
impl Eq for AssetSpec {}

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
/// with `.url()` **inside a `#context` block** — that's where the injected
/// resolved-map is readable.
#[ty(scope)]
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Asset {
    request: AssetRequest,
}

impl Asset {
    /// Build a handle from a spec (used by the per-type constructors).
    pub(crate) fn new(spec: AssetSpec) -> Self {
        Self {
            request: AssetRequest { spec },
        }
    }
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
    ///   href: asset.sass("main.scss").url(),
    /// ))
    /// ```
    #[func(contextual)]
    fn url(&self, context: Tracked<Context>) -> HintedStrResult<Str> {
        Ok(resolve_or_request(context.styles()?, &self.request.spec))
    }

    /// The asset's resolved output (e.g. compiled CSS, or a file's contents) —
    /// for inlining instead of linking. Mirrors the native
    /// [`read`]($read): UTF-8 `str` by default, raw `bytes` with
    /// `encoding: none`.
    ///
    /// Contextual, like [`url`](Self::url) — call it inside `#context`:
    ///
    /// ```typ
    /// #context html.elem("style", asset.sass("main.scss").read())
    /// #context raw-html(asset.file("icon.svg").read())
    /// ```
    ///
    /// Reads path-backed assets from disk on demand, so it never holds the
    /// bytes in RAM longer than the call.
    #[func(contextual)]
    fn read(
        &self,
        context: Tracked<Context>,
        /// The encoding to read the asset with. If `{none}`, returns raw bytes;
        /// otherwise the bytes are decoded as UTF-8 into a string.
        #[named]
        #[default(Some(Encoding::Utf8))]
        encoding: Option<Encoding>,
    ) -> HintedStrResult<Readable> {
        read_or_request(context.styles()?, &self.request.spec, encoding)
    }
}

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
        let _ = sink.tx.send(AssetRequest { spec: spec.clone() });
    }
    miss()
}

/// Resolve a spec's URL off the style chain, or request it (returns
/// [`ASSET_PENDING`] on a miss). Shared by [`Asset::url`] and the native image
/// rule.
pub(crate) fn resolve_or_request(styles: StyleChain, spec: &AssetSpec) -> Str {
    resolve_with(
        styles,
        spec,
        |asset| Str::from(asset.url.as_str()),
        || Str::from(ASSET_PENDING),
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
    encoding: Option<Encoding>,
) -> HintedStrResult<Readable> {
    resolve_with(
        styles,
        spec,
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
        Some(Encoding::Utf8) => Ok(Readable::Str(
            bytes
                .to_str()
                .map_err(|err| eco_format!("asset is not valid UTF-8: {err}"))?,
        )),
    }
}

/// The empty placeholder returned on a miss, of the type `encoding` selects.
fn decode_empty(encoding: Option<Encoding>) -> Readable {
    match encoding {
        None => Readable::Bytes(Bytes::new(Vec::<u8>::new())),
        Some(Encoding::Utf8) => Readable::Str(Str::from("")),
    }
}

impl Repr for Asset {
    fn repr(&self) -> EcoString {
        EcoString::inline("asset(..)")
    }
}

/// Placeholder URL returned for an as-yet-unresolved asset. By convergence
/// every consumer has re-run against a populated map, so this never survives
/// into output (a leak means non-convergence — a bug).
const ASSET_PENDING: &str = "/__twyla-asset-pending__";

/// Resolve a path argument to the `FileId` it names, relative to the calling
/// file (mirrors how `read`/`image` resolve their paths). Shared by the
/// per-type constructors.
pub(crate) fn resolve_path(path: Spanned<PathOrStr>) -> SourceResult<FileId> {
    Ok(path
        .v
        .resolve_if_some(path.span.id())
        .at(path.span)?
        .intern())
}

/// Build the `asset` module (`asset.file`, `asset.sass`) for the global scope.
pub fn module() -> Module {
    let mut scope = Scope::new();
    scope.define_func::<file::file>();
    scope.define_func::<sass::sass>();
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

/// Host element carrying the discovery sink (the epoch-keyed half).
#[elem]
pub struct TwylaAssetSink {
    /// A [`Value::Dyn`] wrapping [`AssetSink`].
    #[default(Value::None)]
    pub sink: Value,
}

/// The resolved-asset map carried on the style chain: `AssetSpec ->
/// ResolvedAsset`. The full record rides the chain so both `.url()` and
/// `.read()` resolve off it; injecting it never loads asset bytes into RAM (see
/// [`ResolvedAsset`]).
///
/// `Hash`/`Eq` key on each entry's `(spec, content_hash, url)` — the only parts
/// that change observable output — **never** the bytes or path in `built.emit`.
/// Hashing the bytes would make this `O(total asset bytes)` per relayout
/// iteration (and bloat the comemo key); the `content_hash` fingerprint is a
/// cheap, sound stand-in for "the bytes". `Hash` is **order-independent**
/// (per-entry hashes sorted) so the same entry set hashes identically
/// regardless of discovery order, and `Eq` is written to match.
#[ty(name = "twyla-resolved-assets")]
#[derive(Clone, Debug)]
pub struct ResolvedAssets(HashMap<AssetSpec, ResolvedAsset>);

impl ResolvedAssets {
    fn get(&self, spec: &AssetSpec) -> Option<&ResolvedAsset> {
        self.0.get(spec)
    }

    /// The per-entry hash fingerprint — the only fields that affect observable
    /// output. Shared by `Hash` and `Eq` so the two stay consistent.
    fn entry_fingerprints(&self) -> Vec<u128> {
        let mut entries: Vec<u128> = self
            .0
            .iter()
            .map(|(spec, asset)| hash128(&(spec, asset.built.content_hash, &asset.url)))
            .collect();
        entries.sort_unstable();
        entries
    }
}

impl PartialEq for ResolvedAssets {
    fn eq(&self, other: &Self) -> bool {
        self.entry_fingerprints() == other.entry_fingerprints()
    }
}

impl Hash for ResolvedAssets {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.entry_fingerprints().hash(state);
    }
}

impl Repr for ResolvedAssets {
    fn repr(&self) -> EcoString {
        EcoString::inline("twyla-resolved-assets(..)")
    }
}

/// Write-only channel for reporting discovered asset requests out of the
/// otherwise-pure realization pass. Hash/Eq key on `epoch` (a discovery
/// generation), never the channel — see the module docs for why a constant
/// hash is unsound with a persistent resolver.
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
// Per-type build output + resolved record
// ---------------------------------------------------------------------------

/// What a per-type `build` ([`file::build`], [`sass::build`]) produces. The
/// resolver turns it into a [`ResolvedAsset`] by adding the fingerprinted name,
/// output path, and URL (all shared logic).
#[derive(Clone, Debug)]
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
        let mut f = fs::File::open(&path)?;
        let mtime = f.metadata()?.modified().ok();
        let mut text = String::new();
        f.read_to_string(&mut text)?;
        Ok((Upstream { path, mtime }, text))
    }

    pub fn new_read_bytes(path: PathBuf) -> io::Result<(Self, Vec<u8>)> {
        let mut f = fs::File::open(&path)?;
        let mtime = f.metadata()?.modified().ok();
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        Ok((Upstream { path, mtime }, bytes))
    }
}

/// One processed asset: where it lives, how to emit it, and what it depends on.
///
/// This is the value carried on the style chain (inside [`ResolvedAssets`]), so
/// `asset.*().url()` / `.read()` read straight off it. Its `built.emit` is
/// either in-memory bytes (Arc-shared, cheap to clone onto the chain) or a path
/// the bytes are read from on demand — so injecting the whole record never pulls
/// asset bytes into RAM.
#[derive(Clone, Debug)]
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

impl ResolvedAsset {
    /// The on-disk source files this asset depends on — for the serve watcher.
    pub fn upstream_paths(&self) -> impl Iterator<Item = &Path> {
        self.built.upstream.iter().map(|u| u.path.as_path())
    }
}

// ---------------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------------

/// Hands out a distinct base epoch per [`AssetResolver`] (so resolvers sharing
/// comemo's process-global cache don't collide) and a fresh epoch on each
/// eviction (so a persistent resolver re-discovers cleanly). See [`AssetSink`].
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Owns the discovery channel and a **persistent** resolved-asset store across
/// compiles (one per [`crate::render::RenderWorld`]). Between compiles,
/// [`revalidate`](Self::revalidate) evicts assets whose sources changed; what
/// survives seeds the next compile's map, so warm rebuilds reuse the realize
/// cache.
pub struct AssetResolver {
    epoch: u64,
    ctx: TwylaContext,
    tx: Sender<AssetRequest>,
    rx: Receiver<AssetRequest>,
    resolved: HashMap<AssetSpec, ResolvedAsset>,
}

impl AssetResolver {
    /// A fresh, empty resolver. The store fills as assets are discovered and
    /// persists for the resolver's lifetime.
    pub fn new(ctx: &TwylaContext) -> Self {
        let (tx, rx) = unbounded();
        Self {
            epoch: EPOCH.fetch_add(1, Ordering::Relaxed),
            ctx: ctx.clone(),
            tx,
            rx,
            resolved: HashMap::new(),
        }
    }

    /// The sink style — chained every iteration. Hashes on the current `epoch`.
    pub fn sink_style(&self) -> LazyHash<Style> {
        let sink = AssetSink {
            epoch: self.epoch,
            tx: self.tx.clone(),
        };
        TwylaAssetSink::sink.set(Value::dynamic(sink)).wrap()
    }

    /// The resolved-map style for the current store — rebuilt each iteration as
    /// the store drains. Clones the whole store onto the chain; that's cheap
    /// because `ResolvedAsset`'s bytes are Arc-shared or a path (never deep
    /// byte copies), and the map's `Hash` ignores them anyway.
    pub fn map_style(&self) -> LazyHash<Style> {
        TwylaAssetMap::map
            .set(Value::dynamic(ResolvedAssets(self.resolved.clone())))
            .wrap()
    }

    /// Drain the discovery channel and process each newly-seen request.
    /// Returns `true` if nothing new was resolved (assets are *settled*).
    pub fn drain_and_process(&mut self, world: Tracked<dyn World + '_>) -> SourceResult<bool> {
        let mut settled = true;
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

    /// Process one spec into a [`ResolvedAsset`]: dispatch to the per-type
    /// build, then add the shared fingerprinted name / output path / URL.
    fn process(
        &self,
        world: Tracked<dyn World + '_>,
        spec: &AssetSpec,
    ) -> SourceResult<ResolvedAsset> {
        let built = match spec {
            AssetSpec::File { file } => file::build(world, *file, &self.ctx)?,
            AssetSpec::Sass { file, minify } => {
                sass::build(world, *file, *minify, &self.ctx)?
            }
            AssetSpec::Raw { bytes, ext } => raw::build(bytes.clone(), ext.clone()),
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

    /// Evict every resolved asset matching `pred`. Bumps the discovery epoch if
    /// anything was removed, so the next compile re-discovers the evicted specs
    /// cleanly (see [`AssetSink`]). The single eviction core behind both
    /// [`revalidate`](Self::revalidate) and a future path-aware `invalidate`.
    fn evict_where(&mut self, mut pred: impl FnMut(&ResolvedAsset) -> bool) -> bool {
        let before = self.resolved.len();
        self.resolved.retain(|_, asset| !pred(asset));
        let evicted = self.resolved.len() != before;
        if evicted {
            self.epoch = EPOCH.fetch_add(1, Ordering::Relaxed);
        }
        evicted
    }

    /// Evict assets whose on-disk sources changed since they were resolved
    /// (mtime check). Called at the top of each compile; a no-op on the first
    /// compile (empty store). Returns whether anything was evicted.
    ///
    /// The path-free invalidation seam: when a path-aware watcher lands, add an
    /// `invalidate(&changed_paths)` that calls [`evict_where`](Self::evict_where)
    /// with an intersection predicate instead of stat'ing every upstream.
    pub fn revalidate(&mut self) -> bool {
        self.evict_where(|asset| {
            asset
                .built
                .upstream
                .iter()
                .any(|u| mtime(&u.path).ok() != u.mtime)
        })
    }

    /// A snapshot of every currently-resolved asset (for emission).
    pub fn resolved_assets(&self) -> Vec<ResolvedAsset> {
        self.resolved.values().cloned().collect()
    }
}

/// Last-modified time of an on-disk path
fn mtime(path: &Path) -> io::Result<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified())
}

#[cfg(test)]
mod tests;
