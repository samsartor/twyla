//! Normalized HTML tree — shared parsing/DOM infrastructure.
//!
//! [`parse_html`] turns an HTML string into a normalized [`Node`] tree;
//! [`serialize`] turns one back into a string. Several consumers read that
//! tree:
//!
//! - [`crate::diff`] — structural AST comparison for the porting harness.
//! - [`links`] — link discovery/classification/resolution for the convert audit.
//! - [`select`] — subtree selection.
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

pub mod links;
pub mod parse;
pub mod select;
pub mod serialize;

use std::collections::BTreeMap;

pub use links::{LinkClass, UrlRef, extract, resolve};
pub use parse::parse_html;
pub use select::{Selector, find_inner};
pub use serialize::{serialize, serialize_fragment};

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
/// in the tree. Used by the convert harness to undo zola's anchor-only-link
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
