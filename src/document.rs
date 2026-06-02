//! The `document` element and `documents()` builtin.
//!
//! Maintainer notes (the `///` docs below are the user-facing API docs;
//! this module doc is the implementation story):
//!
//! `document` is bound — in [`crate::prelude`] — to [`TwylaDocument`], which
//! shadows typst's native `document`
//! Because the fields are settable, `#set document(..)` puts them on the style
//! chain, which is what makes them both readable on the current page
//! (`#context document.title`, via typst's `field_from_styles` fallback for
//! element field access) and harvestable per-document by `crate::compile`.
//!
//! `documents()` reads the whole-site list back off the style chain. Twyla
//! harvests it Rust-side and injects it through [`TwylaDocumentList`]'s hidden
//! style field (the same trick typst uses for `TargetElem`). A bare
//! `documents` binding can't work — a bare identifier resolves at eval time,
//! but the data only exists at realization time — and `for` only iterates
//! Array/Dict/Str/Bytes, so it has to be a function returning an array.

use comemo::Tracked;
use ecow::EcoString;
use typst::diag::HintedStrResult;
use typst::foundations::{
    Array, Binding, Content, Context, Datetime, NativeElement as _, Scope, Smart, Value, elem, func,
};

/// Metadata for the current page.
///
/// Set it once, near the top of a page, with a `set` rule. Every field is
/// then available on this page via `#context document.<field>`, and the whole
/// site's metadata is available through [`documents`].
///
/// ```typ
/// #set document(
///   title: "Rewriting My Blog",
///   date: datetime(year: 2026, month: 4, day: 12),
///   description: "Why I moved off Markdown.",
///   kind: "post",
/// )
/// ```
#[elem(name = "document")]
pub struct TwylaDocument {
    /// The output location of the document. For example, `"foo/index.html"` or
    /// `"foo.html"` to create a page called "foo". Can be used to create a page
    /// literally called "main".
    pub output: Smart<EcoString>,

    /// The page's title.
    pub title: Option<Content>,

    /// The page's publication date.
    pub date: Option<Datetime>,

    /// A short description or summary of the page.
    pub description: Option<Content>,

    /// What kind of page this is, e.g. `"post"` or `"page"` — used to group
    /// pages in listings and feeds. If `auto`, will be set to:
    /// - "home" for `main.typ`
    /// - "dir" for `*/main.typ`
    /// - "page" for for all others
    pub kind: Smart<EcoString>,

    /// Arbitrary extra data for your own use. Available as `document.extra`
    /// and as the `extra` field of this page's [`documents`] entry.
    #[default(Value::None)]
    pub extra: Value,

    /// Whether this page is a draft. Drafts are still built, but are
    /// conventionally excluded from listings and feeds.
    #[default(false)]
    pub draft: bool,
}

/// The list of every page in the site.
///
/// Returns an array with one dictionary per page, each carrying that page's
/// `url` plus the metadata it set via `#set document(..)`:
/// `url`, `title`, `date`, `description`, `kind`, `draft`, and `extra`.
/// Access fields with ordinary dot syntax, e.g. `doc.title`.
///
/// This is contextual — call it inside a `#context` block:
///
/// ```typ
/// #context for doc in documents() {
///   if doc.kind == "post" and not doc.draft [
///     == #link(doc.url, doc.title)
///     #doc.date.display()
///
///     #doc.description
///   ]
/// }
/// ```
#[func(contextual)]
pub fn documents(
    /// The context to read the page list from.
    context: Tracked<Context>,
) -> HintedStrResult<Array> {
    Ok(context.styles()?.get_cloned(TwylaDocumentList::all))
}

/// Internal host for the [`documents`] list.
///
/// Not constructed by users and not bound in the global scope — it exists only
/// to carry the harvested page list on the style chain, where [`documents`]
/// reads it back (mirrors how typst's `TargetElem` hosts the `target` field).
#[elem]
pub struct TwylaDocumentList {
    /// Every page's metadata, injected by [`crate::compile`] each compile.
    #[default(Array::new())]
    pub all: Array,
}

pub fn install(global: &mut Scope) {
    global.bind(
        "document".into(),
        Binding::detached(crate::document::TwylaDocument::ELEM),
    );
    global.define_func::<crate::document::documents>();
}
