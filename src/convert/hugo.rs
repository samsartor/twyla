//! Hugo-specific discovery and md→typ path/route mapping.
//!
//! Hugo's routing rules (no custom permalinks):
//! - `_index.md` / `index.md` in a directory → `main.typ` (section/leaf index).
//! - any other `foo.md` → `foo.typ` (identity — no slugification).
//! - routes use path segments verbatim (Hugo does not convert underscores to
//!   hyphens by default, unlike Zola's `slugify.paths = On`).

use std::path::{Path, PathBuf};

use crate::project::TwylaContext;

pub use super::zola::MappedPage;

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
        let meta =
            std::fs::metadata(&path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            // Skip directories starting with `_`: Hugo treats these as page-resource
            // containers (e.g. `_downloads/`, `_images/`), not routed sections.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('_') {
                continue;
            }
            collect_md(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

fn map_page(
    ctx: &TwylaContext,
    content_dir: &Path,
    md_path: PathBuf,
) -> Result<MappedPage, String> {
    let rel = md_path
        .strip_prefix(content_dir)
        .map_err(|_| format!("{} is not under content/", md_path.display()))?;
    let stem = rel
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{} has no file stem", md_path.display()))?;
    let is_index = stem == "_index" || stem == "index";

    // Directory segments — verbatim, no slugification.
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
        Some(parent) if !parent.as_os_str().is_empty() => content_dir.join(parent).join(&typ_name),
        _ => content_dir.join(&typ_name),
    };

    // Route: path segments as-is; index files contribute no extra segment.
    let mut route_segments: Vec<String> = dir_segments.iter().map(|s| s.to_string()).collect();
    if !is_index {
        route_segments.push(stem.to_string());
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

    // Kind follows the source filename (main.typ = index), not the route.
    let kind = ctx.default_kind(&typ_rel_to_root.to_string_lossy());

    Ok(MappedPage {
        md_path,
        typ_path,
        route,
        kind,
        output_override,
    })
}
