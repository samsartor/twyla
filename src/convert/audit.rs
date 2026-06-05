//! The link audit — a single pass over parsed pages producing [`Finding`]s.
//!
//! Two checks, both validating against **twyla's own** manifest (never the
//! ground-truth `public/`, so the old "borrow zola's output" hack can't make a
//! page pass):
//!
//! 1. *Self-containment* — every internal URL on a twyla page must resolve in
//!    twyla's manifest. The navigable subset doubles as a broken-internal-link
//!    check.
//! 2. *Navigable stability* — every navigable URL the ground truth exposes must
//!    still resolve in twyla's manifest (so e.g. `/resume.pdf` survives the
//!    port). A missing one gets a move-to-`static/` hint when the file still
//!    exists under `content/`.
//!
//! Findings are deduplicated by resolved key so chrome links repeated across
//! every page (a footer `resume.pdf`) report once.

use std::collections::HashSet;

use crate::convert::manifest::Manifest;
use crate::convert::report::Finding;
use crate::html::{self, LinkClass, Node};
use crate::project::TwylaContext;

/// A page to audit: its manifest key (the base for relative-URL resolution)
/// plus its parsed tree.
pub struct Page {
    pub key: String,
    pub tree: Node,
}

/// Run both audit passes and return the (deduplicated) findings.
pub fn run(
    ctx: &TwylaContext,
    twyla_pages: &[Page],
    gt_pages: &[Page],
    twyla_manifest: &Manifest,
    base_url: Option<&str>,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // 1. Self-containment + broken internal links.
    let mut seen_unreachable = HashSet::new();
    for page in twyla_pages {
        for r in html::extract(&page.tree) {
            let Some(key) = html::resolve(&r.raw, &page.key, base_url) else {
                continue;
            };
            if twyla_manifest.contains(&key) {
                continue;
            }
            if seen_unreachable.insert(key) {
                findings.push(Finding::Unreachable {
                    page: page.key.clone(),
                    url: r.raw.clone(),
                    navigable: r.class == LinkClass::Navigable,
                });
            }
        }
    }

    // 2. Navigable stability against the ground truth.
    let mut seen_dropped = HashSet::new();
    for page in gt_pages {
        for r in html::extract(&page.tree) {
            if r.class != LinkClass::Navigable {
                continue;
            }
            let Some(key) = html::resolve(&r.raw, &page.key, base_url) else {
                continue;
            };
            if twyla_manifest.contains(&key) {
                continue;
            }
            if seen_dropped.insert(key.clone()) {
                findings.push(Finding::NavigableDropped {
                    page: page.key.clone(),
                    url: r.raw.clone(),
                    hint: move_hint(ctx, &key),
                });
            }
        }
    }

    findings
}

/// The help hint for a dropped navigable URL. A `…/index.html` key is a page
/// (not a movable file); a verbatim file still under `content/` should move to
/// `static/` to keep its URL; otherwise it's simply gone.
fn move_hint(ctx: &TwylaContext, key: &str) -> String {
    if key.ends_with("/index.html") || key == "index.html" {
        return format!("expected {key}, not produced by twyla");
    }
    if ctx.content_dir().join(key).is_file() {
        format!("content/{key} exists — move it to static/{key} to preserve the URL")
    } else {
        format!("expected {key}, not produced by twyla")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::parse_html;

    fn manifest(keys: &[&str]) -> Manifest {
        keys.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_broken_link_and_dropped_navigable_with_move_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("content")).unwrap();
        std::fs::write(dir.path().join("content/resume.pdf"), "").unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();

        let twyla_manifest = manifest(&["guis-1/index.html", "assets/x-abc.png"]);

        let twyla_pages = vec![Page {
            key: "guis-1/index.html".to_string(),
            tree: parse_html(
                r#"<a href="/guis-1">ok</a>
                   <a href="/guis-4">broken</a>
                   <img src="/assets/x-abc.png">"#,
            ),
        }];
        let gt_pages = vec![Page {
            key: "guis-1/index.html".to_string(),
            tree: parse_html(r#"<a href="/resume.pdf">resume</a><a href="/guis-1">self</a>"#),
        }];

        let findings = run(&ctx, &twyla_pages, &gt_pages, &twyla_manifest, None);

        // One broken twyla link (guis-4), one dropped navigable (resume.pdf).
        let unreachable: Vec<_> = findings
            .iter()
            .filter_map(|f| match f {
                Finding::Unreachable { url, navigable, .. } => Some((url.as_str(), *navigable)),
                _ => None,
            })
            .collect();
        assert_eq!(unreachable, vec![("/guis-4", true)]);

        let dropped: Vec<_> = findings
            .iter()
            .filter_map(|f| match f {
                Finding::NavigableDropped { url, hint, .. } => Some((url.as_str(), hint.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].0, "/resume.pdf");
        assert!(
            dropped[0].1.contains("move it to static/resume.pdf"),
            "hint was: {}",
            dropped[0].1
        );
    }
}
