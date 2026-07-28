//! The `document` element and `documents()` builtin.

use std::fmt::Debug;
use std::hash::{Hash, Hasher};

use comemo::{Track, Tracked};
use ecow::{EcoString, eco_vec};
use iddqd::IdHashItem;
use sha2::{Digest, Sha256};
use typst::diag::{At as _, SourceDiagnostic, SourceResult};
use typst::engine::Engine;
use typst::foundations::{
    Array, AutoValue, Binding, Content, Context, Datetime, Dict, Func, IntoValue as _,
    NativeElement as _, Packed, Scope, ShowFn, Smart, Str, StyleChain, Value, cast, elem, func,
    scope,
};
use typst::introspection::{History, Introspect, Introspector, Location};
use typst::syntax::{FileId, Span};

use crate::project::{TwylaContext, present_output};

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
///         output: auto,
///     )[Hello from a page within a page!].url(),
/// )[This is a link to a page within a page]
/// ```
#[elem(scope, name = "document")]
pub struct TwylaDocument {
    /// Where to write the document, as a bundle output path — e.g.
    /// `"foo/index.html"` for a page called "foo", or `"foo.html"`. Can be used
    /// to create a page literally called "main".
    ///
    /// A *relative* path (no leading `/`) resolves against a base: for a
    /// full-file page, the source's folder below `content/`
    /// (`content/blog/post.typ` → `blog/`, so `output: "extra.html"` →
    /// `blog/extra.html`); for an inline `document(..)`, the enclosing page's
    /// output directory. A leading `/` is bundle-root-absolute
    /// (`output: "/feed.xml"`), ignoring that base. When `auto`, a full-file
    /// page derives its output from the source filename and an inline document
    /// gets a fingerprinted `assets/document-<hash>.html` path.
    ///
    /// A function receives the document's full input hash, `"html"`, and its
    /// source stem (empty for an inline document), and returns a path:
    /// `(hash, ext, stem) => "previews/" + hash + "." + ext`.
    #[default(DocumentOutput::Auto)]
    pub output: DocumentOutput,

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
        engine: &mut Engine,
        context: Tracked<Context>,
        /// The document instance — present only on a method call (`doc.url()`),
        /// absent on the static `document.url()`.
        #[default]
        this: Option<Content>,
    ) -> SourceResult<Str> {
        let styles = context.styles().at(Span::detached())?;

        let meta = match this {
            // Instance: register a DocumentIntrospect in case the this TwylaDocument
            // element is never shown (eg called like `document(output: "foo.html")[].url()`)
            // and then use it to look up the resolved url.
            Some(content) => {
                let elem = content.to_packed::<TwylaDocument>().unwrap();
                let doc = request(engine, styles, elem)?;
                engine.introspect(ResolvedDocumentIntrospect(doc))
            }
            // Static: the current page's url.
            None => engine.introspect(DocumentAtIntrospect(context.location().unwrap())),
        };
        match meta.get("url") {
            Ok(Value::Str(url)) => Ok(url.clone()),
            _ => Ok(Str::from(DOC_PENDING)),
        }
    }
}

pub fn request(
    engine: &mut Engine,
    styles: StyleChain,
    elem: &Packed<TwylaDocument>,
) -> SourceResult<ResolvedDocument> {
    let mut doc = ResolvedDocument {
        output: String::new(),
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

    let policy = elem.output.get_cloned(styles);
    let raw = match policy {
        DocumentOutput::Auto => {
            EcoString::from(format!("/assets/document-{}.html", doc.fingerprint()))
        }
        DocumentOutput::Fixed(out) => out,
        DocumentOutput::Derive(func) => derive_output(engine, elem.span(), &func, &doc, "")?,
    };
    // A *relative* `output:` resolves against the enclosing page's output
    // *directory* — the parent output (carried on the chain by the loop) minus
    // its filename. An absolute (`/`-prefixed) output ignores the anchor. The
    // anchor is absent only during the discarded metadata-harvest pass, where a
    // relative output yields `Ok(None)`; we keep `raw` there and the main loop
    // re-resolves it once the parent output is on the chain.
    let parent = match styles.get_cloned(TwylaDocument::output) {
        DocumentOutput::Fixed(path) => Some(path),
        DocumentOutput::Auto | DocumentOutput::Derive(_) => None,
    };
    let anchor = parent.as_deref().map(parent_dir);
    let output = match TwylaContext::resolve_output(anchor, raw.as_str()) {
        Ok(Some(out)) => out,
        Ok(None) => raw.to_string(),
        Err(msg) => {
            return Err(eco_vec![SourceDiagnostic::error(
                elem.span(),
                EcoString::from(msg),
            )]);
        }
    };
    doc.output = output;
    Ok(doc)
}

/// Output policy for a document.
#[derive(Debug, Clone, PartialEq, Hash)]
pub enum DocumentOutput {
    /// Derive the conventional source path for a full-file page, or a
    /// fingerprinted path for an inline document.
    Auto,
    /// Write to this document-relative path.
    Fixed(EcoString),
    /// Derive a path from `(full-hash, "html", source-stem)`.
    Derive(Func),
}

cast! {
    DocumentOutput,
    self => match self {
        Self::Auto => AutoValue.into_value(),
        Self::Fixed(v) => v.into_value(),
        Self::Derive(v) => v.into_value(),
    },
    v: AutoValue => {
        let _ = v;
        Self::Auto
    },
    v: EcoString => Self::Fixed(v),
    v: Func => Self::Derive(v),
}

/// The directory portion of an output path — everything before the last `/`, or
/// `""` for a root-level output. Anchors a relative child `output:` on the
/// enclosing page's directory.
fn parent_dir(output: &str) -> &str {
    match output.rfind('/') {
        Some(i) => &output[..i],
        None => "",
    }
}

/// Placeholder returned by the static `document.url()` when no current-page
/// output is on the chain (the metadata-harvest render, which is discarded). A
/// real compile injects the output, so this never reaches emitted HTML — a leak
/// would signal that injection regressed.
const DOC_PENDING: &str = "/__twyla-doc-pending__";

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

pub const RENDER_INTROSPECTION: ShowFn<TwylaDocument> = |elem, engine, styles| {
    let doc = request(engine, styles, elem)?;
    engine.introspect(ResolvedDocumentIntrospect(doc));
    Ok(Content::empty())
};

/// One resolved page of the site: its `document` metadata plus its body and
/// originating source. The single page type, produced two ways — harvested from
/// a `content/*.typ` file ([`crate::compile`]) or discovered from an inline
/// `document(..)` (the discovery scan collects it into the
/// [`crate::resolver::Resolver`], deduped by `output`). A document is "resolved"
/// the moment it is discovered (unlike an asset, which must first be built), so
/// there is no separate request type — this *is* the resolved record. It still
/// has to be compiled to HTML, which produces a `render::OutputDoc` wrapping it.
///
/// Keyed by `output` (its [`IdHashItem`] key), the same way a [`ResolvedAsset`]
/// is keyed by its spec.
#[derive(Clone, PartialEq, Hash, Debug)]
pub struct ResolvedDocument {
    pub output: String,
    pub title: Option<Content>,
    pub date: Option<Datetime>,
    pub description: Option<Content>,
    pub kind: EcoString,
    pub extra: Value,
    pub draft: bool,
    pub body: Content,
    /// The file this page came from — for the duplicate-output diagnostic.
    /// `None` for a document built from a detached span. Documents are
    /// re-discovered every compile (cheap, unlike assets), and their source
    /// `.typ` files are already typst dependencies, so no mtime tracking is
    /// needed here.
    pub source: Option<FileId>,
}

impl IdHashItem for ResolvedDocument {
    type Key<'a> = &'a str;

    fn key(&self) -> &str {
        &self.output
    }

    iddqd::id_upcast!();
}

impl ResolvedDocument {
    /// A stable SHA-256 over the inputs that define this document, excluding
    /// its output path and source location. The first half names `auto`
    /// documents; the full value is passed to output callbacks.
    pub fn sha256_hex(&self) -> EcoString {
        let mut hasher = Sha256Hasher::default();
        (
            "twyla-document-v1",
            &self.title,
            &self.date,
            &self.description,
            &self.kind,
            &self.extra,
            &self.draft,
            &self.body,
        )
            .hash(&mut hasher);
        EcoString::from(hex::encode(hasher.finalize()))
    }

    fn fingerprint(&self) -> EcoString {
        let hash = self.sha256_hex();
        EcoString::from(&hash[..32])
    }

    /// The `documents()` dictionary form of this row.
    pub fn to_dict(&self, ctx: &TwylaContext) -> Dict {
        let mut d = Dict::new();
        d.insert("url".into(), ctx.document_url(&self.output).into_value());
        // Shown in root-absolute form (user-presentation boundary); the stored
        // `self.output` and every internal comparison stay no-slash.
        d.insert("output".into(), present_output(&self.output).into_value());
        d.insert("title".into(), self.title.clone().into_value());
        d.insert("date".into(), self.date.into_value());
        d.insert("description".into(), self.description.clone().into_value());
        d.insert("draft".into(), self.draft.into_value());
        d.insert("kind".into(), self.kind.clone().into_value());
        d.insert("extra".into(), self.extra.clone());
        d
    }
}

/// Evaluate a document output callback with the same arguments assets expose.
pub(crate) fn derive_output(
    engine: &mut Engine,
    span: Span,
    func: &Func,
    doc: &ResolvedDocument,
    stem: &str,
) -> SourceResult<EcoString> {
    let value = func.call(
        engine,
        Context::none().track(),
        [
            Str::from(doc.sha256_hex().as_str()),
            Str::from("html"),
            Str::from(stem),
        ],
    )?;
    value.cast().map_err(|err| {
        let mut diag = SourceDiagnostic::error(span, err.message().clone());
        for hint in err.hints() {
            diag = diag.with_hint(hint.clone());
        }
        eco_vec![diag]
    })
}

#[derive(Default)]
struct Sha256Hasher(Sha256);

impl Sha256Hasher {
    fn finalize(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

impl Hasher for Sha256Hasher {
    fn finish(&self) -> u64 {
        let digest = self.0.clone().finalize();
        u64::from_le_bytes(digest[..8].try_into().unwrap())
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

pub const DOCUMENTS_LIST_KEY: u128 = 0xdb18a2675fe9365ff4f5e2734237ff5c;

#[derive(Clone, PartialEq, Hash, Debug)]
pub struct ResolvedDocumentIntrospect(pub ResolvedDocument);

impl Introspect for ResolvedDocumentIntrospect {
    type Output = Dict;

    fn introspect(
        &self,
        _engine: &mut Engine,
        introspector: Tracked<dyn Introspector + '_>,
    ) -> Self::Output {
        let Some(Value::Array(array)) = introspector.value(DOCUMENTS_LIST_KEY) else {
            return Dict::new();
        };
        for doc in array {
            let Value::Dict(doc) = doc else { continue };
            let Ok(Value::Str(output)) = doc.get("output") else {
                continue;
            };
            // The dict's `output` is shown root-absolute; compare in the
            // no-slash internal form against the stored `ResolvedDocument.output`.
            if output.trim_start_matches('/') == self.0.output {
                return doc.clone();
            }
        }
        Dict::new()
    }

    fn diagnose(&self, _history: &History<Self::Output>) -> SourceDiagnostic {
        SourceDiagnostic::warning(self.0.body.span(), "this page's metadata did not stabilize")
            .with_hint("its `document(..)` fields resolve differently each pass")
    }
}

#[derive(Clone, PartialEq, Hash, Debug)]
pub struct DocumentAtIntrospect(pub Location);

impl Introspect for DocumentAtIntrospect {
    type Output = Dict;

    fn introspect(
        &self,
        _engine: &mut Engine,
        introspector: Tracked<dyn Introspector + '_>,
    ) -> Self::Output {
        let Some(path) = introspector.path(self.0) else {
            return Dict::new();
        };
        // Compare against the documents listing in the no-leading-slash key
        // form. The bundle path is already no-slash; the dict's `output` is
        // shown root-absolute, so strip its leading slash before comparing.
        let at_output = path.get_without_slash();
        let Some(Value::Array(array)) = introspector.value(DOCUMENTS_LIST_KEY) else {
            return Dict::new();
        };
        for doc in array {
            let Value::Dict(doc) = doc else { continue };
            let Ok(Value::Str(doc_output)) = doc.get("output") else {
                continue;
            };
            if doc_output.trim_start_matches('/') == at_output {
                return doc.clone();
            }
        }
        Dict::new()
    }

    fn diagnose(&self, _history: &History<Self::Output>) -> SourceDiagnostic {
        SourceDiagnostic::warning(Span::detached(), "this page's metadata did not stabilize")
            .with_hint("`document.*` for the current page resolves differently each pass")
    }
}

#[derive(Clone, PartialEq, Hash, Debug)]
pub struct DocumentsArrayIntrospect;

impl Introspect for DocumentsArrayIntrospect {
    type Output = Array;

    fn introspect(
        &self,
        _engine: &mut Engine,
        introspector: Tracked<dyn Introspector + '_>,
    ) -> Self::Output {
        let Some(Value::Array(array)) = introspector.value(DOCUMENTS_LIST_KEY) else {
            return Array::new();
        };
        array.clone()
    }

    fn diagnose(&self, _history: &History<Self::Output>) -> SourceDiagnostic {
        SourceDiagnostic::warning(Span::detached(), "the set of pages did not stabilize").with_hint(
            "a `#context`-generated `document(..)` is likely emitting a new page every pass \
             (e.g. deriving its `output` from `documents()` itself)",
        )
    }
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
#[func]
pub fn documents(engine: &mut Engine) -> Array {
    engine.introspect(DocumentsArrayIntrospect)
}

pub fn install(global: &mut Scope) {
    global.bind(
        "document".into(),
        Binding::detached(crate::document::TwylaDocument::ELEM),
    );
    global.define_func::<crate::document::documents>();
}
