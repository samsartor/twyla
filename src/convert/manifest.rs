//! Output manifests — the set of file keys a build produces, as a plain
//! `HashSet<String>` of root-relative, `/`-separated keys (no leading slash —
//! the form [`crate::html::resolve`] produces).
//!
//! The link audit checks every referenced URL against a manifest: twyla's own
//! (must be self-contained, no borrowing from `public/`) and the zola ground
//! truth (for navigable-URL stability).

use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::project::TwylaContext;
use crate::render::Outputs;

/// Twyla's own manifest — built without touching `public/`: every output key
/// the compile produced (pages, assets, static files). Since `build` writes
/// the very same [`Outputs`] map, the manifest and the build can't drift — a
/// file the audit treats as "shipped" is one `build` emits.
pub fn twyla(outputs: &Outputs) -> HashSet<String> {
    outputs.iter().map(|o| o.key().to_owned()).collect()
}

/// The ground-truth manifest — every file under `gt_dir` (zola's `public/`)
/// plus the shared `static/`, as root-relative `/`-keys.
pub fn ground_truth(ctx: &TwylaContext, gt_dir: &Path) -> io::Result<HashSet<String>> {
    let mut paths = HashSet::new();
    if gt_dir.is_dir() {
        collect_files(gt_dir, gt_dir, &mut paths)?;
    }
    let static_dir = ctx.static_dir();
    if static_dir.is_dir() {
        collect_files(&static_dir, &static_dir, &mut paths)?;
    }
    Ok(paths)
}

/// Recursively insert every file under `dir` (relative to `base`). Follows
/// symlinks via `fs::metadata` — matching what `build` emits (e.g. a symlinked
/// `static/scripts`).
fn collect_files(base: &Path, dir: &Path, out: &mut HashSet<String>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = std::fs::metadata(&path)?; // follows symlinks
        if meta.is_dir() {
            collect_files(base, &path, out)?;
        } else if meta.is_file()
            && let Ok(rel) = path.strip_prefix(base)
        {
            out.insert(crate::project::path_key(rel));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{Output, OutputDoc, Outputs};
    use iddqd::IdHashMap;

    #[test]
    fn twyla_manifest_collects_doc_and_static_keys() {
        // The manifest is just the keys of the compiled `Outputs`. Build one
        // with a page plus the `static/` tree and check both land.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("static/scripts")).unwrap();
        std::fs::write(root.join("static/style.css"), "").unwrap();
        std::fs::write(root.join("static/scripts/site.js"), "").unwrap();
        let ctx = TwylaContext::new(root, None).unwrap();

        let mut all = IdHashMap::new();
        all.insert_unique(Output::Doc(OutputDoc {
            output_path: "foo/index.html".to_owned(),
            html: String::new(),
            meta: None,
        }))
        .unwrap();
        for out in ctx.static_outputs().unwrap() {
            all.insert_unique(out).unwrap();
        }
        let m = twyla(&Outputs { all });

        for present in [
            "foo/index.html",  // compiled page
            "style.css",       // static at root
            "scripts/site.js", // nested static
        ] {
            assert!(m.contains(present), "missing {present}");
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
