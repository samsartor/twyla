//! Unified-diff rendering of a [`Divergence`].
//!
//! The comparator reports *where* (a path) and *what kind* (a reason) of the
//! first divergence, but a path + message alone forces you to cross-read both
//! HTML files to see the cause. This module renders the divergent node — framed
//! by its parent and a window of sibling context — as indented lines from each
//! tree and runs a line-based LCS diff, producing a familiar `-`/`+` patch hunk.
//!
//! Rendered at the diff site (where both trees are in hand) since a
//! [`Divergence`] doesn't retain them.

use std::collections::HashMap;
use std::fmt::Write as _;

use owo_colors::{OwoColorize, Stream};

use crate::diff::compare::{Divergence, DivergenceReason, PathStep};
use crate::html::{Element, Node};

/// `convert`'s report prints to stdout; color follows that stream's tty.
const OUT: Stream = Stream::Stdout;
/// Unchanged lines of context kept around each change.
const CONTEXT: usize = 3;
/// Truncate any single rendered line past this many chars.
const MAX_LINE: usize = 200;
/// Cost ceiling for the (quadratic) line LCS: `expected.len() * actual.len()`.
/// Above this a page is too large to patch-render, and we fall back to the
/// textual reason. Sized so whole real pages diff fine (~4000 lines/side).
const MAX_DIFF_CELLS: usize = 16_000_000;

impl Divergence {
    /// A `-`/`+` unified-diff hunk of the divergence (`-` expected, `+`
    /// actual) — far more legible than the path + reason that
    /// [`Display`](std::fmt::Display) gives.
    ///
    /// `above`/`below` are how many sibling nodes of context to show before and
    /// after the divergent node (the parent frame is always shown, which also
    /// covers the "no sibling on that side" case).
    pub fn render_patch(
        &self,
        expected_root: &Node,
        actual_root: &Node,
        above: usize,
        below: usize,
    ) -> String {
        self.try_render(expected_root, actual_root, above, below)
            // Navigation shouldn't fail, but never hide a divergence if it
            // does — fall back to the bare path + reason.
            .unwrap_or_else(|| self.to_string())
    }

    fn try_render(
        &self,
        expected_root: &Node,
        actual_root: &Node,
        above: usize,
        below: usize,
    ) -> Option<String> {
        // Recover the raw child indices from the expected tree (the path's
        // per-tag/per-kind steps were assigned against it), then apply the same
        // indices to actual — valid because every level above the first
        // divergence matched.
        let raw = path_to_raw(expected_root, &self.path)?;

        // Sibling-window mode: the divergence is a specific child of a parent
        // (not a whole-children count mismatch, which has no single culprit).
        // Diff the parent's open tag + a window of siblings around the culprit.
        if let Some((&idx, parent_raw)) = raw.split_last() {
            if !matches!(self.reason, DivergenceReason::ChildCountMismatch { .. }) {
                let exp = node_at(expected_root, parent_raw)?;
                let act = node_at(actual_root, parent_raw)?;
                return render_hunk(
                    &window_lines(exp, idx, above, below),
                    &window_lines(act, idx, above, below),
                );
            }
        }

        // Fallback: diff the whole subtree at the path (document root, or a
        // child-count mismatch).
        let exp = node_at(expected_root, &raw)?;
        let act = node_at(actual_root, &raw)?;
        render_hunk(&render_side(exp), &render_side(act))
    }
}

/// Diff two rendered sides into a trimmed `-`/`+` hunk. Returns `None` (so the
/// caller falls back to the textual reason) when either the renderings are
/// identical — the pretty-printer normalizes some differences away (whitespace
/// text, comments, count mismatches on those), and an empty hunk would print as
/// a bare route line — or the inputs are too large to diff affordably.
fn render_hunk(expected: &[String], actual: &[String]) -> Option<String> {
    if expected.len().saturating_mul(actual.len()) > MAX_DIFF_CELLS {
        return None;
    }
    let ops = diff_ops(expected, actual);
    if !ops.iter().any(|o| !matches!(o, Op::Eq(_))) {
        return None;
    }
    Some(format_ops(&ops, CONTEXT).join("\n"))
}

/// Render a parent's open tag, the children in `[idx-above ..= idx+below]`
/// (clamped to the child list), and its close tag. The parent frame orients
/// the hunk and stands in as context when the culprit has no sibling on a side.
/// Elided siblings outside the window are marked with `⋮`.
fn window_lines(parent: &Node, idx: usize, above: usize, below: usize) -> Vec<String> {
    let children = children_of(parent).unwrap_or(&[]);
    let mut out = Vec::new();

    if let Node::Element(el) = parent {
        out.push(open_tag(el));
    }
    if children.is_empty() {
        if let Node::Element(el) = parent {
            out.push(format!("</{}>", el.name));
        }
        return out;
    }

    let lo = idx.saturating_sub(above);
    let hi = (idx + below).min(children.len() - 1);
    if lo > 0 {
        out.push("  ⋮".to_string());
    }
    for child in &children[lo..=hi] {
        push_node(child, 1, &mut out);
    }
    if hi + 1 < children.len() {
        out.push("  ⋮".to_string());
    }
    if let Node::Element(el) = parent {
        out.push(format!("</{}>", el.name));
    }
    out
}

// ---- tree navigation -----------------------------------------------------

fn children_of(node: &Node) -> Option<&[Node]> {
    match node {
        Node::Document(c) => Some(c),
        Node::Element(e) => Some(&e.children),
        _ => None,
    }
}

/// Counters mirroring `compare::compare_children`'s step assignment, so a
/// [`PathStep`] resolves to the raw child index it was minted from.
#[derive(Default)]
struct Counters {
    tags: HashMap<String, usize>,
    text: usize,
    comment: usize,
    doctype: usize,
}

impl Counters {
    fn matches(&self, step: &PathStep, child: &Node) -> bool {
        match (step, child) {
            (PathStep::Child { tag, index }, Node::Element(el)) => {
                el.name == *tag && self.tags.get(&el.name).copied().unwrap_or(0) == *index
            }
            (PathStep::Text(i), Node::Text(_)) => self.text == *i,
            (PathStep::Comment(i), Node::Comment(_)) => self.comment == *i,
            (PathStep::Doctype(i), Node::Doctype(_)) => self.doctype == *i,
            _ => false,
        }
    }

    fn advance(&mut self, child: &Node) {
        match child {
            Node::Element(el) => *self.tags.entry(el.name.clone()).or_insert(0) += 1,
            Node::Text(_) => self.text += 1,
            Node::Comment(_) => self.comment += 1,
            Node::Doctype(_) => self.doctype += 1,
            Node::Document(_) => {}
        }
    }
}

/// Convert a path of [`PathStep`]s into raw child indices, navigating `root`.
fn path_to_raw(root: &Node, path: &[PathStep]) -> Option<Vec<usize>> {
    let mut raw = Vec::with_capacity(path.len());
    let mut node = root;
    for step in path {
        let children = children_of(node)?;
        let mut counters = Counters::default();
        let mut found = None;
        for (i, child) in children.iter().enumerate() {
            if counters.matches(step, child) {
                found = Some(i);
                break;
            }
            counters.advance(child);
        }
        let i = found?;
        raw.push(i);
        node = &children[i];
    }
    Some(raw)
}

/// Follow raw child indices to a node.
fn node_at<'a>(root: &'a Node, raw: &[usize]) -> Option<&'a Node> {
    let mut node = root;
    for &i in raw {
        node = children_of(node)?.get(i)?;
    }
    Some(node)
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
/// lines: changes, plus `ctx` lines of context around each. Unchanged runs
/// beyond that — and the interior of long same-kind change runs (a wholesale
/// added/removed subtree) — collapse to a `⋮ (n lines)` marker.
fn format_ops(ops: &[Op], ctx: usize) -> Vec<String> {
    let n = ops.len();
    let mut keep = vec![true; n];

    // Drop unchanged lines that aren't within `ctx` of a change.
    for i in 0..n {
        if matches!(ops[i], Op::Eq(_)) {
            let lo = i.saturating_sub(ctx);
            let hi = (i + ctx + 1).min(n);
            keep[i] = ops[lo..hi].iter().any(|o| !matches!(o, Op::Eq(_)));
        }
    }

    // Collapse the middle of long runs of same-kind changes, keeping `ctx` at
    // each end — e.g. a 500-line removed SVG shows its first and last few lines.
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
        if i - start > 2 * ctx + 1 {
            for k in &mut keep[start + ctx..i - ctx] {
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
            Op::Del(l) => format!("- {l}").if_supports_color(OUT, |t| t.red()).to_string(),
            Op::Ins(l) => format!("+ {l}").if_supports_color(OUT, |t| t.green()).to_string(),
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
    use crate::diff::{RelaxConfig, diff};
    use crate::html::parse_html;

    /// Diff two fragments and render the patch (default 1 sibling of context).
    /// Stdout is captured in tests, so owo emits no color — assertions match
    /// plain `-`/`+` lines.
    fn patch(expected: &str, actual: &str) -> String {
        patch_ctx(expected, actual, 1, 1)
    }

    fn patch_ctx(expected: &str, actual: &str, above: usize, below: usize) -> String {
        let e = parse_html(expected);
        let a = parse_html(actual);
        let d = diff(&e, &a, &RelaxConfig::new()).expect_err("expected a divergence");
        d.render_patch(&e, &a, above, below)
    }

    #[test]
    fn attr_change_is_a_minus_plus_pair() {
        let p = patch(
            "<div><p class=\"foo\">hi</p></div>",
            "<div><p class=\"bar\">hi</p></div>",
        );
        assert!(p.contains("<div>"), "parent frame missing, got:\n{p}");
        assert!(p.contains("-   <p class=\"foo\">hi</p>"), "got:\n{p}");
        assert!(p.contains("+   <p class=\"bar\">hi</p>"), "got:\n{p}");
    }

    #[test]
    fn tag_change_shows_both_tags() {
        let p = patch("<div><span>x</span></div>", "<div><b>x</b></div>");
        assert!(p.contains("-   <span>x</span>"), "got:\n{p}");
        assert!(p.contains("+   <b>x</b>"), "got:\n{p}");
    }

    #[test]
    fn extra_child_shows_added_line_with_context() {
        let p = patch("<ul><li>a</li></ul>", "<ul><li>a</li><li>b</li></ul>");
        assert!(p.contains("  <ul>"), "context tag missing, got:\n{p}");
        assert!(p.contains("+   <li>b</li>"), "got:\n{p}");
    }

    #[test]
    fn text_change_diffs_the_text_line() {
        let p = patch("<p>hello</p>", "<p>world</p>");
        assert!(p.contains("-   hello"), "got:\n{p}");
        assert!(p.contains("+   world"), "got:\n{p}");
    }

    #[test]
    fn sibling_context_shows_neighbors() {
        let exp = "<ul><li>a</li><li class=\"x\">b</li><li>c</li></ul>";
        let act = "<ul><li>a</li><li class=\"y\">b</li><li>c</li></ul>";
        // Default 1/1: the preceding and following <li> show as context.
        let p = patch(exp, act);
        assert!(p.contains("<li>a</li>"), "above sibling missing, got:\n{p}");
        assert!(p.contains("<li>c</li>"), "below sibling missing, got:\n{p}");
        assert!(p.contains("-   <li class=\"x\">b</li>"), "got:\n{p}");
        assert!(p.contains("+   <li class=\"y\">b</li>"), "got:\n{p}");
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
        let d = diff(&exp, &act, &RelaxConfig::new()).expect_err("expected a divergence");
        let out = d.render_patch(&exp, &act, 1, 1);
        assert!(!out.trim().is_empty(), "fallback must not be empty");
        assert!(out.contains("child count"), "should show the reason, got:\n{out}");
    }

    #[test]
    fn zero_context_hides_siblings_but_keeps_frame() {
        let exp = "<ul><li>a</li><li class=\"x\">b</li><li>c</li></ul>";
        let act = "<ul><li>a</li><li class=\"y\">b</li><li>c</li></ul>";
        let p = patch_ctx(exp, act, 0, 0);
        assert!(p.contains("<ul>"), "parent frame missing, got:\n{p}");
        assert!(!p.contains("<li>a</li>"), "should hide siblings, got:\n{p}");
        assert!(!p.contains("<li>c</li>"), "should hide siblings, got:\n{p}");
        assert!(p.contains("⋮"), "elision marker missing, got:\n{p}");
    }
}

