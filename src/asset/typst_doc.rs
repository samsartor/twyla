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
//! **Assets compile as plain, stock typst** — a stock [`Library`] *without*
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
//! compiles the real project file as `main`; an inline content value is
//! already evaluated, so it skips eval and goes straight into typst's relayout
//! loop ([`compile_content`]). Every build shares one stock library
//! ([`stock_library`]), so comemo can reuse work across builds. `svg`/`png`/`pdf`
//! compile to a [`PagedDocument`] and serialize via `typst_svg` / `typst_render`
//! / `typst_pdf`; `html` compiles to an [`HtmlDocument`] like the rest of the
//! site. Project files the sub-compile loads (the document, its imports,
//! images it embeds) are read *by the sub-world itself*, through the
//! resolver's [`DepRecorder`] — not through the live world's `FileStore` — so
//! each lands in the asset's ledger deps with the stamp and hash of exactly
//! the bytes the compile used (the typst-native analogue of sass's
//! `RecordingFs`), and nothing stays resident once the build is done. Editing
//! the document or any of its imports re-triggers a rebuild. Package files are
//! read the same way, after a recorded package lookup
//! ([`locate_package`]), so installing a missing `@local` package (or editing
//! one) invalidates the asset too.
//!
//! The sub-compile's warnings are kept with the cached entry, and the sources
//! its diagnostics point into are captured from the build and kept alongside
//! (`diag_sources`), so rendering a snippet never loads a sub-document file
//! into the live world's persistent `FileStore`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use super::{AssetSpec, Built, OutputReq, emit_or_request, sha256};
use crate::ledger::DepRecorder;
use crate::project::TwylaContext;
use crate::render::{Emit, locate_package};
use comemo::Track;
use comemo::Tracked;
use ecow::{EcoString, eco_format, eco_vec};
use typst::diag::{
    FileError, FileResult, HintedStrResult, PackageResult, SourceDiagnostic, SourceResult, Warned,
};
use typst::foundations::{
    Bytes, Cast, Content, Datetime, Duration, Packed, PathOrStr, ShowFn, StyleChain, cast, elem,
    func, scope,
};
use typst::foundations::{Output, Target, TargetElem};
use typst::layout::Abs;
use typst::loading::Encoding;
use typst::syntax::package::PackageSpec;
use typst::syntax::{FileId, RootedPath, Source, Span, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_html::HtmlDocument;
use typst_kit::files::FsRoot;
use typst_kit::packages::SystemPackages;
use typst_layout::PagedDocument;
use typst_library::Feature;
use typst_library::engine::{Engine, Route, Sink, Traced};
use typst_library::introspection::{EmptyIntrospector, Introspector, MAX_ITERS, analyze};
use typst_pdf::PdfOptions;
use typst_utils::Protected;

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

    /// Minify `svg` output with svgm (best-effort — unparsable output warns
    /// and passes through). Ignored by the other formats (and excluded from
    /// their cache key). Off by default so raw compiles stay byte-identical to
    /// `typst compile`.
    #[default(false)]
    pub minify: bool,

    /// Bundle-relative output path policy.
    #[default(OutputReq::Auto)]
    pub output: OutputReq,
}

asset_methods!(TypstAsset, spec, output, Some(Encoding::Utf8));

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
    let minify = match format {
        Format::Svg => elem.minify.get(styles),
        _ => false,
    };
    Ok(AssetSpec::Typst {
        input,
        format,
        ppi,
        minify,
    })
}

fn output(elem: &Packed<TypstAsset>, styles: StyleChain) -> HintedStrResult<OutputReq> {
    Ok(elem.output.get_cloned(styles))
}

/// Default show: a bare `asset.typst(..)` emits and renders nothing.
pub const SHOW_RULE: ShowFn<TypstAsset> = |elem, engine, styles| {
    emit_or_request(
        engine,
        spec(elem, styles),
        elem.span(),
        output(elem, styles),
    )
};

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
#[derive(Debug, Clone, Hash)]
pub enum TypstInput {
    /// A project `.typ` file, compiled as `main` (read + watched).
    File(FileId),
    /// An inline content value, compiled through a synthetic `main`.
    Content(Content),
}

/// Content-addressed: two inline contents are the same input iff their
/// typst hashes are equal. Typst's own `Content` equality is much looser than
/// its `Hash` (it skips each element's location, label and span, among
/// others), so a derived `PartialEq` would let two specs that hash apart
/// compare equal. That breaks every map keyed by [`AssetSpec`]: a lookup
/// could land on an *equal* entry for a different spec (a bucket-tag
/// collision is enough), skip the build, and leave the spec unresolved in
/// the hash-keyed introspector — shipping a pending URL.
impl PartialEq for TypstInput {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File(a), Self::File(b)) => a == b,
            (Self::Content(a), Self::Content(b)) => {
                typst_utils::hash128(a) == typst_utils::hash128(b)
            }
            _ => false,
        }
    }
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
/// fonts and the date delegated to the live `world`. A path source compiles
/// the real project file as `main`; an inline content value is bound into the
/// stock global scope and compiled through a one-line synthetic `main`. Every
/// project and package file the sub-compile reads goes through `rec`.
///
/// The sub-compile's warnings go to `warnings`. The sources the build's
/// diagnostics (warnings, or the returned errors) point into are pushed to
/// `diag_sources`, for rendering them later without the live world.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    world: Tracked<dyn World + '_>,
    input: &TypstInput,
    format: Format,
    ppi: i64,
    minify: bool,
    ctx: &TwylaContext,
    packages: &SystemPackages,
    span: Span,
    rec: &DepRecorder,
    warnings: &mut Vec<SourceDiagnostic>,
    diag_sources: &mut Vec<Source>,
) -> SourceResult<Built> {
    let (main_id, main_src, stem, content) = match input {
        TypstInput::File(file) => {
            // Compile the real project file as `main`; the sub-world reads it
            // (and its imports) through the recorder.
            let stem = Path::new(file.vpath().get_without_slash())
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned);
            (*file, None, stem, None)
        }
        TypstInput::Content(content) => {
            // Already-evaluated content is laid out directly (see
            // [`compile_content`]); the empty main is never evaluated, it just
            // gives the world a `main`. No file stem → hash-only output name.
            let id = FileId::new(RootedPath::new(
                VirtualRoot::Project,
                VirtualPath::new("__twyla_inline__.typ").unwrap(),
            ));
            let src = Source::new(id, EcoString::new().into());
            (id, Some(src), None, Some(content))
        }
    };

    let subworld = TypstWorld {
        world,
        library: stock_library(),
        main_id,
        main_src,
        root: &ctx.root,
        packages,
        rec,
        loaded: Mutex::new(HashMap::new()),
        located: Mutex::new(HashMap::new()),
    };

    let mut sub_warnings = Vec::new();
    let result = serialize(
        &subworld,
        content,
        format,
        ppi,
        minify,
        span,
        &mut sub_warnings,
    );
    // Capture the sources of everything the sub-compile has to say, while
    // the sub-world still holds them.
    let errors = result.as_ref().err().map(|e| e.as_slice()).unwrap_or(&[]);
    subworld.capture_sources(errors.iter().chain(&sub_warnings), diag_sources);
    warnings.extend(sub_warnings);
    let bytes = result?;

    // A pending placeholder must never ship: the subcompile can't resolve
    // it, so it's an error (in any text format it could appear in).
    if bytes
        .windows(super::ASSET_PENDING.len())
        .any(|window| window == super::ASSET_PENDING.as_bytes())
    {
        return Err(eco_vec![
            SourceDiagnostic::error(
                span,
                "a Twyla asset inside this `asset.typst` document could not be resolved",
            )
            .with_hint(
                "`asset.typst` compiles with stock Typst; content constructed outside it can \
                 carry Twyla calls into the subcompile, but nested asset requests are not part \
                 of the surrounding site's discovery loop",
            )
            .with_hint("use an inline `document(..)` when the generated HTML needs Twyla assets")
        ]);
    }

    let bytes = Bytes::new(bytes);
    Ok(Built {
        sha256: sha256(&bytes),
        emit: Emit::Bytes(bytes),
        ext: Some(format.ext().to_owned()),
        stem,
        dimensions: None,
        elem_id: None,
        output_path: None,
        companions: Vec::new(),
        preload: Vec::new(),
    })
}

/// Compile the sub-document and serialize it to `format`. The sub-compile's
/// warnings go to `warnings`.
fn serialize(
    world: &TypstWorld,
    content: Option<&Content>,
    format: Format,
    ppi: i64,
    minify: bool,
    span: Span,
    warnings: &mut Vec<SourceDiagnostic>,
) -> SourceResult<Vec<u8>> {
    Ok(match format {
        Format::Html => {
            let Warned {
                output,
                warnings: w,
            } = compile::<HtmlDocument>(world, content);
            warnings.extend(w);
            typst_html::html(&output?, &typst_html::HtmlOptions { pretty: true })?.into_bytes()
        }
        Format::Svg | Format::Png | Format::Pdf => {
            let Warned {
                output,
                warnings: w,
            } = compile::<PagedDocument>(world, content);
            warnings.extend(w);
            let doc = output?;
            match format {
                Format::Svg => {
                    let options = typst_svg::SvgOptions {
                        pretty: true,
                        ..Default::default()
                    };
                    let svg = typst_svg::svg_merged(&doc, &options, Abs::zero());
                    if minify {
                        super::svg::minify_str(svg, span, warnings).into_bytes()
                    } else {
                        svg.into_bytes()
                    }
                }
                Format::Pdf => typst_pdf::pdf(&doc, &PdfOptions::default())?,
                Format::Png => {
                    let options = typst_render::RenderOptions {
                        pixel_per_pt: (ppi as f64 / 72.0).into(),
                        render_bleed: false,
                    };
                    // Each page paints its own fill (white unless `fill: none`), so leave the
                    // canvas transparent: a transparent page stays transparent.
                    let pixmap = typst_render::render_merged(&doc, &options, Abs::zero(), None);
                    pixmap
                        .encode_png()
                        .map_err(|err| err_at(span, eco_format!("PNG encode failed: {err}")))?
                }
                Format::Html => unreachable!("html handled above"),
            }
        }
    })
}

/// The stock library every `asset.typst` sub-compile runs against: the
/// standard typst stdlib (with the HTML feature, needed for `format: "html"`
/// and harmless otherwise) but NONE of twyla's `install_stdlib` splices — no
/// custom rules, no `document`/`asset`/`content`.
///
/// One instance for the whole process, not one per build: the library is
/// part of nearly every memoized call's key, so a shared one (hashed once)
/// lets comemo reuse work across sub-compiles — figures sharing a package,
/// styles, or subcontent — and across the dev server's rebuilds. Inline
/// content is therefore never injected into the library's scope (which would
/// make every build's library distinct); see [`compile_content`].
fn stock_library() -> Arc<LazyHash<Library>> {
    static STOCK: LazyLock<Arc<LazyHash<Library>>> = LazyLock::new(|| {
        Arc::new(LazyHash::new(
            Library::builder()
                .with_features([Feature::Html].into_iter().collect())
                .build(),
        ))
    });
    STOCK.clone()
}

/// Compile the sub-document: the world's `main`, or already-evaluated
/// `content`.
fn compile<T: Output>(world: &TypstWorld, content: Option<&Content>) -> Warned<SourceResult<T>> {
    match content {
        Some(content) => compile_content(world, content),
        None => typst::compile::<T>(world),
    }
}

/// `typst::compile` for content that's already evaluated: typst's relayout
/// loop (`compile_impl`) minus the eval of `main`.
fn compile_content<T: Output>(world: &dyn World, content: &Content) -> Warned<SourceResult<T>> {
    let world = world.track();
    let traced = Traced::default();
    let traced = traced.track();
    let mut sink = Sink::new();
    let output = (|| {
        let library = world.library();
        if T::target() == Target::Html {
            // What `typst::compile` says for HTML output, for parity.
            sink.warn(
                SourceDiagnostic::warning(
                    Span::detached(),
                    "html export is under active development and incomplete",
                )
                .with_hint("its behaviour may change at any time")
                .with_hint("do not rely on this feature for production use cases")
                .with_hint("see https://github.com/typst/typst/issues/5512 for more information"),
            );
        }
        let base = StyleChain::new(&library.styles);
        let target = TargetElem::target.set(T::target()).wrap();
        let styles = base.chain(&target);
        let empty = EmptyIntrospector;
        let mut history: Vec<T> = Vec::new();
        let mut document: T;
        loop {
            let introspector = history
                .last()
                .map(|doc| doc.introspector())
                .unwrap_or(&empty);
            let constraint = comemo::Constraint::new();
            let mut subsink = Sink::new();
            let mut engine = Engine {
                library,
                world,
                introspector: Protected::new(introspector.track_with(&constraint)),
                traced,
                sink: subsink.track_mut(),
                route: Route::default(),
            };
            document = T::create(&mut engine, content, styles)?;
            if constraint.validate(document.introspector()) {
                sink.extend_from_sink(subsink);
                break;
            }
            if history.len() == MAX_ITERS - 1 {
                let mut introspectors = [&empty as &dyn Introspector; MAX_ITERS + 1];
                for i in 1..MAX_ITERS {
                    introspectors[i] = history[i - 1].introspector();
                }
                introspectors[MAX_ITERS] = document.introspector();
                let warnings = analyze(world, introspectors, subsink.introspections());
                sink.extend_from_sink(subsink);
                for warning in warnings {
                    sink.warn(warning);
                }
                break;
            }
            history.push(document);
        }
        let delayed = sink.delayed();
        if !delayed.is_empty() {
            return Err(delayed);
        }
        Ok(document)
    })()
    .map_err(|mut diags| {
        let mut seen = std::collections::HashSet::new();
        diags.retain(|d| seen.insert(typst_utils::hash128(&(&d.span, &d.message))));
        diags
    });
    Warned {
        output,
        warnings: sink.warnings(),
    }
}

/// A spanned error from any `Display` payload.
fn err_at(span: Span, msg: impl std::fmt::Display) -> ecow::EcoVec<SourceDiagnostic> {
    eco_vec![SourceDiagnostic::error(
        span,
        EcoString::from(msg.to_string())
    )]
}

/// A recording [`World`] with no meaningful `main`, for loading files *outside*
/// a compile — `asset.image`'s vector rasterization hands it to
/// [`SvgImage::with_fonts_images`](typst::visualize::SvgImage::with_fonts_images)
/// so an SVG's linked images are read through `rec` (and join the asset's
/// deps) instead of the live world's `FileStore`. Library, fonts and date
/// delegate to the live `world`.
pub(crate) fn file_world<'a>(
    world: Tracked<'a, dyn World + 'a>,
    ctx: &'a TwylaContext,
    packages: &'a SystemPackages,
    rec: &'a DepRecorder,
) -> TypstWorld<'a> {
    let main = Source::detached("");
    TypstWorld {
        library: Arc::new(world.library().clone()),
        world,
        main_id: main.id(),
        main_src: Some(main),
        root: &ctx.root,
        packages,
        rec,
        loaded: Mutex::new(HashMap::new()),
        located: Mutex::new(HashMap::new()),
    }
}

/// A [`World`] for an `asset.typst` sub-compile: serves a stock library + a
/// `main` (a synthetic source for inline content, or the real project file for
/// a path source), reads project and package files itself through the
/// recorder, and delegates fonts and the date to the live world.
///
/// Owns the stock library (it returns a reference, so it can't be borrowed
/// across the `Tracked` boundary — the same reason `SnippetWorld` clones). The
/// font book is read off the live world per call.
pub(crate) struct TypstWorld<'a> {
    world: Tracked<'a, dyn World + 'a>,
    library: Arc<LazyHash<Library>>,
    main_id: FileId,
    /// `Some` for an inline-content compile (the synthetic main); `None` for a
    /// path source (the real file is read like any other project file).
    main_src: Option<Source>,
    /// The project root project-file ids resolve against.
    root: &'a Path,
    /// Where package-file ids resolve (see [`locate_package`]).
    packages: &'a SystemPackages,
    rec: &'a DepRecorder,
    /// Files loaded so far, so each is read (and recorded) once per build —
    /// typst asks for the same file many times. Dropped with the sub-world
    /// when the build ends.
    loaded: Mutex<HashMap<FileId, FileResult<Loaded>>>,
    /// Package roots looked up so far, so each lookup is recorded once.
    located: Mutex<HashMap<PackageSpec, PackageResult<FsRoot>>>,
}

/// A loaded file: its bytes, and its parsed source once asked for.
#[derive(Clone)]
struct Loaded {
    bytes: Bytes,
    source: Option<Source>,
}

impl TypstWorld<'_> {
    /// Where file `id` lives on disk. A package id first looks the package
    /// up, recording the lookup.
    fn path(&self, id: FileId) -> FileResult<PathBuf> {
        match id.root() {
            VirtualRoot::Project => Ok(self.root.join(id.vpath().get_without_slash())),
            VirtualRoot::Package(spec) => {
                let mut located = self.located.lock().unwrap();
                let root = located.entry(spec.clone()).or_insert_with(|| {
                    locate_package(self.packages, spec, &mut |dep| self.rec.ingest([dep]))
                });
                root.clone()?.resolve(id.vpath())
            }
        }
    }

    /// Load a file through the recorder, once per build.
    fn load(&self, id: FileId, as_source: bool) -> FileResult<Loaded> {
        let mut loaded = self.loaded.lock().unwrap();
        let entry = loaded.entry(id).or_insert_with(|| {
            let path = self.path(id)?;
            self.rec
                .read(&path)
                .map(|bytes| Loaded {
                    bytes: Bytes::new(bytes),
                    source: None,
                })
                .map_err(|err| FileError::from_io(err, &path))
        });
        if as_source
            && let Ok(file) = entry
            && file.source.is_none()
        {
            let text = std::str::from_utf8(&file.bytes).map_err(|_| FileError::InvalidUtf8)?;
            file.source = Some(Source::new(id, text.into()));
        }
        entry.clone()
    }

    /// Push the source of every file `diags` point into (their spans, hints
    /// and traces) that this build loaded, parsing it if it was only loaded
    /// as bytes. Files that failed to load (and so can't be pointed into)
    /// are skipped.
    fn capture_sources<'d>(
        &self,
        diags: impl Iterator<Item = &'d SourceDiagnostic>,
        out: &mut Vec<Source>,
    ) {
        for diag in diags {
            let spans = std::iter::once(diag.span)
                .chain(diag.hints.iter().map(|hint| hint.span))
                .chain(diag.trace.iter().map(|point| point.span.into()));
            for id in spans.filter_map(|span| span.id()) {
                if out.iter().any(|source| source.id() == id) {
                    continue;
                }
                let source = match &self.main_src {
                    Some(src) if id == self.main_id => Some(src.clone()),
                    _ => match self.loaded.lock().unwrap().get(&id) {
                        Some(Ok(file)) => file.source.clone().or_else(|| {
                            let text = std::str::from_utf8(&file.bytes).ok()?;
                            Some(Source::new(id, text.into()))
                        }),
                        _ => None,
                    },
                };
                out.extend(source);
            }
        }
    }
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
        Ok(self.load(id, true)?.source.expect("parsed by load"))
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.main_id
            && let Some(src) = &self.main_src
        {
            return Ok(Bytes::new(src.text().as_bytes().to_vec()));
        }
        Ok(self.load(id, false)?.bytes)
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.world.font(index)
    }
    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        self.world.today(offset)
    }
}

#[cfg(test)]
mod tests {
    use super::TypstInput;
    use typst::foundations::Content;
    use typst::syntax::Source;
    use typst::text::TextElem;

    /// Typst's `Content` equality ignores spans (and locations, labels…) but
    /// its hash doesn't: inline-content inputs must follow the hash, or the
    /// asset maps' `Eq`/`Hash` disagree.
    #[test]
    fn inline_content_inputs_compare_by_hash() {
        let source = Source::detached("x");
        let s1 = source.root().span();
        let s2 = source.root().children().next().unwrap().span();
        assert_ne!(s1, s2);
        let a: Content = TextElem::packed("x").spanned(s1);
        let b: Content = TextElem::packed("x").spanned(s2);
        assert_eq!(a, b, "typst's own equality is span-blind");
        assert_ne!(TypstInput::Content(a.clone()), TypstInput::Content(b));
        assert_eq!(TypstInput::Content(a.clone()), TypstInput::Content(a));
    }
}
