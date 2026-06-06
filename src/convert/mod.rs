//! `twyla convert --from zola` — the whole-site porting harness.
//!
//! Discovers markdown, generates the missing typst neighbours, compiles the
//! whole site in memory, and validates it against the zola ground truth —
//! structurally (per-page diff) and by *link validity* (the [`audit`]). Every
//! phase pushes [`report::Finding`]s into one flat list the CLI renders.

pub mod audit;
pub mod draft;
pub mod ir;
pub mod manifest;
pub mod report;
pub mod zola;

use std::collections::HashSet;
use std::path::PathBuf;

use crate::diff::{Matcher, RelaxConfig, RelaxationRule, diff};
use crate::html::{parse_html, rewrite_own_page_anchor_hrefs};
use crate::project::TwylaContext;
use crate::render::render_site_with_assets;

use report::Finding;

/// How `convert` treats existing/missing `.typ` drafts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertMode {
    /// Generate drafts only where the `.typ` is missing; leave the rest.
    Generate,
    /// Regenerate every draft, clobbering existing `.typ` files.
    Overwrite,
    /// Never write; diff + audit only (the read-only CI/skill gate).
    Verify,
}

/// Everything a convert run needs.
pub struct ConvertOptions {
    pub ctx: TwylaContext,
    pub mode: ConvertMode,
    /// Zola's built output to diff against (defaults to `public/`).
    pub ground_truth: PathBuf,
    /// When set, also write twyla's compiled HTML here for inspection.
    pub output_dir: Option<PathBuf>,
    /// When set, scope diff + audit + completeness to a single route slug.
    pub only: Option<String>,
    /// Sibling nodes of context to show above/below a divergence in the diff.
    pub above: usize,
    pub below: usize,
}

/// A setup/IO failure (exit code 2). Carries any findings already collected
/// (e.g. drafts written) so the CLI can still show them.
pub struct ConvertError {
    pub findings: Vec<Finding>,
    pub message: String,
}

/// Run the full convert pipeline. On success returns the finding list (the CLI
/// maps [`report::has_failure`] to exit code 1, otherwise 0).
pub fn run(opts: ConvertOptions) -> Result<Vec<Finding>, ConvertError> {
    let ctx = &opts.ctx;
    let base_url = ctx.base_url.as_deref();
    let mut findings = Vec::new();

    if !opts.ground_truth.is_dir() {
        return Err(ConvertError {
            findings,
            message: format!(
                "ground truth {} is not a directory — run zola build first, or pass --ground-truth",
                opts.ground_truth.display()
            ),
        });
    }

    // 1. Discover + map markdown, then ensure drafts per mode.
    let pages = zola::discover(ctx).map_err(|message| ConvertError {
        findings: Vec::new(),
        message,
    })?;
    let draft_findings = draft::ensure_drafts(ctx, &pages, opts.mode).map_err(|e| ConvertError {
        findings: Vec::new(),
        message: format!("writing drafts: {e}"),
    })?;
    findings.extend(draft_findings);

    // 2. Compile the whole site in memory.
    let site = render_site_with_assets(ctx).map_err(|e| ConvertError {
        findings: std::mem::take(&mut findings),
        message: format!("compile failed:\n{e}"),
    })?;

    // 3. Manifests — twyla's own (self-contained) and the ground truth (used to
    // excuse links broken in both builds).
    let twyla_manifest = manifest::twyla(ctx, &site).map_err(|e| ConvertError {
        findings: std::mem::take(&mut findings),
        message: format!("building twyla manifest: {e}"),
    })?;
    let gt_manifest = manifest::ground_truth(ctx, &opts.ground_truth).map_err(|e| ConvertError {
        findings: std::mem::take(&mut findings),
        message: format!("building ground-truth manifest: {e}"),
    })?;

    // 4. Per-page diff against the ground truth.
    let preset = convert_preset();
    let produced: HashSet<&str> = site.docs.iter().filter_map(|d| d.path.to_str()).collect();

    let mut twyla_pages = Vec::new();
    // Dedup identical text-only attr drift across pages (e.g. the same `<pre>`
    // background on every code page) so it reports once.
    let mut seen_attr_drift = HashSet::new();
    for doc in &site.docs {
        let route = doc.path.to_string_lossy().to_string();
        if !route_matches(&route, &opts.only) {
            continue;
        }
        let actual = parse_html(&doc.html);
        let gt_path = opts.ground_truth.join(&doc.path);
        match std::fs::read_to_string(&gt_path) {
            Ok(gt_html) => {
                let mut expected = parse_html(&gt_html);
                // Undo zola's anchor-only-link absolutization for this page.
                let prefix = format!("{}#", ctx.document_url(&route));
                rewrite_own_page_anchor_hrefs(&mut expected, &prefix);
                // Surface attribute drift on text-only-relaxed elements (e.g.
                // `<pre>`) as warnings — the structural diff drops those attrs.
                // Runs on the un-normalized trees, before they're collapsed.
                findings.extend(audit::text_only_attr_drift(
                    &preset,
                    &route,
                    &expected,
                    &actual,
                    &mut seen_attr_drift,
                ));
                // Bake the relaxations into both trees, so the diff is a pure
                // structural comparison and the rendered patch shows only
                // non-relaxed differences. (`actual` is kept un-normalized for
                // the link audit below.)
                let expected = preset.normalize(&expected);
                let actual_norm = preset.normalize(&actual);
                match diff(&expected, &actual_norm) {
                    Ok(()) => findings.push(Finding::PagePass {
                        route: route.clone(),
                    }),
                    Err(d) => findings.push(Finding::PageDiff {
                        route: route.clone(),
                        divergence: d.render_patch(&expected, &actual_norm, opts.above, opts.below),
                    }),
                }
            }
            // No ground-truth peer — a completeness concern, reported below.
            Err(_) => {}
        }
        twyla_pages.push(audit::Page {
            key: route,
            tree: actual,
        });
    }

    // 5. Ground-truth pages for the navigable-stability audit.
    let gt_routes = gt_index_pages(&opts.ground_truth);
    let mut gt_pages = Vec::new();
    for key in &gt_routes {
        if !route_matches(key, &opts.only) {
            continue;
        }
        if let Ok(html) = std::fs::read_to_string(opts.ground_truth.join(key)) {
            gt_pages.push(audit::Page {
                key: key.clone(),
                tree: parse_html(&html),
            });
        }
    }

    // 6. Link audit (self-containment, broken links, navigable stability).
    findings.extend(audit::run(
        ctx,
        &twyla_pages,
        &gt_pages,
        &twyla_manifest,
        &gt_manifest,
        base_url,
    ));

    // 7. Completeness — every expected route must be produced.
    findings.extend(completeness(&pages, &gt_routes, &produced, &opts.only));

    // 8. Optionally write the compiled HTML for inspection.
    if let Some(out) = &opts.output_dir {
        for doc in &site.docs {
            let dest = out.join(&doc.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| ConvertError {
                    findings: std::mem::take(&mut findings),
                    message: format!("creating {}: {e}", parent.display()),
                })?;
            }
            std::fs::write(&dest, &doc.html).map_err(|e| ConvertError {
                findings: std::mem::take(&mut findings),
                message: format!("writing {}: {e}", dest.display()),
            })?;
        }
    }

    Ok(findings)
}

/// The universal relaxations for a zola↔twyla page diff. URL-bearing
/// attributes relax to *value-ignored-but-present* (the link audit proves
/// validity); `<pre>` compares text-only (syntect vs verbatim); table-cell
/// `style` is ignored (typst layout doesn't reflect into per-cell CSS).
pub fn convert_preset() -> RelaxConfig {
    RelaxConfig::new()
        .relax(Matcher::Tag("pre".into()), RelaxationRule::TextOnly)
        .relax(
            Matcher::Tag("td".into()),
            RelaxationRule::IgnoreAttribute("style".into()),
        )
        .relax(
            Matcher::Tag("th".into()),
            RelaxationRule::IgnoreAttribute("style".into()),
        )
        .relax(
            Matcher::AnyTagAttrExists { attr: "src".into() },
            RelaxationRule::IgnoreAttributeValue("src".into()),
        )
        .relax(
            Matcher::AnyTagAttrExists {
                attr: "srcset".into(),
            },
            RelaxationRule::IgnoreAttributeValue("srcset".into()),
        )
        // `<a href>` stays strict (page-link equality); only sub-resource
        // `href` (link/base) relaxes its value.
        .relax(
            Matcher::Tag("link".into()),
            RelaxationRule::IgnoreAttributeValue("href".into()),
        )
        .relax(
            Matcher::Tag("base".into()),
            RelaxationRule::IgnoreAttributeValue("href".into()),
        )
}

/// Whether `route` (a manifest key like `guis-1/index.html`) is in scope for an
/// `--only <slug>` filter. `None` matches everything.
fn route_matches(route: &str, only: &Option<String>) -> bool {
    match only {
        None => true,
        Some(slug) => route_slug(route) == slug.trim_matches('/'),
    }
}

/// The slug of a route: `guis-1/index.html` → `guis-1`, `index.html` → ``.
fn route_slug(route: &str) -> &str {
    route
        .strip_suffix("/index.html")
        .or_else(|| route.strip_suffix("index.html").map(|_| ""))
        .unwrap_or(route)
}

/// Every `index.html` route under the ground-truth dir.
fn gt_index_pages(gt_dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    gather_index_html(gt_dir, gt_dir, &mut out);
    out.sort();
    out
}

fn gather_index_html(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            gather_index_html(base, &path, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("index.html") {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(
                    rel.components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
        }
    }
}

/// Route-scoped completeness: every route the zola mapping or ground truth
/// expects must be produced by twyla; routes twyla invents with no peer warn.
fn completeness(
    pages: &[zola::MappedPage],
    gt_routes: &[String],
    produced: &HashSet<&str>,
    only: &Option<String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    let mut expected: HashSet<String> = pages.iter().map(|p| p.route.clone()).collect();
    expected.extend(gt_routes.iter().cloned());

    for route in &expected {
        if !route_matches(route, only) {
            continue;
        }
        if !produced.contains(route.as_str()) {
            findings.push(Finding::RouteMissing {
                route: route.clone(),
            });
        }
    }
    for route in produced {
        if !route_matches(route, only) {
            continue;
        }
        if !expected.contains(*route) {
            findings.push(Finding::RouteExtra {
                route: route.to_string(),
            });
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_slug_and_matches() {
        assert_eq!(route_slug("guis-1/index.html"), "guis-1");
        assert_eq!(route_slug("what-is-color/ai-cut/index.html"), "what-is-color/ai-cut");
        assert_eq!(route_slug("index.html"), "");
        assert!(route_matches("guis-1/index.html", &Some("guis-1".to_string())));
        assert!(route_matches("guis-1/index.html", &None));
        assert!(!route_matches("guis-2/index.html", &Some("guis-1".to_string())));
    }
}
