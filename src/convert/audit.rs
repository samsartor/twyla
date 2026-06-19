//! The link audit — a single pass over parsed pages producing [`Finding`]s.
//!
//! Two checks, both validating that a URL resolves in **twyla's own** manifest
//! (never the ground-truth `public/`, so the old "borrow zola's output" hack
//! can't make a page pass):
//!
//! 1. *Self-containment* — every internal URL on a twyla page must resolve in
//!    twyla's manifest. The navigable subset doubles as a broken-internal-link
//!    check.
//! 2. *Navigable stability* — every navigable URL the ground truth exposes must
//!    still resolve in twyla's manifest (so e.g. `/resume.pdf` survives the
//!    port). A missing one gets a move-to-`static/` hint when the file still
//!    exists under `content/`.
//!
//! Both checks ignore URLs the **ground-truth build doesn't produce either**: a
//! link broken in `public/` too (e.g. a `dissertation.pdf` served by a
//! webserver rule, in neither build) isn't a porting regression, so we only
//! flag URLs zola actually produced that twyla failed to.
//!
//! Findings are deduplicated by resolved key so chrome links repeated across
//! every page (a footer `resume.pdf`) report once.

use std::collections::{BTreeMap, HashSet};

use crate::convert::report::Finding;
use crate::diff::{RelaxConfig, RelaxationRule};
use crate::html::{self, Element, LinkClass, Node};
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
    twyla_manifest: &HashSet<String>,
    gt_manifest: &HashSet<String>,
    base_url: Option<&str>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    // A missing feed is downgraded to one "not implemented yet" warning rather
    // than the usual failure; shared across both checks so it reports once even
    // though the feed is linked from every page's `<head>` and the GT pages too.
    let mut seen_feed = HashSet::new();

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
            // Broken in the ground truth too → not a porting regression
            // (an external/webserver-handled URL in neither build).
            if !gt_manifest.contains(&key) {
                continue;
            }
            if is_feed(&key) {
                if seen_feed.insert(key) {
                    findings.push(Finding::FeedNotImplemented { url: r.raw.clone() });
                }
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
            // Only count it as dropped if zola actually produced it; a URL the
            // ground truth links but doesn't ship either is external, not lost.
            if !gt_manifest.contains(&key) {
                continue;
            }
            if is_feed(&key) {
                if seen_feed.insert(key) {
                    findings.push(Finding::FeedNotImplemented { url: r.raw.clone() });
                }
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

/// Whether a manifest key is an Atom feed (`atom.xml`, at the root or any
/// section). Feeds aren't generated yet, so a missing one is a planned-work
/// warning rather than a failure.
fn is_feed(key: &str) -> bool {
    key == "atom.xml" || key.ends_with("/atom.xml")
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

/// Warn when a text-only-relaxed element (e.g. `<pre>`) has attributes that
/// differ from the ground truth. Its content is deliberately text-diffed (so a
/// different syntax-highlight theme doesn't fail the page), which also drops its
/// attrs from the structural diff — this surfaces that drift as a warning.
///
/// Elements are paired by document order; if the two trees don't have the same
/// number of text-only elements the structure already diverged (the main diff
/// reports it) and we skip. `seen` deduplicates identical drift across pages
/// (the same `<pre>` background on every code page reports once).
pub fn text_only_attr_drift(
    cfg: &RelaxConfig,
    page_key: &str,
    expected: &Node,
    actual: &Node,
    seen: &mut HashSet<String>,
) -> Vec<Finding> {
    let exp = collect_text_only(cfg, expected);
    let act = collect_text_only(cfg, actual);
    if exp.len() != act.len() {
        return Vec::new();
    }
    let mut findings = Vec::new();
    for (e, a) in exp.iter().zip(act.iter()) {
        if e.attrs == a.attrs {
            continue;
        }
        let detail = attr_diff_detail(&e.attrs, &a.attrs);
        if seen.insert(format!("{}\n{detail}", e.name)) {
            findings.push(Finding::TextOnlyAttrDrift {
                page: page_key.to_string(),
                tag: e.name.clone(),
                detail,
            });
        }
    }
    findings
}

/// Collect, in document order, every element a `TextOnly` rule matches.
fn collect_text_only<'a>(cfg: &RelaxConfig, node: &'a Node) -> Vec<&'a Element> {
    let mut out = Vec::new();
    walk_text_only(cfg, node, &mut out);
    out
}

fn walk_text_only<'a>(cfg: &RelaxConfig, node: &'a Node, out: &mut Vec<&'a Element>) {
    match node {
        Node::Document(children) => {
            for c in children {
                walk_text_only(cfg, c, out);
            }
        }
        Node::Element(el) => {
            if cfg
                .find_rules(el)
                .iter()
                .any(|r| matches!(r, RelaxationRule::TextOnly))
            {
                out.push(el);
            }
            for c in &el.children {
                walk_text_only(cfg, c, out);
            }
        }
        _ => {}
    }
}

/// One line per differing attribute: `name: "gt" vs "twyla"` (or `(absent)`).
fn attr_diff_detail(exp: &BTreeMap<String, String>, act: &BTreeMap<String, String>) -> String {
    let mut keys: Vec<&String> = exp.keys().chain(act.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| exp.get(*k) != act.get(*k))
        .map(|k| {
            format!(
                "{k}: {} vs {}",
                show_attr(exp.get(k)),
                show_attr(act.get(k))
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn show_attr(v: Option<&String>) -> String {
    match v {
        Some(s) => format!("{s:?}"),
        None => "(absent)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::parse_html;

    fn manifest(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_broken_link_and_dropped_navigable_with_move_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("content")).unwrap();
        std::fs::write(dir.path().join("content/resume.pdf"), "").unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();

        let twyla_manifest = manifest(&["guis-1/index.html", "assets/x-abc.png"]);
        // Zola produced guis-4 and resume.pdf (so twyla failing to is a
        // regression worth flagging).
        let gt_manifest = manifest(&["guis-1/index.html", "guis-4/index.html", "resume.pdf"]);

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

        let findings = run(
            &ctx,
            &twyla_pages,
            &gt_pages,
            &twyla_manifest,
            &gt_manifest,
            None,
        );

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

    #[test]
    fn links_broken_in_ground_truth_too_are_excused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("content")).unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();

        // Neither build produces dissertation.pdf (a webserver rule serves it).
        let twyla_manifest = manifest(&["page/index.html"]);
        let gt_manifest = manifest(&["page/index.html"]);

        let twyla_pages = vec![Page {
            key: "page/index.html".to_string(),
            tree: parse_html(r#"<a href="/dissertation.pdf">thesis</a>"#),
        }];
        let gt_pages = vec![Page {
            key: "page/index.html".to_string(),
            tree: parse_html(r#"<a href="/dissertation.pdf">thesis</a>"#),
        }];

        let findings = run(
            &ctx,
            &twyla_pages,
            &gt_pages,
            &twyla_manifest,
            &gt_manifest,
            None,
        );

        // Broken in both builds → neither a self-containment nor a dropped-URL
        // finding.
        assert!(
            !findings.iter().any(|f| matches!(
                f,
                Finding::Unreachable { .. } | Finding::NavigableDropped { .. }
            )),
            "dissertation.pdf should be excused, got: {findings:?}"
        );
    }

    #[test]
    fn missing_feed_is_a_warning_not_a_failure() {
        use crate::convert::report::Severity;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("content")).unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();

        // Zola produces atom.xml; twyla doesn't yet.
        let twyla_manifest = manifest(&["page/index.html"]);
        let gt_manifest = manifest(&["page/index.html", "atom.xml"]);

        let twyla_pages = vec![Page {
            key: "page/index.html".to_string(),
            tree: parse_html(r#"<a href="/atom.xml">feed</a>"#),
        }];
        let gt_pages = vec![Page {
            key: "page/index.html".to_string(),
            tree: parse_html(r#"<a href="/atom.xml">feed</a>"#),
        }];

        let findings = run(
            &ctx,
            &twyla_pages,
            &gt_pages,
            &twyla_manifest,
            &gt_manifest,
            None,
        );

        // Exactly one feed finding (deduped across both checks), and it's a Warn.
        let feeds: Vec<_> = findings
            .iter()
            .filter(|f| matches!(f, Finding::FeedNotImplemented { .. }))
            .collect();
        assert_eq!(feeds.len(), 1, "got: {findings:?}");
        assert_eq!(feeds[0].severity(), Severity::Warn);
        assert!(
            !findings.iter().any(|f| f.severity() == Severity::Fail),
            "a missing feed must not fail the run, got: {findings:?}"
        );
    }

    #[test]
    fn pre_attr_drift_warns_once() {
        use crate::convert::report::Severity;
        use crate::diff::Matcher;

        let cfg = RelaxConfig::new().relax(Matcher::Tag("pre".into()), RelaxationRule::TextOnly);
        // Zola's <pre> carries a theme background/color; twyla's doesn't (fixed
        // in CSS). Content is highlighted differently too — but text-only.
        let exp = parse_html(r#"<pre style="background:#2b303b;color:#c0c5ce">let x = 1;</pre>"#);
        let act = parse_html(r#"<pre><span class="k">let</span> x = 1;</pre>"#);

        let mut seen = HashSet::new();
        let f = text_only_attr_drift(&cfg, "code/index.html", &exp, &act, &mut seen);
        assert_eq!(f.len(), 1, "got: {f:?}");
        match &f[0] {
            Finding::TextOnlyAttrDrift { tag, detail, .. } => {
                assert_eq!(tag, "pre");
                assert!(detail.contains("style"), "detail: {detail}");
                assert_eq!(f[0].severity(), Severity::Warn);
            }
            other => panic!("wrong finding: {other:?}"),
        }

        // The same drift on another page is deduped away.
        let again = text_only_attr_drift(&cfg, "other/index.html", &exp, &act, &mut seen);
        assert!(again.is_empty(), "should dedup, got: {again:?}");
    }
}
