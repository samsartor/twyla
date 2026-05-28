//! Render typst content to HTML via the bundle export feature.
//!
//! Every compile goes through the same shape: twyla generates a virtual
//! `main.typ` in memory containing one `#document(path, body)` call per
//! routed page, then compiles that as a bundle and walks the HTML
//! documents out.
//!
//! [`RenderWorld`] is the persistent compile state — typst-kit's
//! [`FileStore`] backs the file/source caches, dependency tracking, and
//! the stale-source-reuse optimization. Long-lived instances (used by
//! `twyla serve`) keep their comemo cache across compiles; one-shot
//! callers (`build`, `check`, `render`) just construct a fresh world,
//! compile once, and drop it.
//!
//! Two public one-shot entry points:
//!
//! - [`render_site`] — scan `<root>/content/*.typ`, compile the
//!   multi-document bundle, return every routed HTML.
//! - [`render_slug`] — same compile, filter the bundle down to a
//!   single doc. (`render_slug` runs the *full* bundle because
//!   cross-doc labels, queries, and home-page enumeration only work
//!   when every page is present.)
//!
//! After typst-html serializes, the resolution pass replaces every
//! `<script type="x-twyla-raw-html">..</script>` with its inner body —
//! the workaround for the missing `html.raw` primitive.

use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;

use typst::diag::{FileError, FileResult, SourceDiagnostic, Warned};
use typst::foundations::{Bytes, Datetime, Dict, Duration, IntoValue};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_bundle::{Bundle, BundleDocument, BundleFile};
use typst_kit::files::{FileLoader, FileStore};
use typst_kit::fonts::FontStore;
use typst_library::Feature;

use crate::project::TwylaContext;

/// Twyla's emit-raw-HTML placeholder. Matches the helper in user typst
/// code: `raw-html(content)` produces a `<script>` with this type, and the
/// resolution pass below replaces each instance with the script body
/// inlined.
const RAW_HTML_MARKER: &str = r#"<script type="x-twyla-raw-html">"#;

/// Virtual path for the generated main. Lives only in the in-memory
/// source map — never read from disk.
const VIRTUAL_MAIN_VPATH: &str = "/__twyla_main.typ";

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
pub struct RoutedDoc {
    /// The bundle-relative output path, e.g., `guis-2/index.html`.
    pub path: PathBuf,
    /// Fully resolved HTML (placeholders already substituted).
    pub html: String,
}

/// Compile every page under `<root>/content/` as a single bundle.
///
/// One-shot wrapper: builds a fresh [`RenderWorld`], compiles, drops.
/// `twyla serve` skips this and reuses a long-lived world directly so
/// the comemo cache survives between requests.
pub fn render_site(ctx: &TwylaContext) -> Result<Vec<RoutedDoc>, RenderError> {
    let world = RenderWorld::new(ctx)?;
    world.compile_bundle()
}

/// Compile the full bundle and return the doc routed at `<slug>`.
///
/// The compile is multi-document even when the caller wants one slug,
/// because typst-html resolves cross-doc `link(<label>)`, `query(..)`
/// across the bundle, and the home page's `query(<twyla-post>)`
/// enumeration only sees siblings if they're present. Filtering to one
/// doc happens after compile, not before.
pub fn render_slug(
    ctx: &TwylaContext,
    slug: &str,
) -> Result<RoutedDoc, RenderError> {
    let world = RenderWorld::new(ctx)?;
    let mut docs = world.compile_bundle()?;
    let target = ctx.default_route(slug).bundle_path;
    let idx = docs.iter().position(|d| d.path == target).ok_or_else(|| {
        RenderError {
            messages: vec![format!(
                "bundle did not contain a document for slug {slug:?} \
                 (expected path {target:?})"
            )],
            kind: RenderErrorKind::Bundle,
        }
    })?;
    Ok(docs.swap_remove(idx))
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

/// Synthesize the virtual `main.typ` body.
///
/// Each slug becomes one `#document(<path>, ..)` call wrapping a
/// `#include` of the content file. Per-document routing context
/// (the current page's slug, title, date, etc.) flows in via the
/// template's `set-page(..)` call inside the included body — the
/// template publishes its dict into a typst `state` (per-doc lookup)
/// and a `<twyla-post>` metadata label (cross-doc enumeration).
/// No twyla-side metadata is injected here.
fn generate_main(ctx: &TwylaContext, slugs: &[String]) -> String {
    let mut out = String::new();
    out.push_str(
        "// Generated by twyla. Each #document() emits one routed HTML\n\
         // file by including the matching content/*.typ source. The\n\
         // included file's page-template handles per-doc state via\n\
         // base.typ's set-page() helper.\n\n",
    );
    for slug in slugs {
        let route = ctx.default_route(slug);
        out.push_str(&format!(
            "#document(\"{path}\")[\n  \
             #include \"/content/{slug}.typ\"\n]\n\n",
            path = route.bundle_path.display(),
        ));
    }
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
    ctx: TwylaContext,
    main_id: FileId,
    library: LazyHash<Library>,
    fonts: FontStore,
    files: FileStore<TwylaLoader>,
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

        let vpath = VirtualPath::new(VIRTUAL_MAIN_VPATH)
            .map_err(|e| setup_err(format!("invalid virtual main path: {e:?}")))?;
        let main_id =
            FileId::new(RootedPath::new(VirtualRoot::Project, vpath));

        let slugs = ctx.scan_pages().map_err(setup_err)?;
        let main_bytes = Bytes::new(generate_main(&ctx, &slugs).into_bytes());

        let loader = TwylaLoader {
            root: ctx.root.clone(),
            main_id,
            main_bytes: Mutex::new(main_bytes),
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

    /// The context this world was built from. Useful for callers that
    /// need to reach project-layout state (paths, base_url) without
    /// threading the context separately.
    pub fn ctx(&self) -> &TwylaContext {
        &self.ctx
    }

    /// Re-scan `<root>/content/` and update the virtual main bytes to
    /// match the current slug set. Call after any file
    /// addition/removal under `content/` so the next compile picks up
    /// the new layout.
    ///
    /// Returns the new slug list so callers can log it. Errors from
    /// `read_dir` (e.g., content/ vanished mid-session) propagate; the
    /// main is left at its previous bytes if the scan fails.
    pub fn refresh_main(&mut self) -> Result<Vec<String>, String> {
        let slugs = self.ctx.scan_pages()?;
        let bytes = Bytes::new(generate_main(&self.ctx, &slugs).into_bytes());
        *self.files.loader().main_bytes.lock().unwrap() = bytes;
        Ok(slugs)
    }

    /// Mark every cached file slot stale. The next [`compile_bundle`]
    /// call will reload affected files from disk; FileStore reuses the
    /// existing Source object for in-place reparse where bytes match.
    ///
    /// Pair with `comemo::evict(N)` to also age out memoized compile
    /// results — see `typst-cli/src/watch.rs` for the canonical
    /// invocation.
    pub fn reset(&mut self) {
        self.files.reset();
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
    pub fn compile_bundle(&self) -> Result<Vec<RoutedDoc>, RenderError> {
        let Warned { output, warnings } = typst::compile::<Bundle>(self);

        for w in &warnings {
            eprintln!("warning: {}", w.message);
        }

        let bundle = output.map_err(|errors| RenderError {
            messages: errors
                .iter()
                .map(|e| format_diagnostic(self, e))
                .collect(),
            kind: RenderErrorKind::Compile,
        })?;

        let mut docs = Vec::new();
        for (path, file) in bundle.files.iter() {
            let doc = match file {
                BundleFile::Document(BundleDocument::Html(doc)) => doc,
                _ => continue,
            };
            let raw = typst_html::html(doc).map_err(|errors| RenderError {
                messages: errors
                    .iter()
                    .map(|e| format_diagnostic(self, e))
                    .collect(),
                kind: RenderErrorKind::Compile,
            })?;
            docs.push(RoutedDoc {
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
        Ok(docs)
    }
}

/// FileLoader for [`FileStore`]. Serves the virtual main from an
/// in-memory `Mutex<Bytes>` (updatable via
/// [`RenderWorld::refresh_main`]) and every other id from the on-disk
/// project root.
struct TwylaLoader {
    root: PathBuf,
    main_id: FileId,
    main_bytes: Mutex<Bytes>,
}

impl TwylaLoader {
    /// Resolve a file id to its on-disk path. Returns
    /// [`FileError::NotFound`] for the virtual main (no real path) so
    /// callers iterating dependencies can `filter_map` it away.
    /// Package ids (typst-universe etc.) are also rejected — twyla
    /// doesn't depend on any packages yet.
    fn resolve(&self, id: FileId) -> FileResult<PathBuf> {
        if id == self.main_id {
            return Err(FileError::NotFound(PathBuf::from(VIRTUAL_MAIN_VPATH)));
        }
        let vpath = id.vpath();
        match id.root() {
            VirtualRoot::Project => Ok(self.root.join(vpath.get_without_slash())),
            VirtualRoot::Package(_) => {
                Err(FileError::NotFound(PathBuf::from(vpath.get_without_slash())))
            }
        }
    }
}

impl FileLoader for TwylaLoader {
    fn load(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.main_id {
            return Ok(self.main_bytes.lock().unwrap().clone());
        }
        let path = self.resolve(id)?;
        std::fs::read(&path).map(Bytes::new).map_err(|e| match e.kind() {
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
    format!(
        "error: {} ({}:{}:{})",
        e.message,
        path,
        line + 1,
        col + 1
    )
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

    fn test_ctx() -> TwylaContext {
        TwylaContext {
            root: PathBuf::from("/tmp"),
            base_url: None,
        }
    }

    #[test]
    fn generate_main_emits_one_document_per_slug() {
        let ctx = test_ctx();
        let slugs = vec!["guis-1".to_string(), "guis-2".to_string()];
        let src = generate_main(&ctx, &slugs);
        assert!(src.contains("#document(\"guis-1/index.html\")"));
        assert!(src.contains("#document(\"guis-2/index.html\")"));
        assert!(src.contains("#include \"/content/guis-1.typ\""));
    }

    #[test]
    fn generate_main_routes_home_to_bundle_root() {
        let ctx = test_ctx();
        let slugs = vec!["_index".to_string(), "guis-1".to_string()];
        let src = generate_main(&ctx, &slugs);
        assert!(src.contains("#document(\"index.html\")"));
        assert!(src.contains("#include \"/content/_index.typ\""));
        assert!(src.contains("#document(\"guis-1/index.html\")"));
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
                &[
                    "href=\"#streams\"",
                    "href=\"#stream-based-reactivity\"",
                ],
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
