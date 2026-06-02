//! Twyla's version typst's `compile` / `compile_impl` fixed-point loop,
//! plus twyla's per-document metadata harvest.

use std::collections::{HashMap, HashSet};

use comemo::{Track, Tracked, TrackedMut};
use ecow::{EcoString, EcoVec, eco_vec};
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult, Warned};
use typst::foundations::{
    Array, BundlePath, Content, Datetime, Dict, IntoValue, NativeElement, Output, Style,
    StyleChain, Styles, Target, TargetElem, Value,
};
use typst::syntax::{FileId, Span, VirtualPath};
use typst::utils::LazyHash;
use typst_bundle::Bundle;
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{EmptyIntrospector, Introspector, Locator, MAX_ITERS, analyze};
use typst_library::model::{DocumentElem, DocumentInfo};
use typst_library::routines::{Arenas, RealizationKind};
use typst_utils::Protected;

use crate::asset::{AssetResolver, ResolvedAsset};
use crate::document::{TwylaDocument, TwylaDocumentList};
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
) -> Warned<SourceResult<CompiledBundle>> {
    let mut sink = Sink::new();
    let traced = Traced::default();
    let output = compile_bundle_impl(ctx, world.track(), traced.track(), &mut sink, sources)
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
) -> SourceResult<CompiledBundle> {
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

    // Evaluate each content file once into its body content.
    let evaled = engine.parallelize(sources, |engine, id| {
        let body = eval_file(engine, *id)?;
        let meta = harvest_metadata(ctx, engine, *id, &body)?;
        SourceResult::Ok((body, meta))
    });

    let mut bodies = Vec::new();
    let mut harvested = Vec::new();
    let mut documents = Array::new();
    for res in evaled {
        let (body, meta) = res?;
        let path = BundlePath::new(VirtualPath::new(&meta.output).unwrap()).unwrap();
        bodies.push(DocumentElem::new(path, body).pack());
        documents.push(meta.to_dict().into_value());
        harvested.push(meta);
    }

    // Inject the page list onto the style chain so `documents()` resolves it during the render pass.
    let docs_style = TwylaDocumentList::all.set(documents).wrap();

    // The resolver drains discovered assets between relayout iterations and
    // processes them. Cold seed for now; `serve` will seed from a warm store.
    let mut resolver = AssetResolver::new(ctx, HashMap::new());

    let content = Content::sequence(bodies);
    let injected = [docs_style];
    let bundle =
        relayout::<Bundle>(world, traced, sink, &content, &injected, Some(&mut resolver))?;
    Ok((bundle, harvested, resolver.into_resolved()))
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

/// The fixed-point relayout loop, operating on already-evaluated `content`.
/// Mirrors the loop in `typst::compile_impl`, with twyla's asset resolution
/// folded into the *same* loop (no nesting). `injected` is extra styles chained
/// on top of the base (the `documents()` page list). `assets`, when present,
/// drives asset-URL convergence: its resolved-map is re-injected every
/// iteration as it drains, and the loop won't break until assets are *settled*
/// as well as typst's own introspection — otherwise typst could declare
/// convergence on a round where a freshly-discovered asset still shows its
/// placeholder.
fn relayout<T: Output>(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    content: &Content,
    injected: &[LazyHash<Style>],
    mut assets: Option<&mut AssetResolver>,
) -> SourceResult<T> {
    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(T::target()).wrap();
    // Constant across iterations (and constant-hashing) — build once.
    let sink_style = assets.as_deref().map(|a| a.sink_style());
    let empty_introspector = EmptyIntrospector;

    let mut history: Vec<T> = Vec::new();
    let mut document: T;

    // Relayout until all introspections stabilize.
    // If that doesn't happen within five attempts, we give up.
    loop {
        // Build this iteration's style chain. Variable-length chaining can't be
        // done link-by-link (each link must outlive the chain), so fold target
        // + injected + resolved-map + sink into one `Styles` and chain it once.
        // The resolved-map is rebuilt each iteration as the resolver drains.
        let mut links: Vec<LazyHash<Style>> = Vec::with_capacity(injected.len() + 3);
        links.push(target.clone());
        links.extend(injected.iter().cloned());
        if let Some(a) = assets.as_deref() {
            links.push(a.map_style());
        }
        if let Some(ss) = &sink_style {
            links.push(ss.clone());
        }
        let combined: Styles = links.into_iter().collect();
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

        document = T::create(&mut engine, content, styles)?;

        // Drain discovered assets and process them off to the side; `settled`
        // is true when nothing new was resolved this round.
        let assets_settled = match assets.as_deref_mut() {
            Some(a) => a.drain_and_process(world)?,
            None => true,
        };

        if constraint.validate(document.introspector()) && assets_settled {
            sink.extend_from_sink(subsink);
            break;
        }

        if history.len() >= MAX_ITERS - 1 {
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
    let output = output.unwrap_or_else(|| ctx.default_output(id.vpath().get_without_slash()));
    let kind = kind.unwrap_or_else(|| match output.as_str() {
        "index.html" => EcoString::inline("root"),
        _ if output.ends_with("/index.html") => EcoString::inline("dir"),
        _ => EcoString::inline("page"),
    });
    let url = ctx.url_for(&output);

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

/// Deduplicate diagnostics. Mirrors `typst::deduplicate`.
fn deduplicate(mut diags: EcoVec<SourceDiagnostic>) -> EcoVec<SourceDiagnostic> {
    let mut unique = HashSet::new();
    diags.retain(|diag| {
        let hash = typst_utils::hash128(&(&diag.span, &diag.message));
        unique.insert(hash)
    });
    diags
}
