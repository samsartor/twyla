//! Relaxation rules — opt-in exceptions to the default strict comparison.
//!
//! A `RelaxConfig` is a list of `(Matcher, RelaxationRule)` pairs evaluated in
//! order; the first matching rule wins. Adding a new relaxation is a single
//! `.relax(matcher, rule)` builder call.
//!
//! *Default config has no rules.* New relaxations should be added explicitly
//! (and ideally discussed before becoming a default) so the porting harness
//! stays honest about where twyla and zola diverge.

use std::collections::BTreeMap;

use crate::html::{Element, Node};

/// A list of relaxation rules. Constructed via the builder methods.
#[derive(Debug, Clone, Default)]
pub struct RelaxConfig {
    rules: Vec<(Matcher, RelaxationRule)>,
}

impl RelaxConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a rule. First-match-wins during comparison.
    pub fn relax(mut self, matcher: Matcher, rule: RelaxationRule) -> Self {
        self.rules.push((matcher, rule));
        self
    }

    /// Every rule whose matcher applies to `el`, in declaration order. The
    /// comparator short-circuits on the first `IgnoreEntirely`/`TextOnly` and
    /// otherwise unions the per-attribute rules — so an element can have
    /// several attributes relaxed at once (e.g. both `src` and `srcset`).
    pub fn find_rules(&self, el: &Element) -> Vec<&RelaxationRule> {
        self.rules
            .iter()
            .filter_map(|(m, r)| m.matches(el).then_some(r))
            .collect()
    }

    /// Bake these relaxations into a canonical copy of `node`: drop ignored
    /// attributes, blank value-ignored ones, reduce text-only elements to their
    /// text, and replace ignored subtrees with an empty placeholder.
    ///
    /// This is the single definition of what each relaxation *means*. The
    /// comparator and the patch renderer both run on canonical trees, so a
    /// relaxed difference is gone before either looks: the comparator stays a
    /// pure structural walk, and the diff never shows a relaxed change.
    pub fn normalize(&self, node: &Node) -> Node {
        match node {
            Node::Document(children) => {
                Node::Document(children.iter().map(|c| self.normalize(c)).collect())
            }
            Node::Element(el) => Node::Element(self.normalize_element(el)),
            other => other.clone(),
        }
    }

    fn normalize_element(&self, el: &Element) -> Element {
        let rules = self.find_rules(el);

        // IgnoreEntirely: collapse to an empty placeholder (tag kept so a
        // genuine tag mismatch at this position still shows).
        if rules
            .iter()
            .any(|r| matches!(r, RelaxationRule::IgnoreEntirely))
        {
            return Element {
                name: el.name.clone(),
                attrs: BTreeMap::new(),
                children: Vec::new(),
            };
        }

        // TextOnly: reduce to the concatenated text; attrs and structure ignored.
        if rules.iter().any(|r| matches!(r, RelaxationRule::TextOnly)) {
            let mut text = String::new();
            collect_text(&el.children, &mut text);
            return Element {
                name: el.name.clone(),
                attrs: BTreeMap::new(),
                children: vec![Node::Text(text)],
            };
        }

        // Per-attribute relaxations, then recurse. `IgnoreAttribute` drops the
        // attr; `IgnoreAttributeValue` blanks the value but keeps the key, so a
        // missing-on-one-side attr is still a divergence.
        let mut attrs = el.attrs.clone();
        for r in &rules {
            match r {
                RelaxationRule::IgnoreAttribute(name) => {
                    attrs.remove(name);
                }
                RelaxationRule::IgnoreAttributeValue(name) => {
                    if let Some(v) = attrs.get_mut(name) {
                        v.clear();
                    }
                }
                RelaxationRule::IgnoreEntirely | RelaxationRule::TextOnly => {}
            }
        }
        Element {
            name: el.name.clone(),
            attrs,
            children: el.children.iter().map(|c| self.normalize(c)).collect(),
        }
    }
}

/// Concatenate the text content of a node sequence (recursing into elements).
fn collect_text(nodes: &[Node], out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => collect_text(&e.children, out),
            _ => {}
        }
    }
}

/// Identifies which element(s) a relaxation rule applies to.
#[derive(Debug, Clone)]
pub enum Matcher {
    /// Match all elements with this tag name (lowercase).
    Tag(String),
    /// Match elements with this tag name AND `attr` equal to `value`.
    TagAttr {
        tag: String,
        attr: String,
        value: String,
    },
    /// Match elements with this tag name AND `attr` present (any value).
    TagAttrExists { tag: String, attr: String },
    /// Match *any* element that has `attr` present, regardless of tag. Used to
    /// relax URL-bearing attributes (`src`, `srcset`) across all elements.
    AnyTagAttrExists { attr: String },
}

impl Matcher {
    pub fn matches(&self, el: &Element) -> bool {
        match self {
            Matcher::Tag(t) => &el.name == t,
            Matcher::TagAttr { tag, attr, value } => {
                &el.name == tag && el.attrs.get(attr) == Some(value)
            }
            Matcher::TagAttrExists { tag, attr } => &el.name == tag && el.attrs.contains_key(attr),
            Matcher::AnyTagAttrExists { attr } => el.attrs.contains_key(attr),
        }
    }
}

/// What to do when a relaxation matches.
#[derive(Debug, Clone)]
pub enum RelaxationRule {
    /// Treat the entire subtree as equal regardless of content.
    IgnoreEntirely,
    /// Ignore a specific attribute on the matched element entirely — present
    /// or absent, any value. Other attrs and children still compared strictly.
    IgnoreAttribute(String),
    /// Require the attribute to be *present on both sides* (missing on either is
    /// a divergence) but ignore its *value*. The URL-attr relaxation: an
    /// `<img src>` must exist on both, but a content-hashed value may differ
    /// from zola's plain filename.
    IgnoreAttributeValue(String),
    /// Compare only the concatenated text content of the element. Useful for
    /// e.g. `<pre>` blocks where syntax-highlighting markup differs across
    /// generators but the source text should match.
    TextOnly,
}
