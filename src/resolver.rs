//! The shared resolver: one place that owns every built asset and discovered
//! document for a compile, plus the on-disk dependency tracking ([`Upstream`] /
//! [`mtime`]) and source-change revalidation both kinds share.
//!
//! Assets and documents are deliberately *not* unified behind a trait. They
//! share a lot of bookkeeping — which lives here — but differ fundamentally in
//! shape: an asset is content-addressed by its [`AssetSpec`] and its output
//! path/URL is a *derived* naming layer (and 1:N once per-request output paths
//! land), whereas a document is 1:1 with its `output`, which is part of its own
//! spec. So the design is one struct, two maps, shared helpers — not a generic.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use std::{fs, io};

use comemo::Tracked;
use iddqd::IdHashMap;
use typst::World;
use typst::diag::SourceResult;
use typst::syntax::VirtualRoot;
use typst_utils::hash128;

use crate::asset::{AssetReq, AssetSpec, ResolvedAsset, file, image, raw, sass, typst_doc};
use crate::document::{DocumentReq, ResolvedDocument};
use crate::project::TwylaContext;

/// Owns every resolved asset and discovered document for a build.
///
/// The caller drives it: a one-shot build uses a fresh resolver; `serve` reuses
/// one across recompiles so its stores persist (and [`revalidate`] evicts what
/// changed). Both maps are `Arc`-backed so each fixed-point iteration can
/// snapshot the whole set into a `TwylaIntrospector` (and the convergence
/// history) with a cheap refcount bump — [`Arc::make_mut`] pays the copy only
/// when a map actually grows.
pub struct Resolver {
    ctx: TwylaContext,
    /// Every asset built so far, keyed by its [`AssetSpec`].
    assets: Arc<IdHashMap<ResolvedAsset>>,
    /// Every document discovered so far, keyed by its `output` path.
    documents: Arc<IdHashMap<ResolvedDocument>>,
}

impl Resolver {
    /// A fresh, empty resolver. The stores fill as assets/documents are
    /// discovered and persist for the resolver's lifetime.
    pub fn new(ctx: &TwylaContext) -> Self {
        Self {
            ctx: ctx.clone(),
            assets: Arc::new(IdHashMap::new()),
            documents: Arc::new(IdHashMap::new()),
        }
    }

    // -- snapshots: cheap Arc clones for the introspector + convergence history -

    /// A cheap snapshot of the current asset set (refcount bump).
    pub fn assets_snapshot(&self) -> Arc<IdHashMap<ResolvedAsset>> {
        Arc::clone(&self.assets)
    }

    /// A cheap snapshot of the current document set (refcount bump).
    pub fn documents_snapshot(&self) -> Arc<IdHashMap<ResolvedDocument>> {
        Arc::clone(&self.documents)
    }

    // -- accessors for emission and listings --------------------------------

    /// Every currently-resolved asset (for emission).
    pub fn resolved_assets(&self) -> impl Iterator<Item = &ResolvedAsset> {
        self.assets.iter()
    }

    /// Every collected document's request row, in deterministic (`output`-keyed)
    /// order (for the `documents()` listing and final harvest).
    pub fn documents(&self) -> impl Iterator<Item = &DocumentReq> {
        self.documents.iter().map(|stored| &stored.doc)
    }

    // -- discovery: build/collect on demand, deduping by key ----------------

    /// Ensure `req`'s spec is built and stored. A no-op if it already is, so
    /// repeated requests for the same spec across iterations (or call sites)
    /// resolve exactly once.
    pub fn resolve_asset(
        &mut self,
        world: Tracked<dyn World + '_>,
        req: &AssetReq,
    ) -> SourceResult<()> {
        if self.assets.contains_key(&req.spec) {
            return Ok(());
        }
        let asset = self.build_asset(world, &req.spec, req.span)?;
        Arc::make_mut(&mut self.assets).insert_overwrite(asset);
        Ok(())
    }

    /// Ensure `req`'s document is collected and stored, deduping by `output` —
    /// so a document that is both shown inline and `.url()`'d, or `.url()`'d
    /// repeatedly, is emitted exactly once.
    pub fn collect_document(&mut self, req: &DocumentReq) {
        if self.documents.contains_key(req.output.as_str()) {
            return;
        }
        let resolved = self.resolve_document(req);
        Arc::make_mut(&mut self.documents).insert_overwrite(resolved);
    }

    /// Process one spec into a [`ResolvedAsset`]: dispatch to the per-type build,
    /// then add the shared fingerprinted output path + URL.
    fn build_asset(
        &self,
        world: Tracked<dyn World + '_>,
        spec: &AssetSpec,
        span: typst::syntax::Span,
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

    /// Turn a discovered document request into a stored record: bolt on the
    /// watched source ([`Upstream`], for invalidation) and a content hash.
    fn resolve_document(&self, req: &DocumentReq) -> ResolvedDocument {
        ResolvedDocument {
            content_hash: hash128(&req.body),
            upstream: self.doc_source(req),
            doc: req.clone(),
        }
    }

    /// The on-disk source of a discovered document, for invalidation. Only
    /// project files have a watched path; package files are immutable and a
    /// detached span has no file, so both yield `None`.
    fn doc_source(&self, req: &DocumentReq) -> Option<Upstream> {
        let id = req.source?;
        match id.root() {
            VirtualRoot::Project => {
                Some(Upstream::new_lazy(self.ctx.root.join(id.vpath().get_without_slash())))
            }
            VirtualRoot::Package(_) => None,
        }
    }

    // -- invalidation -------------------------------------------------------

    /// Evict assets *and* documents whose on-disk sources changed since they
    /// were resolved (mtime check). Returns whether anything was evicted. A
    /// no-op on the first compile (empty stores); called at the top of each
    /// compile so what survives seeds the next one.
    pub fn revalidate(&mut self) -> bool {
        let assets = self.evict_assets_where(|asset| {
            asset
                .built
                .upstream
                .iter()
                .any(|u| mtime(&u.path).ok() != u.mtime)
        });
        let docs = self.evict_docs_where(|stored| match &stored.upstream {
            Some(u) => mtime(&u.path).ok() != u.mtime,
            None => false,
        });
        assets || docs
    }

    /// Evict every resolved asset matching `pred`. Returns whether anything was
    /// removed. The path-free invalidation seam: when a path-aware watcher
    /// lands, add an `invalidate(&changed_paths)` that calls this with an
    /// intersection predicate instead of stat'ing every upstream.
    fn evict_assets_where(&mut self, mut pred: impl FnMut(&ResolvedAsset) -> bool) -> bool {
        let before = self.assets.len();
        if before == 0 {
            return false;
        }
        Arc::make_mut(&mut self.assets).retain(|asset| !pred(&asset));
        self.assets.len() != before
    }

    /// Evict every collected document matching `pred`. Returns whether anything
    /// was removed.
    fn evict_docs_where(&mut self, mut pred: impl FnMut(&ResolvedDocument) -> bool) -> bool {
        let before = self.documents.len();
        if before == 0 {
            return false;
        }
        Arc::make_mut(&mut self.documents).retain(|stored| !pred(&stored));
        self.documents.len() != before
    }
}

/// One on-disk file an asset or document depends on, with its mtime at resolve
/// time (for [`Resolver::revalidate`]).
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
        f.read_to_string(&mut text).map_err(|e| annotate(&path, e))?;
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
pub fn annotate(path: &Path, err: io::Error) -> io::Error {
    let shown = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    io::Error::new(err.kind(), format!("{}: {err}", shown.display()))
}

/// Last-modified time of an on-disk path.
pub fn mtime(path: &Path) -> io::Result<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified())
}
