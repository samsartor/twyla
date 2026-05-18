//! Normalized [`Node`] → HTML string.
//!
//! Counterpart to `parse_html`. The roundtrip
//! `parse_html → serialize → parse_html` is idempotent, since both ends
//! apply the same normalization rules. That's what makes
//! `twyla-extract | twyla-diff` work: the extracted fragment can be
//! re-parsed and compared without losing meaning.
//!
//! This is not a faithful HTML producer; it doesn't preserve original
//! whitespace, attribute order, or comments. Use it for diffing flows
//! only, not for serving HTML to browsers.

use crate::diff::{Element, Node};

/// Serialize a sequence of nodes (e.g. a fragment) as HTML.
pub fn serialize_fragment(nodes: &[Node]) -> String {
    let mut out = String::new();
    for n in nodes {
        write_node(n, &mut out);
    }
    out
}

/// Serialize a whole document or single node.
pub fn serialize(node: &Node) -> String {
    let mut out = String::new();
    write_node(node, &mut out);
    out
}

fn write_node(node: &Node, out: &mut String) {
    match node {
        Node::Document(children) => {
            for c in children {
                write_node(c, out);
            }
        }
        Node::Doctype(name) => {
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            out.push('>');
        }
        Node::Element(el) => write_element(el, out),
        Node::Text(t) => write_text(t, out),
        Node::Comment(c) => {
            out.push_str("<!--");
            out.push_str(c);
            out.push_str("-->");
        }
    }
}

fn write_element(el: &Element, out: &mut String) {
    out.push('<');
    out.push_str(&el.name);
    for (k, v) in &el.attrs {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        write_attr_value(v, out);
        out.push('"');
    }

    if is_void(&el.name) {
        out.push_str(" />");
        return;
    }

    out.push('>');

    if is_raw_text(&el.name) {
        // <script>/<style>: bodies must round-trip verbatim. They're a
        // text child in our tree thanks to parse_html. Anything else
        // would be a malformed tree and we just skip it.
        for c in &el.children {
            if let Node::Text(t) = c {
                out.push_str(t);
            }
        }
    } else {
        for c in &el.children {
            write_node(c, out);
        }
    }

    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
}

fn write_text(t: &str, out: &mut String) {
    for c in t.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(c),
        }
    }
}

fn write_attr_value(v: &str, out: &mut String) {
    for c in v.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(c),
        }
    }
}

/// HTML5 void elements — no closing tag, no body.
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

fn is_raw_text(name: &str) -> bool {
    matches!(name, "script" | "style")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse::parse_html;

    fn roundtrip(s: &str) -> String {
        serialize(&parse_html(s))
    }

    #[test]
    fn idempotent_after_first_roundtrip() {
        // Roundtrip-of-roundtrip should be a fixed point.
        let once = roundtrip("<div class=\"b a\"><p>hi  there</p></div>");
        let twice = roundtrip(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn void_elements_self_close() {
        let s = roundtrip("<div><br><hr></div>");
        assert!(s.contains("<br />"));
        assert!(s.contains("<hr />"));
    }

    #[test]
    fn escapes_text() {
        let s = roundtrip("<p>a &lt; b &amp; c</p>");
        // After parse, text becomes "a < b & c"; after serialize, escaped again.
        assert!(s.contains("a &lt; b &amp; c"));
    }

    #[test]
    fn pre_preserves_whitespace() {
        let s = roundtrip("<pre>line one\n  indented</pre>");
        assert!(s.contains("line one\n  indented"));
    }
}
