//! `asset.typst` — compile a typst document and emit it as a fingerprinted
//! asset in a chosen format (`svg` / `png` / `pdf` / `html`).
//!
//! The source is either a project `.typ` file (a path) or an inline `content`
//! value:
//!
//! ```typ
//! // A standalone document compiled to PDF (e.g. a résumé):
//! #context html.elem("a", attrs: (href: asset.typst("/resume/cv.typ", format: "pdf").url()))[CV]
//!
//! // Inline content rendered to a vector icon, inlined via raw-html:
//! #context raw-html(asset.typst(circle(fill: blue), format: "svg").read())
//! ```
//!
//! **Assets compile as plain, stock typst** — a fresh [`Library`] *without*
//! twyla's builtins (`document` / `asset` / `content` / the native HTML rules).
//! A typst asset can't cross-link into the site or read `#document`; it's a
//! self-contained artifact whose output is a pure function of its source +
//! format. That purity is what lets it ride the same [`AssetSpec`] cache key as
//! every other asset (proven by the two jj-workspace spikes: a stock-library
//! compile is byte-deterministic, and inheriting the site library instead
//! silently leaks unresolved-asset placeholders). The compile still runs *in
//! the same process*, delegating file/font/package loading to the live world —
//! so `#import`s, `@preview` packages, real diagnostics, and hot-reload all
//! work.
//!
//! Mechanism (see `crate::examples::SnippetWorld`, the precedent): a path source
//! compiles the real project file as `main`; an inline content value is bound
//! into the stock library's global scope under a private name and compiled
//! through a one-line synthetic `main` (`#__twyla_inline`). `svg`/`png`/`pdf`
//! compile to a [`PagedDocument`] and serialize via `typst_svg` / `typst_render`
//! / `typst_pdf`; `html` compiles to an [`HtmlDocument`] like the rest of the
//! site. Every file the sub-compile reads is recorded into the asset's
//! `upstream` set (the typst-native analogue of sass's `RecordingFs`), so
//! editing the document or any of its imports re-triggers a rebuild.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use comemo::Tracked;
use ecow::{EcoString, eco_format, eco_vec};
use typst::diag::{FileResult, HintedStrResult, SourceDiagnostic, SourceResult, Warned};
use typst::foundations::{
    Bytes, Cast, Content, Context, Datetime, Duration, IntoValue, Packed, PathOrStr, ShowFn, Str,
    StyleChain, cast, elem, func, scope,
};
use typst::layout::Abs;
use typst::loading::{Encoding, Readable};
use typst::syntax::{FileId, RootedPath, Source, Span, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::visualize::Color;
use typst::{Library, LibraryExt, World};
use typst_html::HtmlDocument;
use typst_layout::PagedDocument;
use typst_library::Feature;
use typst_pdf::PdfOptions;
use typst_utils::hash128;

use super::{
    AssetSpec, Built, Upstream, read_or_request, resolve_or_request, show_unresolved,
};
use crate::project::TwylaContext;
use crate::render::Emit;

/// Compile a typst document (content or a project `.typ` file) to a
/// fingerprinted asset.
///
/// `format` (default `svg`) and `ppi` are settable, so
/// `#set asset.typst(format: "pdf")` configures a whole scope.
///
/// ```typ
/// #context html.elem("a", attrs: (
///   href: asset.typst("/resume/cv.typ", format: "pdf").url(),
/// ))[Download my CV]
/// ```
#[elem(scope, name = "typst")]
pub struct TypstAsset {
    /// The document to compile: an inline `content` value, or a path string to
    /// a project `.typ` file (resolved relative to the calling file).
    #[required]
    pub source: TypstSource,

    /// Output format: `"svg"` (the default), `"png"`, `"pdf"`, or `"html"`.
    /// `svg`/`png`/`pdf` lay the document out as pages; `html` compiles it the
    /// same way the site's pages are compiled.
    #[default(Format::Svg)]
    pub format: Format,

    /// Pixels per inch for the `png` format. Ignored by the other formats (and
    /// excluded from the cache key for them, so it never fragments their
    /// output).
    #[default(144)]
    pub ppi: i64,
}

#[scope]
impl TypstAsset {
    /// The resolved, fingerprinted URL of the compiled document. Contextual —
    /// call it inside `#context`.
    #[func(contextual)]
    fn url(context: Tracked<Context>, this: Content) -> HintedStrResult<Str> {
        let elem = this.into_packed::<TypstAsset>().unwrap();
        let styles = context.styles()?;
        Ok(resolve_or_request(styles, &spec(&elem, styles)?, elem.span()))
    }

    /// The compiled document's bytes — for inlining instead of linking (e.g.
    /// `raw-html(asset.typst(.., format: "svg").read())`). Defaults to a UTF-8
    /// `str` (the right choice for the text formats `svg`/`html`); pass
    /// `encoding: none` for the binary formats `png`/`pdf`.
    #[func(contextual)]
    fn read(
        context: Tracked<Context>,
        this: Content,
        /// The encoding to read the asset with. If `{none}`, returns raw bytes;
        /// otherwise the bytes are decoded as UTF-8 into a string.
        #[named]
        #[default(Some(Encoding::Utf8))]
        encoding: Option<Encoding>,
    ) -> HintedStrResult<Readable> {
        let elem = this.into_packed::<TypstAsset>().unwrap();
        let styles = context.styles()?;
        read_or_request(styles, &spec(&elem, styles)?, elem.span(), encoding)
    }
}

/// Build a `asset.typst` element's [`AssetSpec`] from its (style-resolved)
/// fields. A path source is resolved to its [`FileId`]; an inline content value
/// is captured verbatim (content-addressed). `ppi` only enters the key for the
/// `png` format.
fn spec(elem: &Packed<TypstAsset>, styles: StyleChain) -> HintedStrResult<AssetSpec> {
    let format = elem.format.get(styles);
    let input = match &elem.source {
        TypstSource::Path(path) => {
            TypstInput::File(path.resolve_if_some(elem.span().id())?.intern())
        }
        TypstSource::Content(content) => TypstInput::Content(content.clone()),
    };
    let ppi = match format {
        Format::Png => elem.ppi.get(styles),
        _ => 0,
    };
    Ok(AssetSpec::Typst { input, format, ppi })
}

/// Default show: a bare `asset.typst(..)` cannot be rendered — resolve it with
/// `.url()`/`.read()`. Registered for the in-page targets in [`crate::rules`].
pub const SHOW_RULE: ShowFn<TypstAsset> =
    |elem, _engine, _styles| show_unresolved(elem.span(), "typst");

/// The `source` field's value: a path to a project `.typ` file, or an inline
/// `content` value. A string casts to [`Path`](Self::Path) (a path, *not* text
/// content); any other content casts to [`Content`](Self::Content).
#[derive(Debug, Clone, PartialEq, Hash)]
pub enum TypstSource {
    /// A path to a `.typ` file, resolved relative to the calling file.
    Path(PathOrStr),
    /// An inline content value.
    Content(Content),
}

cast! {
    TypstSource,
    self => match self {
        Self::Path(v) => v.into_value(),
        Self::Content(v) => v.into_value(),
    },
    // `PathOrStr` captures strings/paths first, so a string argument is a path
    // (not auto-coerced to text content); everything else content-y is content.
    v: PathOrStr => Self::Path(v),
    v: Content => Self::Content(v),
}

/// The resolved document source carried in [`AssetSpec::Typst`]: a project file
/// id, or a captured content value (content-addressed, like
/// [`AssetSpec::Raw`](super::AssetSpec::Raw) bytes).
#[derive(Debug, Clone, PartialEq, Hash)]
pub enum TypstInput {
    /// A project `.typ` file, compiled as `main` (read + watched).
    File(FileId),
    /// An inline content value, compiled through a synthetic `main`.
    Content(Content),
}

/// Output format for a compiled typst asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Cast)]
pub enum Format {
    /// Scalable vector graphics (pages laid out, merged into one SVG).
    Svg,
    /// Raster PNG (rendered at `ppi` pixels per inch, pages stacked).
    Png,
    /// PDF document.
    Pdf,
    /// HTML document, compiled like the site's own pages.
    Html,
}

impl Format {
    /// The output file extension (also the static server's Content-Type key).
    fn ext(self) -> &'static str {
        match self {
            Format::Svg => "svg",
            Format::Png => "png",
            Format::Pdf => "pdf",
            Format::Html => "html",
        }
    }
}

/// Compile a typst document and serialize it to `format`.
///
/// Runs in-process against a **stock** [`Library`] (no twyla builtins), with
/// file/font/package loading delegated to the live `world`. A path source
/// compiles the real project file as `main`; an inline content value is bound
/// into the stock global scope and compiled through a one-line synthetic
/// `main`. Every file the sub-compile reads is recorded into `upstream`.
pub(crate) fn build(
    world: Tracked<dyn World + '_>,
    input: &TypstInput,
    format: Format,
    ppi: i64,
    ctx: &TwylaContext,
    span: Span,
) -> SourceResult<Built> {
    // A stock library: the standard typst stdlib (with the HTML feature, needed
    // for `format: "html"` and harmless otherwise) but NONE of twyla's
    // `install_stdlib` splices — no custom rules, no `document`/`asset`/`content`.
    let mut library = Library::builder()
        .with_features([Feature::Html].into_iter().collect())
        .build();

    let (main_id, main_src, stem) = match input {
        TypstInput::File(file) => {
            // Compile the real project file as `main`; the delegating loader
            // reads it (and its imports) through the live world.
            let stem = Path::new(file.vpath().get_without_slash())
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned);
            (*file, None, stem)
        }
        TypstInput::Content(content) => {
            // Bind the captured value into the stock global scope and reference
            // it from a trivial synthetic main (the library is per-compile, so
            // the mutation is local). No file stem → hash-only output name.
            library
                .global
                .scope_mut()
                .define("__twyla_inline", content.clone().into_value());
            let id = FileId::new(RootedPath::new(
                VirtualRoot::Project,
                VirtualPath::new("__twyla_inline__.typ").unwrap(),
            ));
            let src = Source::new(id, EcoString::inline("#__twyla_inline").into());
            (id, Some(src), None)
        }
    };

    let subworld = TypstWorld {
        world,
        library: LazyHash::new(library),
        main_id,
        main_src,
        reads: Mutex::new(HashSet::new()),
    };

    let bytes = match format {
        Format::Html => {
            let doc = compile_html(&subworld)?;
            typst_html::html(&doc)?.into_bytes()
        }
        Format::Svg | Format::Png | Format::Pdf => {
            let doc = compile_paged(&subworld)?;
            match format {
                Format::Svg => typst_svg::svg_merged(&doc, Abs::zero()).into_bytes(),
                Format::Pdf => typst_pdf::pdf(&doc, &PdfOptions::default())?,
                Format::Png => {
                    let ppp = ppi as f32 / 72.0;
                    let pixmap =
                        typst_render::render_merged(&doc, ppp, Abs::zero(), Some(Color::WHITE));
                    pixmap
                        .encode_png()
                        .map_err(|err| err_at(span, eco_format!("PNG encode failed: {err}")))?
                }
                Format::Html => unreachable!("html handled above"),
            }
        }
    };

    let upstream = collect_upstream(subworld.reads.into_inner().unwrap_or_default(), ctx);
    let bytes = Bytes::new(bytes);
    Ok(Built {
        content_hash: hash128(&bytes),
        emit: Emit::Bytes(bytes),
        upstream,
        ext: Some(format.ext().to_owned()),
        stem,
        dimensions: None,
    })
}

/// Compile the sub-document to a [`PagedDocument`] (svg/png/pdf), discarding
/// warnings (there is no engine sink to route them to from an asset build).
fn compile_paged(world: &TypstWorld) -> SourceResult<PagedDocument> {
    let Warned { output, warnings: _ } = typst::compile::<PagedDocument>(world);
    output
}

/// Compile the sub-document to an [`HtmlDocument`] (html format).
fn compile_html(world: &TypstWorld) -> SourceResult<HtmlDocument> {
    let Warned { output, warnings: _ } = typst::compile::<HtmlDocument>(world);
    output
}

/// Turn the sub-compile's recorded file reads into the asset's `upstream` set.
/// Only project files are watched (package files are immutable), mirroring
/// `AssetResolver::doc_source`.
fn collect_upstream(reads: HashSet<FileId>, ctx: &TwylaContext) -> Vec<Upstream> {
    reads
        .into_iter()
        .filter_map(|id| match id.root() {
            VirtualRoot::Project => {
                Some(Upstream::new_lazy(ctx.root.join(id.vpath().get_without_slash())))
            }
            VirtualRoot::Package(_) => None,
        })
        .collect()
}

/// A spanned error from any `Display` payload.
fn err_at(span: Span, msg: impl std::fmt::Display) -> ecow::EcoVec<SourceDiagnostic> {
    eco_vec![SourceDiagnostic::error(span, EcoString::from(msg.to_string()))]
}

/// A [`World`] for an `asset.typst` sub-compile: serves a stock library + a
/// `main` (a synthetic source for inline content, or the real project file for
/// a path source) and delegates everything else to the live world, recording
/// every file id it reads so they become the asset's `upstream` set.
///
/// Owns the stock library (it returns a reference, so it can't be borrowed
/// across the `Tracked` boundary — the same reason `SnippetWorld` clones). The
/// font book is read off the live world per call.
struct TypstWorld<'a> {
    world: Tracked<'a, dyn World + 'a>,
    library: LazyHash<Library>,
    main_id: FileId,
    /// `Some` for an inline-content compile (the synthetic main); `None` for a
    /// path source (the real file is served through `world`).
    main_src: Option<Source>,
    reads: Mutex<HashSet<FileId>>,
}

impl World for TypstWorld<'_> {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        self.world.book()
    }
    fn main(&self) -> FileId {
        self.main_id
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main_id
            && let Some(src) = &self.main_src
        {
            return Ok(src.clone());
        }
        self.reads.lock().unwrap().insert(id);
        self.world.source(id)
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.main_id
            && let Some(src) = &self.main_src
        {
            return Ok(Bytes::new(src.text().as_bytes().to_vec()));
        }
        self.reads.lock().unwrap().insert(id);
        self.world.file(id)
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.world.font(index)
    }
    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        self.world.today(offset)
    }
}
