//! HTML AST-equivalence harness — the porting feedback loop.
//!
//! Walks two normalized [`crate::html::Node`] trees in lockstep and reports the
//! first structural divergence. Used by the convert harness, which diffs every
//! page against the zola ground truth.
//!
//! Parsing, the `Node`/`Element` types, serialization, and subtree selection
//! live in [`crate::html`] — this module is purely the *comparator* plus its
//! opt-in *relaxation rules* (see [`relax`]), which let a caller ignore specific
//! attributes, ignore an attribute's value, skip subtrees, or compare an element
//! by text only. The framework is data-driven — adding a relaxation is a
//! few-line change to a [`RelaxConfig`]. The default config has *no* rules:
//! out of the box it's the strictest comparison the normalization permits.

pub mod compare;
pub mod patch;
pub mod relax;

pub use compare::{Divergence, DivergenceReason, PathStep, diff};
pub use relax::{Matcher, RelaxConfig, RelaxationRule};
