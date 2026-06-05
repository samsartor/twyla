//! Subtree selection on parsed HTML.
//!
//! Isolates the relevant region of a page before diffing — e.g. "match just
//! the post body."
//!
//! Selector grammar today is intentionally minimal:
//!
//! - `class:<token>` — first element whose `class` attribute contains
//!   `<token>` (after the parser's class-token sort).
//! - `tag:<name>`    — first element with the given tag name.
//!
//! First-match wins, document-order. Returns the children of the matched
//! element (i.e. inner content); the wrapper itself is dropped so that both
//! sides of a porting comparison can use different wrapper selectors but
//! still diff the same inner fragment.

use crate::html::{Element, Node};

/// A subtree selector. Parse from a `"<kind>:<value>"` string.
#[derive(Debug, Clone)]
pub enum Selector {
    Class(String),
    Tag(String),
}

impl Selector {
    /// Parse a selector string. Returns `None` for unknown forms.
    pub fn parse(s: &str) -> Option<Self> {
        let (kind, value) = s.split_once(':')?;
        match kind {
            "class" => Some(Self::Class(value.to_string())),
            "tag" => Some(Self::Tag(value.to_string())),
            _ => None,
        }
    }

    fn matches(&self, el: &Element) -> bool {
        match self {
            Selector::Tag(t) => el.name == *t,
            Selector::Class(token) => el
                .attrs
                .get("class")
                .map(|cs| cs.split_ascii_whitespace().any(|c| c == token))
                .unwrap_or(false),
        }
    }
}

/// Walk the tree, return the children of the first element matching `sel`.
/// Returns `None` if nothing matches.
pub fn find_inner<'a>(root: &'a Node, sel: &Selector) -> Option<&'a [Node]> {
    walk(root, sel)
}

fn walk<'a>(node: &'a Node, sel: &Selector) -> Option<&'a [Node]> {
    match node {
        Node::Document(children) => children.iter().find_map(|c| walk(c, sel)),
        Node::Element(el) => {
            if sel.matches(el) {
                return Some(&el.children);
            }
            el.children.iter().find_map(|c| walk(c, sel))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::parse::parse_html;

    #[test]
    fn class_match_returns_inner() {
        let n = parse_html(
            "<div><section class=\"a b\"><p>hi</p></section></div>",
        );
        let inner = find_inner(&n, &Selector::Class("b".into())).unwrap();
        assert_eq!(inner.len(), 1);
        assert!(matches!(&inner[0], Node::Element(e) if e.name == "p"));
    }

    #[test]
    fn tag_match_returns_inner() {
        let n = parse_html("<div><main><p>x</p></main></div>");
        let inner = find_inner(&n, &Selector::Tag("main".into())).unwrap();
        assert_eq!(inner.len(), 1);
    }

    #[test]
    fn first_match_wins() {
        let n = parse_html("<div class=\"x\">a</div><div class=\"x\">b</div>");
        let inner = find_inner(&n, &Selector::Class("x".into())).unwrap();
        assert!(matches!(&inner[0], Node::Text(t) if t == "a"));
    }

    #[test]
    fn no_match_returns_none() {
        let n = parse_html("<div><p>hi</p></div>");
        assert!(find_inner(&n, &Selector::Class("missing".into())).is_none());
    }

    #[test]
    fn parse_selector() {
        assert!(matches!(Selector::parse("class:foo"), Some(Selector::Class(c)) if c == "foo"));
        assert!(matches!(Selector::parse("tag:body"), Some(Selector::Tag(t)) if t == "body"));
        assert!(Selector::parse("invalid").is_none());
        assert!(Selector::parse("xpath://foo").is_none());
    }
}
