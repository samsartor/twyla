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
use typst_kit::downloader::SystemDownloader;
use typst_kit::files::{FileLoader, FileStore};
use typst_kit::fonts::FontStore;
use typst_kit::packages::SystemPackages;
use typst_library::Feature;

use crate::asset::{AssetResolver, ResolvedAsset};
use crate::compile::HarvestedDoc;
use crate::project::TwylaContext;

/// Twyla's emit-raw-HTML placeholder. Matches the helper in user typst
/// code: `raw-html(content)` produces a `<script>` with this type, and the
/// resolution pass below replaces each instance with the script body
/// inlined.
const RAW_HTML_MARKER: &str = r#"<script type="x-twyla-raw-html">"#;

/// Pre-formatted errors. Caller prints these on stderr and exits.
#[derive(Debug)]
pub struct RenderError {
    pub messages: Vec<String>,
    pub kind: RenderErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderErrorKind {
    /// World setup, project layout, or path resolution failed.
    Setup,
    /// Typst compilation failed. `messages` holds the diagnostics.
    Compile,
    /// The bundle didn't contain the expected HTML documents.
    Bundle,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, m) in self.messages.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{m}")?;
        }
        Ok(())
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
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(start) = input[cursor..].find(RAW_HTML_MARKER) {
        let abs = cursor + start;
        let body_start = abs + RAW_HTML_MARKER.len();
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

fn setup_err(msg: impl Into<String>) -> RenderError {
    RenderError {
        messages: vec![msg.into()],
        kind: RenderErrorKind::Setup,
    }
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

        // Expose project config to typst via `sys.inputs`. Today: just
        // `base_url` (empty string when unset — site templates read it
        // with a fallback so relative URLs work out of the box on
        // localhost). Add more keys here as the config layer grows.
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
        crate::prelude::install(&mut library);

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
    pub fn compile_bundle(&self, resolver: &mut AssetResolver) -> Result<Vec<OutputDoc>, RenderError> {
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
        for w in &warnings {
            eprintln!("warning: {}", w.message);
        }

        let (bundle, harvested, assets) = output.map_err(|errors| RenderError {
            messages: errors.iter().map(|e| format_diagnostic(self, e)).collect(),
            kind: RenderErrorKind::Compile,
        })?;

        let mut docs = Vec::new();
        for (path, file) in bundle.files.iter() {
            let doc = match file {
                BundleFile::Document(BundleDocument::Html(doc)) => doc,
                _ => continue,
            };
            let raw = typst_html::html(doc).map_err(|errors| RenderError {
                messages: errors.iter().map(|e| format_diagnostic(self, e)).collect(),
                kind: RenderErrorKind::Compile,
            })?;
            docs.push(OutputDoc {
                path: PathBuf::from(path.get_without_slash()),
                html: resolve_raw_html_placeholders(&raw),
            });
        }

        if docs.is_empty() {
            return Err(RenderError {
                messages: vec!["bundle produced no HTML documents".to_string()],
                kind: RenderErrorKind::Bundle,
            });
        }

        docs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok((docs, harvested, assets))
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

/// Format a typst diagnostic with the file path and line/col of the
/// reported span — the default `e.message` omits both, which makes the
/// "expected string, found content" kind of message hard to act on.
fn format_diagnostic(world: &dyn World, e: &SourceDiagnostic) -> String {
    let span = e.span;
    let Some(id) = span.id() else {
        return format!("error: {}", e.message);
    };
    let Ok(src) = world.source(id) else {
        return format!("error: {}", e.message);
    };
    let Some(range) = src.range(span) else {
        return format!("error: {}", e.message);
    };
    // line/col of the start of the range.
    let (line, col) = src
        .lines()
        .byte_to_line_column(range.start)
        .unwrap_or((0, 0));
    let path = id.vpath().get_without_slash();
    format!("error: {} ({}:{}:{})", e.message, path, line + 1, col + 1)
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
