//! Unified-diff rendering of a [`Divergence`].
//!
//! The comparator reports *where* (a path) and *what kind* (a reason) of the
//! first divergence, but a path + message alone forces you to cross-read both
//! HTML files to see the cause. This module renders the divergent subtree from
//! each tree as indented lines and runs a line-based LCS diff, producing a
//! familiar `-`/`+` patch hunk with a few lines of context.
//!
//! Rendered at the diff site (where both trees are in hand) since a
//! [`Divergence`] doesn't retain them.

use std::collections::HashMap;
use std::fmt::Write as _;

use owo_colors::{OwoColorize, Stream};

use crate::diff::compare::{Divergence, PathStep};
use crate::html::{Element, Node};

/// `convert`'s report prints to stdout; color follows that stream's tty.
const OUT: Stream = Stream::Stdout;
/// Unchanged lines of context kept around each change.
const CONTEXT: usize = 3;
/// Truncate any single rendered line past this many chars.
const MAX_LINE: usize = 200;
/// Cap each side's rendering so a divergence high in the tree can't dump the
/// whole document (the LCS is also quadratic, so this bounds work too).
const MAX_SIDE_LINES: usize = 400;

impl Divergence {
    /// A `-`/`+` unified-diff hunk of the divergent subtree (`-` expected,
    /// `+` actual) — far more legible than the path + reason that
    /// [`Display`](std::fmt::Display) gives.
    pub fn render_patch(&self, expected_root: &Node, actual_root: &Node) -> String {
        // Locate the divergent subtree in both trees by recovering the raw
        // child indices from the expected tree (the per-tag/per-kind path steps
        // were assigned against it), then applying the same indices to actual —
        // valid because every level above the first divergence matched.
        path_to_raw(expected_root, &self.path)
            .and_then(|raw| {
                let exp = node_at(expected_root, &raw)?;
                let act = node_at(actual_root, &raw)?;
                Some(unified(&render_side(exp), &render_side(act)).join("\n"))
            })
            // Navigation shouldn't fail, but never hide a divergence if it
            // does — fall back to the bare path + reason.
            .unwrap_or_else(|| self.to_string())
    }
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

/// Render a node to indented lines (one tag / text run per line), capped at
/// [`MAX_SIDE_LINES`].
fn render_side(node: &Node) -> Vec<String> {
    let mut out = Vec::new();
    push_node(node, 0, &mut out);
    if out.len() > MAX_SIDE_LINES {
        out.truncate(MAX_SIDE_LINES);
        out.push("…".to_string());
    }
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

/// LCS line alignment of two rendered sides.
fn diff_ops<'a>(a: &'a [String], b: &'a [String]) -> Vec<Op<'a>> {
    let (n, m) = (a.len(), b.len());
    // lcs[i][j] = LCS length of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
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

/// Format the ops as a colored unified hunk: changes plus [`CONTEXT`] lines of
/// surrounding context, with long unchanged runs collapsed to `⋮`.
fn unified(a: &[String], b: &[String]) -> Vec<String> {
    let ops = diff_ops(a, b);

    // Mark every op within CONTEXT of a change as "shown".
    let changed: Vec<bool> = ops
        .iter()
        .map(|o| !matches!(o, Op::Eq(_)))
        .collect();
    let show: Vec<bool> = (0..ops.len())
        .map(|i| {
            let lo = i.saturating_sub(CONTEXT);
            let hi = (i + CONTEXT + 1).min(ops.len());
            changed[lo..hi].iter().any(|c| *c)
        })
        .collect();

    let mut out = Vec::new();
    let mut skipping = false;
    for (i, op) in ops.iter().enumerate() {
        if !show[i] {
            if !skipping && !out.is_empty() {
                out.push(dim("  ⋮"));
            }
            skipping = true;
            continue;
        }
        skipping = false;
        out.push(match op {
            Op::Eq(l) => dim(&format!("  {l}")),
            Op::Del(l) => format!("- {l}").if_supports_color(OUT, |t| t.red()).to_string(),
            Op::Ins(l) => format!("+ {l}").if_supports_color(OUT, |t| t.green()).to_string(),
        });
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

    /// Diff two fragments and render the patch. Stdout is captured in tests, so
    /// owo emits no color — assertions match plain `-`/`+` lines.
    fn patch(expected: &str, actual: &str) -> String {
        let e = parse_html(expected);
        let a = parse_html(actual);
        let d = diff(&e, &a, &RelaxConfig::new()).expect_err("expected a divergence");
        d.render_patch(&e, &a)
    }

    #[test]
    fn attr_change_is_a_minus_plus_pair() {
        let p = patch(
            "<div><p class=\"foo\">hi</p></div>",
            "<div><p class=\"bar\">hi</p></div>",
        );
        assert!(p.contains("- <p class=\"foo\">hi</p>"), "got:\n{p}");
        assert!(p.contains("+ <p class=\"bar\">hi</p>"), "got:\n{p}");
    }

    #[test]
    fn tag_change_shows_both_tags() {
        let p = patch("<div><span>x</span></div>", "<div><b>x</b></div>");
        assert!(p.contains("- <span>x</span>"), "got:\n{p}");
        assert!(p.contains("+ <b>x</b>"), "got:\n{p}");
    }

    #[test]
    fn extra_child_shows_added_line_with_context() {
        let p = patch(
            "<ul><li>a</li></ul>",
            "<ul><li>a</li><li>b</li></ul>",
        );
        assert!(p.contains("  <ul>"), "context tag missing, got:\n{p}");
        assert!(p.contains("+   <li>b</li>"), "got:\n{p}");
    }

    #[test]
    fn text_change_diffs_the_text_line() {
        let p = patch("<p>hello</p>", "<p>world</p>");
        assert!(p.contains("- hello"), "got:\n{p}");
        assert!(p.contains("+ world"), "got:\n{p}");
    }
}
