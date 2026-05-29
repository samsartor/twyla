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
use ecow::{EcoVec, eco_vec};
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult, Warned};
use typst::foundations::{Content, Output, StyleChain, Target, TargetElem, Value};
use typst::syntax::Span;
use typst_bundle::Bundle;
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{
    EmptyIntrospector, Introspector, Locator, MAX_ITERS, analyze,
};
use typst_library::model::{DocumentElem, DocumentInfo};
use typst_library::routines::{Arenas, RealizationKind};
use typst_utils::Protected;

use crate::twyla_doc::TwylaDocument;

/// One page's harvested twyla metadata.
#[derive(Debug, Clone)]
pub struct HarvestedDoc {
    /// Bundle path, e.g. `spike-doc/index.html` — the route, read from the
    /// native `DocumentElem` we collected.
    pub path: String,
    /// The page's `document.extra` (`Value::None` if unset).
    pub extra: Value,
    /// The page's `document.draft`.
    pub draft: bool,
}

/// Compile the virtual main as a [`Bundle`] *and* harvest each page's twyla
/// `document` metadata, from a single evaluation of the main source.
pub fn compile_bundle(world: &dyn World) -> Warned<SourceResult<(Bundle, Vec<HarvestedDoc>)>> {
    let mut sink = Sink::new();
    let traced = Traced::default();
    let output =
        compile_bundle_impl(world.track(), traced.track(), &mut sink).map_err(deduplicate);
    Warned {
        output,
        warnings: sink.warnings(),
    }
}

fn compile_bundle_impl(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
) -> SourceResult<(Bundle, Vec<HarvestedDoc>)> {
    let content = eval_main(world, traced, sink)?;
    let bundle = relayout::<Bundle>(world, traced, sink, &content)?;
    let harvested = harvest_bundle(world, traced, sink, &content)?;
    Ok((bundle, harvested))
}

/// Evaluate the virtual main source into content. Mirrors the eval step of
/// `typst::compile_impl`. Runs exactly once per compile.
fn eval_main(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
) -> SourceResult<Content> {
    let library = world.library();
    let main = world.main();
    let main = world.source(main).map_err(|err| {
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
        &main,
    )?
    .content())
}

/// The fixed-point relayout loop, operating on already-evaluated `content`.
/// Mirrors the loop in `typst::compile_impl`.
fn relayout<T: Output>(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    content: &Content,
) -> SourceResult<T> {
    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(T::target()).wrap();
    let styles = base.chain(&target);
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

/// Realize the virtual main, walk top-level `DocumentElem`s, and harvest each
/// one's twyla fields from its body's resolved style chain.
///
/// `#set document(..)` written *inside* an `#include`d file does not leak to a
/// sibling emitted after the include (set-rule scoping), so we can't read a
/// page's twyla fields by appending a `#context` block in `generate_main`.
/// Native `DocumentInfo` only dodges this because realize walks the whole body
/// subtree and catches every `set document` via a hook hardcoded to the native
/// element (`typst-realize/src/lib.rs:609`). We do the same for twyla's element
/// without that hook by re-realizing each body with `RealizationKind::Document`
/// and reading the fields off the folded child style chains — using only public
/// APIs (no fork; vendors only `collect`'s walk).
///
/// Spike-grade: an `EmptyIntrospector` (no convergence loop — `extra`/`draft`
/// don't depend on introspection). The real version folds this into [`relayout`].
fn harvest_bundle(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
    content: &Content,
) -> SourceResult<Vec<HarvestedDoc>> {
    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(Target::Bundle).wrap();
    let top_styles = base.chain(&target);

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

    // Top-level (bundle) realization: yields document elements as opaque
    // children (their bodies are compiled separately).
    let arenas = Arenas::default();
    let top_map = top_styles.to_map().outside();
    let top_styles = StyleChain::new(&top_map);
    let children = (engine.library.routines.realize)(
        RealizationKind::Bundle,
        &mut engine,
        &mut locator,
        &arenas,
        content,
        top_styles,
    )?;

    let mut out = Vec::new();
    for (elem, styles) in &children {
        let Some(doc) = elem.to_packed::<DocumentElem>() else {
            continue;
        };
        let path = doc.path.as_ref().get_without_slash().to_string();

        // Re-realize this document's body with a Document-kind realization
        // (mirrors compile_document's per-doc realize). The body's top-of-file
        // `#set document(..)` folds into the realized children's chains.
        let body_map = styles.to_map().outside();
        let body_styles = StyleChain::new(&body_map);
        let body_arenas = Arenas::default();
        let mut info = DocumentInfo::default();
        let body_children = (engine.library.routines.realize)(
            RealizationKind::Document { info: &mut info },
            &mut engine,
            &mut locator,
            &body_arenas,
            &doc.body,
            body_styles,
        )?;

        let mut extra = Value::None;
        let mut draft = false;
        for (_, child_styles) in &body_children {
            if child_styles.has(TwylaDocument::extra) {
                extra = child_styles.get_cloned(TwylaDocument::extra);
            }
            draft = child_styles.get(TwylaDocument::draft);
        }
        out.push(HarvestedDoc { path, extra, draft });
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
