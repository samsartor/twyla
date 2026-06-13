//! Twyla's version typst's `compile` / `compile_impl` fixed-point loop,
//! plus twyla's per-document metadata harvest.

use std::collections::HashSet;

use comemo::{Track, Tracked, TrackedMut};
use ecow::{EcoString, EcoVec, eco_vec};
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult, Warned};
use typst::foundations::{
    Array, BundlePath, Content, Datetime, Dict, IntoValue, NativeElement, Output, Smart,
    StyleChain, Styles, Target, TargetElem, Value,
};
use typst::syntax::{FileId, Span, VirtualPath};
use typst_bundle::Bundle;
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{EmptyIntrospector, Introspector, Locator, MAX_ITERS, analyze};
use typst_library::model::{DocumentElem, DocumentInfo};
use typst_library::routines::{Arenas, RealizationKind};
use typst_utils::Protected;

use crate::asset::{AssetResolver, ResolvedAsset};
use crate::document::{DiscoveredDoc, TwylaDocument, TwylaDocumentList};
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
    resolver: &mut AssetResolver,
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
    resolver: &mut AssetResolver,
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
    Ok((bundle, harvested, resolver.resolved_assets()))
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
    resolver: &mut AssetResolver,
) -> SourceResult<Bundle> {
    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(Bundle::target()).wrap();
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
    let output =
        output.unwrap_or_else(|| ctx.default_document_output(id.vpath().get_without_slash()));
    let kind =
        kind.unwrap_or_else(|| EcoString::from(ctx.default_kind(id.vpath().get_without_slash())));
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
    resolver: &AssetResolver,
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
fn build_content(files: &[(Content, HarvestedDoc)], resolver: &AssetResolver) -> Content {
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
    use crate::asset::AssetResolver;
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
        let mut resolver = AssetResolver::new(&world.ctx);
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

    /// TARGET, blocked on `document.url()`: an inline document *consumed by a
    /// method* — never shown — must still be discovered and emitted. Today
    /// discovery rides the show rule (`document::RENDER_NOTHING`), so a document
    /// that `.url()` swallows before realization would route nothing, and the
    /// `src` here would dangle. The fix is backup discovery inside `.url()`
    /// (mirroring assets), deduped by output — see the discovery-eagerness
    /// discussion. Un-ignore when `document.url()` lands.
    #[test]
    #[ignore = "blocked on document.url() + method-side discovery"]
    fn document_consumed_by_url_is_still_emitted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("content")).unwrap();
        std::fs::write(
            dir.path().join("content/main.typ"),
            "#html.elem(\"iframe\", attrs: (\n  \
               src: document(output: \"sub/index.html\")[Sub body].url(),\n))",
        )
        .unwrap();
        let ctx = TwylaContext::new(dir.path(), Some("https://example.com".into())).unwrap();
        let world = RenderWorld::new(&ctx).unwrap();
        let mut resolver = AssetResolver::new(&world.ctx);
        let outputs = world.compile_bundle(&mut resolver).unwrap();

        // The swallowed document is emitted as its own output, body intact.
        let sub = match outputs.get("sub/index.html") {
            Some(crate::render::Output::Doc(d)) => d.html.clone(),
            other => panic!("sub document not emitted: {other:?}"),
        };
        assert!(sub.contains("Sub body"), "sub body missing:\n{sub}");

        // The iframe on the main page points at the resolved sub URL, not a
        // leftover asset/placeholder.
        let main = match outputs.get("index.html") {
            Some(crate::render::Output::Doc(d)) => d.html.clone(),
            other => panic!("main page missing: {other:?}"),
        };
        assert!(main.contains("/sub/"), "iframe src not resolved:\n{main}");
    }
}
