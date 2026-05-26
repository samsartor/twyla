//! Render a typst entrypoint to an HTML string.
//!
//! The pipeline:
//!
//! 1. Build a [`RenderWorld`] rooted at the typst project directory.
//! 2. Compile as a [`Bundle`] (multi-document export). The bundle export
//!    feature is what unlocks `#document(path, ..)`.
//! 3. Filter the bundle for HTML documents; today only single-doc
//!    entrypoints are supported (the routing story is in
//!    `doc/index.typ` § Architecture).
//! 4. Run the resolution pass over the serialized HTML — see
//!    [`resolve_raw_html_placeholders`].
//!
//! Errors are formatted into pre-rendered strings so the caller doesn't
//! need to depend on `typst::diag` to print them.

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
    /// The bundle didn't contain exactly one HTML document.
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

/// Compile a typst entrypoint to a fully-resolved HTML string.
///
/// `root` is the typst project root (`/foo.typ` paths resolve against it).
/// `entrypoint` must live under `root` and be the file to compile.
///
/// Warnings are printed to stderr; only hard errors bail.
pub fn render_to_html(root: &Path, entrypoint: &Path) -> Result<String, RenderError> {
    let world = RenderWorld::new(root, entrypoint).map_err(|e| RenderError {
        messages: vec![e],
        kind: RenderErrorKind::Setup,
    })?;

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

    let html_docs: Vec<_> = bundle
        .files
        .iter()
        .filter_map(|(path, file)| match file {
            BundleFile::Document(BundleDocument::Html(doc)) => Some((path, doc)),
            _ => None,
        })
        .collect();

    let (_path, doc) = match html_docs.as_slice() {
        [one] => *one,
        [] => {
            return Err(RenderError {
                messages: vec!["bundle produced no HTML documents".to_string()],
                kind: RenderErrorKind::Bundle,
            });
        }
        many => {
            return Err(RenderError {
                messages: vec![format!(
                    "bundle produced {} HTML documents; only single-doc \
                     entrypoints are supported today",
                    many.len()
                )],
                kind: RenderErrorKind::Bundle,
            });
        }
    };

    let raw_html = typst_html::html(doc).map_err(|errors| RenderError {
        messages: errors
            .iter()
            .map(|e| format_diagnostic(&world, e))
            .collect(),
        kind: RenderErrorKind::Compile,
    })?;

    Ok(resolve_raw_html_placeholders(&raw_html))
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

struct RenderWorld {
    root: PathBuf,
    main_id: FileId,
    library: LazyHash<Library>,
    fonts: FontStore,
    sources: Mutex<HashMap<FileId, Source>>,
    files: Mutex<HashMap<FileId, Bytes>>,
}

impl RenderWorld {
    fn new(root: &Path, entrypoint: &Path) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|e| {
            format!("cannot canonicalize root {}: {e}", root.display())
        })?;
        let entrypoint = entrypoint.canonicalize().map_err(|e| {
            format!("cannot canonicalize entrypoint {}: {e}", entrypoint.display())
        })?;
        if !entrypoint.starts_with(&root) {
            return Err(format!(
                "entrypoint {} is not under root {}",
                entrypoint.display(),
                root.display()
            ));
        }

        let mut fonts = FontStore::new();
        fonts.extend(typst_kit::fonts::embedded());
        fonts.extend(typst_kit::fonts::system());

        let main_id = file_id_for(&root, &entrypoint)?;

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
            sources: Mutex::new(HashMap::new()),
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

fn file_id_for(root: &Path, path: &Path) -> Result<FileId, String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| format!("{} not under {}", path.display(), root.display()))?;
    let s = rel
        .to_str()
        .ok_or_else(|| format!("non-utf8 path: {}", rel.display()))?;
    let vpath = VirtualPath::new(format!("/{s}"))
        .map_err(|e| format!("invalid virtual path: {e:?}"))?;
    Ok(FileId::new(RootedPath::new(VirtualRoot::Project, vpath)))
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
}
