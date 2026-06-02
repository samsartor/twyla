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
        Ok(Self { root, base_url })
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

    /// Discover routed pages from the filesystem layout. Top-level
    /// `content/*.typ` only — no subdirectory recursion yet (the
    /// corpus doesn't need it).
    pub fn scan_pages(&self) -> Result<Vec<PathBuf>, String> {
        let content_dir = self.content_dir();
        let entries = std::fs::read_dir(&content_dir)
            .map_err(|e| format!("cannot read {}: {e}", content_dir.display()))?;
        let mut paths = Vec::new();
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
            paths.push(path);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_output_for_main() {
        let ctx = TwylaContext {
            root: PathBuf::from("/tmp"),
            base_url: None,
        };
        let r = ctx.default_document_output("content/main.typ");
        assert_eq!(r, PathBuf::from("index.html"));
    }

    #[test]
    fn default_output_for_nested_main() {
        let ctx = TwylaContext {
            root: PathBuf::from("/tmp"),
            base_url: None,
        };
        let r = ctx.default_document_output("content/foobar/main.typ");
        assert_eq!(r, PathBuf::from("foobar/index.html"));
    }

    #[test]
    fn default_output_for_other() {
        let ctx = TwylaContext {
            root: PathBuf::from("/tmp"),
            base_url: None,
        };
        let r = ctx.default_document_output("content/guis-2.typ");
        assert_eq!(r, PathBuf::from("guis-2/index.html"));
    }

    #[test]
    fn require_base_url_errors_when_unset() {
        let ctx = TwylaContext {
            root: PathBuf::from("/tmp"),
            base_url: None,
        };
        assert!(ctx.require_base_url().is_err());
    }

    #[test]
    fn require_base_url_returns_value_when_set() {
        let ctx = TwylaContext {
            root: PathBuf::from("/tmp"),
            base_url: Some("https://example.com".to_string()),
        };
        assert_eq!(ctx.require_base_url().unwrap(), "https://example.com");
    }
}
