//! Tree-walking comparator + divergence reporting.

use std::collections::HashMap;
use std::fmt;

use crate::html::{Element, Node};

/// A structural difference between two trees, located by `path`.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub path: Vec<PathStep>,
    pub reason: DivergenceReason,
}

/// One step along the path from document root to the divergent node.
#[derive(Debug, Clone)]
pub enum PathStep {
    Child { tag: String, index: usize },
    Text(usize),
    Comment(usize),
    Doctype(usize),
}

#[derive(Debug, Clone)]
pub enum DivergenceReason {
    NodeKindMismatch {
        expected: &'static str,
        actual: &'static str,
    },
    TagMismatch {
        expected: String,
        actual: String,
    },
    AttrValueMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    AttrMissing {
        name: String,
        expected_value: String,
    },
    AttrExtra {
        name: String,
        actual_value: String,
    },
    ChildCountMismatch {
        expected: usize,
        actual: usize,
    },
    TextMismatch {
        expected: String,
        actual: String,
    },
    DoctypeMismatch {
        expected: String,
        actual: String,
    },
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "DIVERGENCE at {}", format_path(&self.path))?;
        write!(f, "  {}", self.reason)
    }
}

impl fmt::Display for DivergenceReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use DivergenceReason::*;
        match self {
            NodeKindMismatch { expected, actual } => {
                write!(f, "node kind differs: expected {}, got {}", expected, actual)
            }
            TagMismatch { expected, actual } => {
                write!(f, "tag differs: expected <{}>, got <{}>", expected, actual)
            }
            AttrValueMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "attribute {} differs: expected {:?}, got {:?}",
                name, expected, actual
            ),
            AttrMissing {
                name,
                expected_value,
            } => write!(
                f,
                "attribute {} (={:?}) missing in actual",
                name, expected_value
            ),
            AttrExtra { name, actual_value } => write!(
                f,
                "attribute {} (={:?}) unexpected in actual",
                name, actual_value
            ),
            ChildCountMismatch { expected, actual } => write!(
                f,
                "child count differs: expected {}, got {}",
                expected, actual
            ),
            TextMismatch { expected, actual } => {
                write!(f, "text differs: expected {:?}, got {:?}", expected, actual)
            }
            DoctypeMismatch { expected, actual } => write!(
                f,
                "doctype differs: expected {:?}, got {:?}",
                expected, actual
            ),
        }
    }
}

fn format_path(path: &[PathStep]) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    let mut out = String::new();
    for step in path {
        match step {
            PathStep::Child { tag, index } => {
                out.push('/');
                out.push_str(tag);
                out.push_str(&format!("[{}]", index));
            }
            PathStep::Text(i) => out.push_str(&format!("/#text[{}]", i)),
            PathStep::Comment(i) => out.push_str(&format!("/#comment[{}]", i)),
            PathStep::Doctype(i) => out.push_str(&format!("/#doctype[{}]", i)),
        }
    }
    out
}

/// Compare two parsed trees, returning `Ok(())` on structural equivalence or
/// `Err(Divergence)` at the first difference. This is a *pure structural* walk:
/// apply any relaxation rules first via [`RelaxConfig::normalize`](crate::diff::RelaxConfig::normalize),
/// which erases relaxed differences before the comparison sees them.
pub fn diff(expected: &Node, actual: &Node) -> Result<(), Divergence> {
    let mut path = Vec::new();
    compare_nodes(expected, actual, &mut path)
}

fn compare_nodes(
    expected: &Node,
    actual: &Node,
    path: &mut Vec<PathStep>,
) -> Result<(), Divergence> {
    match (expected, actual) {
        (Node::Document(le), Node::Document(ra)) => compare_children(le, ra, path),
        (Node::Element(le), Node::Element(ra)) => compare_elements(le, ra, path),
        (Node::Text(le), Node::Text(ra)) => {
            if le == ra {
                Ok(())
            } else {
                Err(Divergence {
                    path: path.clone(),
                    reason: DivergenceReason::TextMismatch {
                        expected: le.clone(),
                        actual: ra.clone(),
                    },
                })
            }
        }
        (Node::Doctype(le), Node::Doctype(ra)) => {
            if le == ra {
                Ok(())
            } else {
                Err(Divergence {
                    path: path.clone(),
                    reason: DivergenceReason::DoctypeMismatch {
                        expected: le.clone(),
                        actual: ra.clone(),
                    },
                })
            }
        }
        // Comments are ignored by default. Drift in comments alone is not an
        // error; only structural differences in element trees count.
        (Node::Comment(_), Node::Comment(_)) => Ok(()),
        _ => Err(Divergence {
            path: path.clone(),
            reason: DivergenceReason::NodeKindMismatch {
                expected: expected.kind_name(),
                actual: actual.kind_name(),
            },
        }),
    }
}

fn compare_elements(
    expected: &Element,
    actual: &Element,
    path: &mut Vec<PathStep>,
) -> Result<(), Divergence> {
    if expected.name != actual.name {
        return Err(Divergence {
            path: path.clone(),
            reason: DivergenceReason::TagMismatch {
                expected: expected.name.clone(),
                actual: actual.name.clone(),
            },
        });
    }

    for (k, v) in &expected.attrs {
        match actual.attrs.get(k) {
            None => {
                return Err(Divergence {
                    path: path.clone(),
                    reason: DivergenceReason::AttrMissing {
                        name: k.clone(),
                        expected_value: v.clone(),
                    },
                });
            }
            Some(av) if av != v => {
                return Err(Divergence {
                    path: path.clone(),
                    reason: DivergenceReason::AttrValueMismatch {
                        name: k.clone(),
                        expected: v.clone(),
                        actual: av.clone(),
                    },
                });
            }
            _ => {}
        }
    }
    for (k, v) in &actual.attrs {
        if !expected.attrs.contains_key(k) {
            return Err(Divergence {
                path: path.clone(),
                reason: DivergenceReason::AttrExtra {
                    name: k.clone(),
                    actual_value: v.clone(),
                },
            });
        }
    }

    compare_children(&expected.children, &actual.children, path)
}

fn compare_children(
    expected: &[Node],
    actual: &[Node],
    path: &mut Vec<PathStep>,
) -> Result<(), Divergence> {
    if expected.len() != actual.len() {
        return Err(Divergence {
            path: path.clone(),
            reason: DivergenceReason::ChildCountMismatch {
                expected: expected.len(),
                actual: actual.len(),
            },
        });
    }

    let mut tag_index: HashMap<String, usize> = HashMap::new();
    let mut text_i = 0usize;
    let mut comment_i = 0usize;
    let mut doctype_i = 0usize;

    for (e, a) in expected.iter().zip(actual.iter()) {
        let step = match e {
            Node::Element(el) => {
                let idx = tag_index.entry(el.name.clone()).or_insert(0);
                let step = PathStep::Child {
                    tag: el.name.clone(),
                    index: *idx,
                };
                *idx += 1;
                step
            }
            Node::Text(_) => {
                let s = PathStep::Text(text_i);
                text_i += 1;
                s
            }
            Node::Comment(_) => {
                let s = PathStep::Comment(comment_i);
                comment_i += 1;
                s
            }
            Node::Doctype(_) => {
                let s = PathStep::Doctype(doctype_i);
                doctype_i += 1;
                s
            }
            Node::Document(_) => continue, // shouldn't happen mid-tree
        };
        path.push(step);
        let res = compare_nodes(e, a, path);
        path.pop();
        res?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::relax::{Matcher, RelaxConfig, RelaxationRule};
    use crate::html::parse::parse_html;

    fn assert_match(left: &str, right: &str) {
        let l = parse_html(left);
        let r = parse_html(right);
        match diff(&l, &r) {
            Ok(()) => {}
            Err(d) => panic!("expected match, got divergence:\n{}", d),
        }
    }

    fn assert_diff(left: &str, right: &str) -> Divergence {
        let l = parse_html(left);
        let r = parse_html(right);
        match diff(&l, &r) {
            Ok(()) => panic!("expected divergence, got match"),
            Err(d) => d,
        }
    }

    /// Apply a relaxation config (normalize both sides) then diff — the path the
    /// convert harness uses.
    fn diff_relaxed(left: &str, right: &str, cfg: &RelaxConfig) -> Result<(), Divergence> {
        diff(&cfg.normalize(&parse_html(left)), &cfg.normalize(&parse_html(right)))
    }

    #[test]
    fn identical_matches() {
        assert_match(
            "<p class=\"a\">hi</p>",
            "<p class=\"a\">hi</p>",
        );
    }

    #[test]
    fn whitespace_collapsed() {
        assert_match(
            "<p>hello   world</p>",
            "<p>hello world</p>",
        );
    }

    #[test]
    fn indentation_between_blocks_ignored() {
        assert_match(
            "<div><p>a</p><p>b</p></div>",
            "<div>\n  <p>a</p>\n  <p>b</p>\n</div>",
        );
    }

    #[test]
    fn attribute_order_irrelevant() {
        assert_match(
            "<a href=\"x\" id=\"q\">hi</a>",
            "<a id=\"q\" href=\"x\">hi</a>",
        );
    }

    #[test]
    fn class_token_order_irrelevant() {
        assert_match(
            "<div class=\"alpha beta gamma\"></div>",
            "<div class=\"gamma alpha beta\"></div>",
        );
    }

    #[test]
    fn pre_preserves_whitespace() {
        // Different inner whitespace inside <pre> must NOT match.
        let d = assert_diff(
            "<pre>line one\n    indented</pre>",
            "<pre>line one indented</pre>",
        );
        assert!(matches!(d.reason, DivergenceReason::TextMismatch { .. }));
    }

    #[test]
    fn pre_identical_matches() {
        assert_match(
            "<pre>line one\n    indented</pre>",
            "<pre>line one\n    indented</pre>",
        );
    }

    #[test]
    fn tag_drift_detected_with_path() {
        let d = assert_diff(
            "<div><p>hi</p></div>",
            "<div><span>hi</span></div>",
        );
        assert!(matches!(d.reason, DivergenceReason::TagMismatch { .. }));
        let p = format_path(&d.path);
        assert!(p.contains("/body[0]/div[0]"), "path was {}", p);
    }

    #[test]
    fn attribute_value_drift_detected() {
        let d = assert_diff(
            "<a href=\"x\">hi</a>",
            "<a href=\"y\">hi</a>",
        );
        match d.reason {
            DivergenceReason::AttrValueMismatch {
                name,
                expected,
                actual,
            } => {
                assert_eq!(name, "href");
                assert_eq!(expected, "x");
                assert_eq!(actual, "y");
            }
            r => panic!("wrong reason: {:?}", r),
        }
    }

    #[test]
    fn missing_attribute_detected() {
        let d = assert_diff(
            "<a href=\"x\" id=\"q\">hi</a>",
            "<a href=\"x\">hi</a>",
        );
        assert!(matches!(d.reason, DivergenceReason::AttrMissing { .. }));
    }

    #[test]
    fn extra_attribute_detected() {
        let d = assert_diff(
            "<a href=\"x\">hi</a>",
            "<a href=\"x\" id=\"q\">hi</a>",
        );
        assert!(matches!(d.reason, DivergenceReason::AttrExtra { .. }));
    }

    #[test]
    fn text_drift_detected() {
        let d = assert_diff("<p>hello</p>", "<p>goodbye</p>");
        assert!(matches!(d.reason, DivergenceReason::TextMismatch { .. }));
    }

    #[test]
    fn child_count_drift_detected() {
        let d = assert_diff(
            "<div><p>a</p><p>b</p></div>",
            "<div><p>a</p></div>",
        );
        assert!(matches!(d.reason, DivergenceReason::ChildCountMismatch { .. }));
    }

    #[test]
    fn comments_ignored_by_default() {
        // Comments on the expected side, none on the actual side: should still match.
        assert_match(
            "<div><!-- note --><p>hi</p></div>",
            "<div><p>hi</p></div>",
        );
    }

    #[test]
    fn relax_ignore_attribute() {
        let cfg = RelaxConfig::new().relax(
            Matcher::Tag("a".to_string()),
            RelaxationRule::IgnoreAttribute("data-x".to_string()),
        );
        diff_relaxed(
            "<a href=\"x\" data-x=\"1\">hi</a>",
            "<a href=\"x\" data-x=\"999\">hi</a>",
            &cfg,
        )
        .expect("should match with relaxation");
    }

    #[test]
    fn relax_text_only_pre() {
        // syntax-highlighted vs plain — same source text but different structure.
        let cfg =
            RelaxConfig::new().relax(Matcher::Tag("pre".to_string()), RelaxationRule::TextOnly);
        diff_relaxed(
            "<pre><span class=\"k\">fn</span> <span class=\"i\">main</span>() {}</pre>",
            "<pre>fn main() {}</pre>",
            &cfg,
        )
        .expect("should match under TextOnly");
    }

    #[test]
    fn relax_ignore_attribute_value_allows_differing_value() {
        // Hashed asset URL vs plain filename — same element, value ignored.
        let cfg = RelaxConfig::new().relax(
            Matcher::AnyTagAttrExists {
                attr: "src".to_string(),
            },
            RelaxationRule::IgnoreAttributeValue("src".to_string()),
        );
        diff_relaxed("<img src=\"/foo.png\">", "<img src=\"/assets/foo-abc123.png\">", &cfg)
            .expect("differing src value should match under IgnoreAttributeValue");
    }

    #[test]
    fn relax_ignore_attribute_value_still_requires_presence() {
        // Value ignored, but a missing attr on the actual side is still a diff.
        let cfg = RelaxConfig::new().relax(
            Matcher::AnyTagAttrExists {
                attr: "src".to_string(),
            },
            RelaxationRule::IgnoreAttributeValue("src".to_string()),
        );
        let d = diff_relaxed("<img src=\"/foo.png\">", "<img alt=\"x\">", &cfg)
            .expect_err("missing src must still diverge");
        assert!(matches!(d.reason, DivergenceReason::AttrMissing { .. }));
    }

    #[test]
    fn multiple_rules_union_on_one_element() {
        // src and srcset both relaxed via two AnyTagAttrExists rules.
        let cfg = RelaxConfig::new()
            .relax(
                Matcher::AnyTagAttrExists {
                    attr: "src".to_string(),
                },
                RelaxationRule::IgnoreAttributeValue("src".to_string()),
            )
            .relax(
                Matcher::AnyTagAttrExists {
                    attr: "srcset".to_string(),
                },
                RelaxationRule::IgnoreAttributeValue("srcset".to_string()),
            );
        diff_relaxed(
            "<img src=\"/a.png\" srcset=\"/a.png 1x\">",
            "<img src=\"/assets/a-h.png\" srcset=\"/assets/a-h.png 1x\">",
            &cfg,
        )
        .expect("both src and srcset values should be relaxed");
    }

    #[test]
    fn relax_ignore_entirely() {
        // Differing nav widgets, but we don't care about their contents.
        let cfg = RelaxConfig::new()
            .relax(Matcher::Tag("nav".to_string()), RelaxationRule::IgnoreEntirely);
        diff_relaxed(
            "<nav class=\"x\"><a href=\"/a\">a</a></nav>",
            "<nav class=\"y\"><span>completely different</span></nav>",
            &cfg,
        )
        .expect("should match under IgnoreEntirely");
    }
}
