//! SPIKE — per-document metadata harvest.
//!
//! The decisive experiment for the `documents` design. `#set document(..)`
//! written *inside* an included content file does not leak to a sibling
//! emitted after the `#include` (set-rule scoping), so we can't read a
//! page's twyla fields by appending a `#context` block in `generate_main`.
//! Native `DocumentInfo` dodges this only because realize walks the whole
//! body subtree and catches every `set document` via a hook hardcoded to
//! the native element (`typst-realize/src/lib.rs:609`).
//!
//! This module proves twyla can do the same for its *own* element without
//! that hook and without forking `bundle_impl`: re-realize each document
//! body with `RealizationKind::Document` and read `TwylaDocument::extra`
//! off the folded style chain of the realized children. Everything it
//! touches is a public API (`routines.realize`, `RealizationKind`,
//! `DocumentElem`, `StyleChain`), so it stays no-fork — it vendors only
//! `collect`'s ~15-line walk, not the sealed introspector/link/finalize
//! machinery.

use comemo::Track;
use typst::World;
use typst::diag::SourceResult;
use typst::foundations::{StyleChain, Target, TargetElem, Value};
use typst::utils::Protected;
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{EmptyIntrospector, Locator};
use typst_library::model::{DocumentElem, DocumentInfo};
use typst_library::routines::{Arenas, RealizationKind};

use crate::twyla_doc::TwylaDocument;

/// One page's harvested twyla metadata.
#[derive(Debug, Clone)]
pub struct HarvestedDoc {
    /// Bundle path, e.g. `spike-doc/index.html` — the route, known here
    /// from the native `DocumentElem` we collected.
    pub path: String,
    /// The page's `document.extra` (`Value::None` if unset).
    pub extra: Value,
    /// The page's `document.draft`.
    pub draft: bool,
}

/// Realize the virtual main, walk top-level `DocumentElem`s, and harvest
/// each one's twyla fields from its body's resolved style chain.
///
/// Spike-grade: a fresh eval+realize with an `EmptyIntrospector` (no
/// convergence loop — `extra`/`draft` don't depend on introspection). The
/// real version folds this into the vendored relayout loop.
pub fn harvest(world: &dyn World) -> SourceResult<Vec<HarvestedDoc>> {
    let world = world.track();
    let traced = Traced::default();
    let traced = traced.track();
    let mut sink = Sink::new();

    let library = world.library();
    let base = StyleChain::new(&library.styles);
    let target = TargetElem::target.set(Target::Bundle).wrap();
    let top_styles = base.chain(&target);

    // Eval the virtual main into content (same as compile_impl).
    let main = world.source(world.main()).map_err(|_| eco())?;
    let content = typst_eval::eval(
        world,
        library,
        traced,
        sink.track_mut(),
        Route::default().track(),
        &main,
    )
    .map_err(|_| eco())?
    .content();

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

    // Top-level (bundle) realization: yields the document elements as
    // opaque children (their bodies are compiled separately).
    let arenas = Arenas::default();
    let top_map = top_styles.to_map().outside();
    let top_styles = StyleChain::new(&top_map);
    let children = (engine.library.routines.realize)(
        RealizationKind::Bundle,
        &mut engine,
        &mut locator,
        &arenas,
        &content,
        top_styles,
    )?;

    let mut out = Vec::new();
    for (elem, styles) in &children {
        let Some(doc) = elem.to_packed::<DocumentElem>() else {
            continue;
        };
        let path = doc.path.as_ref().get_without_slash().to_string();

        // Re-realize this document's body with a Document-kind realization
        // (mirrors compile_document's per-doc realize). The body's
        // top-of-file `#set document(..)` folds into the realized
        // children's chains, where we can read our fields.
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

        // Read the twyla fields off the first child that carries them.
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

/// Placeholder error vec (spike maps eval failures to an empty diagnostic
/// set; the real loop threads real diagnostics).
fn eco() -> ecow::EcoVec<typst::diag::SourceDiagnostic> {
    ecow::EcoVec::new()
}
