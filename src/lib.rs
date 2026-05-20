//! twyla — a typst-based static site generator.
//!
//! Today the library exposes two modules used by the `twyla` CLI:
//!
//! - [`render`] — compile a typst entrypoint to a fully-resolved HTML
//!   string. The eventual build pipeline will sit on top of this.
//! - [`diff`] — structural HTML AST diff with opt-in relaxations. Used
//!   by the porting workflow to verify zola ↔ typst equivalence.

pub mod diff;
pub mod render;
