//! Relaxation rules — opt-in exceptions to the default strict comparison.
//!
//! A `RelaxConfig` is a list of `(Matcher, RelaxationRule)` pairs evaluated in
//! order; the first matching rule wins. Adding a new relaxation is a single
//! `.relax(matcher, rule)` builder call.
//!
//! *Default config has no rules.* New relaxations should be added explicitly
//! (and ideally discussed before becoming a default) so the porting harness
//! stays honest about where twyla and zola diverge.

use crate::diff::Element;

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

    /// Returns the first rule whose matcher applies to `el`, if any.
    pub fn find_rule(&self, el: &Element) -> Option<&RelaxationRule> {
        self.rules
            .iter()
            .find_map(|(m, r)| m.matches(el).then_some(r))
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
}

impl Matcher {
    pub fn matches(&self, el: &Element) -> bool {
        match self {
            Matcher::Tag(t) => &el.name == t,
            Matcher::TagAttr { tag, attr, value } => {
                &el.name == tag && el.attrs.get(attr) == Some(value)
            }
            Matcher::TagAttrExists { tag, attr } => {
                &el.name == tag && el.attrs.contains_key(attr)
            }
        }
    }
}

/// What to do when a relaxation matches.
#[derive(Debug, Clone)]
pub enum RelaxationRule {
    /// Treat the entire subtree as equal regardless of content.
    IgnoreEntirely,
    /// Ignore a specific attribute on the matched element. Other attrs and
    /// children still compared strictly.
    IgnoreAttribute(String),
    /// Compare only the concatenated text content of the element. Useful for
    /// e.g. `<pre>` blocks where syntax-highlighting markup differs across
    /// generators but the source text should match.
    TextOnly,
}
