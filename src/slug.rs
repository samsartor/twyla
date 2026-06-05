//! Slugification shared by markdown import (heading anchors) and the zola
//! path/route mapping.
//!
//! Zola's default `slugify.paths = On` / `slugify.anchors = On` both run the
//! `slug` crate: lowercase, transliterate, collapse runs of non-alphanumeric to
//! a single `-`, trim. We reproduce the ASCII subset that covers the corpus
//! (it's already lowercase ASCII; the only live effect is `_` → `-`). Unicode
//! transliteration (e.g. `héhé` → `hehe`) is *not* reproduced — no page in the
//! corpus needs it; revisit with the `slug` crate if that changes.

/// Slugify plain text into a lowercase, hyphen-separated identifier. Runs of
/// non-alphanumeric characters collapse to one `-`; leading/trailing `-` are
/// stripped.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true; // leading: suppress a leading dash
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underscores_and_spaces_become_hyphens() {
        assert_eq!(slugify("content_aware_tiles"), "content-aware-tiles");
        assert_eq!(slugify("ai_cut"), "ai-cut");
        assert_eq!(slugify("Hello, World!"), "hello-world");
    }

    #[test]
    fn already_slugged_is_stable() {
        assert_eq!(slugify("guis-1"), "guis-1");
        assert_eq!(slugify("what-is-color"), "what-is-color");
    }
}
