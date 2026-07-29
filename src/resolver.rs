//! The shared resolver: one place that owns every built asset and discovered
//! document for a compile, plus the on-disk dependency tracking ([`Upstream`] /
//! [`mtime`]) assets use for source-change revalidation.
//!
//! Assets and documents are deliberately *not* unified behind a trait. They
//! share a lot of bookkeeping — which lives here — but differ fundamentally:
//! an asset is content-addressed by its [`AssetSpec`], must be *built* (so it
//! persists across recompiles and is revalidated by mtime), and its output
//! path/URL is a derived naming layer. A document is "resolved" the moment it's
//! discovered (no build), keyed 1:1 by its `output`, and is cheap enough to
//! re-discover from scratch every compile — so it needs no caching or
//! revalidation at all. One struct, two maps, shared helpers — not a generic.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use std::{fs, io};

use comemo::Tracked;
use ecow::eco_vec;
use iddqd::IdHashMap;
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult};

use crate::asset::{
    AssetReq, AssetSpec, OutputReq, Resolution, ResolvedAsset, file, image, raw, sass, svg,
    typst_doc,
};
use crate::document::ResolvedDocument;
use crate::project::TwylaContext;

/// Owns every resolved asset and discovered document for a build.
///
/// The caller drives it: a one-shot build uses a fresh resolver; `serve` reuses
/// one across recompiles so the asset store persists (and [`revalidate`] evicts
/// what changed). Each *entry* is `Arc`-wrapped, so a fixed-point iteration
/// snapshots a whole map into a `TwylaIntrospector` (and the convergence
/// history) by cloning the table over shared `Arc`s — cheap — and mutating one
/// entry ([`Arc::make_mut`]) deep-copies just that entry, not the whole map.
pub struct Resolver {
    ctx: TwylaContext,
    /// Every asset built so far, keyed by its [`AssetSpec`].
    assets: IdHashMap<Arc<ResolvedAsset>>,
    /// Every document discovered so far, keyed by its `output` path.
    documents: IdHashMap<Arc<ResolvedDocument>>,
    /// Warnings pushed by asset builds this compile (e.g. a best-effort svg
    /// minify that fell back to verbatim). Drained into the sink by the
    /// compile loop after each discovery pass; a build only runs (and so only
    /// warns) when its spec isn't already cached.
    warnings: Vec<SourceDiagnostic>,
}

impl Resolver {
    /// A fresh, empty resolver. The stores fill as assets/documents are
    /// discovered and persist for the resolver's lifetime.
    pub fn new(ctx: &TwylaContext) -> Self {
        Self {
            ctx: ctx.clone(),
            assets: IdHashMap::new(),
            documents: IdHashMap::new(),
            warnings: Vec::new(),
        }
    }

    /// Drain the warnings asset builds queued since the last call.
    pub fn take_warnings(&mut self) -> Vec<SourceDiagnostic> {
        std::mem::take(&mut self.warnings)
    }

    // -- snapshots: cheap Arc clones for the introspector + convergence history -

    /// A cheap snapshot of the current asset set (refcount bump).
    pub fn assets_snapshot(&self) -> IdHashMap<Arc<ResolvedAsset>> {
        self.assets.clone()
    }

    /// A cheap snapshot of the current document set (refcount bump).
    pub fn documents_snapshot(&self) -> IdHashMap<Arc<ResolvedDocument>> {
        self.documents.clone()
    }

    // -- accessors for emission and listings --------------------------------

    /// Every currently-resolved asset, regardless of how it's used. This is the
    /// set whose source files must be *watched* (a `.read()`-only asset is never
    /// emitted but its source still affects the inlined output) — see
    /// [`emittable_assets`](Self::emittable_assets) for the narrower set actually
    /// written.
    pub fn resolved_assets(&self) -> impl Iterator<Item = &Arc<ResolvedAsset>> {
        self.assets.iter()
    }

    /// The assets actually written as files: one item per distinct resolved
    /// non-read output path. An asset only ever `.read()` (inlined into the HTML,
    /// never linked or explicitly emitted) is omitted — nothing references the
    /// file, so writing it would just litter the output dir.
    pub fn emittable_assets(&self) -> impl Iterator<Item = (String, Arc<ResolvedAsset>)> + '_ {
        self.assets.iter().flat_map(|asset| {
            let mut paths = Vec::<String>::new();
            for resolution in &asset.resolutions {
                if matches!(resolution.policy, OutputReq::Read) {
                    continue;
                }
                if !paths.contains(&resolution.output_path) {
                    paths.push(resolution.output_path.clone());
                }
            }
            paths.into_iter().map(|path| (path, Arc::clone(asset)))
        })
    }

    /// Every collected document, in deterministic (`output`-keyed) order (for
    /// the `documents()` listing and final harvest).
    pub fn documents(&self) -> impl Iterator<Item = &ResolvedDocument> {
        self.documents.iter().map(|stored| &**stored)
    }

    /// Forget every discovered document. Called once at the start of each
    /// compile: unlike assets (expensive to build, cached across recompiles),
    /// documents are cheap to re-discover and their source `.typ` files are
    /// already typst dependencies, so there's no reason to persist them — each
    /// compile rediscovers the full set fresh.
    pub fn clear_documents(&mut self) {
        self.documents = IdHashMap::new();
    }

    /// Forget how assets were used (clears each asset's `outputs`). Called once
    /// at the start of each compile: assets themselves persist across recompiles,
    /// but their usage is rebuilt from scratch as this compile re-discovers
    /// requests, so a call site that dropped its `.url()` stops emitting the file.
    pub fn clear_asset_usage(&mut self) {
        if self.assets.is_empty() {
            return;
        }
        for mut asset in self.assets.iter_mut() {
            let asset = Arc::make_mut(&mut *asset);
            asset.outputs.clear();
            asset.resolutions.clear();
        }
    }

    // -- discovery: build/collect on demand, deduping by key ----------------

    /// Ensure `req`'s spec is built and stored, and record how this request uses
    /// it. The build is a no-op if the spec is already resolved (so repeated
    /// requests across iterations or call sites build once); the usage
    /// ([`outputs`](ResolvedAsset::outputs)) is unioned every time, so an asset
    /// both `.url()` and `.read()` ends up carrying both.
    pub fn resolve_asset(
        &mut self,
        world: Tracked<dyn World + '_>,
        req: &AssetReq,
    ) -> SourceResult<()> {
        if !self.assets.contains_key(&req.spec) {
            let asset = self.build_asset(world, &req.spec, req.span)?;
            self.assets.insert_overwrite(Arc::new(asset));
        }
        let mut asset = self.assets.get_mut(&req.spec).expect("just resolved");
        let asset = Arc::make_mut(&mut *asset);
        for output in &req.outputs {
            if !asset.outputs.contains(output) {
                asset.outputs.push(output.clone());
            }
        }
        Ok(())
    }

    /// Resolve per-usage output policies into concrete bundle paths and URLs.
    pub fn resolve_outputs(
        &mut self,
        mut eval_derive: impl FnMut(&typst::foundations::Func, &ResolvedAsset) -> SourceResult<String>,
    ) -> SourceResult<()> {
        let ctx = self.ctx.clone();
        for mut stored in self.assets.iter_mut() {
            let asset = Arc::make_mut(&mut *stored);
            let default_path = ctx.default_asset_output(&asset.built);

            let mut explicit = Vec::<(OutputReq, String)>::new();
            for policy in &asset.outputs {
                let path = match policy {
                    OutputReq::Fixed(raw) => Some(
                        ctx.resolve_asset_output(raw.as_str())
                            .map_err(|err| eco_vec![SourceDiagnostic::error(asset.span, err)])?,
                    ),
                    OutputReq::Derive(func) => {
                        let raw = eval_derive(func, asset)?;
                        Some(
                            ctx.resolve_asset_output(&raw).map_err(|err| {
                                eco_vec![SourceDiagnostic::error(asset.span, err)]
                            })?,
                        )
                    }
                    OutputReq::Read | OutputReq::Auto => None,
                };
                if let Some(path) = path {
                    explicit.push((policy.clone(), path));
                }
            }

            let mut distinct_explicit = Vec::<String>::new();
            for (_, path) in &explicit {
                if !distinct_explicit.contains(path) {
                    distinct_explicit.push(path.clone());
                }
            }
            let auto_path = match distinct_explicit.as_slice() {
                [path] => path.clone(),
                _ => default_path,
            };

            let mut resolutions = Vec::new();
            for policy in &asset.outputs {
                let path = explicit
                    .iter()
                    .find(|(p, _)| p == policy)
                    .map(|(_, path)| path.clone())
                    .unwrap_or_else(|| auto_path.clone());
                let url = ctx.asset_url(&path);
                resolutions.push(Resolution {
                    policy: policy.clone(),
                    output_path: path,
                    url,
                });
            }
            resolutions.sort_by(|a, b| {
                a.output_path
                    .cmp(&b.output_path)
                    .then_with(|| format!("{:?}", a.policy).cmp(&format!("{:?}", b.policy)))
            });
            asset.output_path = auto_path.clone();
            asset.url = ctx.asset_url(&auto_path);
            asset.resolutions = resolutions;
        }
        Ok(())
    }

    /// Collect a discovered document, keyed by `output`. The *same* document
    /// reaching this twice — shown inline and `.url()`'d, or `.url()`'d
    /// repeatedly — dedups silently to one. Two *different* documents claiming
    /// one `output` is a conflict and errors.
    pub fn collect_document(&mut self, doc: &ResolvedDocument) -> SourceResult<()> {
        if let Some(existing) = self.documents.get(doc.output.as_str()) {
            if **existing != *doc {
                return Err(eco_vec![
                    SourceDiagnostic::error(
                        doc.body.span(),
                        format!("two different documents target the output `{}`", doc.output),
                    )
                    .with_hint("each `document(..)` needs a distinct `output:`")
                ]);
            }
            return Ok(());
        }
        self.documents.insert_overwrite(Arc::new(doc.clone()));
        Ok(())
    }

    /// Process one spec into a [`ResolvedAsset`]: dispatch to the per-type build,
    /// then add the shared fingerprinted output path + URL.
    fn build_asset(
        &mut self,
        world: Tracked<dyn World + '_>,
        spec: &AssetSpec,
        span: typst::syntax::Span,
    ) -> SourceResult<ResolvedAsset> {
        let built = match spec {
            AssetSpec::File { file } => file::build(world, *file, &self.ctx, span)?,
            AssetSpec::Sass {
                source,
                format,
                minify,
            } => sass::build(world, source, *format, *minify, &self.ctx, span)?,
            // `Raw` is in-memory bytes — it can't fail, so it needs no span.
            AssetSpec::Raw { bytes, ext } => raw::build(bytes.clone(), ext.clone()),
            AssetSpec::Svg { source, minify, id } => {
                svg::build(source, *minify, id, &self.ctx, span, &mut self.warnings)?
            }
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
            AssetSpec::Typst {
                input,
                format,
                ppi,
                minify,
            } => typst_doc::build(
                world,
                input,
                *format,
                *ppi,
                *minify,
                &self.ctx,
                span,
                &mut self.warnings,
            )?,
        };

        let output_path = self.ctx.default_asset_output(&built);
        let url = self.ctx.asset_url(&output_path);

        Ok(ResolvedAsset {
            spec: spec.clone(),
            span,
            built,
            output_path,
            url,
            resolutions: Vec::new(),
            // Filled in by `resolve_asset` from the request(s) that reached it.
            outputs: Vec::new(),
        })
    }

    // -- invalidation -------------------------------------------------------

    /// Evict assets whose on-disk sources changed since they were resolved
    /// (mtime check). Returns whether anything was evicted. A no-op on the first
    /// compile (empty store); called at the top of each compile so what survives
    /// seeds the next one. Documents aren't revalidated — they're cleared and
    /// re-discovered each compile ([`clear_documents`](Self::clear_documents)).
    pub fn revalidate(&mut self) -> bool {
        self.evict_assets_where(|asset| {
            asset
                .built
                .upstream
                .iter()
                .any(|u| mtime(&u.path).ok() != u.mtime)
        })
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
        self.assets.retain(|asset| !pred(&asset));
        self.assets.len() != before
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
pub fn annotate(path: &Path, err: io::Error) -> io::Error {
    let shown = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    io::Error::new(err.kind(), format!("{}: {err}", shown.display()))
}

/// Last-modified time of an on-disk path.
pub fn mtime(path: &Path) -> io::Result<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified())
}
