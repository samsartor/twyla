//! Zola-specific discovery and md→typ path/route mapping.
//!
//! Verified against `~/Src/zola` + a built `public/`: zola's only deviations
//! from twyla's [`default_document_output`](TwylaContext::default_document_output)
//! are (a) index naming — `_index.md`/`index.md` are the directory index — and
//! (b) `slugify.paths = On`, which slugifies each filename stem.
//!
//! Mapping policy (the `.typ` filename mirrors the `.md` stem; divergence is
//! made explicit via an `output:` override rather than a renamed file):
//!
//! - `_index.md` / `index.md` → `main.typ` in the same directory.
//! - any other `foo.md` → `foo.typ` (identity — underscores preserved).
//! - the *route* slugifies every path segment, so `content_aware_tiles.md`
//!   routes to `/content-aware-tiles/` and `what-is-color/ai_cut.md` to
//!   `/what-is-color/ai-cut/`.
//! - when the slugified route differs from twyla's default output for the
//!   `.typ`, `output_override` carries the route so the draft pins it with
//!   `#set document(output: ..)`.

use std::path::{Path, PathBuf};

use crate::project::TwylaContext;
use crate::slug::slugify;

/// A markdown source mapped to its twyla neighbour and route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedPage {
    /// Absolute path to the source `.md`.
    pub md_path: PathBuf,
    /// Absolute path to the twyla `.typ` neighbour.
    pub typ_path: PathBuf,
    /// Zola's route as a manifest key, e.g. `guis-1/index.html`,
    /// `content-aware-tiles/index.html`, `index.html`.
    pub route: String,
    /// `Some(route)` when zola's route differs from twyla's default output for
    /// `typ_path`; the draft must `#set document(output: route)`.
    pub output_override: Option<String>,
}

/// Discover every `content/**/*.md` and map each to a [`MappedPage`], sorted by
/// source path.
pub fn discover(ctx: &TwylaContext) -> Result<Vec<MappedPage>, String> {
    let content_dir = ctx.content_dir();
    let mut md_paths = Vec::new();
    collect_md(&content_dir, &mut md_paths)?;
    md_paths.sort();

    md_paths
        .into_iter()
        .map(|md_path| map_page(ctx, &content_dir, md_path))
        .collect()
}

fn collect_md(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("scan error: {e}"))?;
        let path = entry.path();
        let meta = std::fs::metadata(&path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            collect_md(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

fn map_page(ctx: &TwylaContext, content_dir: &Path, md_path: PathBuf) -> Result<MappedPage, String> {
    let rel = md_path
        .strip_prefix(content_dir)
        .map_err(|_| format!("{} is not under content/", md_path.display()))?;
    let stem = rel
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{} has no file stem", md_path.display()))?;
    let is_index = stem == "_index" || stem == "index";

    // Directory segments (verbatim — they're already the dir names on disk).
    let dir_segments: Vec<&str> = rel
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // `.typ` neighbour: filename mirrors the md stem, except index → main.
    let typ_name = if is_index {
        "main.typ".to_string()
    } else {
        format!("{stem}.typ")
    };
    let typ_path = match rel.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            content_dir.join(parent).join(&typ_name)
        }
        _ => content_dir.join(&typ_name),
    };

    // Route: slugify every segment; an index contributes no extra segment.
    let mut route_segments: Vec<String> = dir_segments.iter().map(|s| slugify(s)).collect();
    if !is_index {
        route_segments.push(slugify(stem));
    }
    let route = if route_segments.is_empty() {
        "index.html".to_string()
    } else {
        format!("{}/index.html", route_segments.join("/"))
    };

    // Override only when twyla's filename-derived default diverges.
    let typ_rel_to_root = typ_path
        .strip_prefix(&ctx.root)
        .map_err(|_| format!("{} is not under the project root", typ_path.display()))?;
    let default_output = ctx.default_document_output(&typ_rel_to_root.to_string_lossy());
    let output_override = if default_output == route {
        None
    } else {
        Some(route.clone())
    };

    Ok(MappedPage {
        md_path,
        typ_path,
        route,
        output_override,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> (tempfile::TempDir, TwylaContext) {
        let dir = tempfile::tempdir().unwrap();
        let content = dir.path().join("content");
        std::fs::create_dir_all(content.join("what-is-color")).unwrap();
        for rel in [
            "_index.md",
            "guis-1.md",
            "content_aware_tiles.md",
            "what-is-color/index.md",
            "what-is-color/ai_cut.md",
        ] {
            std::fs::write(content.join(rel), "+++\ntitle = \"x\"\n+++\n").unwrap();
        }
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        (dir, ctx)
    }

    fn find<'a>(pages: &'a [MappedPage], md_suffix: &str) -> &'a MappedPage {
        pages
            .iter()
            .find(|p| p.md_path.ends_with(md_suffix))
            .unwrap_or_else(|| panic!("no mapped page for {md_suffix}"))
    }

    #[test]
    fn maps_index_and_filenames_and_routes() {
        let (_d, ctx) = site();
        let pages = discover(&ctx).unwrap();

        let home = find(&pages, "_index.md");
        assert!(home.typ_path.ends_with("content/main.typ"));
        assert_eq!(home.route, "index.html");
        assert_eq!(home.output_override, None);

        let guis = find(&pages, "guis-1.md");
        assert!(guis.typ_path.ends_with("content/guis-1.typ"));
        assert_eq!(guis.route, "guis-1/index.html");
        assert_eq!(guis.output_override, None);

        // Underscore stem: filename identity, route slugified, override set.
        let tiles = find(&pages, "content_aware_tiles.md");
        assert!(tiles.typ_path.ends_with("content/content_aware_tiles.typ"));
        assert_eq!(tiles.route, "content-aware-tiles/index.html");
        assert_eq!(
            tiles.output_override.as_deref(),
            Some("content-aware-tiles/index.html")
        );

        // Page-bundle index → main.typ, no override.
        let wic = find(&pages, "what-is-color/index.md");
        assert!(wic.typ_path.ends_with("content/what-is-color/main.typ"));
        assert_eq!(wic.route, "what-is-color/index.html");
        assert_eq!(wic.output_override, None);

        // Nested non-index underscore: identity filename, slugified route + override.
        let ai = find(&pages, "what-is-color/ai_cut.md");
        assert!(ai.typ_path.ends_with("content/what-is-color/ai_cut.typ"));
        assert_eq!(ai.route, "what-is-color/ai-cut/index.html");
        assert_eq!(
            ai.output_override.as_deref(),
            Some("what-is-color/ai-cut/index.html")
        );
    }
}
