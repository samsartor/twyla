//! Vendored copy of typst's `compile` / `compile_impl` fixed-point loop,
//! plus twyla's per-document metadata harvest.
//!
//! `typst::compile` is all-or-nothing: it owns the relayout loop and hands
//! back only the final output. Twyla needs to sit *inside* that loop — and,
//! eventually, after each iteration harvest per-document metadata and feed it
//! into the next via the style chain so contextual builtins like `documents`
//! see it, all within typst's existing introspection fixed-point. See
//! doc/planning.typ (Crux 1 / Crux 2).
//!
//! The loop body is a faithful reproduction of `typst/src/lib.rs::compile_impl`
//! at rev de6f400, minus timing instrumentation (twyla doesn't depend on
//! `typst-timing`) and with `deduplicate` on `std::collections::HashSet`.
//!
//! [`compile_bundle`] is the twyla entry point: it evaluates the virtual main
//! **once** and reuses that single `Content` for both the relayout loop (which
//! produces the [`Bundle`]) and the [`harvest_bundle`] pass (which reads each
//! page's twyla `document` fields off its body's resolved style chain). The
//! harvest does its own realize — the price of not forking the sealed
//! `bundle_impl` introspector/link machinery — but shares the (expensive)
//! eval. Folding the harvest *into* the relayout loop (to inject `documents`
//! for the next iteration) is the next step; today it's a post-loop pass.

use std::collections::HashSet;

use comemo::{Track, Tracked};
use ecow::{EcoString, EcoVec, eco_vec};
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult, Warned};
use typst::foundations::{
    Array, BundlePath, Content, Datetime, Dict, IntoValue, NativeElement, Output, Style,
    StyleChain, Target, TargetElem, Value,
};
use typst::syntax::{FileId, Span};
use typst::utils::LazyHash;
use typst_bundle::Bundle;
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{
    EmptyIntrospector, Introspector, Locator, MAX_ITERS, analyze,
};
use typst_library::model::{DocumentElem, DocumentInfo};
use typst_library::routines::{Arenas, RealizationKind};
use typst_utils::Protected;

use crate::twyla_doc::{TwylaDocument, TwylaDocumentList};

/// A page to route: its source file and the bundle path it emits at.
///
/// Twyla owns routing, so it assembles the bundle `Content` itself: each
/// source is evaluated independently and wrapped in a native `DocumentElem`
/// at `path`, then the lot is sequenced. This replaces the old generated
/// `main.typ` that `#include`d every page — no string templating, typed
/// routing, and per-file spans for diagnostics.
#[derive(Debug, Clone)]
pub struct RoutedSource {
    /// The content file to evaluate (e.g. `/content/hello.typ`).
    pub id: FileId,
    /// Where the resulting document lands in the bundle (e.g.
    /// `hello/index.html`).
    pub path: BundlePath,
    /// User-facing URL of the page (e.g. `/hello/`), surfaced as
    /// `documents().*.url`.
    pub url: String,
}

/// A content file after evaluation: its route plus its body content.
struct EvaledPage {
    path: BundlePath,
    url: String,
    body: Content,
}

/// One page's harvested twyla metadata — the row that becomes one
/// `documents()` entry and feeds feeds/listings.
#[derive(Debug, Clone)]
pub struct HarvestedDoc {
    /// User-facing URL (e.g. `/hello/`).
    pub url: String,
    /// `document.title` (`None` if unset).
    pub title: Option<Content>,
    /// `document.date`.
    pub date: Option<Datetime>,
    /// `document.description`.
    pub description: Option<Content>,
    /// `document.draft`.
    pub draft: bool,
    /// Resolved page kind: the explicit `document.kind` if set, else derived
    /// from the route (`/` → `"root"`, else `"page"`).
    pub kind: EcoString,
    /// `document.extra` (`Value::None` if unset).
    pub extra: Value,
}

impl HarvestedDoc {
    /// The `documents()` dictionary form of this row.
    fn to_dict(&self) -> Dict {
        let mut d = Dict::new();
        d.insert("url".into(), self.url.clone().into_value());
        d.insert("title".into(), self.title.clone().into_value());
        d.insert("date".into(), self.date.into_value());
        d.insert("description".into(), self.description.clone().into_value());
        d.insert("draft".into(), self.draft.into_value());
        d.insert("kind".into(), self.kind.clone().into_value());
        d.insert("extra".into(), self.extra.clone());
        d
    }
}

/// Compile `pages` into a [`Bundle`] *and* harvest each page's twyla
/// `document` metadata. Each source is evaluated exactly once; that single
/// eval feeds both the relayout loop and the harvest.
pub fn compile_bundle(
    world: &dyn World,
    pages: &[RoutedSource],
) -> Warned<SourceResult<(Bundle, Vec<HarvestedDoc>)>> {
    let mut sink = Sink::new();
    let traced = Traced::default();
    let output =
        compile_bundle_impl(world.track(), traced.track(), &mut sink, pages).map_err(deduplicate);
    Warned {
        output,
        warnings: sink.warnings(),
    }
}

fn compile_bundle_impl(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    pages: &[RoutedSource],
) -> SourceResult<(Bundle, Vec<HarvestedDoc>)> {
    // Evaluate each content file once into its body content.
    let evaled: Vec<EvaledPage> = pages
        .iter()
        .map(|page| {
            Ok(EvaledPage {
                path: page.path.clone(),
                url: page.url.clone(),
                body: eval_file(world, traced, sink, page.id)?,
            })
        })
        .collect::<SourceResult<_>>()?;

    // Harvest each page's metadata *before* assembling, so the `documents()`
    // array is available to inject into the render pass below.
    let harvested = harvest_bodies(world, traced, sink, &evaled)?;
    let docs_array: Array = harvested.iter().map(|h| h.to_dict().into_value()).collect();

    // Assemble the combined bundle content: wrap each body in a native
    // `DocumentElem` at its route, sequence the lot. This is what the
    // generated `main.typ` used to express as `#document(path)[#include]`.
    let content = Content::sequence(
        evaled
            .iter()
            .map(|p| DocumentElem::new(p.path.clone(), p.body.clone()).pack()),
    );

    // Inject the page list onto the style chain so `documents()` resolves it
    // during the render pass.
    let docs_style = TwylaDocumentList::all.set(docs_array).wrap();
    let bundle = relayout::<Bundle>(world, traced, sink, &content, Some(&docs_style))?;
    Ok((bundle, harvested))
}

/// Evaluate a single content file into its body content. Mirrors the eval
/// step of `typst::compile_impl`, but per-file (no virtual main / include).
fn eval_file(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    id: FileId,
) -> SourceResult<Content> {
    let library = world.library();
    let source = world.source(id).map_err(|err| {
        eco_vec![SourceDiagnostic::error(
            Span::detached(),
            ecow::EcoString::from(err)
        )]
    })?;
    Ok(typst_eval::eval(
        world,
        library,
        traced,
        sink.track_mut(),
        Route::default().track(),
        &source,
    )?
    .content())
}

/// The fixed-point relayout loop, operating on already-evaluated `content`.
/// Mirrors the loop in `typst::compile_impl`. `injected` is an optional extra
/// style chained on top of the base (twyla uses it to inject the `documents()`
/// page list).
fn relayout<T: Output>(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    content: &Content,
    injected: Option<&LazyHash<Style>>,
) -> SourceResult<T> {
    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(T::target()).wrap();
    let styles = base.chain(&target);
    let styles = match injected {
        Some(s) => styles.chain(s),
        None => styles,
    };
    let empty_introspector = EmptyIntrospector;

    let mut history: Vec<T> = Vec::new();
    let mut document: T;

    // Relayout until all introspections stabilize.
    // If that doesn't happen within five attempts, we give up.
    loop {
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

        if constraint.validate(document.introspector()) {
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
/// chain. Because twyla assembled the bundle itself, we already hold each
/// page's `(path, body)` — so we realize each *body* directly with
/// `RealizationKind::Document` (no top-level bundle realize + `DocumentElem`
/// walk needed, unlike when typst owned the assembly).
///
/// Why a realize at all: `#set document(..)` written inside a content file
/// doesn't leak to a sibling, so we can't read the fields by appending a
/// `#context` block. Native `DocumentInfo` only dodges this because realize
/// walks the whole body subtree and catches every `set document` via a hook
/// hardcoded to the native element (`typst-realize/src/lib.rs:609`). We do the
/// same for twyla's element without that hook: realize the body and read the
/// fields off the folded child style chains. Public APIs only — no fork.
///
/// Spike-grade: an `EmptyIntrospector` (no convergence loop — `extra`/`draft`
/// don't depend on introspection). The real version folds this into [`relayout`].
fn harvest_bodies(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    pages: &[EvaledPage],
) -> SourceResult<Vec<HarvestedDoc>> {
    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(Target::Bundle).wrap();
    let styles = base.chain(&target);

    let empty = EmptyIntrospector;
    let mut locator = Locator::root().split();
    let mut engine = Engine {
        library,
        world,
        introspector: Protected::new(empty.track()),
        traced,
        sink: sink.track_mut(),
        route: Route::default(),
    };

    // Documents are top-level siblings sharing the bundle base chain, so every
    // body realizes under the same `outside` styles.
    let style_map = styles.to_map().outside();
    let styles = StyleChain::new(&style_map);

    let mut out = Vec::new();
    for page in pages {
        let arenas = Arenas::default();
        let mut info = DocumentInfo::default();
        let children = (engine.library.routines.realize)(
            RealizationKind::Document { info: &mut info },
            &mut engine,
            &mut locator,
            &arenas,
            &page.body,
            styles,
        )?;

        // Read twyla's `document` fields off the folded child style chains.
        // Each top-of-file `#set document(..)` applies to subsequent siblings,
        // so any realized child carries the page-level values.
        let mut title = None;
        let mut date = None;
        let mut description = None;
        let mut kind: Option<EcoString> = None;
        let mut extra = Value::None;
        let mut draft = false;
        for (_, cs) in &children {
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
                kind = cs.get_cloned(TwylaDocument::kind);
            }
            if cs.has(TwylaDocument::extra) {
                extra = cs.get_cloned(TwylaDocument::extra);
            }
            draft = cs.get(TwylaDocument::draft);
        }

        // Resolve `kind`: explicit wins, else derive from the route.
        let kind = kind.unwrap_or_else(|| {
            if page.url == "/" {
                EcoString::inline("root")
            } else {
                EcoString::inline("page")
            }
        });

        out.push(HarvestedDoc {
            url: page.url.clone(),
            title,
            date,
            description,
            draft,
            kind,
            extra,
        });
    }

    Ok(out)
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
