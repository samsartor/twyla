//! Render typst content to HTML via the bundle export feature.
//!
//! [`RenderWorld`] is the persistent compile state — typst-kit's
//! [`FileStore`] backs the file/source caches, dependency tracking, and
//! the stale-source-reuse optimization. Long-lived instances (used by
//! `twyla serve`) keep their comemo cache across compiles; one-shot
//! callers (`build`, `check`, `render`) just construct a fresh world,
//! compile once, and drop it.

use std::fmt;
use std::path::PathBuf;

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

use crate::asset::{AssetResolver, ResolvedAsset};
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
    /// The bundle-relative output path, e.g., `guis-2/index.html`.
    pub path: PathBuf,
    /// Fully resolved HTML (placeholders already substituted).
    pub html: String,
}

/// A full compile: the routed pages plus every processed asset. `build` writes
/// both to disk; `serve` serves both from the latest compile.
#[derive(Debug, Clone)]
pub struct SiteOutput {
    pub docs: Vec<OutputDoc>,
    pub assets: Vec<ResolvedAsset>,
}

/// The full output of one compile: rendered pages, harvested per-page metadata,
/// and processed assets — all from the single eval.
type CompiledPages = (Vec<OutputDoc>, Vec<HarvestedDoc>, Vec<ResolvedAsset>);

/// Compile every page under `<root>/content/` as a single bundle.
///
/// One-shot wrapper: builds a fresh [`RenderWorld`], compiles, drops.
/// `twyla serve` skips this and reuses a long-lived world directly so
/// the comemo cache survives between requests.
pub fn render_site(ctx: &TwylaContext) -> Result<Vec<OutputDoc>, RenderError> {
    let world = RenderWorld::new(ctx)?;
    world.compile_bundle(&mut AssetResolver::new(ctx))
}

/// Like [`render_site`] but also returns the processed assets — what `build`
/// needs to emit `<dir>/assets/...` alongside the pages.
pub fn render_site_with_assets(ctx: &TwylaContext) -> Result<SiteOutput, RenderError> {
    let world = RenderWorld::new(ctx)?;
    world.compile_site(&mut AssetResolver::new(ctx))
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
fn plain_err(msg: impl Into<String>, kind: RenderErrorKind) -> RenderError {
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
fn terminal_color() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Render diagnostics to a string via typst-kit's emitter. With `color`, the
/// string carries ANSI escapes; otherwise it's plain text. Emitting into an
/// in-memory buffer only fails on encoding errors, which we don't expect.
fn emit_to_string(
    world: &dyn DiagnosticWorld,
    diags: &[SourceDiagnostic],
    color: bool,
) -> String {
    let mut buf = if color { Buffer::ansi() } else { Buffer::no_color() };
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
        install_stdlib(&mut library);

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
    /// Reads through `FileStore`, so repeated calls hit the comemo
    /// cache. Between compiles, call [`reset`](Self::reset) and
    /// `comemo::evict(..)` to invalidate; for content/ shape changes,
    /// also call [`refresh_main`](Self::refresh_main).
    pub fn compile_bundle(
        &self,
        resolver: &mut AssetResolver,
    ) -> Result<Vec<OutputDoc>, RenderError> {
        Ok(self.compile_bundle_with_meta(resolver)?.0)
    }

    /// Compile both pages and assets — what `build`/`serve` emit. The assets
    /// come from the same single eval as the pages.
    pub fn compile_site(&self, resolver: &mut AssetResolver) -> Result<SiteOutput, RenderError> {
        let (docs, _harvested, assets) = self.compile_bundle_with_meta(resolver)?;
        Ok(SiteOutput { docs, assets })
    }

    /// Like [`compile_bundle`](Self::compile_bundle) but also returns the
    /// per-page twyla `document` metadata and the processed [`ResolvedAsset`]s
    /// harvested/resolved during the *same* compile (one eval). Both come out
    /// of [`crate::compile::compile_bundle`].
    ///
    /// The `resolver` is caller-owned (not stored in the world): a one-shot
    /// caller passes a fresh one; `serve` reuses one across recompiles so its
    /// store persists. Passing it in keeps these methods `&self` — a field
    /// would force interior mutability, since `self` is also handed to typst as
    /// `&dyn World` for the duration of the compile.
    pub fn compile_bundle_with_meta(
        &self,
        resolver: &mut AssetResolver,
    ) -> Result<CompiledPages, RenderError> {
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

        let (bundle, harvested, assets) =
            output.map_err(|errors| compile_err(self, &errors))?;

        let mut docs = Vec::new();
        for (path, file) in bundle.files.iter() {
            let doc = match file {
                BundleFile::Document(BundleDocument::Html(doc)) => doc,
                _ => continue,
            };
            let raw = typst_html::html(doc).map_err(|errors| compile_err(self, &errors))?;
            docs.push(OutputDoc {
                path: PathBuf::from(path.get_without_slash()),
                html: resolve_raw_html_placeholders(&raw),
            });
        }

        if docs.is_empty() {
            return Err(plain_err(
                "bundle produced no HTML documents",
                RenderErrorKind::Bundle,
            ));
        }

        docs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok((docs, harvested, assets))
    }
}

/// Install twyla's native customizations into a freshly built library.
pub fn install_stdlib(library: &mut Library) {
    crate::rules::install(&mut library.rules);
    let global = library.global.scope_mut();
    crate::document::install(global);
    crate::asset::install(global);
    crate::content::install(global);
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
        let docs = render_site(&ctx).expect("render_site");
        let paths: Vec<_> = docs.iter().map(|d| d.path.clone()).collect();
        assert!(
            paths.contains(&PathBuf::from("guis-1/index.html")),
            "missing guis-1 in {paths:?}",
        );
        assert!(
            paths.contains(&PathBuf::from("guis-2/index.html")),
            "missing guis-2 in {paths:?}",
        );
        assert!(
            paths.contains(&PathBuf::from("guis-3/index.html")),
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
            let doc = docs
                .iter()
                .find(|d| d.path == PathBuf::from(path))
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
