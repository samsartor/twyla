//! Intermediate representation for converted content, plus the typst renderer.
//!
//! The convert pipeline is two passes:
//!
//! 1. an SSG-specific **frontend** (today [`crate::import`] — zola markdown +
//!    shortcodes) builds this IR, and
//! 2. the shared **renderer** below turns the IR into a typst draft.
//!
//! Supporting another SSG means writing another frontend that targets this same
//! IR; the renderer is reused. The IR is intentionally a shallow tree of
//! [`Block`]/[`Inline`] nodes — the common core is CommonMark, shortcodes are a
//! generic `name + args + body`, and HTML reuses [`crate::html::Node`].

use std::fmt::Write as _;

use crate::html::Node as HtmlNode;
use crate::slug::hugo_slugify;

/// Per-column table alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

/// Inline content — the leaves that make up content
#[derive(Debug, Clone)]
pub enum Inline {
    Text(String),
    /// Inline `` `code` ``.
    Code(String),
    Emph(Content),
    Strong(Content),
    Strike(Content),
    Link {
        dest: String,
        content: Content,
    },
    /// Inline shortcode `{{ name(args) }}` → `#name(args)`. `args` is already
    /// translated to typst named-argument form (`k: "v"`).
    Shortcode {
        name: String,
        args: String,
    },
    /// A paired shortcode (`{% name(args) %}…{% end %}`) used in inline
    /// position → `#name(args)[…]` on the same line, so typst keeps it in the
    /// surrounding paragraph instead of splitting a block out.
    ShortcodeBody {
        name: String,
        args: String,
        content: Content,
    },
    /// Inline HTML element — children are markdown inlines.
    Html {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Content,
    },
    /// Inline image: `![alt](src)` → `#html.elem("img", attrs: …)`.
    Image {
        src: String,
        alt: String,
    },
    SoftBreak,
    HardBreak,
    /// Untranslatable inline content, rendered as a TODO comment.
    Raw(String),
    /// Pre-rendered typst emitted verbatim. The escape hatch for block content
    /// that lands in an inline context (e.g. a code block inside an inline
    /// `<a>` element), where the frontend renders it early and embeds it.
    Verbatim(String),
}

/// Content - the actual contents of a paragraph, heading, list item, etc
pub type Content = Vec<Inline>;

/// Block-level content.
#[derive(Debug, Clone)]
pub enum Block {
    Heading {
        level: u8,
        /// Explicit heading ID from Hugo/goldmark `{#id}` attribute syntax.
        /// When set, this is used as-is for the typst label instead of
        /// slugifying the heading text.
        id: Option<String>,
        content: Content,
    },
    Para(Content),
    Code {
        lang: String,
        text: String,
    },
    Quote(Vec<Block>),
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Table {
        align: Vec<Align>,
        head: Vec<Content>,
        rows: Vec<Vec<Content>>,
    },
    Rule,
    /// Block shortcode `{% name(args) %}…{% end %}` with a markdown body.
    Shortcode {
        name: String,
        args: String,
        body: Vec<Block>,
    },
    /// Block-level HTML (a parsed fragment).
    Html(HtmlNode),
    /// Untranslatable block content, rendered as a TODO comment.
    Raw(String),
}

// ---- renderer ------------------------------------------------------------

/// Render a document body using Zola's slugification for heading anchors.
pub fn render(blocks: &[Block]) -> String {
    render_with_slug(blocks, "", crate::slug::slugify)
}

/// Render a document body using Hugo/goldmark slugification for heading anchors.
/// `label_prefix` is prepended to every heading label and same-page anchor
/// target to prevent cross-page label conflicts in the typst bundle.
pub fn render_hugo(blocks: &[Block], label_prefix: &str) -> String {
    render_with_slug(blocks, label_prefix, hugo_slugify)
}

fn render_with_slug(blocks: &[Block], prefix: &str, slug: fn(&str) -> String) -> String {
    let mut out = String::new();
    for b in blocks {
        render_block(b, &mut out, prefix, slug);
    }
    out
}

fn render_block(b: &Block, out: &mut String, prefix: &str, slug: fn(&str) -> String) {
    match b {
        Block::Heading { level, id, content } => {
            for _ in 0..*level {
                out.push('=');
            }
            out.push(' ');
            render_inlines(content, out, prefix);
            // Label: use explicit `{#id}` attribute when present (Hugo/goldmark),
            // otherwise slugify the heading text. Always scoped with `prefix` to
            // avoid cross-page label conflicts in the typst bundle.
            let label = match id.as_deref() {
                Some(explicit) => format!("{prefix}{explicit}"),
                None => format!("{prefix}{}", slug(&plain_text(content))),
            };
            writeln!(out, " <{label}>\n").unwrap();
        }
        Block::Para(content) => {
            render_inlines(content, out, prefix);
            out.push_str("\n\n");
        }
        Block::Code { lang, text } => {
            out.push_str("```");
            out.push_str(lang);
            out.push('\n');
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
        }
        Block::Quote(blocks) => {
            out.push_str("#html.blockquote[\n");
            for b in blocks {
                render_block(b, out, prefix, slug);
            }
            out.push_str("]\n\n");
        }
        Block::List { ordered, items } => {
            let marker = if *ordered { "+ " } else { "- " };
            for item in items {
                let mut buf = String::new();
                render_item(item, &mut buf, prefix, slug);
                // Indent continuation lines under the marker (width of `- `):
                // an unindented line ends the item, so typst would split it out
                // of the list (`</ul><p>…</p><ul>`).
                out.push_str(marker);
                indent_continuation(&buf, "  ", out);
            }
            out.push('\n');
        }
        Block::Table { align, head, rows } => render_table(align, head, rows, out, prefix),
        Block::Rule => out.push_str("#html.hr()\n\n"),
        Block::Shortcode { name, args, body } => {
            writeln!(out, "\n#{name}({args})[").unwrap();
            for b in body {
                render_block(b, out, prefix, slug);
            }
            // Trim the trailing block break so the body stays a single
            // paragraph inside the shortcode (avoids a stray empty `<p>`).
            while out.ends_with(char::is_whitespace) {
                out.pop();
            }
            out.push_str("]\n\n");
        }
        Block::Html(node) => {
            convert_html_node(node, out);
            out.push_str("\n\n");
        }
        Block::Raw(s) => {
            write!(out, "/* TODO twyla-convert: {s} */\n\n").unwrap();
        }
    }
}

/// A list item: inline the common single-paragraph case (`- text`), otherwise
/// render its blocks.
fn render_item(blocks: &[Block], out: &mut String, prefix: &str, slug: fn(&str) -> String) {
    if let [Block::Para(content)] = blocks {
        render_inlines(content, out, prefix);
        out.push('\n');
    } else {
        for b in blocks {
            render_block(b, out, prefix, slug);
        }
    }
}

/// Append `text`, indenting every line after the first by `pad` (empty lines
/// stay empty). Used to keep a list item's continuation content — extra
/// paragraphs, hard breaks, nested lists — under its marker.
fn indent_continuation(text: &str, pad: &str, out: &mut String) {
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if i > 0 && !line.trim_start().is_empty() {
            out.push_str(pad);
        }
        out.push_str(line);
    }
}

fn render_table(
    align: &[Align],
    head: &[Content],
    rows: &[Vec<Content>],
    out: &mut String,
    prefix: &str,
) {
    out.push_str("\n// TODO twyla-convert: check table styling\n");
    writeln!(out, "#table(").unwrap();
    writeln!(out, "  columns: {},", align.len()).unwrap();
    if align.iter().any(|a| *a != Align::None) {
        let cols: Vec<&str> = align.iter().map(|a| align_name(*a)).collect();
        writeln!(out, "  align: ({}),", cols.join(", ")).unwrap();
    }
    if !head.is_empty() {
        out.push_str("  table.header(");
        for cell in head {
            out.push('[');
            render_inlines(cell, out, prefix);
            out.push_str("], ");
        }
        out.push_str("),\n");
    }
    for row in rows {
        out.push_str("  ");
        for cell in row {
            out.push('[');
            render_inlines(cell, out, prefix);
            out.push_str("], ");
        }
        out.push('\n');
    }
    out.push_str(")\n\n");
}

fn render_inlines(inlines: &[Inline], out: &mut String, prefix: &str) {
    for (idx, i) in inlines.iter().enumerate() {
        // Typst greedily parses `#expr](text)` / `#expr)(text)` as function
        // calls. Insert an empty content block to break the ambiguity when a
        // text node starting with `(` follows a code expression.
        if idx > 0 && code_expr_ending(&inlines[idx - 1]) {
            if let Inline::Text(s) = i {
                if s.starts_with('(') {
                    out.push_str("/**/");
                }
            }
        }

        // When `_`/`*` would be directly adjacent to an alphanumeric character
        // on either side, typst won't open or close the delimiter there. Use
        // the function-call form instead.
        let needs_func_emph = matches!(i, Inline::Emph(_) | Inline::Strong(_)) && {
            let next_alnum = inlines.get(idx + 1).is_some_and(|next| {
                matches!(next, Inline::Text(s) if s.starts_with(|c: char| c.is_alphanumeric()))
            });
            let prev_alnum = out.chars().last().is_some_and(|c| c.is_alphanumeric());
            next_alnum || prev_alnum
        };
        match (i, needs_func_emph) {
            (Inline::Emph(c), true) => {
                out.push_str("#emph[");
                render_inlines(c, out, prefix);
                out.push(']');
            }
            (Inline::Strong(c), true) => {
                out.push_str("#strong[");
                render_inlines(c, out, prefix);
                out.push(']');
            }
            _ => render_inline(i, out, prefix),
        }
    }
}

fn code_expr_ending(i: &Inline) -> bool {
    matches!(
        i,
        Inline::Html { .. }
            | Inline::Link { .. }
            | Inline::ShortcodeBody { .. }
            | Inline::Strike(_)
    )
}

fn render_inline(i: &Inline, out: &mut String, prefix: &str) {
    match i {
        Inline::Text(s) => out.push_str(&escape_markup(s)),
        Inline::Code(s) => write!(out, "`{s}`").unwrap(),
        Inline::Emph(c) => wrap(out, "_", c, "_", prefix),
        Inline::Strong(c) => wrap(out, "*", c, "*", prefix),
        Inline::Strike(c) => wrap(out, "#strike[", c, "]", prefix),
        Inline::Link { dest, content } => {
            // Hugo {{< ref >}} shortcodes in link destinations are mangled by
            // pulldown-cmark's angle-bracket stripping: `<!--TWYLA-HUGO:...-->`
            // becomes `!--TWYLA-HUGO:...--`. Render the link text as-is with a
            // TODO comment so the porter can fix the href manually.
            if dest.contains("TWYLA-HUGO") {
                out.push_str("/* TODO twyla-convert: hugo ref link */");
                render_inlines(content, out, prefix);
                return;
            }
            if let Some(frag) = dest.strip_prefix('#') {
                // Same-page anchor link: prefix with the page label prefix so
                // it matches the correspondingly-prefixed heading label.
                write!(out, "#link(<{prefix}{frag}>)[").unwrap();
            } else {
                write!(out, "#link(\"{}\")[", escape_typst_string(dest)).unwrap();
            }
            render_inlines(content, out, prefix);
            out.push(']');
        }
        Inline::Shortcode { name, args } => write!(out, "#{name}({args})").unwrap(),
        Inline::ShortcodeBody {
            name,
            args,
            content,
        } => {
            write!(out, "#{name}({args})[").unwrap();
            render_inlines(content, out, prefix);
            out.push(']');
        }
        Inline::Html {
            tag,
            attrs,
            children,
        } => {
            let dict = typst_attrs(attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            // Wrap block-ish tags in `box()` so they stay inline: typst splits a
            // paragraph around an inline element it doesn't group into paragraphs
            // (`should_group_into_pars == false`, e.g. `<div>`/`<section>`).
            let boxed = !groups_into_pars(tag);
            if boxed {
                out.push_str("#box(");
                write!(out, "html.elem(\"{tag}\"{dict})").unwrap();
            } else {
                write!(out, "#html.elem(\"{tag}\"{dict})").unwrap();
            }
            if !(children.is_empty() && is_void(tag)) {
                out.push('[');
                render_inlines(children, out, prefix);
                out.push(']');
            }
            if boxed {
                out.push(')');
            }
        }
        Inline::Image { src, alt } => {
            if src.starts_with("http://") || src.starts_with("https://") {
                let dict = typst_attrs([("src", src.as_str()), ("alt", alt.as_str())].into_iter());
                write!(out, "#html.elem(\"img\"{dict})").unwrap();
            } else if alt.is_empty() {
                write!(out, "#image(\"{}\")", escape_typst_string(src)).unwrap();
            } else {
                write!(
                    out,
                    "#image(\"{}\", alt: \"{}\")",
                    escape_typst_string(src),
                    escape_typst_string(alt)
                )
                .unwrap();
            }
        }
        Inline::SoftBreak => out.push('\n'),
        Inline::HardBreak => out.push_str(" \\\n"),
        Inline::Raw(s) => write!(out, "/* TODO twyla-convert: {s} */").unwrap(),
        Inline::Verbatim(s) => out.push_str(s),
    }
}

fn wrap(out: &mut String, open: &str, content: &[Inline], close: &str, prefix: &str) {
    out.push_str(open);
    render_inlines(content, out, prefix);
    out.push_str(close);
}

/// Plain text of an inline run — used to derive heading slugs.
fn plain_text(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for i in inlines {
        plain_text_into(i, &mut s);
    }
    s
}

fn plain_text_into(i: &Inline, s: &mut String) {
    match i {
        Inline::Text(t) | Inline::Code(t) => s.push_str(t),
        Inline::Emph(c)
        | Inline::Strong(c)
        | Inline::Strike(c)
        | Inline::Link { content: c, .. }
        | Inline::Html { children: c, .. }
        | Inline::ShortcodeBody { content: c, .. } => {
            for x in c {
                plain_text_into(x, s);
            }
        }
        _ => {}
    }
}

// ---- shared helpers (also used by the frontend's assembly) ----------------

/// Escape a string for a typst string literal (`"…"`).
pub fn escape_typst_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Backslash-escape characters that start typst markup syntax, so literal
/// prose renders as text. Applied to text leaves — never to code.
pub fn escape_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '#' | '$' | '*' | '_' | '`' | '<' | '@' | '~' | '[' | ']'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Render attributes as the `, attrs: ("k": "v", …)` part of an `html.elem`
/// call (empty when there are none). String keys keep `data-foo` valid.
fn typst_attrs<'a>(attrs: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let parts: Vec<String> = attrs
        .map(|(k, v)| {
            format!(
                "\"{}\": \"{}\"",
                escape_typst_string(k),
                escape_typst_string(v)
            )
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(", attrs: ({})", parts.join(", "))
    }
}

/// Recursively convert a parsed HTML node into `#html.elem` calls. The
/// document/html/head/body wrappers html5ever inserts are unwrapped.
fn convert_html_node(node: &HtmlNode, out: &mut String) {
    match node {
        HtmlNode::Document(children) => convert_html_siblings(children, out),
        HtmlNode::Doctype(_) | HtmlNode::Comment(_) => {}
        HtmlNode::Text(t) => out.push_str(&escape_markup(t)),
        HtmlNode::Element(el) => {
            if matches!(el.name.as_str(), "html" | "head" | "body") {
                convert_html_siblings(&el.children, out);
                return;
            }
            let dict = typst_attrs(el.attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            write!(out, "#html.elem(\"{}\"{dict})", el.name).unwrap();
            if el.children.is_empty() && is_void(&el.name) {
                return;
            }
            out.push('[');
            convert_html_siblings(&el.children, out);
            out.push(']');
        }
    }
}

/// Render a sequence of sibling HTML nodes, inserting `#[]` between an element
/// node and a following text node that starts with `(` — typst would otherwise
/// parse the `(` as additional function arguments to the preceding expression.
fn convert_html_siblings(children: &[HtmlNode], out: &mut String) {
    let mut prev_was_elem = false;
    for c in children {
        if prev_was_elem {
            if let HtmlNode::Text(t) = c {
                if t.starts_with('(') {
                    out.push_str("/**/");
                }
            }
        }
        prev_was_elem = matches!(c, HtmlNode::Element(_));
        convert_html_node(c, out);
    }
}

/// Whether typst keeps `tag` in a paragraph when it appears inline. We mirror
/// typst's own [`should_group_into_pars`](typst_html::tag::should_group_into_pars)
/// so an inline `html.elem` gets `box()`-wrapped exactly when typst would
/// otherwise break the paragraph around it. Unknown tags are assumed to group
/// (no box).
fn groups_into_pars(tag: &str) -> bool {
    typst_html::HtmlTag::intern(tag)
        .map(typst_html::tag::should_group_into_pars)
        .unwrap_or(true)
}

/// HTML5 void elements — emitted without a body. Public because the frontend
/// also needs it: a void tag has no close event, so it must become a leaf
/// rather than an open container.
pub fn is_void(name: &str) -> bool {
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

fn align_name(a: Align) -> &'static str {
    match a {
        Align::Left => "left",
        Align::Center => "center",
        Align::Right => "right",
        Align::None => "auto",
    }
}
