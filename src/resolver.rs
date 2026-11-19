//! The shared resolver: one place that owns every built asset and discovered
//! document for a compile, plus each asset's dependency [ledger](crate::ledger)
//! entries, which drive source-change revalidation and the dev server's watch
//! set.
//!
//! Every asset build gets its own [`DepRecorder`], owned here (not by the
//! build), so the files a build touched are known whether it succeeded or
//! failed. A failure is cached too ([`FailedAsset`]): it's re-reported as the
//! same error on every compile, without rebuilding, until one of its deps
//! changes — and its deps are watched, so fixing the broken file is noticed.
//! Build warnings are stored with the entry and re-emitted on every compile
//! that uses it, not just the one that built it.
//!
//! Assets and documents are deliberately *not* unified behind a trait. They
//! share a lot of bookkeeping — which lives here — but differ fundamentally:
//! an asset is content-addressed by its [`AssetSpec`], must be *built* (so it
//! persists across recompiles and is revalidated against its deps), and its output
//! path/URL is a derived naming layer. A document is "resolved" the moment it's
//! discovered (no build), keyed 1:1 by its `output`, and is cheap enough to
//! re-discover from scratch every compile — so it needs no caching or
//! revalidation at all. One struct, two maps, shared helpers — not a generic.

// `AssetSpec` keys `failed`; see the note atop `crate::asset`.
#![allow(clippy::mutable_key_type)]

use std::collections::HashMap;
use std::sync::Arc;

use comemo::Tracked;
use ecow::{EcoVec, eco_vec};
use iddqd::IdHashMap;
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult};

use crate::asset::{
    AssetReq, AssetSpec, OutputReq, Resolution, ResolvedAsset, file, image, raw, sass, svg,
    typst_doc,
};
use crate::document::ResolvedDocument;
use crate::ledger::{self, Dep, DepRecorder, Revalidation};
use crate::project::TwylaContext;
use typst::syntax::{FileId, Source, Span};
use typst_kit::packages::SystemPackages;

/// Owns every resolved asset and discovered document for a build.
///
/// The caller drives it: a one-shot build uses a fresh resolver; `serve` reuses
/// one across recompiles so the asset store persists (and
/// [`revalidate`](Self::revalidate) evicts what changed). Each *entry* is `Arc`-wrapped, so a fixed-point iteration
/// snapshots a whole map into a `TwylaIntrospector` (and the convergence
/// history) by cloning the table over shared `Arc`s — cheap — and mutating one
/// entry ([`Arc::make_mut`]) deep-copies just that entry, not the whole map.
pub struct Resolver {
    ctx: TwylaContext,
    /// Every asset built so far, keyed by its [`AssetSpec`]. Each carries its
    /// ledger [`deps`](ResolvedAsset::deps) and build warnings.
    assets: IdHashMap<Arc<ResolvedAsset>>,
    /// Every spec whose build failed, with what it touched. Disjoint from
    /// `assets`. Not visible to the introspector: a failed asset aborts the
    /// compile with its error.
    failed: HashMap<AssetSpec, FailedAsset>,
    /// Every document discovered so far, keyed by its `output` path.
    documents: IdHashMap<Arc<ResolvedDocument>>,
    /// Warnings of the assets requested since the last drain (e.g. a
    /// best-effort svg minify that fell back to verbatim), pushed on every
    /// request — fresh build or cache hit — so a cached asset's warnings
    /// aren't lost after the compile that built it. Drained into the sink by
    /// the compile loop after each discovery pass; the sink deduplicates.
    warnings: Vec<SourceDiagnostic>,
    /// The recorder of the asset build in progress (empty between builds). A
    /// field rather than a local in [`resolve_asset`](Self::resolve_asset)
    /// so that if a build *panics*, what it touched so far survives the
    /// unwind: [`asset_deps`](Self::asset_deps) includes it, and the dev
    /// server collects those before discarding the resolver.
    building: DepRecorder,
    /// Package sources for `asset.typst` sub-compiles, which read package
    /// files themselves (through `building`) rather than through the live
    /// world. See [`crate::render::system_packages`].
    packages: SystemPackages,
    /// Counters for tests: how much work the asset pass did.
    pub stats: ResolverStats,
    /// Test hook: panic right after the next asset build, before its deps
    /// are filed, to exercise panic recovery.
    #[cfg(test)]
    pub(crate) inject_build_panic: bool,
}

/// A cached build failure. See the module docs.
#[derive(Debug, Clone)]
pub struct FailedAsset {
    /// The request span the diagnostics were built against (see [`respan`]).
    pub span: Span,
    /// Everything the failed build touched, up to the failure.
    pub deps: Vec<Dep>,
    pub warnings: Vec<SourceDiagnostic>,
    pub errors: EcoVec<SourceDiagnostic>,
    /// See [`ResolvedAsset::diag_sources`].
    pub diag_sources: Vec<Source>,
}

/// How much work the asset pass did over a resolver's lifetime. Lets tests
/// assert I/O budgets ("editing an unrelated page reads no asset sources")
/// without a filesystem shim.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResolverStats {
    /// Asset builds run (successful or not).
    pub builds: usize,
    /// Files whose contents were read: by builds, and by revalidation
    /// re-hashing a file whose stamp changed.
    pub content_reads: usize,
}

impl Resolver {
    /// A fresh, empty resolver. The stores fill as assets/documents are
    /// discovered and persist for the resolver's lifetime.
    pub fn new(ctx: &TwylaContext) -> Self {
        Self {
            ctx: ctx.clone(),
            assets: IdHashMap::new(),
            failed: HashMap::new(),
            documents: IdHashMap::new(),
            warnings: Vec::new(),
            building: DepRecorder::new(),
            packages: crate::render::system_packages(ctx),
            stats: ResolverStats::default(),
            #[cfg(test)]
            inject_build_panic: false,
        }
    }

    /// Drain the warnings asset builds queued since the last call.
    pub fn take_warnings(&mut self) -> Vec<SourceDiagnostic> {
        std::mem::take(&mut self.warnings)
    }

    /// The sources cached entries' diagnostics point into that their builds
    /// loaded themselves (see [`ResolvedAsset::diag_sources`]): what the
    /// compile's diagnostics are rendered against, ahead of the live world.
    pub fn diag_sources(&self) -> HashMap<FileId, Source> {
        self.assets
            .iter()
            .flat_map(|asset| asset.diag_sources.iter())
            .chain(self.failed.values().flat_map(|f| f.diag_sources.iter()))
            .map(|source| (source.id(), source.clone()))
            .collect()
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

    /// Every currently-resolved asset, regardless of how it's used (see
    /// [`emittable_assets`](Self::emittable_assets) for the narrower set
    /// actually written).
    pub fn resolved_assets(&self) -> impl Iterator<Item = &Arc<ResolvedAsset>> {
        self.assets.iter()
    }

    /// The ledger deps of every cached entry, built *or failed*: what the
    /// dev server must watch. Includes `.read()`-only assets (never emitted,
    /// but their sources still affect the inlined output) and failed builds
    /// (so fixing the file that broke them is noticed).
    pub fn asset_deps(&self) -> impl Iterator<Item = &Dep> {
        self.assets
            .iter()
            .flat_map(|asset| asset.deps.iter())
            .chain(self.failed.values().flat_map(|failed| failed.deps.iter()))
    }

    /// What the asset build in progress has touched so far. Empty unless a
    /// build panicked (see [`building`](Self::building)).
    pub fn interrupted_build_deps(&self) -> Vec<Dep> {
        self.building.deps()
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
            asset.output_spans.clear();
            asset.resolutions.clear();
        }
    }

    // -- discovery: build/collect on demand, deduping by key ----------------

    /// Ensure `req`'s spec is built and stored, and record how this request uses
    /// it. The build is a no-op if the spec is already resolved (so repeated
    /// requests across iterations or call sites build once); the usage
    /// ([`outputs`](ResolvedAsset::outputs)) is unioned every time, so an asset
    /// both `.url()` and `.read()` ends up carrying both.
    ///
    /// A spec whose build failed is not rebuilt: its cached errors are
    /// returned again (until [`revalidate`](Self::revalidate) sees one of its
    /// deps change).
    pub fn resolve_asset(
        &mut self,
        world: Tracked<dyn World + '_>,
        req: &AssetReq,
    ) -> SourceResult<()> {
        if let Some(failed) = self.failed.get(&req.spec) {
            let (from, to) = (failed.span, req.span);
            self.warnings
                .extend(failed.warnings.iter().map(|w| respan(w, from, to)));
            return Err(failed.errors.iter().map(|e| respan(e, from, to)).collect());
        }
        if !self.assets.contains_key(&req.spec) {
            // The recorder belongs to the resolver, not the build: whatever
            // the build touched before failing (or panicking) is kept.
            let mut warnings = Vec::new();
            let mut diag_sources = Vec::new();
            let result = self.build_asset(
                world,
                &req.spec,
                req.span,
                &self.building,
                &mut warnings,
                &mut diag_sources,
            );
            #[cfg(test)]
            if std::mem::take(&mut self.inject_build_panic) {
                panic!("injected asset build panic");
            }
            self.stats.builds += 1;
            self.stats.content_reads += self.building.content_reads();
            let deps = self.building.take();
            match result {
                Ok(mut asset) => {
                    asset.deps = deps;
                    asset.warnings = warnings;
                    asset.diag_sources = diag_sources;
                    self.assets.insert_overwrite(Arc::new(asset));
                }
                Err(errors) => {
                    self.warnings.extend(warnings.iter().cloned());
                    self.failed.insert(
                        req.spec.clone(),
                        FailedAsset {
                            span: req.span,
                            deps,
                            warnings,
                            errors: errors.clone(),
                            diag_sources,
                        },
                    );
                    return Err(errors);
                }
            }
        }
        let mut asset = self.assets.get_mut(&req.spec).expect("just resolved");
        let asset = Arc::make_mut(&mut *asset);
        let (from, to) = (asset.span, req.span);
        self.warnings
            .extend(asset.warnings.iter().map(|w| respan(w, from, to)));
        for output in &req.outputs {
            if !asset.outputs.contains(output) {
                asset.outputs.push(output.clone());
                asset.output_spans.push((output.clone(), req.span));
            }
        }
        Ok(())
    }

    /// Resolve per-usage output policies into concrete bundle paths and URLs.
    pub fn resolve_outputs(
        &mut self,
        mut eval_derive: impl FnMut(
            &typst::foundations::Func,
            &ResolvedAsset,
            Span,
        ) -> SourceResult<String>,
    ) -> SourceResult<()> {
        let ctx = self.ctx.clone();
        for mut stored in self.assets.iter_mut() {
            let asset = Arc::make_mut(&mut *stored);
            let default_path = ctx.default_asset_output(&asset.built);

            let mut explicit = Vec::<(OutputReq, String)>::new();
            for policy in &asset.outputs {
                // Blame the request that asked for this policy this compile.
                let span = asset
                    .output_spans
                    .iter()
                    .find(|(p, _)| p == policy)
                    .map_or(asset.span, |(_, span)| *span);
                let path = match policy {
                    OutputReq::Fixed(raw) => Some(
                        ctx.resolve_asset_output(raw.as_str())
                            .map_err(|err| eco_vec![SourceDiagnostic::error(span, err)])?,
                    ),
                    OutputReq::Derive(func) => {
                        let raw = eval_derive(func, asset, span)?;
                        Some(
                            ctx.resolve_asset_output(&raw)
                                .map_err(|err| eco_vec![SourceDiagnostic::error(span, err)])?,
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
    /// Every file read goes through `rec`; warnings go to `warnings` (stored
    /// with the entry by the caller).
    fn build_asset(
        &self,
        world: Tracked<dyn World + '_>,
        spec: &AssetSpec,
        span: Span,
        rec: &DepRecorder,
        warnings: &mut Vec<SourceDiagnostic>,
        diag_sources: &mut Vec<Source>,
    ) -> SourceResult<ResolvedAsset> {
        let built = match spec {
            AssetSpec::File { file } => file::build(*file, &self.ctx, span, rec)?,
            AssetSpec::Sass {
                source,
                format,
                minify,
            } => sass::build(source, *format, *minify, &self.ctx, span, rec)?,
            // `Raw` is in-memory bytes — it can't fail, so it needs no span.
            AssetSpec::Raw { bytes, ext } => raw::build(bytes.clone(), ext.clone()),
            AssetSpec::Svg { source, minify, id } => {
                svg::build(source, *minify, id, &self.ctx, span, rec, warnings)?
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
                source, *width, *height, *fit, *filter, *format, *quality, &self.ctx, span, rec,
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
                &self.packages,
                span,
                rec,
                warnings,
                diag_sources,
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
            output_spans: Vec::new(),
            // Filled in by `resolve_asset` from the recorder it owns.
            deps: Vec::new(),
            warnings: Vec::new(),
            diag_sources: Vec::new(),
        })
    }

    // -- invalidation -------------------------------------------------------

    /// Check every cached entry (built or failed) against the disk with
    /// [`ledger::revalidate`]: evict the ones whose deps changed, and refresh
    /// the stamps of ones whose deps were only touched. A no-op on the first
    /// compile (empty store); called at the top of each compile so what
    /// survives seeds the next one. Documents aren't revalidated — they're
    /// cleared and re-discovered each compile
    /// ([`clear_documents`](Self::clear_documents)).
    pub fn revalidate(&mut self) {
        let mut reads = 0;
        self.failed.retain(
            |_, failed| match ledger::revalidate(&failed.deps, &mut reads) {
                Revalidation::Valid => true,
                Revalidation::Restamped(deps) => {
                    failed.deps = deps;
                    true
                }
                Revalidation::Changed => false,
            },
        );
        self.assets.retain(
            |mut asset| match ledger::revalidate(&asset.deps, &mut reads) {
                Revalidation::Valid => true,
                Revalidation::Restamped(deps) => {
                    Arc::make_mut(&mut *asset).deps = deps;
                    true
                }
                Revalidation::Changed => false,
            },
        );
        self.stats.content_reads += reads;
    }
}

/// A cached diagnostic, re-pointed at the current request. Diagnostics blame
/// the span of the request that ran the build; spans in an edited file are
/// renumbered, so replaying the old one could point at the wrong code (or
/// nowhere). Spans elsewhere (e.g. inside an `asset.typst` document) are left
/// alone — if that file changed, the entry was evicted anyway.
fn respan(diag: &SourceDiagnostic, from: Span, to: Span) -> SourceDiagnostic {
    let mut diag = diag.clone();
    if diag.span == from {
        diag.span = to;
    }
    diag
}
