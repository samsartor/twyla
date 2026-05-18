//! `twyla-render-page [--root <dir>] <entrypoint.typ>`
//!
//! Compile a typst entrypoint as a `Bundle` (multi-document), find the
//! first emitted HTML document, run twyla's resolution pass over its
//! serialized output, and print to stdout.
//!
//! Project layout:
//!   --root  the typst project root. Typst absolute paths (`/foo.typ`)
//!           resolve against this. Defaults to the parent directory of
//!           the entrypoint, but that's usually wrong for any real
//!           project; pass `--root` explicitly.
//!
//! Resolution pass (see `doc/index.typ` § Resolution pass): every
//! `<script type="x-twyla-raw-html">..bytes..</script>` is replaced with
//! its inner text spliced inline — the workaround for the lack of a
//! first-class `html.raw` in typst.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use typst::diag::{FileError, FileResult, Warned};
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

fn main() -> ExitCode {
    let (root, entrypoint) = match parse_args() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    let world = match RenderWorld::new(&root, &entrypoint) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let Warned { output, warnings } = typst::compile::<Bundle>(&world);

    for w in &warnings {
        eprintln!("warning: {}", w.message);
    }

    let bundle = match output {
        Ok(b) => b,
        Err(errors) => {
            for e in &errors {
                eprintln!("error: {}", e.message);
            }
            return ExitCode::from(1);
        }
    };

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
            eprintln!("error: bundle produced no HTML documents");
            return ExitCode::from(1);
        }
        many => {
            eprintln!(
                "error: bundle produced {} HTML documents; only single-doc \
                 entrypoints are supported today",
                many.len()
            );
            return ExitCode::from(1);
        }
    };

    let raw_html = match typst_html::html(doc) {
        Ok(s) => s,
        Err(errors) => {
            for e in &errors {
                eprintln!("error: {}", e.message);
            }
            return ExitCode::from(1);
        }
    };

    let resolved = resolve_raw_html_placeholders(&raw_html);
    print!("{resolved}");
    ExitCode::from(0)
}

fn parse_args() -> Result<(PathBuf, PathBuf), String> {
    let mut root: Option<PathBuf> = None;
    let mut entrypoint: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => {
                let v = args.next().ok_or("--root requires a value")?;
                root = Some(PathBuf::from(v));
            }
            "-h" | "--help" => return Err(String::new()),
            _ if a.starts_with("--") => return Err(format!("unknown flag: {a}")),
            _ => {
                if entrypoint.is_some() {
                    return Err(format!("unexpected positional arg: {a}"));
                }
                entrypoint = Some(PathBuf::from(a));
            }
        }
    }

    let entrypoint = entrypoint.ok_or("missing <entrypoint.typ>")?;
    let entrypoint = entrypoint.canonicalize().map_err(|e| {
        format!("cannot canonicalize entrypoint {}: {e}", entrypoint.display())
    })?;

    let root = match root {
        Some(r) => r.canonicalize().map_err(|e| {
            format!("cannot canonicalize --root: {e}")
        })?,
        None => entrypoint
            .parent()
            .map(Path::to_path_buf)
            .ok_or("entrypoint has no parent directory")?,
    };

    if !entrypoint.starts_with(&root) {
        return Err(format!(
            "entrypoint {} is not under --root {}",
            entrypoint.display(),
            root.display()
        ));
    }

    Ok((root, entrypoint))
}

fn print_usage() {
    eprintln!("usage: twyla-render-page [--root <dir>] <entrypoint.typ>");
}

/// Replace every `<script type="x-twyla-raw-html">..</script>` with its
/// inner body. See module doc for context.
fn resolve_raw_html_placeholders(input: &str) -> String {
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
        let mut fonts = FontStore::new();
        fonts.extend(typst_kit::fonts::embedded());
        fonts.extend(typst_kit::fonts::system());

        let main_id = file_id_for(root, entrypoint)?;

        Ok(Self {
            root: root.to_path_buf(),
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
