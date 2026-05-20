//! twyla — a typst-based static site generator.
//!
//! Today the library exposes two modules used by the `twyla` CLI:
//!
//! - [`render`] — compile a typst entrypoint to a fully-resolved HTML
//!   string. The eventual build pipeline will sit on top of this.
//! - [`diff`] — structural HTML AST diff with opt-in relaxations. Used
//!   by the porting workflow to verify zola ↔ typst equivalence.
//! - [`import`] — generate a typst draft from a zola markdown post.
//!   Output is scaffolding, not a maintained md↔typ sync.

pub mod diff;
pub mod import;
pub mod render;
