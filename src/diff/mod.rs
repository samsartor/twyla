//! HTML AST-equivalence harness — the porting feedback loop.
//!
//! Parses two HTML strings into a normalized internal tree, walks both in
//! lockstep, and reports the first structural divergence. Built for use as
//! both a CLI (`twyla-diff`) and as a library when integrating into the
//! eventual build pipeline.
//!
//! Default normalizations applied at parse time:
//!
//! - tag / attribute names lowercased
//! - attributes sorted by name
//! - `class` attribute tokens sorted alphabetically
//! - whitespace runs in text nodes collapsed to a single space (then trimmed)
//! - pure-whitespace text nodes dropped
//! - content inside `<pre>`, `<code>`, `<script>`, `<style>`, `<textarea>` is
//!   preserved verbatim — none of the above normalizations apply
//!
//! Beyond that baseline, *relaxation rules* (see [`relax`]) let the caller
//! ignore specific attributes, skip subtrees, or compare an element by text
//! content only. The framework is data-driven — adding a new relaxation is a
//! 5-line change to a `RelaxConfig`. The default config has *no* rules: behavior
//! out of the box is the strictest comparison the normalization permits.

pub mod compare;
pub mod parse;
pub mod relax;
pub mod select;
pub mod serialize;

use std::collections::BTreeMap;

pub use compare::{Divergence, DivergenceReason, PathStep, diff};
pub use parse::parse_html;
pub use relax::{Matcher, RelaxConfig, RelaxationRule};
pub use select::{Selector, find_inner};
pub use serialize::{serialize, serialize_fragment};

// Re-exported as a free function for callers that want to mutate a
// parsed tree in place (e.g. cmd_check undoing zola's absolutization).
// Defined in this module body below.

/// A node in the normalized HTML tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Top-level document wrapper. Children include doctype + `<html>`.
    Document(Vec<Node>),
    /// `<!DOCTYPE name>`. Captured as the bare name.
    Doctype(String),
    Element(Element),
    /// Text content, already normalized per the parse-time rules.
    Text(String),
    /// `<!-- ... -->`. Currently dropped at parse time, so this variant
    /// is unreachable from `parse_html`; kept in the enum so future
    /// callers can construct trees that retain comments if needed.
    Comment(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    /// Sorted by name. `class` attribute values have their tokens sorted.
    pub attrs: BTreeMap<String, String>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Node::Document(_) => "document",
            Node::Doctype(_) => "doctype",
            Node::Element(_) => "element",
            Node::Text(_) => "text",
            Node::Comment(_) => "comment",
        }
    }
}

/// Rewrite `<a href="<prefix>FRAG">` to `<a href="#FRAG">` everywhere
/// in the tree. Used by `cmd_check` to undo zola's anchor-only-link
/// absolutization (`[text](#frag)` → `<a href="<base>/<slug>/#frag">`)
/// so the strict diff matches typst's label-based form (`#link(<frag>)`
/// → `<a href="#frag">`). Both resolve to the same target.
pub fn rewrite_own_page_anchor_hrefs(node: &mut Node, prefix: &str) {
    match node {
        Node::Element(el) => {
            if el.name == "a" {
                if let Some(href) = el.attrs.get("href") {
                    if let Some(frag) = href.strip_prefix(prefix) {
                        let new_href = format!("#{frag}");
                        el.attrs.insert("href".to_string(), new_href);
                    }
                }
            }
            for child in &mut el.children {
                rewrite_own_page_anchor_hrefs(child, prefix);
            }
        }
        Node::Document(children) => {
            for child in children {
                rewrite_own_page_anchor_hrefs(child, prefix);
            }
        }
        _ => {}
    }
}
