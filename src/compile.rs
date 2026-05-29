//! Vendored copy of typst's `compile` / `compile_impl` fixed-point loop.
//!
//! `typst::compile` is all-or-nothing: it owns the relayout loop and hands
//! back only the final output. Twyla needs to sit *inside* that loop — after
//! each iteration it will (in a later change) harvest per-document metadata
//! (`DocumentInfo`) and resolved assets, then feed them into the next
//! iteration via the style chain so contextual builtins like `pages()` see
//! them, all within typst's existing introspection fixed-point. See
//! doc/planning.typ (Crux 1 / Crux 2).
//!
//! This is a faithful reproduction of `typst/src/lib.rs::{compile,
//! compile_impl}` at rev de6f400, with three deliberate, behavior-neutral
//! deviations:
//! - timing instrumentation (`TimingScope`, `timed!`, `#[time]`) dropped —
//!   twyla doesn't depend on `typst-timing`;
//! - `deduplicate` uses `std::collections::HashSet` instead of `FxHashSet`;
//! - the main-file read-error path drops the `.typ`-extension hint (our main
//!   is the always-present in-memory virtual source, so it can't hit it).
//!
//! Output HTML and emitted diagnostics are otherwise identical — the
//! per-iteration hook is intentionally NOT here yet.

use std::collections::HashSet;

use comemo::{Track, Tracked};
use ecow::{EcoVec, eco_vec};
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult, Warned};
use typst::foundations::{Output, StyleChain, Target, TargetElem};
use typst::syntax::Span;
use typst_library::diag::{bail, warning};
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{
    EmptyIntrospector, Introspector, MAX_ITERS, analyze,
};
use typst_library::{Feature, Features};
use typst_utils::Protected;

/// Compiles sources into an output. Mirrors `typst::compile`.
pub fn compile<T>(world: &dyn World) -> Warned<SourceResult<T>>
where
    T: Output,
{
    let mut sink = Sink::new();
    let output = compile_impl::<T>(world.track(), Traced::default().track(), &mut sink)
        .map_err(deduplicate);
    Warned { output, warnings: sink.warnings() }
}

/// The fixed-point relayout loop. Mirrors `typst::compile_impl`.
fn compile_impl<T: Output>(
    world: Tracked<dyn World + '_>,
    traced: Tracked<Traced>,
    sink: &mut Sink,
) -> SourceResult<T> {
    let library = world.library();
    match T::target() {
        Target::Paged => {}
        Target::Html => warn_or_error_for_html(&library.features, sink)?,
        Target::Bundle => warn_or_error_for_bundle(&library.features, sink)?,
    }

    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(T::target()).wrap();
    let styles = base.chain(&target);
    let empty_introspector = EmptyIntrospector;

    // Fetch the main source file once.
    let main = world.main();
    let main = world.source(main).map_err(|err| {
        eco_vec![SourceDiagnostic::error(Span::detached(), ecow::EcoString::from(err))]
    })?;

    // First evaluate the main source file into a module.
    let content = typst_eval::eval(
        world,
        library,
        traced,
        sink.track_mut(),
        Route::default().track(),
        &main,
    )?
    .content();

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

        document = T::create(&mut engine, &content, styles)?;

        if constraint.validate(document.introspector()) {
            sink.extend_from_sink(subsink);
            break;
        }

        if history.len() >= MAX_ITERS - 1 {
            let mut introspectors =
                [&empty_introspector as &dyn Introspector; MAX_ITERS + 1];
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

/// Deduplicate diagnostics. Mirrors `typst::deduplicate`.
fn deduplicate(mut diags: EcoVec<SourceDiagnostic>) -> EcoVec<SourceDiagnostic> {
    let mut unique = HashSet::new();
    diags.retain(|diag| {
        let hash = typst_utils::hash128(&(&diag.span, &diag.message));
        unique.insert(hash)
    });
    diags
}

/// HTML export warns or errors depending on the feature flag. Mirrors the
/// private `typst::warn_or_error_for_html`.
fn warn_or_error_for_html(features: &Features, sink: &mut Sink) -> SourceResult<()> {
    const ISSUE: &str = "https://github.com/typst/typst/issues/5512";
    if features.is_enabled(Feature::Html) {
        sink.warn(warning!(
            Span::detached(),
            "html export is under active development and incomplete";
            hint: "its behaviour may change at any time";
            hint: "do not rely on this feature for production use cases";
            hint: "see {ISSUE} for more information";
        ));
    } else {
        bail!(
            Span::detached(),
            "html export is only available when `--features html` is passed";
            hint: "html export is under active development and incomplete";
            hint: "see {ISSUE} for more information";
        );
    }
    Ok(())
}

/// Bundle export warns or errors depending on the feature flag. Mirrors the
/// private `typst::warn_or_error_for_bundle`.
fn warn_or_error_for_bundle(features: &Features, sink: &mut Sink) -> SourceResult<()> {
    if features.is_enabled(Feature::Bundle) {
        sink.warn(warning!(
            Span::detached(),
            "bundle export is experimental";
            hint: "its behaviour may change at any time";
            hint: "do not rely on this feature for production use cases";
        ));
    } else {
        bail!(
            Span::detached(),
            "bundle export is only available when `--features bundle` is passed";
            hint: "bundle export is experimental";
        );
    }
    Ok(())
}
