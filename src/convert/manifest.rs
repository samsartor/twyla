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

/// Twyla's own manifest — built without touching `public/`. Exactly the paths
/// [`crate::build::emit_plan`] writes (compiled pages, processed assets, and
/// the [`copy roots`](TwylaContext::copy_roots)), so the manifest and the build
/// can't drift: a file the audit treats as "shipped" is one `build` emits.
pub fn twyla(ctx: &TwylaContext, site: &SiteOutput) -> io::Result<Manifest> {
    Ok(crate::build::emit_plan(ctx, site)?
        .into_iter()
        .map(|e| e.path)
        .collect())
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
                out.insert(crate::project::path_key(rel));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::OutputDoc;
    use std::path::PathBuf;

    fn fixture() -> (tempfile::TempDir, SiteOutput) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = root.join("content");
        std::fs::create_dir_all(content.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("static/scripts")).unwrap();
        for rel in ["foo.typ", "foo.md", "img.png", "sub/bar.svg", "sub/baz.typ"] {
            std::fs::write(content.join(rel), "").unwrap();
        }
        std::fs::write(root.join("static/style.css"), "").unwrap();
        std::fs::write(root.join("static/scripts/site.js"), "").unwrap();
        let site = SiteOutput {
            docs: vec![OutputDoc {
                path: PathBuf::from("foo/index.html"),
                html: String::new(),
            }],
            assets: vec![],
        };
        (dir, site)
    }

    #[test]
    fn twyla_manifest_collects_pages_and_static() {
        let (dir, site) = fixture();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        let m = twyla(&ctx, &site).unwrap();

        for present in [
            "foo/index.html",  // compiled page
            "style.css",       // static at root
            "scripts/site.js", // nested static
        ] {
            assert!(m.contains(present), "missing {present}");
        }
        // Colocated content is *not* shipped by default — `emit_content_assets`
        // is off, so `content/` isn't a copy root.
        for absent in ["foo.typ", "foo.md", "sub/baz.typ", "img.png", "sub/bar.svg"] {
            assert!(!m.contains(absent), "should not contain {absent}");
        }
    }

    #[test]
    fn twyla_manifest_includes_colocated_content_when_enabled() {
        let (dir, site) = fixture();
        let mut ctx = TwylaContext::new(dir.path(), None).unwrap();
        ctx.emit_content_assets = true;
        let m = twyla(&ctx, &site).unwrap();

        // With the flag on, `content/` joins the copy roots: non-source files
        // ship, source files (`.typ`/`.md`) still don't.
        for present in ["img.png", "sub/bar.svg"] {
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
