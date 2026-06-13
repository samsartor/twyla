//! The `document` element and `documents()` builtin.

use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};

use comemo::Tracked;
use crossbeam_channel::Sender;
use ecow::{EcoString, eco_vec};
use typst::diag::{At as _, HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{
    Array, Binding, Content, Context, Datetime, Dict, NativeElement as _, Packed, Repr, Scope,
    ShowFn, Smart, Str, StyleChain, Value, elem, func, scope, ty,
};
use typst::syntax::{FileId, Span};

use crate::project::build_document_url;

/// Defines a page of the website.
///
/// Conventionally, document metadata is provided near the top of each page.
/// with a `set` rule. Every field is then available on this page via `#context
/// document.<field>`, and the whole site's metadata is available through
/// #link(<documents>)[`documents()`].
///
/// ```example
/// #set document(
///   title: "Rewriting My Blog",
///   date: datetime(year: 2026, month: 4, day: 12),
///   description: "Why I moved off Markdown.",
///   kind: "post",
/// )
/// ```
///
/// It is also possible to create new pages inline, by constructing documents:
/// ```example
/// #context link(
///     document(
///         title: "Page within a page",
///         output: "subpage/index.html",
///     )[Hello from a page within a page!].url(),
/// )[This is a link to a page within a page]
/// ```
#[elem(scope, name = "document")]
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
    /// `convert` draft shows. If `auto`, defaults from the source filename:
    ///
    /// #table(
    ///   columns: 2,
    ///   table.header[Kind][Default for],
    ///   [`"root"`], [`content/main.typ` — the site index],
    ///   [`"dir"`], [`content/<dir>/main.typ` — a section index],
    ///   [`"page"`], [any other file],
    /// )
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

    /// The body of an _inline_ document. When `document` is called as a
    /// constructor — `#document(output: "x")[stuff]` — this carries `stuff`,
    /// which twyla hoists into its own bundle output (see [`crate::compile`]).
    /// `#set document(..)` never touches this (required positional fields aren't
    /// settable), so the set-rule-only metadata-carrier use is unaffected.
    #[required]
    pub body: Content,
}

#[scope]
impl TwylaDocument {
    /// The page's public URL. Two call forms:
    ///
    /// - `document.url()` (no self) → the *current* page's URL, built from the
    ///   output twyla derived for it.
    /// - `document(output: "x")[..].url()` (method, on an instance) → that
    ///   document's URL, and *discovers it for emission* — so an inline
    ///   document consumed only through `.url()` (never shown) is still routed.
    ///   Discovery dedups by `output`, so calling `.url()` repeatedly, or
    ///   `.url()` on a document that is also shown, emits it exactly once.
    ///
    /// Contextual — call it inside `#context`. `context` precedes the optional
    /// `this` self-positional: the `#[func]` macro classifies special params by
    /// name and forwards them ahead of ordinary positionals (the instance is
    /// prepended as that positional on a method call).
    #[func(contextual)]
    fn url(
        context: Tracked<Context>,
        /// The document instance — present only on a method call (`doc.url()`),
        /// absent on the static `document.url()`.
        #[default]
        this: Option<Content>,
    ) -> SourceResult<Str> {
        let styles = context.styles().at(Span::detached())?;
        let base = base_url_on_chain(styles);

        let output = match this {
            // Instance: read the element's *own* output (not chain-resolved, so
            // it can't accidentally inherit the surrounding page's), then report
            // it on the discovery sink exactly like a shown inline document.
            Some(content) => {
                let elem = content.into_packed::<TwylaDocument>().unwrap();
                let Some(Smart::Custom(output)) = elem.output.as_option() else {
                    return Err(eco_vec![SourceDiagnostic::error(
                        elem.span(),
                        EcoString::from("`document(..).url()` needs an explicit `output:`"),
                    )]);
                };
                let output = output.clone();
                send_document(styles, &elem)?;
                output
            }
            // Static: the current page's output, injected onto the body chain by
            // `crate::compile`. Absent only where the render is discarded (the
            // metadata harvest) — return the placeholder rather than erroring; it
            // never reaches real output.
            None => match styles.get_cloned(TwylaDocument::output) {
                Smart::Custom(output) => output,
                Smart::Auto => return Ok(Str::from(DOC_PENDING)),
            },
        };

        Ok(Str::from(build_document_url(base.as_deref(), &output)))
    }
}

/// Placeholder returned by the static `document.url()` when no current-page
/// output is on the chain (the metadata-harvest render, which is discarded). A
/// real compile injects the output, so this never reaches emitted HTML — a leak
/// would signal that injection regressed.
const DOC_PENDING: &str = "/__twyla-doc-pending__";

/// Read the site `base_url` off the style chain ([`TwylaSite`]), or `None` for a
/// relative-URL build. Lets `document.url()` build URLs without a `TwylaContext`.
fn base_url_on_chain(styles: StyleChain) -> Option<EcoString> {
    match styles.get_cloned(TwylaSite::base_url) {
        Value::Str(s) => Some(EcoString::from(s.as_str())),
        _ => None,
    }
}

/// Internal host carrying the site `base_url` on the style chain, injected by
/// [`crate::compile`] each iteration. Not user-constructible, not bound in the
/// global scope (mirrors [`TwylaDocumentList`]).
#[elem]
pub struct TwylaSite {
    /// The `base_url` as a [`Value::Str`], or [`Value::None`] for a relative
    /// build.
    #[default(Value::None)]
    pub base_url: Value,
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
