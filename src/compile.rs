//! Twyla's version typst's `compile` / `compile_impl` fixed-point loop,
//! plus twyla's per-document metadata harvest.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use comemo::{Track, Tracked, TrackedMut};
use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use iddqd::IdHashMap;
use typst::World;
use typst::diag::{HintedString, SourceDiagnostic, SourceResult, StrResult, Warned};
use typst::foundations::{
    BundlePath, Content, Context, Dynamic, IntoValue, Label, NativeElement, Output, Selector,
    Smart, Str, StyleChain, Styles, Target, TargetElem, Value,
};
use typst::syntax::{FileId, Span, VirtualPath};
use typst_bundle::Bundle;
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{
    DocumentPosition, EmptyIntrospector, Introspection, Introspector, Location, Locator, MAX_ITERS,
    analyze,
};
use typst_library::model::{DocumentElem, DocumentInfo, Numbering};
use typst_library::routines::{Arenas, RealizationKind};
use typst_utils::Protected;

use crate::asset::{AssetReqIntrospect, ResolvedAsset, hash_spec};
use crate::document::{
    DOCUMENTS_LIST_KEY, ResolvedDocument, ResolvedDocumentIntrospect, TwylaDocument,
};
use crate::project::{TwylaContext, present_output};
use crate::resolver::Resolver;

/// The product of one bundle compile, consumed by [`crate::render`] to assemble
/// the output map. `pages` is every page's twyla `document` metadata — a
/// [`ResolvedDocument`] whether it came from a `content/*.typ` file or an inline
/// `document(..)`, same shape, two discovery paths — and `assets` is every
/// emittable asset. Both are plain lists `render` iterates once (it keys its own
/// output map); the element types let it move each entry into an `Output` with
/// no clone.
pub struct CompiledBundle {
    pub bundle: Bundle,
    pub pages: Vec<ResolvedDocument>,
    pub assets: Vec<(String, Arc<ResolvedAsset>)>,
}

/// Compile `pages` into a [`Bundle`] *and* harvest each page's twyla
/// `document` metadata. Each source is evaluated exactly once; that single
/// eval feeds both the relayout loop and the harvest.
pub fn compile_bundle(
    ctx: &TwylaContext,
    world: &dyn World,
    sources: &[FileId],
    resolver: &mut Resolver,
) -> Warned<SourceResult<CompiledBundle>> {
    let mut sink = Sink::new();
    let traced = Traced::default();
    let output = compile_bundle_impl(
        ctx,
        world.track(),
        traced.track(),
        &mut sink,
        sources,
        resolver,
    )
    .map_err(deduplicate);
    Warned {
        output,
        warnings: sink.warnings(),
    }
}

fn compile_bundle_impl(
    ctx: &TwylaContext,
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    sources: &[FileId],
    resolver: &mut Resolver,
) -> SourceResult<CompiledBundle> {
    // Evict assets whose sources changed since the last compile (no-op on the
    // first compile / a fresh resolver). What survives seeds this compile.
    resolver.revalidate();
    // Asset *usage* (which assets are linked vs only inlined) is recomputed from
    // this compile's requests, so a dropped `.url()` stops emitting the file.
    resolver.clear_asset_usage();
    // Documents are re-discovered from scratch each compile (cheap; their
    // sources are typst deps), so drop the previous compile's set — otherwise a
    // since-edited inline document would look like a conflicting one.
    resolver.clear_documents();

    let library = world.library();
    let empty = EmptyIntrospector;
    let mut engine = Engine {
        library,
        world,
        introspector: Protected::new(empty.track()),
        traced,
        sink: sink.track_mut(),
        route: Route::default(),
    };

    // Evaluate each file once and harvest its (documents-independent) `#set
    // document` metadata. Eval is fixed for the whole compile — the relayout
    // loop below only re-*realizes*. Inline / `#context` `document(..)` calls
    // inside these bodies are discovered later, via the sink, during that render
    // — not here (so they ride the same pass as asset resolution).
    let evaled = engine.parallelize(sources, |engine, id| {
        let body = eval_file(engine, *id)?;
        harvest_metadata(ctx, engine, *id, body)
    });
    let mut files: Vec<ResolvedDocument> = Vec::new();
    for res in evaled {
        files.push(res?);
    }

    // One fixed point: typst introspection, asset resolution, and document
    // discovery all converge together (see [`compile_bundle_loop`]).
    let bundle = compile_bundle_loop(ctx, world, traced, sink, &files, resolver)?;

    // Every page (file pages first, then discovered inline documents — both
    // already `ResolvedDocument`s) and every emittable asset, as the flat lists
    // render iterates.
    let mut pages: Vec<ResolvedDocument> = files;
    pages.extend(resolver.documents().cloned());
    let assets: Vec<(String, Arc<ResolvedAsset>)> = resolver.emittable_assets().collect();
    Ok(CompiledBundle {
        bundle,
        pages,
        assets,
    })
}

/// Evaluate a single content file into its body content. Mirrors the eval
/// step of `typst::compile_impl`, but per-file (no virtual main / include).
fn eval_file(engine: &mut Engine, id: FileId) -> SourceResult<Content> {
    let source = engine.world.source(id).map_err(|err| {
        eco_vec![SourceDiagnostic::error(
            Span::detached(),
            ecow::EcoString::from(err)
        )]
    })?;
    Ok(typst_eval::eval(
        engine.world,
        engine.library,
        engine.traced,
        TrackedMut::reborrow_mut(&mut engine.sink),
        Route::default().track(),
        &source,
    )?
    .content())
}

/// The introspector twyla feeds into each realization pass.
///
/// It is an ordinary bundle introspector (the previous iteration's output, or
/// empty on the first pass) *plus* twyla's resolved assets and document
/// listing, surfaced through the open-ended [`Introspector::value`] hook. So
/// `asset.*().url()`/`.read()`, inline `document(..).url()`, and `documents()`
/// all read back through that one tracked hook — their resolution rides the
/// same comemo introspection convergence as counters and queries, with no
/// style-chain channels.
///
/// Every method except [`value`](Self::value) delegates to `inner`; the
/// snapshots are cheap [`Arc`] clones of the resolver's stores (see
/// [`Resolver::assets_snapshot`]), so building one per iteration — and keeping
/// the per-iteration history for [`analyze`] — is nearly free.
pub struct TwylaIntrospector {
    inner: Arc<dyn Introspector>,
    assets: IdHashMap<Arc<ResolvedAsset>>,
    documents: IdHashMap<Arc<ResolvedDocument>>,
    /// The file-page rows of the `documents()` listing, as ready-made dicts.
    /// Constant across the whole compile (derived from the eval'd files), so
    /// shared by `Arc`; the discovered inline documents are appended.
    file_docs: Arc<Vec<Value>>,
    /// For deriving each discovered document's `url` in the listing.
    ctx: TwylaContext,
}

impl TwylaIntrospector {
    /// The `documents()` listing: the constant file-page rows followed by every
    /// inline document discovered so far. Recomputed per call — `value` is hit a
    /// bounded number of times per realization and the set is small.
    fn documents_value(&self) -> Value {
        let mut docs = (*self.file_docs).clone();
        docs.extend(
            self.documents
                .iter()
                .map(|stored| stored.to_dict(&self.ctx).into_value()),
        );
        Value::Array(docs.into_iter().collect())
    }
}

impl Introspector for TwylaIntrospector {
    fn query(&self, selector: &Selector) -> EcoVec<Content> {
        self.inner.query(selector)
    }

    fn query_first(&self, selector: &Selector) -> Option<Content> {
        self.inner.query_first(selector)
    }

    fn query_unique(&self, selector: &Selector) -> StrResult<Content> {
        self.inner.query_unique(selector)
    }

    fn query_label(&self, label: Label) -> StrResult<&Content> {
        self.inner.query_label(label)
    }

    fn query_labelled(&self) -> EcoVec<Content> {
        self.inner.query_labelled()
    }

    fn query_count_before(&self, selector: &Selector, end: Location) -> usize {
        self.inner.query_count_before(selector, end)
    }

    fn label_count(&self, label: Label) -> usize {
        self.inner.label_count(label)
    }

    fn locator(&self, key: u128, base: Location) -> Option<Location> {
        self.inner.locator(key, base)
    }

    fn pages(&self, location: Location) -> Option<NonZeroUsize> {
        self.inner.pages(location)
    }

    fn page(&self, location: Location) -> Option<NonZeroUsize> {
        self.inner.page(location)
    }

    fn position(&self, location: Location) -> Option<DocumentPosition> {
        self.inner.position(location)
    }

    fn page_numbering(&self, location: Location) -> Option<&Numbering> {
        self.inner.page_numbering(location)
    }

    fn page_supplement(&self, location: Location) -> Option<&Content> {
        self.inner.page_supplement(location)
    }

    fn anchor(&self, location: Location) -> Option<&EcoString> {
        self.inner.anchor(location)
    }

    fn document(&self, location: Location) -> Option<Location> {
        self.inner.document(location)
    }

    fn path(&self, location: Location) -> Option<&VirtualPath> {
        self.inner.path(location)
    }

    /// Twyla's extension: answer the documents-listing key with the current
    /// `documents()` array, and any asset-spec-hash key with the resolved asset
    /// (as a `Value::Dyn`, downcast back by [`AssetReqIntrospect`]). Everything
    /// else falls through to the bundle introspector.
    fn value(&self, key: u128) -> Option<Value> {
        if key == DOCUMENTS_LIST_KEY {
            return Some(self.documents_value());
        }
        // Keyed by `hash_spec(spec)`, not the spec itself, so this is a scan
        // rather than a map lookup — the asset set is small.
        self.assets
            .iter()
            .find(|asset| hash_spec(&asset.spec) == key)
            .map(|asset| Value::Dyn(Dynamic::from_arc(asset.clone())))
            .or_else(|| self.inner.value(key))
    }
}

/// The single fixed-point loop: typst's introspection convergence, asset URL
/// resolution, and document discovery all settle here together. Mirrors the
/// loop in `typst::compile_impl`, but each iteration rebuilds the bundle content
/// from the file pages *plus every document discovered so far*, feeds in a
/// [`TwylaIntrospector`] that answers asset/document reads, then resolves
/// whatever that realization requested.
///
/// Why all three share one loop *and* one convergence test: with the
/// introspection system, an `asset.*().url()` / inline `document(..)` is just a
/// read on the introspector ([`Introspector::value`]). A newly resolved asset
/// or discovered document is therefore a `value()` answer that *changed* since
/// it was read this pass, which `constraint.validate` against the post-discovery
/// introspector catches exactly as it catches an unstable counter. So there is
/// no separate "assets settled" / "documents settled" bookkeeping — the comemo
/// constraint is the whole convergence criterion.
fn compile_bundle_loop(
    ctx: &TwylaContext,
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    files: &[ResolvedDocument],
    resolver: &mut Resolver,
) -> SourceResult<Bundle> {
    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let combined: Styles = [TargetElem::target.set(Bundle::target()).wrap()]
        .into_iter()
        .collect();
    let styles = base.chain(&combined);

    // The file-page rows of `documents()` are constant across the compile;
    // build them once and share into every introspector wrapper.
    let file_docs: Arc<Vec<Value>> = Arc::new(
        files
            .iter()
            .map(|doc| doc.to_dict(ctx).into_value())
            .collect(),
    );

    // The first iteration realizes against an empty introspector; subsequent
    // ones against the previous iteration's output wrapper. We keep every output
    // wrapper so `analyze` can replay each recorded introspection across the
    // whole history on non-convergence.
    let empty = TwylaIntrospector {
        inner: Arc::new(EmptyIntrospector),
        assets: IdHashMap::new(),
        documents: IdHashMap::new(),
        file_docs: file_docs.clone(),
        ctx: ctx.clone(),
    };
    let mut history: Vec<TwylaIntrospector> = Vec::new();
    let mut document: Bundle;

    loop {
        // Content = file pages + every document discovered so far, each routed
        // to its own bundle output; grows as discovery proceeds.
        let content = build_content(files, resolver);

        let input: &TwylaIntrospector = history.last().unwrap_or(&empty);
        let constraint = comemo::Constraint::new();

        let mut subsink = Sink::new();
        {
            let mut engine = Engine {
                library,
                world,
                introspector: Protected::new((input as &dyn Introspector).track_with(&constraint)),
                traced,
                sink: subsink.track_mut(),
                route: Route::default(),
            };

            document = Bundle::create(&mut engine, &content, styles)?;
        }

        // Resolve every asset and collect every document this realization
        // requested (recorded as introspections), growing the resolver's stores.
        discover_requests(resolver, world, subsink.introspections())?;

        // Surface warnings the asset builds queued (e.g. a best-effort svg
        // minify that fell back to verbatim). A build only runs on a cache
        // miss, so each warning fires once per (re)build, not per iteration.
        for warning in resolver.take_warnings() {
            subsink.warn(warning);
        }

        // Rebuild the engine after `subsink.introspections()`'s borrow ends, so
        // output-derivation closures can run against the same world/library and
        // warning sink without fighting the tracked `Sink` borrow.
        {
            let mut engine = Engine {
                library,
                world,
                introspector: Protected::new((input as &dyn Introspector).track_with(&constraint)),
                traced,
                sink: subsink.track_mut(),
                route: Route::default(),
            };
            resolve_outputs(resolver, &mut engine)?;
        }

        // This iteration's output introspector: the fresh bundle introspector
        // wrapped with the *post-discovery* asset/document snapshot.
        let output = TwylaIntrospector {
            inner: document.introspector.clone(),
            assets: resolver.assets_snapshot(),
            documents: resolver.documents_snapshot(),
            file_docs: file_docs.clone(),
            ctx: ctx.clone(),
        };

        if constraint.validate(&output as &dyn Introspector) {
            sink.extend_from_sink(subsink);
            break;
        }

        if history.len() >= MAX_ITERS - 1 {
            // Out of attempts. Replay every recorded introspection across the
            // history so each can diagnose its own non-convergence (a twyla
            // asset/document that never stabilized produces a tailored message
            // via its `Introspect::diagnose`). Mirrors `typst::compile_impl`.
            let mut introspectors = [&empty as &dyn Introspector; MAX_ITERS + 1];
            for i in 1..MAX_ITERS {
                introspectors[i] = &history[i - 1];
            }
            introspectors[MAX_ITERS] = &output;

            let warnings = analyze(world, introspectors, subsink.introspections());

            sink.extend_from_sink(subsink);
            for warning in warnings {
                sink.warn(warning);
            }
            break;
        }

        history.push(output);
    }

    // Promote delayed errors.
    let delayed = sink.delayed();
    if !delayed.is_empty() {
        return Err(delayed);
    }

    Ok(document)
}

/// Resolve every asset and collect every document a realization requested.
///
/// Each `asset.*().url()`/`.read()` and inline `document(..)` records an
/// [`Introspection`] during realization; we replay that list to drive
/// resolution. Because the records live in the comemo-tracked [`Sink`], a
/// memoized (cached) realization still reports its assets/documents here, so
/// discovery never misses a page just because it didn't re-run.
fn discover_requests(
    resolver: &mut Resolver,
    world: Tracked<dyn World + '_>,
    introspections: &[Introspection],
) -> SourceResult<()> {
    for introspection in introspections {
        if let Some(req) = introspection.downcast::<AssetReqIntrospect>() {
            resolver.resolve_asset(world, &req.0)?;
        } else if let Some(req) = introspection.downcast::<ResolvedDocumentIntrospect>() {
            resolver.collect_document(&req.0)?;
        }
    }
    Ok(())
}

fn resolve_outputs(resolver: &mut Resolver, engine: &mut Engine) -> SourceResult<()> {
    resolver.resolve_outputs(|func, asset| {
        let ext = Str::from(asset.built.ext.as_deref().unwrap_or("bin"));
        let stem = Str::from(asset.built.stem.as_deref().unwrap_or(""));
        let value = func.call(
            engine,
            Context::none().track(),
            [Str::from(asset.built.sha256_hex().as_str()), ext, stem],
        )?;
        let path: Str = value.cast().map_err(|err| hinted_error(asset.span, err))?;
        Ok(path.to_string())
    })
}

fn hinted_error(span: Span, err: HintedString) -> EcoVec<SourceDiagnostic> {
    let mut diag = SourceDiagnostic::error(span, err.message().clone());
    for hint in err.hints() {
        diag = diag.with_hint(hint.clone());
    }
    eco_vec![diag]
}

/// Harvest each page's twyla `document` fields from its body's resolved style
/// chain.
fn harvest_metadata(
    ctx: &TwylaContext,
    engine: &mut Engine,
    id: FileId,
    body: Content,
) -> SourceResult<ResolvedDocument> {
    let base = StyleChain::new(&engine.library.styles);
    let target = TargetElem::target.set(Target::Bundle).wrap();
    let styles = base.chain(&target);

    // Documents are top-level siblings sharing the bundle base chain, so every
    // body realizes under the same `outside` styles.
    let style_map = styles.to_map().outside();
    let styles = StyleChain::new(&style_map);

    let arenas = Arenas::default();
    let mut info = DocumentInfo::default();
    let children = (engine.library.routines.realize)(
        RealizationKind::Document { info: &mut info },
        engine,
        &mut Locator::root().split(),
        &arenas,
        &body,
        styles,
    )?;

    // Read twyla's `document` fields off the folded child style chains.
    // Each top-of-file `#set document(..)` applies to subsequent siblings,
    // so any realized child carries the page-level values.
    let mut output = None;
    let mut title = None;
    let mut date = None;
    let mut description = None;
    let mut kind = None;
    let mut extra = Value::None;
    let mut draft = false;
    for (_, cs) in &children {
        if cs.has(TwylaDocument::output) {
            output = cs
                .get_cloned(TwylaDocument::output)
                .custom()
                .map(String::from);
        }
        if cs.has(TwylaDocument::title) {
            title = cs.get_cloned(TwylaDocument::title);
        }
        if cs.has(TwylaDocument::date) {
            date = cs.get_cloned(TwylaDocument::date);
        }
        if cs.has(TwylaDocument::description) {
            description = cs.get_cloned(TwylaDocument::description);
        }
        if cs.has(TwylaDocument::kind) {
            kind = cs.get_cloned(TwylaDocument::kind).custom();
        }
        if cs.has(TwylaDocument::extra) {
            extra = cs.get_cloned(TwylaDocument::extra);
        }
        draft = cs.get(TwylaDocument::draft);
    }
    // An explicit `output` is resolved relative to the source's folder stem
    // (leading `/` = bundle root); an `auto` output derives from the source path.
    let source = id.vpath().get_with_slash();
    let output = match output {
        Some(raw) => ctx.resolve_document_output(source, &raw).map_err(|msg| {
            eco_vec![SourceDiagnostic::error(
                Span::detached(),
                eco_format!("in {source}: {msg}"),
            )]
        })?,
        None => ctx.default_document_output(source),
    };
    let kind =
        kind.unwrap_or_else(|| EcoString::from(ctx.default_kind(id.vpath().get_with_slash())));

    Ok(ResolvedDocument {
        output,
        title,
        date,
        description,
        kind,
        extra,
        draft,
        body,
        source: Some(id),
    })
}

/// The bundle content for the current loop iteration: each file body wrapped as
/// its own routed document, plus each discovered document as a sibling.
fn build_content(files: &[ResolvedDocument], resolver: &Resolver) -> Content {
    let mut bodies = Vec::new();
    for doc in files {
        // Re-apply the *derived* output onto the page body so the page can read
        // it back (`#context document.output`). Without this the page sees only
        // the `auto` default — the derivation happens Rust-side in
        // `harvest_metadata` and never reaches the chain. The page's other
        // metadata is already on the chain (its own `#set document(..)`), so —
        // unlike `discovered_sibling` — only `output` is re-applied.
        //
        // The field is shown in root-absolute (`/`-prefixed) form — the
        // user-presentation boundary — while routing (`wrap_document`) and dedup
        // keep the no-slash `doc.output`. The slashed value still anchors
        // relative child `document(output:)` paths: `resolve_output` skips the
        // empty leading segment.
        let mut styles = Styles::new();
        styles.set(
            TwylaDocument::output,
            Smart::Custom(present_output(&doc.output).into()),
        );
        bodies.push(wrap_document(
            &doc.output,
            doc.body.clone().styled_with_map(styles),
        ));
    }
    for doc in resolver.documents() {
        bodies.push(discovered_sibling(doc));
    }
    Content::sequence(bodies)
}

/// Wrap a discovered document as a routed sibling, re-applying its metadata to
/// the body as a style map so `#context document.*` resolves inside it exactly
/// as on a full-file page.
fn discovered_sibling(doc: &ResolvedDocument) -> Content {
    let mut styles = Styles::new();
    // Shown in root-absolute form (user-presentation boundary); routing and
    // child-anchor resolution tolerate / strip the leading slash. See
    // `build_content`.
    styles.set(
        TwylaDocument::output,
        Smart::Custom(present_output(&doc.output).into()),
    );
    styles.set(TwylaDocument::title, doc.title.clone());
    styles.set(TwylaDocument::date, doc.date);
    styles.set(TwylaDocument::description, doc.description.clone());
    styles.set(TwylaDocument::kind, Smart::Custom(doc.kind.clone()));
    styles.set(TwylaDocument::extra, doc.extra.clone());
    styles.set(TwylaDocument::draft, doc.draft);
    wrap_document(&doc.output, doc.body.clone().styled_with_map(styles))
}

/// Wrap one page body in the native [`DocumentElem`] that typst-bundle routes to
/// its own output file (keyed by `output`).
fn wrap_document(output: &str, body: Content) -> Content {
    let path = BundlePath::new(VirtualPath::new(output).unwrap()).unwrap();
    DocumentElem::new(path, body).pack()
}

/// Deduplicate diagnostics. Mirrors `typst::deduplicate`.
fn deduplicate(mut diags: EcoVec<SourceDiagnostic>) -> EcoVec<SourceDiagnostic> {
    let mut unique = HashSet::new();
    diags.retain(|diag| {
        let hash = typst_utils::hash128(&(&diag.span, &diag.message));
        unique.insert(hash)
    });
    diags
}

#[cfg(test)]
mod tests {
    use crate::project::TwylaContext;
    use crate::render::RenderWorld;
    use crate::resolver::Resolver;

    /// First-page HTML from one compile of a one-file site (tempdir harness,
    /// mirrors `crate::asset::tests`).
    fn compile_main(body: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("content")).unwrap();
        std::fs::write(dir.path().join("content/main.typ"), body).unwrap();
        let ctx = TwylaContext::new(dir.path(), Some("https://example.com".into())).unwrap();
        let world = RenderWorld::new(&ctx).unwrap();
        let mut resolver = Resolver::new(&world.ctx);
        let outputs = world.compile_bundle(&mut resolver).unwrap();
        outputs.docs().next().unwrap().html.clone()
    }

    /// A full-file page can read its own *derived* output via `#context
    /// document.output`. Without `build_content` re-applying the derived value
    /// onto the page body, this read sees only the `auto` default (the
    /// derivation lives Rust-side in `harvest_doc`). `content/main.typ` derives
    /// to `index.html`.
    #[test]
    fn page_reads_its_derived_output() {
        let html = compile_main("#context document.output");
        assert!(
            html.contains("index.html"),
            "page did not see its derived output (regressed to `auto`?):\n{html}"
        );
    }

    /// Every output of one compile of a one-file site.
    fn compile_outputs(body: &str) -> crate::render::Outputs {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("content")).unwrap();
        std::fs::write(dir.path().join("content/main.typ"), body).unwrap();
        let ctx = TwylaContext::new(dir.path(), Some("https://example.com".into())).unwrap();
        let world = RenderWorld::new(&ctx).unwrap();
        let mut resolver = Resolver::new(&world.ctx);
        world.compile_bundle(&mut resolver).unwrap()
    }

    /// HTML of one specific output (by bundle key).
    fn doc_html(outputs: &crate::render::Outputs, key: &str) -> String {
        match outputs.get(key) {
            Some(crate::render::Output::Doc(d)) => d.html.clone(),
            other => panic!("output {key:?} not a document: {other:?}"),
        }
    }

    /// Static `document.url()` (no self) returns the current page's URL, built
    /// from its derived output + the site `base_url`. `content/main.typ` →
    /// output `index.html` → `https://example.com/`.
    #[test]
    fn current_page_url_is_derived() {
        let html = compile_main("#context document.url()");
        assert!(
            html.contains("https://example.com/"),
            "static document.url() did not resolve to the page URL:\n{html}"
        );
        assert!(
            !html.contains("__twyla-doc-pending__"),
            "doc-url placeholder leaked into output:\n{html}"
        );
    }

    /// An inline document *consumed by `.url()`* — never shown — is still
    /// discovered and emitted as its own output, and the method returns its URL
    /// (`base_url` + output). The reverse of show-driven discovery: here the
    /// element is swallowed before realization, so `.url()` must report it.
    #[test]
    fn document_consumed_by_url_is_still_emitted() {
        let outputs = compile_outputs(
            "#context html.elem(\"iframe\", attrs: (\n  \
               src: document(output: \"sub/index.html\")[Sub body].url(),\n))",
        );

        let sub = doc_html(&outputs, "sub/index.html");
        assert!(sub.contains("Sub body"), "sub body missing:\n{sub}");

        let main = doc_html(&outputs, "index.html");
        assert!(
            main.contains("https://example.com/sub/"),
            "iframe src not resolved to the sub URL:\n{main}"
        );
    }

    /// Dedup, the property Sam called out: a document that is BOTH shown inline
    /// AND `.url()`'d (here twice) — all targeting one `output` — is emitted
    /// exactly once, and the compile still converges. Discovery dedups by
    /// `output` (in the resolver) and the output map is keyed by path, so
    /// neither repeated `.url()` calls nor show+url duplicate it.
    #[test]
    fn url_and_show_discovery_dedup_to_one_output() {
        let outputs = compile_outputs(
            "#document(output: \"dup/index.html\")[Dup body]\n\
             #context document(output: \"dup/index.html\")[Dup body].url()\n\
             #context document(output: \"dup/index.html\")[Dup body].url()",
        );

        let dups = outputs
            .docs()
            .filter(|d| d.output_path == "dup/index.html")
            .count();
        assert_eq!(dups, 1, "document emitted {dups} times, expected exactly 1");
    }

    /// The user-presentation boundary: a page reads its own output as a
    /// root-absolute string (`/index.html`), and a `documents()` entry's
    /// `output` field is likewise `/`-prefixed — while the file is still routed
    /// and written under the no-slash key (`index.html`). Mirrors how a URL is
    /// root-absolute; the slash exists only in user-facing values.
    #[test]
    fn output_is_presented_root_absolute() {
        // `#context document.output` for the current page.
        let html = compile_main("#context document.output");
        assert!(
            html.contains("/index.html"),
            "document.output not shown root-absolute (`/index.html`):\n{html}"
        );

        // A `documents()` entry's `output` field, for the same page.
        let listed = compile_main(
            "#context {\n  \
               for d in documents() [#d.output]\n\
             }",
        );
        assert!(
            listed.contains("/index.html"),
            "documents() entry `output` not root-absolute:\n{listed}"
        );

        // The output is nonetheless routed/written under the no-slash key.
        let outputs = compile_outputs("hello");
        assert!(
            outputs.get("index.html").is_some(),
            "page not routed to the no-slash key `index.html`"
        );
        assert!(
            outputs.get("/index.html").is_none(),
            "page leaked a slash-prefixed key into the output map"
        );
    }

    /// A *relative* inline `document(output:)` still anchors on the enclosing
    /// page's output directory even though that anchor is now carried on the
    /// chain in root-absolute form (`/blog/index.html`): `resolve_output` skips
    /// the empty leading segment. `content/blog/main.typ` → `blog/` anchor, so
    /// `output: "extra.html"` lands at the no-slash key `blog/extra.html`.
    #[test]
    fn relative_child_output_resolves_under_slashed_anchor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("content/blog")).unwrap();
        std::fs::write(
            dir.path().join("content/blog/main.typ"),
            "#document(output: \"extra.html\")[Extra body]",
        )
        .unwrap();
        let ctx = TwylaContext::new(dir.path(), Some("https://example.com".into())).unwrap();
        let world = RenderWorld::new(&ctx).unwrap();
        let mut resolver = Resolver::new(&world.ctx);
        let outputs = world.compile_bundle(&mut resolver).unwrap();

        assert!(
            outputs.get("blog/extra.html").is_some(),
            "relative child output did not resolve under the slashed anchor"
        );
    }
}
