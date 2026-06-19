//! Unified-diff rendering of a page comparison.
//!
//! A [`Divergence`] tells the caller a page differs (and carries a textual
//! reason); this renders the actual difference as a familiar `-`/`+` patch.
//! Both normalized trees are rendered to indented lines and run through a
//! line-based LCS diff, so every change on the page shows in one hunk with
//! `-A`/`-B` lines of context. Identical regions collapse to `⋮`.
//!
//! Rendered at the diff site (where both trees are in hand) since a
//! [`Divergence`] doesn't retain them.

use std::fmt::Write as _;

use owo_colors::{OwoColorize, Stream};

use crate::diff::compare::Divergence;
use crate::html::{Element, Node};

/// `convert`'s report prints to stdout; color follows that stream's tty.
const OUT: Stream = Stream::Stdout;
/// Lines kept at each end when collapsing the interior of a long run of
/// same-kind changes (a wholesale added/removed subtree).
const CHANGE_KEEP: usize = 3;
/// Truncate any single rendered line past this many chars.
const MAX_LINE: usize = 200;
/// Cost ceiling for the (quadratic) line LCS: `expected.len() * actual.len()`.
/// Above this a page is too large to patch-render, and we fall back to the
/// textual reason. Sized so whole real pages diff fine (~4000 lines/side).
const MAX_DIFF_CELLS: usize = 16_000_000;

impl Divergence {
    /// A `-`/`+` unified-diff patch of the whole page (`-` expected, `+`
    /// actual) — far more legible than the path + reason that
    /// [`Display`](std::fmt::Display) gives. `above`/`below` are the lines of
    /// unchanged context kept before/after each change.
    ///
    /// Falls back to the textual reason when the page is too large to diff or
    /// when the normalized trees render identically (the pretty-printer drops
    /// some differences, e.g. whitespace-only text).
    pub fn render_patch(
        &self,
        expected_root: &Node,
        actual_root: &Node,
        above: usize,
        below: usize,
    ) -> String {
        render_hunk(
            &render_side(expected_root),
            &render_side(actual_root),
            above,
            below,
        )
        .unwrap_or_else(|| self.to_string())
    }
}

/// Diff two rendered sides into a trimmed `-`/`+` hunk. Returns `None` (so the
/// caller falls back to the textual reason) when the renderings are identical —
/// the pretty-printer normalizes some differences away (whitespace text,
/// comments) and an empty hunk would print as a bare route line — or the inputs
/// are too large to diff affordably.
fn render_hunk(
    expected: &[String],
    actual: &[String],
    above: usize,
    below: usize,
) -> Option<String> {
    if expected.len().saturating_mul(actual.len()) > MAX_DIFF_CELLS {
        return None;
    }
    let ops = diff_ops(expected, actual);
    if !ops.iter().any(|o| !matches!(o, Op::Eq(_))) {
        return None;
    }
    Some(format_ops(&ops, above, below).join("\n"))
}

// ---- pretty printing -----------------------------------------------------

/// Render a node to indented lines (one tag / text run per line). Rendered in
/// full — [`format_ops`] collapses the identical parts, and [`render_hunk`]
/// guards the cost — so the actual divergence is never truncated out of view.
fn render_side(node: &Node) -> Vec<String> {
    let mut out = Vec::new();
    push_node(node, 0, &mut out);
    out
}

fn push_node(node: &Node, depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    match node {
        Node::Document(children) => {
            for c in children {
                push_node(c, depth, out);
            }
        }
        Node::Doctype(name) => out.push(format!("{pad}<!DOCTYPE {name}>")),
        Node::Comment(c) => out.push(format!("{pad}<!--{}-->", clip(c))),
        Node::Text(t) => {
            let t = t.trim();
            if !t.is_empty() {
                out.push(format!("{pad}{}", clip(t)));
            }
        }
        Node::Element(el) => push_element(el, depth, out),
    }
}

fn push_element(el: &Element, depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    let open = open_tag(el);

    if is_void(&el.name) {
        out.push(format!("{pad}{open}"));
        return;
    }

    // Inline an empty element or one wrapping a single text run, so a text or
    // attribute change shows as a single `-`/`+` line.
    match el.children.as_slice() {
        [] => out.push(format!("{pad}{open}</{}>", el.name)),
        [Node::Text(t)] => out.push(format!("{pad}{open}{}</{}>", clip(t.trim()), el.name)),
        children => {
            out.push(format!("{pad}{open}"));
            for c in children {
                push_node(c, depth + 1, out);
            }
            out.push(format!("{pad}</{}>", el.name));
        }
    }
}

/// `<tag a="x" b="y">` (attrs are already sorted in the tree) or `<tag … />`
/// for void elements.
fn open_tag(el: &Element) -> String {
    let mut s = format!("<{}", el.name);
    for (k, v) in &el.attrs {
        let _ = write!(s, " {k}=\"{}\"", clip(v));
    }
    if is_void(&el.name) {
        s.push_str(" />");
    } else {
        s.push('>');
    }
    s
}

fn clip(s: &str) -> String {
    if s.chars().count() > MAX_LINE {
        let head: String = s.chars().take(MAX_LINE).collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

// ---- line diff -----------------------------------------------------------

enum Op<'a> {
    Eq(&'a str),
    Del(&'a str),
    Ins(&'a str),
}

/// LCS line alignment of two rendered sides. The caller ([`render_hunk`])
/// bounds `a.len() * b.len()` so this `O(n·m)` table stays affordable; `u32`
/// cells (line counts never overflow) halve its memory.
fn diff_ops<'a>(a: &'a [String], b: &'a [String]) -> Vec<Op<'a>> {
    let (n, m) = (a.len(), b.len());
    // lcs[i][j] = LCS length of a[i..] and b[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::new();
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Eq(&a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            ops.push(Op::Del(&a[i]));
            i += 1;
        } else {
            ops.push(Op::Ins(&b[j]));
            j += 1;
        }
    }
    ops.extend(a[i..].iter().map(|l| Op::Del(l)));
    ops.extend(b[j..].iter().map(|l| Op::Ins(l)));
    ops
}

/// Format the ops as a colored unified hunk, keeping only the load-bearing
/// lines: changes, plus `above` unchanged lines before each change and `below`
/// after. Unchanged runs beyond that — and the interior of long same-kind
/// change runs (a wholesale added/removed subtree) — collapse to a
/// `⋮ (n lines)` marker.
fn format_ops(ops: &[Op], above: usize, below: usize) -> Vec<String> {
    let n = ops.len();
    let mut keep = vec![true; n];

    // Keep an unchanged line when a change is near: within `above` lines after
    // it (this line is context *above* that change) or `below` lines before it
    // (context *below* a change) — i.e. any change in `[i - below, i + above]`.
    for i in 0..n {
        if matches!(ops[i], Op::Eq(_)) {
            let lo = i.saturating_sub(below);
            let hi = (i + above + 1).min(n);
            keep[i] = ops[lo..hi].iter().any(|o| !matches!(o, Op::Eq(_)));
        }
    }

    // Collapse the middle of long runs of same-kind changes, keeping
    // `CHANGE_KEEP` at each end — e.g. a 500-line removed SVG shows its first
    // and last few lines.
    let mut i = 0;
    while i < n {
        if matches!(ops[i], Op::Eq(_)) {
            i += 1;
            continue;
        }
        let start = i;
        let del = matches!(ops[i], Op::Del(_));
        while i < n && !matches!(ops[i], Op::Eq(_)) && matches!(ops[i], Op::Del(_)) == del {
            i += 1;
        }
        if i - start > 2 * CHANGE_KEEP + 1 {
            for k in &mut keep[start + CHANGE_KEEP..i - CHANGE_KEEP] {
                *k = false;
            }
        }
    }

    let mut out = Vec::new();
    let mut gap = 0usize;
    for i in 0..n {
        if !keep[i] {
            gap += 1;
            continue;
        }
        if gap > 0 {
            let s = if gap == 1 { "" } else { "s" };
            out.push(dim(&format!("  ⋮ ({gap} line{s})")));
            gap = 0;
        }
        out.push(match &ops[i] {
            Op::Eq(l) => dim(&format!("  {l}")),
            Op::Del(l) => format!("- {l}")
                .if_supports_color(OUT, |t| t.red())
                .to_string(),
            Op::Ins(l) => format!("+ {l}")
                .if_supports_color(OUT, |t| t.green())
                .to_string(),
        });
    }
    if gap > 0 {
        let s = if gap == 1 { "" } else { "s" };
        out.push(dim(&format!("  ⋮ ({gap} line{s})")));
    }
    out
}

fn dim(s: &str) -> String {
    s.if_supports_color(OUT, |t| t.dimmed()).to_string()
}

#[cfg(test)]
mod tests {
    use crate::diff::diff;
    use crate::html::parse_html;

    /// Diff two fragments and render the patch (default 1 line of context).
    /// Stdout is captured in tests, so owo emits no color — assertions match
    /// plain `-`/`+` lines.
    fn patch(expected: &str, actual: &str) -> String {
        patch_ctx(expected, actual, 1, 1)
    }

    fn patch_ctx(expected: &str, actual: &str, above: usize, below: usize) -> String {
        let e = parse_html(expected);
        let a = parse_html(actual);
        let d = diff(&e, &a).expect_err("expected a divergence");
        d.render_patch(&e, &a, above, below)
    }

    /// Is there a `-`/`+`/context line (whole-document render is deeply indented,
    /// so match on the prefix + content, not exact columns)?
    fn has(patch: &str, prefix: &str, needle: &str) -> bool {
        patch
            .lines()
            .any(|l| l.starts_with(prefix) && l.contains(needle))
    }

    #[test]
    fn attr_change_is_a_minus_plus_pair() {
        let p = patch(
            "<div><p class=\"foo\">hi</p></div>",
            "<div><p class=\"bar\">hi</p></div>",
        );
        assert!(has(&p, "- ", "<p class=\"foo\">hi</p>"), "got:\n{p}");
        assert!(has(&p, "+ ", "<p class=\"bar\">hi</p>"), "got:\n{p}");
    }

    #[test]
    fn tag_change_shows_both_tags() {
        let p = patch("<div><span>x</span></div>", "<div><b>x</b></div>");
        assert!(has(&p, "- ", "<span>x</span>"), "got:\n{p}");
        assert!(has(&p, "+ ", "<b>x</b>"), "got:\n{p}");
    }

    #[test]
    fn extra_child_shows_added_line() {
        let p = patch("<ul><li>a</li></ul>", "<ul><li>a</li><li>b</li></ul>");
        assert!(has(&p, "+ ", "<li>b</li>"), "got:\n{p}");
    }

    #[test]
    fn text_change_diffs_the_text_line() {
        let p = patch("<p>hello</p>", "<p>world</p>");
        assert!(has(&p, "- ", "hello"), "got:\n{p}");
        assert!(has(&p, "+ ", "world"), "got:\n{p}");
    }

    #[test]
    fn whole_page_shows_every_change() {
        // Both paragraphs differ; one patch covers them all (no per-file cap).
        let p = patch(
            "<div><p>aaa</p><p>bbb</p></div>",
            "<div><p>XXX</p><p>YYY</p></div>",
        );
        assert!(has(&p, "- ", "aaa") && has(&p, "+ ", "XXX"), "got:\n{p}");
        assert!(has(&p, "- ", "bbb") && has(&p, "+ ", "YYY"), "got:\n{p}");
    }

    #[test]
    fn context_keeps_above_and_below_lines() {
        let exp = "<ul><li>a</li><li>b</li><li>c</li><li>d</li><li>e</li></ul>";
        let act = "<ul><li>a</li><li>b</li><li>Z</li><li>d</li><li>e</li></ul>";
        // 1 line of context each side of the changed <li>c → <li>Z.
        let p = patch_ctx(exp, act, 1, 1);
        assert!(
            has(&p, "- ", "<li>c</li>") && has(&p, "+ ", "<li>Z</li>"),
            "got:\n{p}"
        );
        assert!(p.contains("<li>b</li>"), "1 line above should show:\n{p}");
        assert!(p.contains("<li>d</li>"), "1 line below should show:\n{p}");
        assert!(!p.contains("<li>a</li>"), "2 above should collapse:\n{p}");
        assert!(!p.contains("<li>e</li>"), "2 below should collapse:\n{p}");
    }

    #[test]
    fn above_and_below_are_independent() {
        let exp = "<ul><li>a</li><li>b</li><li>c</li><li>d</li><li>e</li></ul>";
        let act = "<ul><li>a</li><li>b</li><li>Z</li><li>d</li><li>e</li></ul>";
        // 2 lines above, 0 below.
        let p = patch_ctx(exp, act, 2, 0);
        assert!(p.contains("<li>a</li>"), "2 above should show:\n{p}");
        assert!(p.contains("<li>b</li>"), "1 above should show:\n{p}");
        assert!(
            !p.contains("<li>d</li>"),
            "below=0 should hide the next line:\n{p}"
        );
    }

    #[test]
    fn falls_back_to_reason_when_render_is_identical() {
        use crate::html::{Element, Node};
        use std::collections::BTreeMap;
        let el = |name: &str, children| {
            Node::Element(Element {
                name: name.into(),
                attrs: BTreeMap::new(),
                children,
            })
        };
        let p = el("p", vec![Node::Text("x".into())]);
        // A count mismatch whose only extra child is whitespace text the
        // pretty-printer drops — so both sides render to identical lines.
        let exp = Node::Document(vec![el("div", vec![p.clone(), Node::Text("  ".into())])]);
        let act = Node::Document(vec![el("div", vec![p])]);
        let d = diff(&exp, &act).expect_err("expected a divergence");
        let out = d.render_patch(&exp, &act, 1, 1);
        assert!(!out.trim().is_empty(), "fallback must not be empty");
        assert!(
            out.contains("child count"),
            "should show the reason, got:\n{out}"
        );
    }

    #[test]
    fn zero_context_shows_only_the_change() {
        let exp = "<ul><li>a</li><li>b</li><li>c</li></ul>";
        let act = "<ul><li>a</li><li>Z</li><li>c</li></ul>";
        // 0/0: the change shows, no surrounding context, elided lines marked.
        let p = patch_ctx(exp, act, 0, 0);
        assert!(
            has(&p, "- ", "<li>b</li>") && has(&p, "+ ", "<li>Z</li>"),
            "got:\n{p}"
        );
        assert!(!p.contains("<li>a</li>"), "no context expected, got:\n{p}");
        assert!(!p.contains("<li>c</li>"), "no context expected, got:\n{p}");
        assert!(p.contains("⋮"), "elision marker missing, got:\n{p}");
    }
}
