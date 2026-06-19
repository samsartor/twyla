//! Link discovery, classification, and URL → manifest-path resolution.
//!
//! Two link classes, keyed by element (the convert link audit treats them
//! differently):
//!
//! - **Navigable** (`<a href>`, `<area href>`) — a human or external site can
//!   bookmark these, so they must be *path-stable*: the exact URL has to resolve.
//! - **SubResource** (`<img src>`, `<script src>`, `<link href>`,
//!   `<source src|srcset>`, …) — fetched by the page, never bookmarked, so they
//!   only need to be *reachable*: any content-hashed name is fine.
//!
//! [`resolve`] turns a raw URL into a root-relative manifest key
//! (`guis-1/index.html`, `resume.pdf`, …) or `None` for refs that aren't
//! local-file references (external, `mailto:`, pure fragments, …).

use crate::html::Node;

/// Whether a link is navigated to (must be path-stable) or fetched as a
/// sub-resource (only needs to be reachable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkClass {
    Navigable,
    SubResource,
}

/// One URL reference found on a page, with the element/attribute it came from
/// (for diagnostics) and its class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlRef {
    /// The raw attribute value, before resolution.
    pub raw: String,
    pub class: LinkClass,
    /// Lowercased tag name, e.g. `"a"`, `"img"`.
    pub element: String,
    /// Lowercased attribute name: `"href"`, `"src"`, or `"srcset"`.
    pub attr: String,
}

/// Classify a `(tag, attr)` pair as a link, or `None` if it isn't one we
/// validate. `base@href` is intentionally excluded (it sets the base, it isn't
/// a target).
fn classify(tag: &str, attr: &str) -> Option<LinkClass> {
    match attr {
        "href" => match tag {
            "a" | "area" => Some(LinkClass::Navigable),
            "link" => Some(LinkClass::SubResource),
            _ => None,
        },
        "src" => match tag {
            "img" | "script" | "source" | "iframe" | "audio" | "video" | "embed" | "track"
            | "input" => Some(LinkClass::SubResource),
            _ => None,
        },
        "srcset" => match tag {
            "img" | "source" => Some(LinkClass::SubResource),
            _ => None,
        },
        _ => None,
    }
}

/// Split a `srcset` value into its candidate URLs. Each comma-separated
/// candidate is `<url> [descriptor]`; we take the leading URL token.
fn srcset_urls(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter_map(|cand| cand.split_whitespace().next())
        .filter(|u| !u.is_empty())
        .map(|u| u.to_string())
        .collect()
}

/// Walk a parsed page and yield every URL-bearing reference, classified.
pub fn extract(root: &Node) -> Vec<UrlRef> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn walk(node: &Node, out: &mut Vec<UrlRef>) {
    let Node::Element(el) = node else {
        if let Node::Document(children) = node {
            for c in children {
                walk(c, out);
            }
        }
        return;
    };
    for (attr, value) in &el.attrs {
        let Some(class) = classify(&el.name, attr) else {
            continue;
        };
        if attr == "srcset" {
            for url in srcset_urls(value) {
                out.push(UrlRef {
                    raw: url,
                    class,
                    element: el.name.clone(),
                    attr: attr.clone(),
                });
            }
        } else {
            out.push(UrlRef {
                raw: value.clone(),
                class,
                element: el.name.clone(),
                attr: attr.clone(),
            });
        }
    }
    for c in &el.children {
        walk(c, out);
    }
}

/// True if `s` begins with a URL scheme (`http:`, `mailto:`, `data:`, the
/// ROT13'd `znvygb:`, …) — i.e. a `:` appears before any `/`, `?`, or `#`,
/// preceded only by scheme characters. Such refs are external/non-file and
/// aren't validated against a build manifest.
fn has_scheme(s: &str) -> bool {
    let mut chars = s.char_indices();
    // First char must be a letter.
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for (_, c) in chars {
        match c {
            ':' => return true,
            'a'..='z' | 'A'..='Z' | '0'..='9' | '+' | '.' | '-' => {}
            _ => return false, // hit a non-scheme char (e.g. '/') first
        }
    }
    false
}

/// Resolve a raw URL to a root-relative manifest key, given the page it
/// appears on (`page_route`, itself a manifest key like `guis-1/index.html`)
/// and the optional site `base_url`. Returns `None` for refs that aren't
/// local-file references: external schemes, protocol-relative `//host`,
/// `mailto:`, pure fragments, empty.
///
/// Normalization: strip query/fragment; strip a leading `base_url`; resolve
/// relative URLs against the page's directory; a path with no file extension
/// is a directory route and gets `index.html` appended (`/guis-1` and
/// `/guis-1/` → `guis-1/index.html`; `/resume.pdf` stays a file).
pub fn resolve(raw: &str, page_route: &str, base_url: Option<&str>) -> Option<String> {
    let raw = raw.trim();
    // Drop query + fragment.
    let raw = raw.split(['?', '#']).next().unwrap_or("");
    if raw.is_empty() {
        return None; // pure fragment / empty
    }
    if raw.starts_with("//") {
        return None; // protocol-relative → external host
    }

    // Strip a leading absolute base_url; what remains is root-relative.
    let stripped = base_url
        .and_then(|b| raw.strip_prefix(b))
        .map(|rest| if rest.is_empty() { "/" } else { rest });
    let raw = match stripped {
        Some(rest) => rest,
        None => {
            if has_scheme(raw) {
                return None; // external scheme (http, mailto, data, znvygb…)
            }
            raw
        }
    };

    // Resolve to a root-relative path (no leading slash).
    let path = if let Some(abs) = raw.strip_prefix('/') {
        abs.to_string()
    } else {
        // Relative to the page's directory.
        let dir = page_route.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        if dir.is_empty() {
            raw.to_string()
        } else {
            format!("{dir}/{raw}")
        }
    };

    Some(directory_index(&normalize_path(&path)))
}

/// Collapse `.`/`..` segments and redundant slashes into a clean relative path.
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Apply the directory-route rule: an empty path or a final segment with no
/// file extension is a directory and resolves to its `index.html`.
fn directory_index(path: &str) -> String {
    if path.is_empty() {
        return "index.html".to_string();
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if last.contains('.') {
        path.to_string() // looks like a file
    } else {
        format!("{path}/index.html") // directory route
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::parse_html;

    const BASE: Option<&str> = Some("https://samsartor.com");

    fn r(raw: &str) -> Option<String> {
        resolve(raw, "guis-1/index.html", BASE)
    }

    #[test]
    fn root_and_page_links() {
        assert_eq!(r("/").as_deref(), Some("index.html"));
        assert_eq!(r("/guis-1").as_deref(), Some("guis-1/index.html"));
        assert_eq!(r("/guis-2/").as_deref(), Some("guis-2/index.html"));
    }

    #[test]
    fn file_links_keep_extension() {
        assert_eq!(r("/resume.pdf").as_deref(), Some("resume.pdf"));
        assert_eq!(r("/site.css").as_deref(), Some("site.css"));
        assert_eq!(r("/scripts/site.js").as_deref(), Some("scripts/site.js"));
    }

    #[test]
    fn base_url_is_stripped() {
        assert_eq!(
            r("https://samsartor.com/guis-1/#hybrid-mode").as_deref(),
            Some("guis-1/index.html")
        );
        assert_eq!(
            r("https://samsartor.com/interior_mutability.png").as_deref(),
            Some("interior_mutability.png")
        );
        assert_eq!(r("https://samsartor.com").as_deref(), Some("index.html"));
    }

    #[test]
    fn externals_and_fragments_skipped() {
        assert_eq!(r("#hybrid-mode"), None);
        assert_eq!(r("mailto:cap@samsartor.com"), None);
        assert_eq!(r("znvygb:zr@fnzfnegbe.pbz"), None); // ROT13 mailto
        assert_eq!(r("https://github.com/samsartor"), None);
        assert_eq!(r("//cdn.example.com/x.js"), None);
        assert_eq!(r("data:image/png;base64,AAAA"), None);
    }

    #[test]
    fn relative_resolves_against_page_dir() {
        assert_eq!(
            resolve("foo.png", "what-is-color/index.html", BASE).as_deref(),
            Some("what-is-color/foo.png")
        );
        assert_eq!(
            resolve("../resume.pdf", "what-is-color/index.html", BASE).as_deref(),
            Some("resume.pdf")
        );
    }

    #[test]
    fn extract_classifies_by_element() {
        let doc = parse_html(
            r#"<a href="/guis-2">x</a>
               <img src="/a.png" srcset="/a.png 1x, /a@2x.png 2x">
               <link href="/site.css">
               <script src="/s.js"></script>"#,
        );
        let refs = extract(&doc);
        let nav: Vec<_> = refs
            .iter()
            .filter(|u| u.class == LinkClass::Navigable)
            .map(|u| u.raw.as_str())
            .collect();
        assert_eq!(nav, vec!["/guis-2"]);
        let sub: Vec<_> = refs
            .iter()
            .filter(|u| u.class == LinkClass::SubResource)
            .map(|u| u.raw.as_str())
            .collect();
        // srcset split into two candidates + img src + link href + script src.
        assert!(sub.contains(&"/a.png"));
        assert!(sub.contains(&"/a@2x.png"));
        assert!(sub.contains(&"/site.css"));
        assert!(sub.contains(&"/s.js"));
    }
}
