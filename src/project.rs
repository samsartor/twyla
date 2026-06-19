//! Project layout and configuration.
//!
//! [`TwylaContext`] is the single source of truth for "where things live
//! in this project" (root, content/, static/, templates/, output/) and
//! "what knobs is the user passing in" (base_url, future config).
//! Construct one at command entry; pass it down to `render`, `serve`,
//! `build`, etc.
//!
//! The struct stays plain so future `serde::Deserialize` from
//! `twyla.toml` slots in without restructuring callers. Today, all
//! fields come from CLI args ([`ContextArgs`] in `main`).

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{asset, render::Output};

/// Everything a twyla command needs to operate on a project. Cheap to
/// clone — paths are owned `PathBuf`s, no heavy state.
#[derive(Debug, Clone)]
pub struct TwylaContext {
    /// Project root, canonicalized at construction. Every derived path
    /// hangs off this.
    pub root: PathBuf,
    /// Optional base URL for absolute-link rewriting (`cmd_check` uses
    /// this; future typst-side injection will too). `None` means the
    /// project hasn't declared one yet — callers that need it must
    /// error rather than guess.
    pub base_url: Option<String>,
    /// Expose the `twyla-reflect` reflection module to documents (the
    /// `--reflect` / `TWYLA_REFLECT` opt-in). Lets twyla's own reference site
    /// introspect twyla's native builtins; off for normal site builds. See
    /// [`crate::reflect`].
    pub reflect: bool,
    /// Compile every `typ`/`example` code block in the site as a throwaway
    /// one-page site and fail the build if any don't compile (the
    /// `--test-examples` / `TWYLA_TEST_EXAMPLES` opt-in). See [`crate::examples`].
    pub test_examples: bool,
}

/// Build a document's public URL from the site `base_url` and its bundle output
/// path: strip a trailing `index.html` (directory-style URLs), then join under
/// `base_url` (absolute) or `/` (relative). Pure function of its inputs, so it
/// backs both [`TwylaContext::document_url`] and `document.url()`'s on-chain
/// computation (which has `base_url` but no `TwylaContext`).
pub(crate) fn build_document_url(base_url: Option<&str>, output: &str) -> String {
    let path = output.strip_suffix("index.html").unwrap_or(output);
    match base_url {
        Some(base) => format!(
            "{}/{}",
            base.trim_end_matches('/'),
            path.trim_start_matches('/')
        ),
        None => format!("/{}", path.trim_start_matches('/')),
    }
}

/// Present a stored (no-leading-slash) output path to user typst as a
/// root-absolute string (`blog/index.html` → `/blog/index.html`). Internally
/// every output path is stored, compared, routed, and written without a leading
/// slash (a leading `/` makes `Path::join`/`strip_prefix` treat it as
/// OS-absolute); the slash is re-attached *only* at the user-presentation
/// boundary — the `document.output` field and the `output` field of each
/// `documents()` entry — mirroring how a URL is root-absolute. Idempotent if a
/// slash is already present.
pub(crate) fn present_output(output: &str) -> String {
    format!("/{}", output.trim_start_matches('/'))
}

/// Normalize a relative path into a `/`-separated, no-leading-slash manifest
/// key — the form [`crate::html::resolve`] and the output manifests share.
pub(crate) fn path_key(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

impl TwylaContext {
    /// Build a context rooted at `root`. Canonicalizes once so all
    /// downstream comparisons are against the resolved path. Trims
    /// trailing slashes from `base_url` so concatenation
    /// (`base_url + "/" + path`) never produces `//`.
    pub fn new(root: impl AsRef<Path>, base_url: Option<String>) -> Result<Self, String> {
        let root = root.as_ref();
        let root = root
            .canonicalize()
            .map_err(|e| format!("cannot canonicalize root {}: {e}", root.display()))?;
        let base_url = base_url.map(|s| s.trim_end_matches('/').to_string());
        Ok(Self {
            root,
            base_url,
            reflect: false,
            test_examples: false,
        })
    }

    /// A bare context for unit tests: fields take their defaults and `root` is
    /// used verbatim (no canonicalization, so it needn't exist on disk). Adding
    /// a field updates only this — not every test. Set `base_url` on the
    /// returned value when a test needs it.
    #[cfg(test)]
    pub(crate) fn stub(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            base_url: None,
            reflect: false,
            test_examples: false,
        }
    }

    /// `<root>/content/` — typst source files routed into the bundle.
    pub fn content_dir(&self) -> PathBuf {
        self.root.join("content")
    }

    /// `<root>/static/` — copied verbatim into the build output and
    /// served as-is by the dev server.
    pub fn static_dir(&self) -> PathBuf {
        self.root.join("static")
    }

    /// Default output directory for `twyla build`. `<root>/public/`,
    /// matching zola so a coexisting workflow doesn't surprise anyone.
    /// `twyla build -o <dir>` overrides at the CLI layer; this is just
    /// the default.
    pub fn default_output_dir(&self) -> PathBuf {
        self.root.join("public")
    }

    /// `base_url` if set, or a descriptive error suitable for the user
    /// to fix on the CLI. Use this in callers that genuinely need it
    /// (`cmd_check`, future `asset-url` resolver) rather than threading
    /// `Option` further.
    pub fn require_base_url(&self) -> Result<&str, String> {
        self.base_url.as_deref().ok_or_else(|| {
            "base URL is required for this command — pass --base-url \
             or set TWYLA_BASE_URL"
                .to_string()
        })
    }

    /// Discover routed pages from the filesystem layout. Recurses
    /// `content/` and collects every `*.typ` whose stem doesn't start
    /// with `_` (the private/partial convention). Subdirectories route
    /// through [`default_document_output`](Self::default_document_output)
    /// (`foo/bar.typ` → `foo/bar/index.html`, `foo/main.typ` →
    /// `foo/index.html`).
    pub fn scan_pages(&self) -> Result<Vec<PathBuf>, String> {
        let mut paths = Vec::new();
        scan_pages_into(&self.content_dir(), &mut paths)?;
        paths.sort();
        Ok(paths)
    }

    /// Every file under `static/` as an [`Output::Static`], keyed by its
    /// root-relative `/`-separated path (`static/site.css` → `site.css`).
    /// A missing `static/` yields nothing.
    pub fn static_outputs(&self) -> Result<impl Iterator<Item = Output>, String> {
        let mut paths = Vec::new();
        let static_dir = self.static_dir();
        if static_dir.is_dir() {
            scan_all_into(&static_dir, &mut paths)?;
        }
        Ok(paths.into_iter().filter_map(move |path| {
            let rel = path.strip_prefix(&static_dir).ok()?;
            Some(Output::Static(path_key(rel), path))
        }))
    }

    /// Bundle-relative directory compiled HTML files are emitted into, e.g.
    /// `somepost/index.html`. Joined under the build output dir on disk and
    /// exposed at the matching root-relative URL ([`document_url`](Self::document_url)).
    pub fn default_document_output(&self, source: &str) -> String {
        // A leading `/` makes the vpath OS-absolute to `Path::join`, which would
        // discard `root`; strip it so it stays root-relative (mirrors
        // [`resolve_document_output`](Self::resolve_document_output)).
        let source = self.root.join(source.trim_start_matches('/'));
        let content_dir = self.content_dir();
        let Ok(path) = source.strip_prefix(&content_dir) else {
            panic!(
                "source {:?} is not within the content dir {:?}",
                source.display(),
                content_dir.display()
            );
        };
        if path.file_stem().is_some_and(|s| s == "main") {
            match path.parent() {
                Some(parent) => parent.join("index.html").to_str().unwrap().to_owned(),
                None => "index.html".to_owned(),
            }
        } else {
            path.with_extension("")
                .join("index.html")
                .to_str()
                .unwrap()
                .to_owned()
        }
    }

    /// The project's single source of truth for resolving an explicit
    /// `document(output: ..)` string to a bundle-relative output path
    /// (`/`-separated, no leading slash — the [`Output`] map key form).
    ///
    /// A leading `/` means **bundle-root-absolute** (`"/feed.xml"` → `feed.xml`),
    /// matching Typst's own path idiom; the `anchor` is then irrelevant.
    /// Otherwise the path is **relative** to `anchor` — a bundle-relative
    /// `/`-separated directory with no surrounding slashes (`Some("")` is the
    /// root). `.`/`..` segments are normalized; `..` above the root, or a path
    /// that normalizes to empty, is an error.
    ///
    /// `anchor` is `None` when the caller has no base to resolve a *relative*
    /// path against yet — an inline document realized before its enclosing page
    /// output is known (the discarded metadata-harvest pass). A relative `raw`
    /// then yields `Ok(None)` and the caller substitutes a placeholder / skips;
    /// an absolute `raw` ignores the missing anchor and still resolves.
    ///
    /// Associated (no `self`): the two anchors are derived elsewhere — the
    /// source's folder stem ([`resolve_document_output`](Self::resolve_document_output))
    /// and the parent page's output dir (on the style chain, in
    /// [`crate::document`]) — but the `/`-vs-relative convention lives only here.
    pub(crate) fn resolve_output(
        anchor: Option<&str>,
        raw: &str,
    ) -> Result<Option<String>, String> {
        // Absolute paths discard the anchor; relative ones need one or defer.
        let segments: Vec<&str> = match raw.strip_prefix('/') {
            Some(abs) => abs.split('/').collect(),
            None => match anchor {
                Some(a) => a.split('/').chain(raw.split('/')).collect(),
                None => return Ok(None),
            },
        };
        let mut stack: Vec<&str> = Vec::new();
        for seg in segments {
            match seg {
                "" | "." => {}
                ".." => {
                    if stack.pop().is_none() {
                        return Err(format!("output path `{raw}` escapes the site root"));
                    }
                }
                s => stack.push(s),
            }
        }
        if stack.is_empty() {
            return Err(format!("output path `{raw}` resolves to an empty path"));
        }
        Ok(Some(stack.join("/")))
    }

    /// Resolve an explicit `document(output: ..)` set on a full-file page to its
    /// bundle output path, anchoring relative paths on the source's **folder
    /// stem** below `content/` (`content/blog/post.typ` → `blog/`), so
    /// `output: "extra.html"` lands at `blog/extra.html` and `output: "/x.html"`
    /// at the root. Delegates the convention to [`resolve_output`](Self::resolve_output).
    ///
    /// `source` is the root-relative source path (`content/...`), as passed to
    /// [`default_document_output`](Self::default_document_output).
    pub fn resolve_document_output(&self, source: &str, raw: &str) -> Result<String, String> {
        let source = self.root.join(source.trim_start_matches('/'));
        let content_dir = self.content_dir();
        let rel = source.strip_prefix(&content_dir).map_err(|_| {
            format!(
                "source {:?} is not within the content dir",
                source.display()
            )
        })?;
        let anchor = rel.parent().map(path_key).unwrap_or_default();
        // A full-file page always has a concrete folder-stem anchor, so
        // resolution never defers (`Ok(None)`) — that path is inline-only.
        Self::resolve_output(Some(&anchor), raw)
            .map(|out| out.expect("full-file output always has a concrete anchor"))
    }

    /// The default `kind` for a page, used when it doesn't set `document.kind`.
    /// Drives both the harvested metadata (in [`crate::compile`]) and the
    /// `convert` draft's `{kind}-template` import.
    ///
    /// Keyed on the *source* path (root- or content-relative `.typ`), because
    /// the discriminator is the filename — `main.typ` is a directory index, a
    /// leaf file is a page — which the output path (always `…/index.html`)
    /// would erase:
    /// - `content/main.typ` → `root` (the site index)
    /// - `content/<dir>/main.typ` → `dir` (a section index)
    /// - anything else → `page`
    pub fn default_kind(&self, source: &str) -> String {
        // The source vpath may carry a leading `/` (`/content/main.typ`); strip
        // it before the `content/` prefix so the index/section/leaf test works.
        let source = source.trim_start_matches('/');
        let rel = source.strip_prefix("content/").unwrap_or(source);
        let path = Path::new(rel);
        if path.file_stem().and_then(|s| s.to_str()) == Some("main") {
            match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => "dir".to_string(),
                _ => "root".to_string(),
            }
        } else {
            "page".to_string()
        }
    }

    pub fn document_url(&self, output: &str) -> String {
        build_document_url(self.base_url.as_deref(), output)
    }

    /// Bundle-relative directory processed assets are emitted into, e.g.
    /// `assets/main-<hash>.css`. Joined under the build output dir on disk and
    /// exposed at the matching root-relative URL ([`asset_url`](Self::asset_url)).
    pub fn default_asset_output(&self, built: &asset::Built) -> String {
        let hex = built.sha256_hex();
        let fingerprint = &hex[..32];
        match &built.stem {
            Some(stem) => format!(
                "assets/{}-{}.{}",
                stem,
                fingerprint,
                built.ext.as_deref().unwrap_or("bin")
            ),
            None => format!(
                "assets/{}.{}",
                fingerprint,
                built.ext.as_deref().unwrap_or("bin")
            ),
        }
    }

    /// Resolve an explicit asset output path at the bundle root.
    pub fn resolve_asset_output(&self, raw: &str) -> Result<String, String> {
        Self::resolve_output(Some(""), raw)?
            .ok_or_else(|| format!("asset output path `{raw}` could not be resolved"))
    }

    pub fn asset_url(&self, output: &str) -> String {
        build_document_url(self.base_url.as_deref(), output)
    }

    /// Whether `content/<slug>.typ` exists on disk. Used by the dev
    /// server's dispatcher to decide page-route vs static-asset
    /// fallback for an incoming URL.
    pub fn page_exists(&self, slug: &str) -> bool {
        self.content_dir().join(format!("{slug}.typ")).is_file()
    }
}

/// Recursively collect `*.typ` pages under `dir` into `out`. Skips files
/// whose stem starts with `_` (private/partial convention); recurses every
/// subdirectory. Directory traversal order is unspecified — the caller sorts.
fn scan_pages_into(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("scan error: {e}"))?;
        let path = entry.path();
        let meta =
            fs::metadata(&path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            scan_pages_into(&path, out)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("typ") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.starts_with('_') {
            continue;
        }
        out.push(path);
    }
    Ok(())
}

/// Recursively collect all files into `out`.
fn scan_all_into(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("scan error: {e}"))?;
        let path = entry.path();
        let meta =
            fs::metadata(&path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            scan_all_into(&path, out)?;
            continue;
        }
        out.push(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_output_for_main() {
        let ctx = TwylaContext::stub("/tmp");
        let r = ctx.default_document_output("content/main.typ");
        assert_eq!(r, PathBuf::from("index.html"));
    }

    #[test]
    fn default_output_for_nested_main() {
        let ctx = TwylaContext::stub("/tmp");
        let r = ctx.default_document_output("content/foobar/main.typ");
        assert_eq!(r, PathBuf::from("foobar/index.html"));
    }

    #[test]
    fn default_output_for_other() {
        let ctx = TwylaContext::stub("/tmp");
        let r = ctx.default_document_output("content/guis-2.typ");
        assert_eq!(r, PathBuf::from("guis-2/index.html"));
    }

    #[test]
    fn resolve_output_convention() {
        // Absolute (leading `/`) ignores the anchor and lands at the root.
        assert_eq!(
            TwylaContext::resolve_output(Some("blog"), "/feed.xml").unwrap(),
            Some("feed.xml".to_string())
        );
        // Relative joins onto the anchor; `.`/`..` normalize.
        assert_eq!(
            TwylaContext::resolve_output(Some("blog"), "extra.html").unwrap(),
            Some("blog/extra.html".to_string())
        );
        assert_eq!(
            TwylaContext::resolve_output(Some("blog/post"), "../sibling.html").unwrap(),
            Some("blog/sibling.html".to_string())
        );
        assert_eq!(
            TwylaContext::resolve_output(Some(""), "x.html").unwrap(),
            Some("x.html".to_string())
        );
        // `..` above the root, or an empty result, is an error.
        assert!(TwylaContext::resolve_output(Some("blog"), "../../x").is_err());
        assert!(TwylaContext::resolve_output(Some("blog"), "/").is_err());
        // A relative path with no anchor defers; an absolute one still resolves.
        assert_eq!(TwylaContext::resolve_output(None, "x.html").unwrap(), None);
        assert_eq!(
            TwylaContext::resolve_output(None, "/x.html").unwrap(),
            Some("x.html".to_string())
        );
    }

    #[test]
    fn resolve_document_output_anchors_on_folder_stem() {
        let ctx = TwylaContext::stub("/tmp");
        // Relative → folder stem of the source below content/.
        assert_eq!(
            ctx.resolve_document_output("content/blog/post.typ", "extra.html")
                .unwrap(),
            "blog/extra.html"
        );
        // A top-level source has an empty stem → resolves at the root.
        assert_eq!(
            ctx.resolve_document_output("content/post.typ", "extra.html")
                .unwrap(),
            "extra.html"
        );
        // Absolute → bundle root regardless of the source's folder.
        assert_eq!(
            ctx.resolve_document_output("content/blog/post.typ", "/feed.xml")
                .unwrap(),
            "feed.xml"
        );
    }

    #[test]
    fn scan_pages_recurses_and_skips_underscores() {
        let dir = tempfile::tempdir().unwrap();
        let content = dir.path().join("content");
        let sub = content.join("what-is-color");
        std::fs::create_dir_all(&sub).unwrap();
        for rel in [
            "guis-1.typ",
            "_index.typ", // private — skipped
            "notes.md",   // not typst — skipped
            "what-is-color/main.typ",
            "what-is-color/ai_cut.typ",
            "what-is-color/index.md", // not typst — skipped
        ] {
            std::fs::write(content.join(rel), "").unwrap();
        }
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        let pages = ctx.scan_pages().unwrap();
        let rels: Vec<_> = pages
            .iter()
            .map(|p| {
                p.strip_prefix(&content)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            rels,
            vec![
                "guis-1.typ",
                "what-is-color/ai_cut.typ",
                "what-is-color/main.typ"
            ],
        );
    }

    #[test]
    fn default_kind_keys_on_source_filename() {
        let ctx = TwylaContext::stub("/tmp");
        assert_eq!(ctx.default_kind("content/main.typ"), "root");
        assert_eq!(ctx.default_kind("content/what-is-color/main.typ"), "dir");
        assert_eq!(ctx.default_kind("content/guis-1.typ"), "page");
        assert_eq!(ctx.default_kind("content/what-is-color/ai_cut.typ"), "page");
    }

    #[test]
    fn require_base_url_errors_when_unset() {
        let ctx = TwylaContext::stub("/tmp");
        assert!(ctx.require_base_url().is_err());
    }

    #[test]
    fn require_base_url_returns_value_when_set() {
        let mut ctx = TwylaContext::stub("/tmp");
        ctx.base_url = Some("https://example.com".to_string());
        assert_eq!(ctx.require_base_url().unwrap(), "https://example.com");
    }
}
