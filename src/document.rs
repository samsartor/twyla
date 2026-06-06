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

use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};

use comemo::Tracked;
use crossbeam_channel::Sender;
use ecow::{EcoString, eco_vec};
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{
    Array, Binding, Content, Context, Datetime, Dict, NativeElement as _, Packed, Repr, Scope,
    ShowFn, Smart, StyleChain, Value, elem, func, ty,
};
use typst::syntax::FileId;

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
    /// pages in listings and feeds, and to pick the `{kind}-template` a
    /// `convert` draft shows. If `auto`, defaults from the source filename
    /// ([`TwylaContext::default_kind`](crate::project::TwylaContext::default_kind)):
    /// - "root" for `content/main.typ` (the site index)
    /// - "dir" for `content/<dir>/main.typ` (a section index)
    /// - "page" for any other file
    pub kind: Smart<EcoString>,

    /// Arbitrary extra data for your own use. Available as `document.extra`
    /// and as the `extra` field of this page's [`documents`] entry. Defaults to
    /// an empty dictionary, so templates can `document.extra.at(.., default: ..)`
    /// without first checking for `none`.
    #[default(Value::Dict(Dict::new()))]
    pub extra: Value,

    /// Whether this page is a draft. Drafts are still built, but are
    /// conventionally excluded from listings and feeds.
    #[default(false)]
    pub draft: bool,

    /// The body of an *inline* document. When `document` is called as a
    /// constructor — `#document(output: "x")[stuff]` — this carries `stuff`,
    /// which twyla hoists into its own bundle output (see [`crate::compile`]).
    /// `#set document(..)` never touches this (required positional fields aren't
    /// settable), so the set-rule-only metadata-carrier use is unaffected.
    #[required]
    pub body: Content,
}

/// An inline `#document(..)[body]` renders to **nothing** where it sits, so
/// `foo #document(output: "x")[stuff] bar` lays out as `foo bar` — but on the
/// way out it reports itself on the discovery sink ([`send_document`]), so
/// twyla can re-home its body as its own bundle output. This is the exact same
/// side-channel trick the asset system uses ([`crate::asset`]): the element
/// doesn't need to *survive* realization, so discovery happens in the normal
/// Html render pass, in the same relayout iteration as asset resolution.
///
/// Registered on the in-page targets (Html/Paged); the rule fires wherever the
/// element is realized, so documents are discovered at any nesting depth.
pub const RENDER_NOTHING: ShowFn<TwylaDocument> = |elem, _engine, styles| {
    send_document(styles, elem)?;
    Ok(Content::empty())
};

/// A document discovered through the sink during realization — the shape of a
/// [`documents`] row plus its body and originating source. Flows Rust-side
/// through [`DocumentSink`] to [`crate::asset::AssetResolver`], which collects
/// it, dedups by `output`, and invalidates by `source` across warm rebuilds.
#[derive(Clone)]
pub struct DiscoveredDoc {
    pub output: String,
    pub title: Option<Content>,
    pub date: Option<Datetime>,
    pub description: Option<Content>,
    pub kind: EcoString,
    pub extra: Value,
    pub draft: bool,
    /// The inline document's body, as written at the call site.
    pub body: Content,
    /// The file the `#document(..)` call was written in — twyla evicts this
    /// document when that file changes.
    pub source: Option<FileId>,
}

/// Write-only channel reporting discovered documents out of an otherwise-pure
/// realization pass. `Hash`/`Eq` key on `epoch` (the resolver's discovery
/// generation), never the channel — identical to `crate::asset`'s `AssetSink`,
/// and for the same reason: a constant hash would let comemo replay a cached
/// realization without re-sending on a warm rebuild. Shares the asset
/// resolver's epoch so the two sinks form one discovery generation.
#[ty(name = "twyla-document-sink")]
#[derive(Clone)]
pub struct DocumentSink {
    epoch: u64,
    tx: Sender<DiscoveredDoc>,
}

impl DocumentSink {
    /// Build a sink for the given discovery `epoch` and channel. Called by the
    /// resolver when chaining the sink onto the style chain each iteration.
    pub fn new(epoch: u64, tx: Sender<DiscoveredDoc>) -> Self {
        Self { epoch, tx }
    }
}

impl Debug for DocumentSink {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "DocumentSink(epoch={})", self.epoch)
    }
}

impl Repr for DocumentSink {
    fn repr(&self) -> EcoString {
        EcoString::inline("twyla-document-sink")
    }
}

impl PartialEq for DocumentSink {
    fn eq(&self, other: &Self) -> bool {
        self.epoch == other.epoch
    }
}

impl Hash for DocumentSink {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.epoch.hash(state);
    }
}

/// Internal host carrying the discovery sink on the style chain (mirrors
/// `TwylaAssetSink`). Not user-constructible, not bound in the global scope.
#[elem]
pub struct TwylaDocumentSink {
    /// A [`Value::Dyn`] wrapping [`DocumentSink`].
    #[default(Value::None)]
    pub sink: Value,
}

/// Read the discovery sink off the style chain and report this inline document,
/// then [`RENDER_NOTHING`] erases it in place. Errors if no explicit `output:`
/// was given — an inline document has no source filename to default from. A
/// no-op if no sink is on the chain (e.g. realized outside twyla's pipeline).
fn send_document(styles: StyleChain, elem: &Packed<TwylaDocument>) -> SourceResult<()> {
    let Smart::Custom(output) = elem.output.get_cloned(styles) else {
        return Err(eco_vec![SourceDiagnostic::error(
            elem.span(),
            EcoString::from("an inline `document(..)` needs an explicit `output:`"),
        )]);
    };
    let doc = DiscoveredDoc {
        output: output.into(),
        title: elem.title.get_cloned(styles),
        date: elem.date.get_cloned(styles),
        description: elem.description.get_cloned(styles),
        kind: elem
            .kind
            .get_cloned(styles)
            .custom()
            .unwrap_or_else(|| EcoString::from("page")),
        extra: elem.extra.get_cloned(styles),
        draft: elem.draft.get(styles),
        body: elem.body.clone(),
        source: elem.span().id(),
    };

    if let Value::Dyn(dynamic) = styles.get_cloned(TwylaDocumentSink::sink)
        && let Some(sink) = dynamic.downcast::<DocumentSink>()
    {
        let _ = sink.tx.send(doc);
    }
    Ok(())
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
