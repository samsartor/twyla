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

/// Where a routed page emits in the bundle and what URL serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRoute {
    /// User-facing URL, leading slash, trailing slash. `/foo/` or `/`.
    pub url_path: String,
    /// Bundle-relative output path. `foo/index.html` or `index.html`.
    pub bundle_path: PathBuf,
}

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

    /// `<root>/templates/` — typst modules importable from content.
    /// Not directly emitted; here for completeness so call sites stop
    /// hardcoding the directory name.
    pub fn templates_dir(&self) -> PathBuf {
        self.root.join("templates")
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
    pub fn scan_pages(&self) -> Result<Vec<String>, String> {
        let content_dir = self.content_dir();
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

    /// Default twyla-inferred route for a page identified by its file
    /// stem. Today this is the *only* route source; when pages can
    /// declare their own URL via typst metadata, this becomes the
    /// fallback for the un-overridden case (signature stays the same,
    /// callers swap to a `route_for(stem, declared)` helper that calls
    /// this when `declared` is `None`).
    ///
    /// `main` → `("/", "index.html")`; everything else →
    /// `("/<stem>/", "<stem>/index.html")`.
    pub fn default_route(&self, stem: &str) -> PageRoute {
        if stem == "main" {
            PageRoute {
                url_path: "/".to_string(),
                bundle_path: PathBuf::from("index.html"),
            }
        } else {
            PageRoute {
                url_path: format!("/{stem}/"),
                bundle_path: PathBuf::from(format!("{stem}/index.html")),
            }
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
    fn default_route_for_index_routes_to_bundle_root() {
        let ctx = TwylaContext {
            root: PathBuf::from("/tmp"),
            base_url: None,
        };
        let r = ctx.default_route("main");
        assert_eq!(r.url_path, "/");
        assert_eq!(r.bundle_path, PathBuf::from("index.html"));
    }

    #[test]
    fn default_route_for_slug_nests_under_slug_dir() {
        let ctx = TwylaContext {
            root: PathBuf::from("/tmp"),
            base_url: None,
        };
        let r = ctx.default_route("guis-2");
        assert_eq!(r.url_path, "/guis-2/");
        assert_eq!(r.bundle_path, PathBuf::from("guis-2/index.html"));
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
