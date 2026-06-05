//! Twyla content builtins: `raw-html` and `plain-text`.
//!
//! Registered into the global scope by [`install`], so user typst calls them
//! bare: `#raw-html("<svg/>")`, `#plain-text(heading)`.

use ecow::EcoString;
use typst::foundations::{Content, NativeElement, Scope, Str, func};
use typst::text::TextElem;
use typst_html::{HtmlAttr, HtmlElem, HtmlTag};

use crate::render::RAW_HTML_SCRIPT_TYPE;

/// Splice a string of raw HTML into the output verbatim.
///
/// Typst has no first-class raw-HTML node, so for now this wraps the markup in
/// `<script type="…">…</script>` — a raw-text element typst won't escape — and
/// twyla's post-render pass ([`crate::render::resolve_raw_html_placeholders`])
/// strips the wrapper back out. Exposing it as a builtin rather than a typst
/// `#let` keeps call sites stable if we later swap the implementation (e.g.
/// parse the HTML with html5ever and emit real typst nodes).
#[func]
pub fn raw_html(
    /// The raw HTML markup to emit unescaped.
    html: EcoString,
) -> Content {
    HtmlElem::new(HtmlTag::constant("script"))
        .with_attr(HtmlAttr::constant("type"), RAW_HTML_SCRIPT_TYPE)
        .with_body(Some(TextElem::packed(html)))
        .pack()
}

/// The plain text of some content — the same flattening typst uses to derive
/// the document `<title>` from `document(title: …)`. Useful for slugs, alt
/// text, and other places that need a string from rich content.
#[func]
pub fn plain_text(
    /// The content to flatten to plain text.
    content: Content,
) -> Str {
    content.plain_text().into()
}

/// Bind `raw-html` and `plain-text` into the global scope.
pub fn install(global: &mut Scope) {
    global.define_func::<raw_html>();
    global.define_func::<plain_text>();
}
