//! Render typst content to HTML via the bundle export feature.
//!
//! Every compile goes through the same shape: twyla generates a virtual
//! `main.typ` in memory containing one `#document(path, body)` call per
//! routed page, then compiles that as a bundle and walks the HTML
//! documents out.
//!
//! Two public entry points:
//!
//! - [`render_site`] — scan `<root>/content/*.typ`, emit one `#document`
//!   per file, compile, return every routed HTML.
//! - [`render_slug`] — same machinery, single-entry virtual main. Used
//!   by `cmd_check`, `cmd_render`, and the dev server's per-request
//!   compile.
//!
//! Per-document routing context (the URL slug page-template needs for
//! the anchor-absolutize link rule) is injected as a `<twyla-page>`
//! metadata label, queryable from within each document's content.
//!
//! After typst-html serializes, the resolution pass replaces every
//! `<script type="x-twyla-raw-html">..</script>` with its inner body —
//! the workaround for the missing `html.raw` primitive.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use typst::diag::{FileError, FileResult, SourceDiagnostic, Warned};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_bundle::{Bundle, BundleDocument, BundleFile};
use typst_kit::fonts::FontStore;
use typst_library::Feature;

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

/// Compile every page under `<root>/content/`.
///
/// Top-level `*.typ` files map to slugs by filename. `_index.typ` and
/// any other underscore-prefixed file are skipped today (drafts /
/// section-index need design; not in revision-1 scope).
pub fn render_site(root: &Path) -> Result<Vec<RoutedDoc>, RenderError> {
    let slugs = scan_pages(root).map_err(setup_err)?;
    if slugs.is_empty() {
        return Err(setup_err(format!(
            "no pages found under {}/content/",
            root.display()
        )));
    }
    let main_src = generate_main(&slugs);
    render_virtual_main(root, &main_src)
}

/// Compile a single page identified by slug. Slug must correspond to
/// `<root>/content/<slug>.typ`.
pub fn render_slug(root: &Path, slug: &str) -> Result<RoutedDoc, RenderError> {
    let main_src = generate_main(&[slug.to_string()]);
    let mut docs = render_virtual_main(root, &main_src)?;
    if docs.len() != 1 {
        return Err(RenderError {
            messages: vec![format!(
                "expected exactly 1 HTML document for slug {slug:?}, got {}",
                docs.len()
            )],
            kind: RenderErrorKind::Bundle,
        });
    }
    Ok(docs.pop().unwrap())
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

/// Discover routed pages from the filesystem layout. Top-level
/// `content/*.typ` only — no subdirectory recursion yet (the corpus
/// doesn't need it).
///
/// Returns slugs (filename without extension). `_index.typ`, drafts,
/// and any other underscore-prefixed file are skipped.
pub fn scan_pages(root: &Path) -> Result<Vec<String>, String> {
    let content_dir = root.join("content");
    let entries = std::fs::read_dir(&content_dir)
        .map_err(|e| format!("cannot read {}: {e}", content_dir.display()))?;
    let mut slugs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("scan error: {e}"))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("typ") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.starts_with('_') {
            continue;
        }
        slugs.push(stem.to_string());
    }
    slugs.sort();
    Ok(slugs)
}

/// Synthesize the virtual `main.typ` body.
///
/// Each slug becomes one `#document(<slug>/index.html, ..)` call. The
/// body opens with a `<twyla-page>`-labelled `metadata((url-path: ..))`
/// so the included file's page-template can read its own routed slug
/// via a per-document `query(<twyla-page>)`.
fn generate_main(slugs: &[String]) -> String {
    let mut out = String::new();
    out.push_str(
        "// Generated by twyla. Each #document() emits one routed HTML\n\
         // file; the <twyla-page> metadata exposes the slug to the\n\
         // included page-template.\n\n",
    );
    for slug in slugs {
        // The metadata-label syntax is markup-mode (`#metadata(..) <label>`),
        // so the document body uses a `[..]` content block.
        out.push_str(&format!(
            "#document(\"{slug}/index.html\")[\n  \
             #metadata((url-path: \"{slug}\")) <twyla-page>\n  \
             #include \"/content/{slug}.typ\"\n]\n\n",
        ));
    }
    out
}

fn render_virtual_main(
    root: &Path,
    main_src: &str,
) -> Result<Vec<RoutedDoc>, RenderError> {
    let world = RenderWorld::new(root, main_src).map_err(setup_err)?;

    let Warned { output, warnings } = typst::compile::<Bundle>(&world);

    for w in &warnings {
        eprintln!("warning: {}", w.message);
    }

    let bundle = output.map_err(|errors| RenderError {
        messages: errors
            .iter()
            .map(|e| format_diagnostic(&world, e))
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
                .map(|e| format_diagnostic(&world, e))
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

fn setup_err(msg: impl Into<String>) -> RenderError {
    RenderError {
        messages: vec![msg.into()],
        kind: RenderErrorKind::Setup,
    }
}

struct RenderWorld {
    root: PathBuf,
    main_id: FileId,
    library: LazyHash<Library>,
    fonts: FontStore,
    sources: Mutex<HashMap<FileId, Source>>,
    files: Mutex<HashMap<FileId, Bytes>>,
}

impl RenderWorld {
    /// `root` is the typst project root (where `/foo.typ` resolves);
    /// `virtual_main_src` is the synthetic main.typ body that drives
    /// the bundle compile.
    fn new(root: &Path, virtual_main_src: &str) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|e| {
            format!("cannot canonicalize root {}: {e}", root.display())
        })?;

        let mut fonts = FontStore::new();
        fonts.extend(typst_kit::fonts::embedded());
        fonts.extend(typst_kit::fonts::system());

        let vpath = VirtualPath::new(VIRTUAL_MAIN_VPATH)
            .map_err(|e| format!("invalid virtual main path: {e:?}"))?;
        let main_id =
            FileId::new(RootedPath::new(VirtualRoot::Project, vpath));
        let main_source = Source::new(main_id, virtual_main_src.to_string());
        let mut sources = HashMap::new();
        sources.insert(main_id, main_source);

        Ok(Self {
            root,
            main_id,
            library: LazyHash::new(
                Library::builder()
                    .with_features(
                        [Feature::Html, Feature::Bundle].into_iter().collect(),
                    )
                    .build(),
            ),
            fonts,
            sources: Mutex::new(sources),
            files: Mutex::new(HashMap::new()),
        })
    }

    fn read_disk(&self, id: FileId) -> FileResult<Vec<u8>> {
        let rel = id.vpath().get_without_slash();
        let on_disk = self.root.join(rel);
        std::fs::read(&on_disk).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => FileError::NotFound(on_disk),
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
        if let Some(s) = self.sources.lock().unwrap().get(&id) {
            return Ok(s.clone());
        }
        let bytes = self.read_disk(id)?;
        let text = String::from_utf8(bytes).map_err(|e| {
            FileError::Other(Some(format!("non-utf8 source file: {e}").into()))
        })?;
        let source = Source::new(id, text);
        self.sources.lock().unwrap().insert(id, source.clone());
        Ok(source)
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if let Some(b) = self.files.lock().unwrap().get(&id) {
            return Ok(b.clone());
        }
        let bytes = Bytes::new(self.read_disk(id)?);
        self.files.lock().unwrap().insert(id, bytes.clone());
        Ok(bytes)
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

    #[test]
    fn generate_main_emits_one_document_per_slug() {
        let slugs = vec!["guis-1".to_string(), "guis-2".to_string()];
        let src = generate_main(&slugs);
        assert!(src.contains("#document(\"guis-1/index.html\")"));
        assert!(src.contains("#document(\"guis-2/index.html\")"));
        assert!(src.contains("<twyla-page>"));
        assert!(src.contains("#include \"/content/guis-1.typ\""));
    }

    /// Exercises the multi-document case end-to-end: scan content/,
    /// compile every page in one bundle, verify each routed doc lands
    /// at the expected path. Catches metadata-label scope leaks between
    /// documents — the per-doc `query(<twyla-page>)` in page-template
    /// would otherwise return wrong values for all but one slug.
    ///
    /// Opt-in via `cargo test -- --ignored` because it depends on the
    /// site repo being present at `$SITE_ROOT` / `~/Src/site`.
    #[test]
    #[ignore]
    fn render_site_smoke() {
        let site = std::env::var_os("SITE_ROOT")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| {
                    let mut p = PathBuf::from(h);
                    p.push("Src/site");
                    p
                })
            })
            .expect("SITE_ROOT or HOME");
        let docs = render_site(&site).expect("render_site");
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
        // Per-document `<twyla-page>` scoping: each doc's anchor-only
        // links should absolutize against ITS slug, not another's.
        for doc in &docs {
            let slug = doc.path.parent().unwrap().to_str().unwrap();
            let foreign_anchor = format!(
                "samsartor.com/{}/#",
                if slug == "guis-1" { "guis-2" } else { "guis-1" },
            );
            if doc.html.contains(&format!("samsartor.com/{slug}/#")) {
                // Has anchor-only links — must NOT also contain a
                // foreign-slug anchor URL.
                assert!(
                    !doc.html.contains(&foreign_anchor),
                    "{slug} contains foreign anchor URL — label scope leak",
                );
            }
        }
    }
}
