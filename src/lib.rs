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
//! - [`session`] — the dev server's persistent rebuild step + watch set.
//! - [`watch`] — the dev server's file watcher (path-aware, race-checked).
//! - [`ledger`] — what each build read from disk, recorded at access time.

pub mod asset;
pub mod build;
pub mod compile;
pub mod content;
pub mod convert;
pub mod diff;
pub mod document;
pub mod examples;
pub mod html;
pub mod import;
pub mod ledger;
pub mod project;
pub mod reflect;
pub mod render;
pub mod resolver;
pub mod rules;
pub mod serve;
pub mod session;
pub mod slug;
pub mod watch;

#[cfg(test)]
mod invariants;
