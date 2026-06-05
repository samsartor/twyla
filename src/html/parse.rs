//! HTML → normalized [`Node`] via html5ever.

use std::collections::BTreeMap;
use std::default::Default;

use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use crate::html::{Element, Node};

/// Tags whose text content is preserved verbatim — no whitespace
/// collapsing, no dropping of pure-whitespace children.
const PRESERVE_TEXT_TAGS: &[&str] = &["pre", "code", "script", "style", "textarea"];

/// Subset of `PRESERVE_TEXT_TAGS` whose pretty-printer artifact whitespace
/// is dropped at parse time. `<script>`/`<style>` bodies are either external
/// (src/href) and meant to be empty, or inline code where surrounding
/// whitespace is semantically irrelevant. `<pre>`/`<code>`/`<textarea>` are
/// not on this list — inter-token whitespace inside syntax-highlighted code
/// is significant and must survive.
const STRIP_WHITESPACE_NODES_TAGS: &[&str] = &["script", "style"];

/// Parse an HTML document string into a normalized [`Node::Document`].
///
/// Panics on malformed UTF-8. html5ever itself is permissive and will not
/// fail on malformed HTML — it recovers per the spec.
pub fn parse_html(s: &str) -> Node {
    let dom: RcDom = parse_document(
        RcDom::default(),
        ParseOpts {
            tree_builder: TreeBuilderOpts {
                drop_doctype: false,
                // Parse `<noscript>` contents as HTML elements rather than
                // text. Otherwise zola's pretty-formatted `<link>` siblings
                // and typst's tightly-packed siblings produce diverging text
                // nodes despite being structurally identical.
                scripting_enabled: false,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .from_utf8()
    .read_from(&mut s.as_bytes())
    .expect("html5ever parse should not fail on UTF-8 input");

    let mut children = Vec::new();
    for child in dom.document.children.borrow().iter() {
        if let Some(n) = convert(child, false) {
            children.push(n);
        }
    }
    Node::Document(filter_block_whitespace(children))
}

fn convert(handle: &Handle, preserve_text: bool) -> Option<Node> {
    match &handle.data {
        NodeData::Doctype { name, .. } => Some(Node::Doctype(name.to_string())),
        NodeData::Text { contents } => {
            let raw = contents.borrow().to_string();
            if preserve_text {
                Some(Node::Text(raw))
            } else {
                let norm = normalize_whitespace(&raw);
                Some(Node::Text(norm))
            }
        }
        // Comments dropped at parse time. Treating them as significant
        // would force every porting test to match comment-by-comment, which
        // is rarely what's wanted — and we already round-trip via html5ever
        // (no comment preservation guarantee from the typst side anyway).
        NodeData::Comment { .. } => None,
        NodeData::Element { name, attrs, .. } => {
            let tag = name.local.to_string().to_ascii_lowercase();
            let new_preserve = preserve_text || PRESERVE_TEXT_TAGS.contains(&tag.as_str());

            let mut attr_map: BTreeMap<String, String> = BTreeMap::new();
            for attr in attrs.borrow().iter() {
                let key = attr.name.local.to_string().to_ascii_lowercase();
                let mut value = attr.value.to_string();
                if key == "class" {
                    let mut tokens: Vec<&str> = value.split_ascii_whitespace().collect();
                    tokens.sort_unstable();
                    value = tokens.join(" ");
                }
                attr_map.insert(key, value);
            }

            let mut children = Vec::new();
            for child in handle.children.borrow().iter() {
                if let Some(c) = convert(child, new_preserve) {
                    children.push(c);
                }
            }

            // Non-preserve: drop text nodes that became empty after the
            // whitespace collapse above. `<script>`/`<style>`: drop pure-
            // whitespace text — typst's pretty-printer wraps even empty
            // `<script src="...">` bodies in a newline+indent, but zola
            // emits them as bare `<script></script>`. Other preserve-text
            // tags (`<pre>`, `<code>`, `<textarea>`) keep all text including
            // inter-token spaces, since syntax-highlighted code blocks emit
            // significant whitespace between adjacent `<span>` tokens.
            let children = if STRIP_WHITESPACE_NODES_TAGS.contains(&tag.as_str()) {
                filter_pure_whitespace_text(children)
            } else if !new_preserve {
                filter_block_whitespace(children)
            } else {
                children
            };

            Some(Node::Element(Element {
                name: tag,
                attrs: attr_map,
                children,
            }))
        }
        NodeData::Document | NodeData::ProcessingInstruction { .. } => None,
    }
}

/// Collapse runs of whitespace to a single space, then trim.
fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            in_ws = false;
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Drop text nodes that are entirely whitespace (or, after normalization,
/// the empty string). Already-significant text is unaffected.
fn filter_block_whitespace(nodes: Vec<Node>) -> Vec<Node> {
    nodes
        .into_iter()
        .filter(|n| match n {
            Node::Text(t) => !t.is_empty(),
            _ => true,
        })
        .collect()
}

/// Drop text nodes whose raw content is 100% ASCII whitespace. Significant
/// text (anything containing non-whitespace) is left untouched, including
/// its surrounding whitespace. Used inside preserve-text tags where the
/// distinction matters.
fn filter_pure_whitespace_text(nodes: Vec<Node>) -> Vec<Node> {
    nodes
        .into_iter()
        .filter(|n| match n {
            Node::Text(t) => t.chars().any(|c| !c.is_whitespace()),
            _ => true,
        })
        .collect()
}
