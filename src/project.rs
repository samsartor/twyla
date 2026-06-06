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

use std::path::{Path, PathBuf};

use crate::asset;

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
    /// Whether colocated content assets — non-source files under `content/` —
    /// ship at their root path, zola-style (`content/foo.svg` → `/foo.svg`).
    /// Off by default: colocated files should live in `static/`. When on,
    /// `content/` joins the [`copy roots`](Self::copy_roots), so `build`,
    /// the manifest, and the dev server all emit/serve it identically. A
    /// future `--serve-content`/`twyla.toml` knob flips this.
    pub emit_content_assets: bool,
}

/// A directory whose files ship verbatim at the output root — copied by
/// [`build`](crate::build), listed in the twyla manifest, and served as a
/// fallback by the dev server. The single source of truth for "non-compiled
/// files that ship," handed out by [`TwylaContext::copy_roots`].
#[derive(Debug, Clone)]
pub struct CopyRoot {
    /// The on-disk directory (e.g. `<root>/static/`).
    pub dir: PathBuf,
    /// Which files under `dir` ship.
    pub filter: CopyFilter,
}

/// Which files a [`CopyRoot`] emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFilter {
    /// Every file (the `static/` tree).
    All,
    /// Non-source files only — excludes `.typ`/`.md` (colocated content assets,
    /// keeping page/markdown sources out of the output).
    NonSource,
}

impl CopyFilter {
    /// Whether a file (named by any path whose extension is meaningful) ships.
    pub fn accepts(&self, path: &Path) -> bool {
        match self {
            CopyFilter::All => true,
            CopyFilter::NonSource => !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("typ") | Some("md")
            ),
        }
    }
}

impl CopyRoot {
    /// Every file under `dir` the filter accepts, as `(root-relative `/`-key,
    /// absolute source path)`. Follows symlinks (we want targets, not links, in
    /// the output). A missing `dir` yields nothing — `static/`/`content/` need
    /// not exist.
    pub fn walk(&self) -> std::io::Result<Vec<(String, PathBuf)>> {
        let mut out = Vec::new();
        if self.dir.is_dir() {
            self.walk_into(&self.dir, &mut out)?;
        }
        Ok(out)
    }

    fn walk_into(&self, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let meta = std::fs::metadata(&path)?; // follows symlinks
            if meta.is_dir() {
                self.walk_into(&path, out)?;
            } else if meta.is_file()
                && self.filter.accepts(&path)
                && let Ok(rel) = path.strip_prefix(&self.dir)
            {
                out.push((path_key(rel), path.clone()));
            }
        }
        Ok(())
    }

    /// Resolve a request path (root-relative, `/`-separated, already traversal-
    /// checked) to an on-disk source under this root — when the filter accepts
    /// it and the file exists. The dev server's live fallback, sharing the
    /// filter so serve and build agree on what's reachable.
    pub fn resolve(&self, rel: &str) -> Option<PathBuf> {
        if !self.filter.accepts(Path::new(rel)) {
            return None;
        }
        let path = self.dir.join(rel);
        path.is_file().then_some(path)
    }
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
            emit_content_assets: false,
        })
    }

    /// A bare context for unit tests: fields take their defaults and `root` is
    /// used verbatim (no canonicalization, so it needn't exist on disk). Adding
    /// a field updates only this — not every test. Set `base_url`/
    /// `emit_content_assets` on the returned value when a test needs them.
    #[cfg(test)]
    pub(crate) fn stub(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            base_url: None,
            emit_content_assets: false,
        }
    }

    /// The verbatim-copy sources, in fallback order: `static/` always (whole
    /// tree), then `content/` for colocated assets when
    /// [`emit_content_assets`](Self::emit_content_assets) is on. The single
    /// place the static-/content-`/`-content rule is decided — `build`, the
    /// manifest, and `serve` all consume this so they can't disagree.
    pub fn copy_roots(&self) -> Vec<CopyRoot> {
        let mut roots = vec![CopyRoot {
            dir: self.static_dir(),
            filter: CopyFilter::All,
        }];
        if self.emit_content_assets {
            roots.push(CopyRoot {
                dir: self.content_dir(),
                filter: CopyFilter::NonSource,
            });
        }
        roots
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

    /// Bundle-relative directory compiled HTML files are emitted into, e.g.
    /// `somepost/index.html`. Joined under the build output dir on disk and
    /// exposed at the matching root-relative URL ([`document_url`](Self::document_url)).
    pub fn default_document_output(&self, source: &str) -> String {
        let source = self.root.join(source);
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
        let path = match output.strip_suffix("index.html") {
            Some(rest) => rest,
            None => output,
        };
        match &self.base_url {
            Some(base) => format!(
                "{}/{}",
                base.trim_end_matches('/'),
                path.trim_start_matches('/')
            ),
            None => format!("/{}", path.trim_start_matches('/')),
        }
    }

    /// Bundle-relative directory processed assets are emitted into, e.g.
    /// `assets/main-<hash>.css`. Joined under the build output dir on disk and
    /// exposed at the matching root-relative URL ([`asset_url`](Self::asset_url)).
    pub fn default_asset_output(&self, built: &asset::Built) -> String {
        match &built.stem {
            Some(stem) => format!(
                "assets/{}-{:032x}.{}",
                stem,
                built.content_hash,
                built.ext.as_deref().unwrap_or("bin")
            ),
            None => format!(
                "assets/{:032x}.{}",
                built.content_hash,
                built.ext.as_deref().unwrap_or("bin")
            ),
        }
    }

    pub fn asset_url(&self, output: &str) -> String {
        match &self.base_url {
            Some(base) => format!(
                "{}/{}",
                base.trim_end_matches('/'),
                output.trim_start_matches('/')
            ),
            None => format!("/{}", output.trim_start_matches('/')),
        }
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
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("scan error: {e}"))?;
        let path = entry.path();
        let meta = std::fs::metadata(&path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
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
            .map(|p| p.strip_prefix(&content).unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            rels,
            vec!["guis-1.typ", "what-is-color/ai_cut.typ", "what-is-color/main.typ"],
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
