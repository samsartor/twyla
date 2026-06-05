//! Output manifests — the set of files a build produces.
//!
//! The link audit checks every referenced URL against a manifest:
//! twyla's own (must be self-contained, no borrowing from `public/`) and the
//! zola ground truth (for navigable-URL stability). Keys are root-relative,
//! `/`-separated, no leading slash — the same form
//! [`crate::html::resolve`] produces.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::project::TwylaContext;
use crate::render::SiteOutput;

/// The set of root-relative output paths a build produces.
#[derive(Debug, Default, Clone)]
pub struct Manifest {
    paths: HashSet<String>,
}

impl Manifest {
    pub fn contains(&self, key: &str) -> bool {
        self.paths.contains(key)
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().map(String::as_str)
    }

    fn insert(&mut self, key: impl Into<String>) {
        self.paths.insert(key.into());
    }
}

impl FromIterator<String> for Manifest {
    fn from_iter<I: IntoIterator<Item = String>>(it: I) -> Self {
        Self {
            paths: it.into_iter().collect(),
        }
    }
}

/// Twyla's own manifest — built without touching `public/`: the compiled
/// pages, every processed (`asset.*`) asset, `static/`, and colocated content
/// assets (non-`.typ`/`.md` under `content/`).
pub fn twyla(ctx: &TwylaContext, site: &SiteOutput) -> io::Result<Manifest> {
    let mut m = Manifest::default();
    for doc in &site.docs {
        m.insert(path_key(&doc.path));
    }
    for asset in &site.assets {
        m.insert(asset.output_path.clone());
    }
    let static_dir = ctx.static_dir();
    if static_dir.is_dir() {
        collect_files(&static_dir, &static_dir, &mut m, &|_| true)?;
    }
    let content_dir = ctx.content_dir();
    if content_dir.is_dir() {
        collect_files(&content_dir, &content_dir, &mut m, &|p| {
            !matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("typ") | Some("md")
            )
        })?;
    }
    Ok(m)
}

/// The ground-truth manifest — the whole `gt_dir` tree (zola's `public/`) plus
/// the shared `static/`.
pub fn ground_truth(ctx: &TwylaContext, gt_dir: &Path) -> io::Result<Manifest> {
    let mut m = Manifest::default();
    if gt_dir.is_dir() {
        collect_files(gt_dir, gt_dir, &mut m, &|_| true)?;
    }
    let static_dir = ctx.static_dir();
    if static_dir.is_dir() {
        collect_files(&static_dir, &static_dir, &mut m, &|_| true)?;
    }
    Ok(m)
}

/// Recursively insert every file under `dir` (relative to `base`) that
/// `include` accepts. Follows symlinks via `fs::metadata` — matching what
/// `build` emits (e.g. a symlinked `static/scripts`).
fn collect_files(
    base: &Path,
    dir: &Path,
    out: &mut Manifest,
    include: &dyn Fn(&Path) -> bool,
) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = std::fs::metadata(&path)?; // follows symlinks
        if meta.is_dir() {
            collect_files(base, &path, out, include)?;
        } else if meta.is_file() && include(&path) {
            if let Ok(rel) = path.strip_prefix(base) {
                out.insert(path_key(rel));
            }
        }
    }
    Ok(())
}

/// Normalize a relative path into a `/`-separated manifest key.
fn path_key(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::OutputDoc;
    use std::path::PathBuf;

    #[test]
    fn twyla_manifest_collects_pages_static_and_colocated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = root.join("content");
        std::fs::create_dir_all(content.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("static/scripts")).unwrap();
        for (rel, base) in [
            ("foo.typ", &content),
            ("foo.md", &content),
            ("img.png", &content),
            ("sub/bar.svg", &content),
            ("sub/baz.typ", &content),
        ] {
            std::fs::write(base.join(rel), "").unwrap();
        }
        std::fs::write(root.join("static/style.css"), "").unwrap();
        std::fs::write(root.join("static/scripts/site.js"), "").unwrap();

        let ctx = TwylaContext::new(root, None).unwrap();
        let site = SiteOutput {
            docs: vec![OutputDoc {
                path: PathBuf::from("foo/index.html"),
                html: String::new(),
            }],
            assets: vec![],
        };
        let m = twyla(&ctx, &site).unwrap();

        for present in [
            "foo/index.html",   // compiled page
            "img.png",          // colocated content asset
            "sub/bar.svg",      // nested colocated asset
            "style.css",        // static at root
            "scripts/site.js",  // nested static
        ] {
            assert!(m.contains(present), "missing {present}");
        }
        for absent in ["foo.typ", "foo.md", "sub/baz.typ"] {
            assert!(!m.contains(absent), "should not contain {absent}");
        }
    }

    #[test]
    fn ground_truth_walks_public_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let public = root.join("public");
        std::fs::create_dir_all(public.join("guis-1")).unwrap();
        std::fs::write(public.join("guis-1/index.html"), "").unwrap();
        std::fs::write(public.join("resume.pdf"), "").unwrap();
        std::fs::create_dir_all(root.join("static")).unwrap();
        std::fs::write(root.join("static/favicon.ico"), "").unwrap();

        let ctx = TwylaContext::new(root, None).unwrap();
        let m = ground_truth(&ctx, &public).unwrap();
        assert!(m.contains("guis-1/index.html"));
        assert!(m.contains("resume.pdf"));
        assert!(m.contains("favicon.ico")); // shared static
    }
}
