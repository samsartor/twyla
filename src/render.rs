//! Render typst content to HTML via the bundle export feature.
//!
//! [`RenderWorld`] is the persistent compile state — typst-kit's
//! [`FileStore`] backs the file/source caches, dependency tracking, and
//! the stale-source-reuse optimization. Long-lived instances (used by
//! `twyla serve`) keep their comemo cache across compiles; one-shot
//! callers (`build`, `check`, `render`) just construct a fresh world,
//! compile once, and drop it.

use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

use iddqd::{IdHashItem, IdHashMap, id_upcast};

use typst::diag::{FileError, FileResult, SourceDiagnostic, Warned};
use typst::foundations::{Bytes, Datetime, Dict, Duration, IntoValue};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_bundle::{BundleDocument, BundleFile};
use typst_kit::diagnostics::termcolor::{Buffer, ColorChoice, StandardStream};
use typst_kit::diagnostics::{self, DiagnosticFormat, DiagnosticWorld};
use typst_kit::downloader::SystemDownloader;
use typst_kit::files::{FileLoader, FileStore};
use typst_kit::fonts::FontStore;
use typst_kit::packages::SystemPackages;
use typst_library::Feature;

use crate::asset::ResolvedAsset;
use crate::resolver::Resolver;
use crate::compile::HarvestedDoc;
use crate::project::TwylaContext;

/// The `type` attribute marking a raw-HTML carrier `<script>`. The builtin
/// [`crate::content::raw_html`] produces an element with this type; the
/// resolution pass below strips the wrapper and inlines the body. Shared so
/// producer and consumer agree on the sentinel.
pub const RAW_HTML_SCRIPT_TYPE: &str = "x-twyla-raw-html";

/// A pre-rendered render/compile failure.
///
/// Compile failures are formatted through typst-kit's diagnostic emitter
/// (the same one `typst-cli` uses): colored source snippets, carets, and
/// hints. Because the emitter borrows the [`World`] to look up source files,
/// the rendering happens *here*, at the failure site, while the world is
/// still alive — the dev server, which holds the error long after the world
/// is dropped, can't render it lazily.
///
/// Two renderings are kept: [`term`](Self::term) (what [`Display`] yields, so
/// any caller that prints the error gets the rich block, colored when stderr
/// is a tty) and [`html`](Self::html) (for the dev server's error page).
#[derive(Debug)]
pub struct RenderError {
    /// Terminal-ready block returned by [`Display`]. Colored when appropriate.
    term: String,
    /// Browser-ready HTML block for the dev server's error page.
    html: String,
    pub kind: RenderErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderErrorKind {
    /// World setup, project layout, or path resolution failed.
    Setup,
    /// Typst compilation failed.
    Compile,
    /// The bundle didn't contain the expected HTML documents.
    Bundle,
}

impl RenderError {
    /// The error rendered as an HTML block, for the dev server error page.
    pub fn html(&self) -> &str {
        &self.html
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.term.trim_end())
    }
}

impl std::error::Error for RenderError {}

/// One routed page emerging from the bundle compile.
#[derive(Debug, Clone)]
pub struct OutputDoc {
    pub output_path: String,
    pub html: String,
    /// The metadata harvested from inside the document.
    /// This only None if the user created a raw Typst document
    /// without going through Twyla somehow.
    pub meta: Option<HarvestedDoc>,
}

/// Some data which can be emitted to the output directory.
#[derive(Clone, Debug, PartialEq)]
pub enum Emit {
    Bytes(Bytes),
    Copy(PathBuf),
}

impl Emit {
    /// Write this output to `dest` — `fs::write` for in-memory bytes, a
    /// stream `fs::copy` for the path-backed variants.
    pub(crate) fn write_to(&self, dest: &Path) -> io::Result<()> {
        match self {
            Emit::Bytes(bytes) => fs::write(dest, bytes),
            Emit::Copy(src) => fs::copy(src, dest).map(drop),
        }
    }

    /// The output bytes — the in-memory buffer, or a fresh read of the
    /// path-backed source. Backs `asset.*().read()`; once a content-addressed
    /// cache lands this reads the cache file uniformly. Not held in RAM for the
    /// `Copy` case, so reading a large asset is pay-as-you-go.
    pub(crate) fn read(&self) -> io::Result<Bytes> {
        match self {
            Emit::Bytes(bytes) => Ok(bytes.clone()),
            Emit::Copy(src) => Ok(Bytes::new(fs::read(src)?)),
        }
    }
}

#[derive(Debug)]
pub enum Output {
    Doc(OutputDoc),
    Asset(ResolvedAsset),
    Static(String, PathBuf),
}

impl Output {
    /// The root-relative `/`-separated output path — this output's key in
    /// [`Outputs`] and its destination under the build dir.
    pub fn key(&self) -> &str {
        match self {
            Output::Doc(d) => d.output_path.as_str(),
            Output::Asset(a) => &a.output_path,
            Output::Static(f, _) => f,
        }
    }

    /// Write this output to `dest`: page HTML and transformed asset bytes go
    /// through `fs::write`; path-backed assets and static files stream-copy.
    pub fn write_to(&self, dest: &Path) -> io::Result<()> {
        match self {
            Output::Doc(d) => fs::write(dest, &d.html),
            Output::Asset(a) => a.built.emit.write_to(dest),
            Output::Static(_, src) => fs::copy(src, dest).map(drop),
        }
    }
}

impl IdHashItem for Output {
    type Key<'a> = &'a str;

    fn key(&self) -> &str {
        Output::key(self)
    }

    id_upcast!();
}

/// Everything one compile produces, keyed by output path: compiled pages
/// (with their harvested metadata), processed assets, and static files. The
/// single object `build` writes, the manifest enumerates, and `serve` answers
/// requests from — so they can't disagree about what the site contains.
pub struct Outputs {
    pub all: IdHashMap<Output>,
}

impl Outputs {
    /// Look up an output by its root-relative key (e.g. `guis-1/index.html`).
    pub fn get(&self, key: &str) -> Option<&Output> {
        self.all.get(key)
    }

    /// Every output, in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = &Output> {
        self.all.iter()
    }

    /// Just the compiled pages (with their harvested metadata).
    pub fn docs(&self) -> impl Iterator<Item = &OutputDoc> {
        self.all.iter().filter_map(|o| match o {
            Output::Doc(d) => Some(d),
            _ => None,
        })
    }

    /// Just the processed assets.
    pub fn assets(&self) -> impl Iterator<Item = &ResolvedAsset> {
        self.all.iter().filter_map(|o| match o {
            Output::Asset(a) => Some(a),
            _ => None,
        })
    }

    /// Write every output under `dir`, creating `dir` and each parent directory
    /// as needed. IO errors are annotated with the offending path. This is the
    /// whole of `twyla build`'s disk work.
    pub fn emit_to_fs(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)
            .map_err(|e| io::Error::new(e.kind(), format!("creating {}: {e}", dir.display())))?;
        for output in self.iter() {
            let dest = dir.join(output.key());
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    io::Error::new(e.kind(), format!("creating {}: {e}", parent.display()))
                })?;
            }
            output.write_to(&dest).map_err(|e| {
                io::Error::new(e.kind(), format!("writing {}: {e}", dest.display()))
            })?;
        }
        Ok(())
    }
}

/// Compile every page under `<root>/content/` into the full set of
/// [`Outputs`] (pages, assets, static files).
///
/// One-shot wrapper: builds a fresh [`RenderWorld`], compiles, drops.
/// `twyla serve` skips this and reuses a long-lived world directly so
/// the comemo cache survives between requests.
pub fn render_site(ctx: &TwylaContext) -> Result<Outputs, RenderError> {
    let world = RenderWorld::new(ctx)?;
    world.compile_bundle(&mut Resolver::new(ctx))
}

/// Replace every `<script type="x-twyla-raw-html">..</script>` with its
/// inner body. The workaround for typst's lack of a first-class
/// `html.raw` — see `doc/index.typ` § Resolution pass.
pub fn resolve_raw_html_placeholders(input: &str) -> String {
    let marker = format!(r#"<script type="{RAW_HTML_SCRIPT_TYPE}">"#);
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(start) = input[cursor..].find(&marker) {
        let abs = cursor + start;
        let body_start = abs + marker.len();
        let Some(rel_end) = input[body_start..].find("</script>") else {
            // Unterminated — bail by emitting the rest unchanged. Shouldn't
            // happen in practice unless typst's serializer changes.
            break;
        };
        let body_end = body_start + rel_end;
        out.push_str(&input[cursor..abs]);
        out.push_str(&input[body_start..body_end]);
        cursor = body_end + "</script>".len();
    }
    out.push_str(&input[cursor..]);
    out
}

/// A render failure with no source span (setup/layout/bundle problems). The
/// message is shown verbatim — there's no snippet to draw.
pub(crate) fn plain_err(msg: impl Into<String>, kind: RenderErrorKind) -> RenderError {
    let msg = msg.into();
    let html = to_html(&msg);
    RenderError {
        term: format!("error: {msg}"),
        html,
        kind,
    }
}

fn setup_err(msg: impl Into<String>) -> RenderError {
    plain_err(msg, RenderErrorKind::Setup)
}

/// Whether to colorize terminal diagnostics: only when stderr is a tty and
/// `NO_COLOR` is unset (matches `twyla convert`'s color rule).
pub(crate) fn terminal_color() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Render diagnostics to a string via typst-kit's emitter. With `color`, the
/// string carries ANSI escapes; otherwise it's plain text. Emitting into an
/// in-memory buffer only fails on encoding errors, which we don't expect.
pub(crate) fn emit_to_string(
    world: &dyn DiagnosticWorld,
    diags: &[SourceDiagnostic],
    color: bool,
) -> String {
    let mut buf = if color {
        Buffer::ansi()
    } else {
        Buffer::no_color()
    };
    let _ = diagnostics::emit(&mut buf, world, diags, DiagnosticFormat::Human);
    String::from_utf8_lossy(&buf.into_inner()).into_owned()
}

/// Convert an ANSI (or plain) string to an HTML block for the dev server. The
/// crate escapes HTML entities; on the rare conversion failure, fall back to
/// the raw text (browsers render unknown escapes harmlessly).
fn to_html(s: &str) -> String {
    ansi_to_html::convert(s).unwrap_or_else(|_| s.to_string())
}

/// Build a [`RenderError`] from typst compile diagnostics, rendering the rich
/// (colored) block now while the `world` is still alive — see [`RenderError`].
fn compile_err(world: &dyn DiagnosticWorld, errors: &[SourceDiagnostic]) -> RenderError {
    let ansi = emit_to_string(world, errors, true);
    let term = if terminal_color() {
        ansi.clone()
    } else {
        emit_to_string(world, errors, false)
    };
    RenderError {
        html: to_html(&ansi),
        term,
        kind: RenderErrorKind::Compile,
    }
}

/// Emit compile warnings to stderr through the same pretty emitter. Warnings
/// don't block rendering, so they only go to the terminal (not the browser).
fn emit_warnings(world: &dyn DiagnosticWorld, warnings: &[SourceDiagnostic]) {
    if warnings.is_empty() {
        return;
    }
    let choice = if terminal_color() {
        ColorChoice::Always
    } else {
        ColorChoice::Never
    };
    let mut stream = StandardStream::stderr(choice);
    let _ = diagnostics::emit(&mut stream, world, warnings, DiagnosticFormat::Human);
}

/// Long-lived typst compile state.
///
/// Wraps a [`FileStore`] (which handles source/byte caching, dependency
/// tracking via slot-access flags, and in-place stale-source reuse on
/// reset). For `twyla serve`, one instance lives behind a mutex across
/// requests so the comemo cache survives between compiles. For one-shot
/// CLI commands, we just build one, compile, drop.
pub struct RenderWorld {
    pub ctx: TwylaContext,
    pub main_id: FileId,
    pub library: LazyHash<Library>,
    pub fonts: FontStore,
    pub files: FileStore<TwylaLoader>,
}

impl RenderWorld {
    /// Set up a world from a [`TwylaContext`]. The context's `root` is
    /// the typst project root; the virtual main is initialized by
    /// scanning `<root>/content/` through the context.
    ///
    /// Call [`refresh_main`](Self::refresh_main) after files are added
    /// or removed under content/ to regenerate the main; the next
    /// [`compile_bundle`](Self::compile_bundle) picks it up.
    pub fn new(ctx: &TwylaContext) -> Result<Self, RenderError> {
        let ctx = ctx.clone();

        let mut fonts = FontStore::new();
        fonts.extend(typst_kit::fonts::embedded());
        fonts.extend(typst_kit::fonts::system());

        // Static `FileId` allocated to act as the site "main".
        let main_id = FileId::unique(RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new("<website>").unwrap(),
        ));

        let loader = TwylaLoader {
            root: ctx.root.clone(),
            packages: SystemPackages::new(SystemDownloader::new(concat!(
                "twyla/",
                env!("CARGO_PKG_VERSION"),
            ))),
        };

        let mut inputs = Dict::new();
        inputs.insert(
            "base_url".into(),
            ctx.base_url.as_deref().unwrap_or("").into_value(),
        );

        // Build the stdlib, then splice twyla's native builtins into the
        // global scope before freezing it in a LazyHash. This is the
        // supported customization path (the library is `build()`-then-
        // mutate; `global` is a `pub Module` with `scope_mut()`), not a
        // typst fork. See `crate::prelude`.
        let mut library = Library::builder()
            .with_features([Feature::Html, Feature::Bundle].into_iter().collect())
            .with_inputs(inputs)
            .build();
        install_stdlib(&mut library, ctx.reflect, ctx.test_examples);

        Ok(Self {
            ctx,
            main_id,
            library: LazyHash::new(library),
            fonts,
            files: FileStore::new(loader),
        })
    }

    /// Paths that the last compile read from disk. Suitable for
    /// `typst_kit::watcher::Watcher::update`. Drops the virtual main
    /// (not a real path).
    pub fn dependencies(&mut self) -> impl Iterator<Item = PathBuf> + '_ {
        let (loader, ids) = self.files.dependencies();
        ids.filter_map(|id| loader.resolve(id).ok())
    }

    /// Compile every routed page in the bundle.
    ///
    /// The `resolver` is caller-owned (not stored in the world): a one-shot
    /// caller passes a fresh one; `serve` reuses one across recompiles so its
    /// store persists. Passing it in keeps these methods `&self` — a field
    /// would force interior mutability, since `self` is also handed to typst as
    /// `&dyn World` for the duration of the compile.
    pub fn compile_bundle(&self, resolver: &mut Resolver) -> Result<Outputs, RenderError> {
        // Reads through `FileStore`, so repeated calls hit the comemo
        // cache. Between compiles, call [`reset`](Self::reset) and
        // `comemo::evict(..)` to invalidate; for content/ shape changes,
        // also call [`refresh_main`](Self::refresh_main).

        let sources = self.ctx.scan_pages().map_err(setup_err)?;
        let fileids: Vec<_> = sources
            .into_iter()
            .map(|path| {
                FileId::new(RootedPath::new(
                    VirtualRoot::Project,
                    VirtualPath::virtualize(&self.ctx.root, &path)
                        .expect("all paths should be within the root"),
                ))
            })
            .collect();
        let Warned { output, warnings } =
            crate::compile::compile_bundle(&self.ctx, self, &fileids, resolver);
        emit_warnings(self, &warnings);

        let (bundle, harvested, assets) = output.map_err(|errors| compile_err(self, &errors))?;

        let mut all = IdHashMap::new();
        for (path, file) in bundle.files.iter() {
            let doc = match file {
                BundleFile::Document(BundleDocument::Html(doc)) => doc,
                _ => continue,
            };
            let raw = typst_html::html(doc).map_err(|errors| compile_err(self, &errors))?;
            if let Err(err) = all.insert_unique(Output::Doc(OutputDoc {
                html: resolve_raw_html_placeholders(&raw),
                output_path: path.get_without_slash().to_owned(),
                meta: None,
            })) {
                return Err(duplication_error(err).0);
            }
        }

        for doc in harvested {
            let mut out = all.get_mut(doc.output.as_str());
            let Some(Output::Doc(out)) = out.as_deref_mut() else {
                panic!("missing document in bundle with path {}", &doc.output);
            };
            out.meta = Some(doc);
        }

        for asset in assets {
            if let Err(err) = all.insert_unique(Output::Asset(asset)) {
                return Err(duplication_error(err).0);
            }
        }

        let static_outputs = match self.ctx.static_outputs() {
            Ok(s) => s,
            Err(err) => return Err(plain_err(err, RenderErrorKind::Bundle)),
        };
        for out in static_outputs {
            if let Err(err) = all.insert_unique(out) {
                return Err(duplication_error(err).0);
            }
        }

        if all.is_empty() {
            return Err(plain_err(
                "bundle produced no outputs",
                RenderErrorKind::Bundle,
            ));
        }

        Ok(Outputs { all })
    }
}

/// The error shown when two outputs conflict
fn duplication_error(err: iddqd::errors::DuplicateItem<Output, &Output>) -> (RenderError, Output) {
    let (new, duplicates) = err.into_parts();
    let new_text = match &new {
        Output::Doc(_d) => format_args!("document"), // TODO: source location?
        Output::Asset(a) => format_args!("asset {:?}", a.spec),
        Output::Static(_, p) => format_args!("static file \"{}\"", p.display()),
    };
    let dup_text = match &duplicates[0] {
        Output::Doc(_d) => format_args!("a document"), // TODO: source location?
        Output::Asset(a) => format_args!("an asset {:?}", a.spec),
        Output::Static(_, p) => format_args!("static file \"{}\"", p.display()),
    };
    (
        plain_err(
            format!("{new_text} conflicts with {dup_text}"),
            RenderErrorKind::Bundle,
        ),
        new,
    )
}

/// Install twyla's native customizations into a freshly built library.
///
/// `reflect` (`--reflect`) splices in the `twyla-reflect` module so twyla's own
/// reference site can introspect its builtins; `test_examples`
/// (`--test-examples`) splices in `twyla-examples` so a docs page can compile +
/// highlight its ` ```example ` blocks. Both are off for normal site builds.
pub fn install_stdlib(library: &mut Library, reflect: bool, test_examples: bool) {
    crate::rules::install(&mut library.rules);
    let global = library.global.scope_mut();
    crate::document::install(global);
    crate::asset::install(global);
    crate::content::install(global);
    if reflect {
        crate::reflect::install(global);
    }
    if test_examples {
        crate::examples::install(global);
    }
}

/// FileLoader for [`FileStore`]. Serves the (empty) virtual main from
/// memory and every other id from the on-disk project root.
pub struct TwylaLoader {
    root: PathBuf,
    /// Serves packages from the  standard data/cache dirs, downloading from
    /// Typst Universe on a cache miss.
    packages: SystemPackages,
}

impl TwylaLoader {
    /// Resolve a file id to its on-disk path. Returns
    /// [`FileError::NotFound`] for the virtual main (no real path) so
    /// callers iterating dependencies can `filter_map` it away.
    /// Package ids resolve through [`SystemPackages`] (data/cache dirs,
    /// then download from Universe), mirroring the typst CLI.
    fn resolve(&self, id: FileId) -> FileResult<PathBuf> {
        let vpath = id.vpath();
        match id.root() {
            VirtualRoot::Project => Ok(self.root.join(vpath.get_without_slash())),
            VirtualRoot::Package(spec) => Ok(self.packages.obtain(spec)?.resolve(vpath)),
        }
    }
}

impl FileLoader for TwylaLoader {
    fn load(&self, id: FileId) -> FileResult<Bytes> {
        // Packages resolve to a (possibly just-downloaded) cache dir and
        // load through `FsRoot` so the path-escape guard applies; project
        // files read directly from the resolved on-disk path.
        if let VirtualRoot::Package(spec) = id.root() {
            return self.packages.obtain(spec)?.load(id.vpath());
        }
        let path = self.resolve(id)?;
        std::fs::read(&path)
            .map(Bytes::new)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => FileError::NotFound(path),
                _ => FileError::Other(Some(e.to_string().into())),
            })
    }
}

/// Names files for diagnostic snippets. Project files show as their root-
/// relative path; package files are prefixed with the package spec.
impl DiagnosticWorld for RenderWorld {
    fn name(&self, id: FileId) -> String {
        let vpath = id.vpath().get_without_slash();
        match id.root() {
            VirtualRoot::Project => vpath.to_string(),
            VirtualRoot::Package(spec) => format!("{spec}/{vpath}"),
        }
    }
}

impl World for RenderWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.book()
    }
    fn main(&self) -> FileId {
        self.main_id
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        self.files.source(id)
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        self.files.file(id)
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.font(index)
    }
    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        // Stable across runs so AST equivalence diffs aren't perturbed by
        // the wall clock. Bump if we ever start exercising date-dependent
        // logic that needs realistic values.
        Datetime::from_ymd(2026, 5, 17)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_single_placeholder() {
        let s = r#"<div><script type="x-twyla-raw-html"><svg/></script></div>"#;
        assert_eq!(resolve_raw_html_placeholders(s), "<div><svg/></div>");
    }

    #[test]
    fn resolve_multiple_placeholders() {
        let s = r#"a<script type="x-twyla-raw-html">X</script>b<script type="x-twyla-raw-html">Y</script>c"#;
        assert_eq!(resolve_raw_html_placeholders(s), "aXbYc");
    }

    #[test]
    fn resolve_leaves_other_scripts_alone() {
        let s = r#"<script>alert(1)</script><script type="x-twyla-raw-html">RAW</script>"#;
        assert_eq!(
            resolve_raw_html_placeholders(s),
            "<script>alert(1)</script>RAW"
        );
    }

    #[test]
    fn resolve_preserves_internal_newlines() {
        let s = "<script type=\"x-twyla-raw-html\"><svg>\n  <path/>\n</svg></script>";
        assert_eq!(resolve_raw_html_placeholders(s), "<svg>\n  <path/>\n</svg>");
    }

    /// Exercises the multi-document case end-to-end: scan content/,
    /// compile every page in one bundle, verify each routed doc lands
    /// at the expected path AND that intra-doc anchor links are
    /// fragment-only (typst-html resolves `link(<label>)` natively per
    /// document — no slug-aware show rule, no cross-doc URL leak).
    ///
    /// Opt-in via `cargo test -- --ignored` because it depends on the
    /// site repo being present at `$SITE_ROOT` / `~/Src/site`.
    #[test]
    #[ignore]
    fn render_site_smoke() {
        let site = std::env::var_os("TWYLA_ROOT")
            .or_else(|| std::env::var_os("SITE_ROOT"))
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| {
                    let mut p = PathBuf::from(h);
                    p.push("Src/site");
                    p
                })
            })
            .expect("TWYLA_ROOT, SITE_ROOT, or HOME");
        let ctx = TwylaContext::new(&site, None).expect("TwylaContext");
        let outputs = render_site(&ctx).expect("render_site");
        let paths: Vec<String> = outputs.docs().map(|d| d.output_path.clone()).collect();
        assert!(
            paths.iter().any(|p| p == "guis-1/index.html"),
            "missing guis-1 in {paths:?}",
        );
        assert!(
            paths.iter().any(|p| p == "guis-2/index.html"),
            "missing guis-2 in {paths:?}",
        );
        assert!(
            paths.iter().any(|p| p == "guis-3/index.html"),
            "missing guis-3 in {paths:?}",
        );
        // Per-doc anchor links: guis-1 has an intra-doc link to
        // <hybrid-mode>, guis-3 has two intra-doc links to <streams>
        // and <stream-based-reactivity>. The label-based form resolves
        // to a fragment-only href via typst-html's introspector — no
        // absolutization, and crucially no cross-doc leak (a prior
        // `query(<twyla-page>).first()` design always returned the
        // first slug, so all anchor links pointed at guis-1).
        let intra_doc_expectations: &[(&str, &[&str])] = &[
            ("guis-1/index.html", &["href=\"#hybrid-mode\""]),
            (
                "guis-3/index.html",
                &["href=\"#streams\"", "href=\"#stream-based-reactivity\""],
            ),
        ];
        for (path, expected_anchors) in intra_doc_expectations {
            let doc = outputs
                .docs()
                .find(|d| d.output_path == *path)
                .unwrap_or_else(|| panic!("missing {path}"));
            for anchor in *expected_anchors {
                assert!(
                    doc.html.contains(anchor),
                    "{path} missing expected intra-doc anchor {anchor:?}",
                );
            }
            // Anchor-only links must not appear in absolutized form
            // (that was the latent bug — show-rule absolutization
            // against the wrong slug).
            assert!(
                !doc.html.contains("samsartor.com/guis-1/#"),
                "{path} contains absolutized anchor — label scope leak",
            );
            assert!(
                !doc.html.contains("samsartor.com/guis-3/#"),
                "{path} contains absolutized anchor — label scope leak",
            );
        }
    }
}
