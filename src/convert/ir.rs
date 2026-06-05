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
use crate::slug::slugify;

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
    /// Inline HTML element — children are markdown inlines.
    Html {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Content,
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

/// Render a document body (sequence of blocks) into typst markup.
pub fn render(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        render_block(b, &mut out);
    }
    out
}

fn render_block(b: &Block, out: &mut String) {
    match b {
        Block::Heading { level, content } => {
            for _ in 0..*level {
                out.push('=');
            }
            out.push(' ');
            render_inlines(content, out);
            // A `<slug>` label so headings are link targets, matching zola's
            // pulldown auto-id (computed from the heading's plain text).
            writeln!(out, " <{}>\n", slugify(&plain_text(content))).unwrap();
        }
        Block::Para(content) => {
            render_inlines(content, out);
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
                render_block(b, out);
            }
            out.push_str("]\n\n");
        }
        Block::List { ordered, items } => {
            for item in items {
                out.push_str(if *ordered { "+ " } else { "- " });
                render_item(item, out);
            }
            out.push('\n');
        }
        Block::Table { align, head, rows } => render_table(align, head, rows, out),
        Block::Rule => out.push_str("#html.hr()\n\n"),
        Block::Shortcode { name, args, body } => {
            writeln!(out, "\n#{name}({args})[").unwrap();
            for b in body {
                render_block(b, out);
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
fn render_item(blocks: &[Block], out: &mut String) {
    if let [Block::Para(content)] = blocks {
        render_inlines(content, out);
        out.push('\n');
    } else {
        for b in blocks {
            render_block(b, out);
        }
    }
}

fn render_table(align: &[Align], head: &[Content], rows: &[Vec<Content>], out: &mut String) {
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
            render_inlines(cell, out);
            out.push_str("], ");
        }
        out.push_str("),\n");
    }
    for row in rows {
        out.push_str("  ");
        for cell in row {
            out.push('[');
            render_inlines(cell, out);
            out.push_str("], ");
        }
        out.push('\n');
    }
    out.push_str(")\n\n");
}

fn render_inlines(inlines: &[Inline], out: &mut String) {
    for i in inlines {
        render_inline(i, out);
    }
}

fn render_inline(i: &Inline, out: &mut String) {
    match i {
        Inline::Text(s) => out.push_str(&escape_markup(s)),
        Inline::Code(s) => write!(out, "`{s}`").unwrap(),
        Inline::Emph(c) => wrap(out, "_", c, "_"),
        Inline::Strong(c) => wrap(out, "*", c, "*"),
        Inline::Strike(c) => wrap(out, "#strike[", c, "]"),
        Inline::Link { dest, content } => {
            if let Some(frag) = dest.strip_prefix('#') {
                write!(out, "#link(<{frag}>)[").unwrap();
            } else {
                write!(out, "#link(\"{}\")[", escape_typst_string(dest)).unwrap();
            }
            render_inlines(content, out);
            out.push(']');
        }
        Inline::Shortcode { name, args } => write!(out, "#{name}({args})").unwrap(),
        Inline::Html {
            tag,
            attrs,
            children,
        } => {
            let dict = typst_attrs(attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            write!(out, "#html.elem(\"{tag}\"{dict})").unwrap();
            if children.is_empty() && is_void(tag) {
                return;
            }
            out.push('[');
            render_inlines(children, out);
            out.push(']');
        }
        Inline::SoftBreak => out.push('\n'),
        Inline::HardBreak => out.push_str(" \\\n"),
        Inline::Raw(s) => write!(out, "/* TODO twyla-convert: {s} */").unwrap(),
        Inline::Verbatim(s) => out.push_str(s),
    }
}

fn wrap(out: &mut String, open: &str, content: &[Inline], close: &str) {
    out.push_str(open);
    render_inlines(content, out);
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
        | Inline::Html { children: c, .. } => {
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
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
        HtmlNode::Document(children) => {
            for c in children {
                convert_html_node(c, out);
            }
        }
        HtmlNode::Doctype(_) | HtmlNode::Comment(_) => {}
        HtmlNode::Text(t) => out.push_str(&escape_markup(t)),
        HtmlNode::Element(el) => {
            if matches!(el.name.as_str(), "html" | "head" | "body") {
                for c in &el.children {
                    convert_html_node(c, out);
                }
                return;
            }
            let dict = typst_attrs(el.attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            write!(out, "#html.elem(\"{}\"{dict})", el.name).unwrap();
            if el.children.is_empty() && is_void(&el.name) {
                return;
            }
            out.push('[');
            for c in &el.children {
                convert_html_node(c, out);
            }
            out.push(']');
        }
    }
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
