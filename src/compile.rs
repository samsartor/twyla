//! Twyla's version typst's `compile` / `compile_impl` fixed-point loop,
//! plus twyla's per-document metadata harvest.

use std::collections::HashSet;
use std::sync::Arc;

use comemo::{Track, Tracked, TrackedMut};
use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use iddqd::IdHashMap;
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult, Warned};
use typst::foundations::{
    Array, BundlePath, Content, Datetime, Dict, Dynamic, IntoValue, NativeElement, Output, Smart,
    StyleChain, Styles, Target, TargetElem, Value,
};
use typst::syntax::{FileId, Span, VirtualPath};
use typst_bundle::{Bundle, BundleIntrospector};
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{EmptyIntrospector, Introspector, Locator, MAX_ITERS, analyze};
use typst_library::model::{DocumentElem, DocumentInfo};
use typst_library::routines::{Arenas, RealizationKind};
use typst_utils::Protected;

use crate::asset::{AssetReqIntrospect, ResolvedAsset, hash_spec};
use crate::resolver::Resolver;
use crate::document::{DOCUMENTS_LIST_KEY, DocumentReqIntrospect, ResolvedDocument};
use crate::project::TwylaContext;

/// One page's harvested twyla metadata — the row that becomes one
/// `documents()` entry and feeds feeds/listings.
#[derive(Debug, Clone)]
pub struct HarvestedDoc {
    /// User-facing URL (e.g. "/hello/").
    pub url: String,
    /// The written location of the document (e.g. "hello/index.html").
    pub output: String,
    pub title: Option<Content>,
    pub date: Option<Datetime>,
    pub description: Option<Content>,
    pub draft: bool,
    pub kind: EcoString,
    pub extra: Value,
}

impl HarvestedDoc {
    /// The `documents()` dictionary form of this row.
    fn to_dict(&self) -> Dict {
        let mut d = Dict::new();
        d.insert("url".into(), self.url.clone().into_value());
        d.insert("output".into(), self.output.clone().into_value());
        d.insert("title".into(), self.title.clone().into_value());
        d.insert("date".into(), self.date.into_value());
        d.insert("description".into(), self.description.clone().into_value());
        d.insert("draft".into(), self.draft.into_value());
        d.insert("kind".into(), self.kind.clone().into_value());
        d.insert("extra".into(), self.extra.clone());
        d
    }
}

/// The product of one bundle compile: the rendered [`Bundle`], each page's
/// harvested twyla `document` metadata, and every asset resolved along the way.
pub type CompiledBundle = (Bundle, Vec<HarvestedDoc>, Vec<ResolvedAsset>);

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
        let meta = harvest_metadata(ctx, engine, *id, &body)?;
        SourceResult::Ok((body, meta))
    });
    let mut files: Vec<(Content, HarvestedDoc)> = Vec::new();
    for res in evaled {
        files.push(res?);
    }

    // One fixed point: typst introspection, asset resolution, and document
    // discovery all converge together (see [`compile_bundle_loop`]).
    let bundle = compile_bundle_loop(ctx, world, traced, sink, &files, resolver)?;

    // Final metadata: each file page, then every discovered document.
    let mut harvested: Vec<HarvestedDoc> = files.into_iter().map(|(_, meta)| meta).collect();
    for doc in resolver.documents() {
        harvested.push(harvested_from_discovered(ctx, doc));
    }
    Ok((bundle, harvested, resolver.resolved_assets().cloned().collect()))
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

pub struct TwylaIntrospector {
    pub inner: Arc<BundleIntrospector>,
    pub resolver: Resolver,
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
        todo!()
    }

    fn locator(&self, key: u128, base: Location) -> Option<Location> {
        todo!()
    }

    fn pages(&self, location: Location) -> Option<NonZeroUsize> {
        todo!()
    }

    fn page(&self, location: Location) -> Option<NonZeroUsize> {
        todo!()
    }

    fn position(&self, location: Location) -> Option<DocumentPosition> {
        todo!()
    }

    fn page_numbering(&self, location: Location) -> Option<&Numbering> {
        todo!()
    }

    fn page_supplement(&self, location: Location) -> Option<&Content> {
        todo!()
    }

    fn anchor(&self, location: Location) -> Option<&EcoString> {
        todo!()
    }

    fn document(&self, location: Location) -> Option<Location> {
        todo!()
    }

    fn path(&self, location: Location) -> Option<&VirtualPath> {
        todo!()
    }

    // TODO: update ustream to return Value instead of &Value
    fn value(&self, key: u128) -> Option<Value> {
        if key == DOCUMENTS_LIST_KEY {
            // TODO: only compute when updated
            return Some(
                self.resolver
                    .documents()
                    .map(|doc| doc.doc.to_dict().to_value())
                    .collect(),
            );
        }
        for asset in self.resolver.resolved_assets() {
            // TODO: can we use the IdHashMap for this lookup somehow?
            if hash_spec(&asset.spec) == key {
                return Some(Value::Dyn(Dynamic::new(asset.clone())));
            }
        }
        None
    }
}

/// The single fixed-point loop: typst's introspection convergence, asset URL
/// resolution, and document discovery all settle here together. Mirrors the
/// loop in `typst::compile_impl`, but each iteration rebuilds the bundle content
/// and `documents()` listing from the file pages *plus every document discovered
/// so far*, then drains both discovery sinks.
///
/// Why all three share one loop: a `#document(..)` (inline or `#context`-
/// generated) reports itself on the doc sink during the very same Html
/// realization that resolves assets, so there's no reason to separate them — and
/// doing so would waste iterations rendering with placeholder assets. Document
/// discovery needs its *own* settle flag (not just typst's introspection check):
/// a freshly discovered doc isn't in the content that was just realized, and the
/// `documents()` list is a style-chain read introspection doesn't track, so
/// neither registers in `constraint`. Convergence therefore requires all of
/// introspection-stable **and** assets-settled **and** documents-settled.
fn compile_bundle_loop(
    ctx: &TwylaContext,
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    files: &[(Content, HarvestedDoc)],
    resolver: &mut Resolver,
) -> SourceResult<Bundle> {
    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(Bundle::target()).wrap();
    // The site `base_url`, carried on the chain so `document.url()` can build
    // absolute URLs without a `TwylaContext`. Constant across the build.
    let base_url = TwylaSite::base_url
        .set(match &ctx.base_url {
            Some(url) => Value::Str(url.as_str().into()),
            None => Value::None,
        })
        .wrap();
    // Constant across iterations within one discovery generation — build once.
    let asset_sink = resolver.sink_style();
    let doc_sink = resolver.doc_sink_style();
    let empty_introspector = EmptyIntrospector;

    let mut history: Vec<Bundle> = Vec::new();
    let mut document: Bundle;

    loop {
        // Rebuild content + the `documents()` listing from the file pages plus
        // every document discovered so far; both grow as discovery proceeds.
        let documents = build_documents_array(ctx, files, resolver);
        let docs_list = TwylaDocumentList::all.set(documents).wrap();
        let content = build_content(files, resolver);

        // Fold target + documents list + asset map + both discovery sinks into
        // one `Styles` and chain it once (variable-length chaining can't be done
        // link-by-link). The asset map is rebuilt each iteration as it drains.
        let combined: Styles = [
            target.clone(),
            base_url.clone(),
            docs_list,
            resolver.map_style(),
            asset_sink.clone(),
            doc_sink.clone(),
        ]
        .into_iter()
        .collect();
        let styles = base.chain(&combined);

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
            traced,
            sink: subsink.track_mut(),
            route: Route::default(),
        };

        document = Bundle::create(&mut engine, &content, styles)?;

        // Drain both discovery channels (assets process eagerly; documents are
        // just collected). Each returns whether it's *settled* this round.
        let assets_settled = resolver.drain_and_process(world)?;
        let docs_settled = resolver.drain_documents();

        if constraint.validate(document.introspector()) && assets_settled && docs_settled {
            sink.extend_from_sink(subsink);
            break;
        }

        if history.len() >= MAX_ITERS - 1 {
            // Distinguish *our* non-convergence (an unstable document set) from
            // typst's own (introspection), which it just warns about and accepts.
            if !docs_settled {
                return Err(eco_vec![SourceDiagnostic::error(
                    Span::detached(),
                    EcoString::from(
                        "the set of `#context`-generated documents did not stabilize; a \
                         generator is likely emitting a new `document(..)` every pass (e.g. \
                         deriving its `output` from `documents()` itself)"
                    ),
                )]);
            }

            let mut introspectors = [&empty_introspector as &dyn Introspector; MAX_ITERS + 1];
            for i in 1..MAX_ITERS {
                introspectors[i] = history[i - 1].introspector();
            }
            introspectors[MAX_ITERS] = document.introspector();

            let warnings = analyze(world, introspectors, subsink.introspections());

            sink.extend_from_sink(subsink);
            for warning in warnings {
                sink.warn(warning);
            }
            break;
        }

        history.push(document);
    }

    // Promote delayed errors.
    let delayed = sink.delayed();
    if !delayed.is_empty() {
        return Err(delayed);
    }

    Ok(document)
}

/// Harvest each page's twyla `document` fields from its body's resolved style
/// chain.
fn harvest_metadata(
    ctx: &TwylaContext,
    engine: &mut Engine,
    id: FileId,
    body: &Content,
) -> SourceResult<HarvestedDoc> {
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
        body,
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
    let url = ctx.document_url(&output);

    Ok(HarvestedDoc {
        output,
        url,
        title,
        date,
        description,
        draft,
        kind,
        extra,
    })
}

/// The `documents()` array for the current loop iteration: every file page,
/// then every document discovered so far (in the resolver's deterministic
/// order).
fn build_documents_array(
    ctx: &TwylaContext,
    files: &[(Content, HarvestedDoc)],
    resolver: &Resolver,
) -> Array {
    let mut documents = Array::new();
    for (_, meta) in files {
        documents.push(meta.to_dict().into_value());
    }
    for doc in resolver.documents() {
        documents.push(harvested_from_discovered(ctx, doc).to_dict().into_value());
    }
    documents
}

/// The bundle content for the current loop iteration: each file body wrapped as
/// its own routed document, plus each discovered document as a sibling.
fn build_content(files: &[(Content, HarvestedDoc)], resolver: &Resolver) -> Content {
    let mut bodies = Vec::new();
    for (body, meta) in files {
        // Re-apply the *derived* output onto the page body so the page can read
        // it back (`#context document.output`). Without this the page sees only
        // the `auto` default — the derivation happens Rust-side in `harvest_doc`
        // and never reaches the chain. (`discovered_sibling` does the same for
        // inline documents; this keeps full-file pages consistent.) Idempotent
        // when the user pinned `output` explicitly: `meta.output` already is it.
        let mut styles = Styles::new();
        styles.set(
            TwylaDocument::output,
            Smart::Custom(meta.output.as_str().into()),
        );
        bodies.push(wrap_document(
            &meta.output,
            body.clone().styled_with_map(styles),
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
fn discovered_sibling(doc: &DiscoveredDoc) -> Content {
    let mut styles = Styles::new();
    styles.set(
        TwylaDocument::output,
        Smart::Custom(doc.output.as_str().into()),
    );
    styles.set(TwylaDocument::title, doc.title.clone());
    styles.set(TwylaDocument::date, doc.date);
    styles.set(TwylaDocument::description, doc.description.clone());
    styles.set(TwylaDocument::kind, Smart::Custom(doc.kind.clone()));
    styles.set(TwylaDocument::extra, doc.extra.clone());
    styles.set(TwylaDocument::draft, doc.draft);
    wrap_document(&doc.output, doc.body.clone().styled_with_map(styles))
}

/// Build a [`HarvestedDoc`] (a `documents()` row + output meta) from a
/// discovered document, deriving its URL from the configured base.
fn harvested_from_discovered(ctx: &TwylaContext, doc: &DiscoveredDoc) -> HarvestedDoc {
    HarvestedDoc {
        url: ctx.document_url(&doc.output),
        output: doc.output.clone(),
        title: doc.title.clone(),
        date: doc.date,
        description: doc.description.clone(),
        draft: doc.draft,
        kind: doc.kind.clone(),
        extra: doc.extra.clone(),
    }
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
    use crate::resolver::Resolver;
    use crate::project::TwylaContext;
    use crate::render::RenderWorld;

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
}
