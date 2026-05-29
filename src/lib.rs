//! twyla — a typst-based static site generator.
//!
//! Modules used by the `twyla` CLI:
//!
//! - [`render`] — compile typst content to HTML through the bundle
//!   export feature. Single-slug and full-site entry points.
//! - [`diff`] — structural HTML AST diff with opt-in relaxations. Used
//!   by the porting workflow to verify zola ↔ typst equivalence.
//! - [`import`] — generate a typst draft from a zola markdown post.
//!   Output is scaffolding, not a maintained md↔typ sync.
//! - [`serve`] — zero-flag dev server.

pub mod build;
pub mod compile;
pub mod diff;
pub mod import;
pub mod prelude;
pub mod project;
pub mod render;
pub mod rules;
pub mod serve;
