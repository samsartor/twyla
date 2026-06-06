//! `twyla import` — the zola-markdown **frontend** of the convert pipeline.
//!
//! It parses a zola markdown post (frontmatter + CommonMark + zola shortcodes +
//! inline/block HTML) into the shared [`crate::convert::ir`] tree, which the IR
//! renderer turns into a typst draft. Supporting another SSG means writing
//! another frontend targeting the same IR; the renderer is reused.
//!
//! Output is a `.typ` draft the porter cleans up by hand — scaffolding, not a
//! maintained md↔typ sync. Coverage targets the personal-site corpus:
//!
//! - TOML frontmatter (`+++ … +++`) → `#set document(..)` + `{kind}-template`.
//! - Headings (with a `<slug>` label), paragraphs, blockquotes, rules,
//!   ordered/unordered lists, fenced/inline code, emphasis, strong,
//!   strikethrough, soft/hard breaks.
//! - Tables → `#table(columns:, align:, table.header(..), ..)`.
//! - Links → `#link(..)` (anchor `#frag` → label link).
//! - Inline and block HTML → `#html.elem("tag", attrs: (..))[..]`.
//! - Zola shortcodes anywhere (mid-paragraph, in table cells) →
//!   `#name(args)` / `#name(args)[body]`.
//!
//! Anything we can't translate (footnotes, math, …) is re-serialized to
//! markdown inside a `/* TODO twyla-convert: … */` comment for the porter.

use std::cell::RefCell;
use std::fmt::Write as _;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, CommentToken, EndTag, StartTag, TagToken, Token, TokenSink, TokenSinkResult,
    Tokenizer, TokenizerOpts,
};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use unscanny::Scanner;

use crate::convert::ir::{self, Align, Block, Content, Inline};
use crate::html;

/// Convert a zola markdown source into a typst draft.
///
/// `kind` selects the `{kind}-template` the draft shows (e.g. `page`, `dir`,
/// `root` — see [`TwylaContext::default_kind`](crate::project::TwylaContext::default_kind)).
/// `output` is an optional explicit output path: when zola's route diverges
/// from twyla's filename-derived default (e.g. an underscore that zola
/// slugifies to a hyphen), the convert harness passes it through so the draft
/// carries `#set document(output: ..)`. Pass `None` for the standalone
/// `twyla import` primitive.
pub fn import_md(input: &str, kind: &str, output: Option<&str>) -> Result<String, String> {
    let (fm_text, body) = split_frontmatter(input)?;
    let meta = parse_frontmatter(fm_text)?;
    // Zola's `<!-- more -->` marks the end of the summary: render the markdown
    // above it as the document description. The marker is dropped; the content
    // above it stays in the body (the full article still shows it).
    let (summary_md, body) = split_summary(body);
    let summary = summary_md.map(|md| ir::render(&parse_blocks(&preprocess_shortcodes(md))));
    let blocks = parse_blocks(&preprocess_shortcodes(&body));
    Ok(assemble(&meta, &ir::render(&blocks), summary.as_deref(), kind, output))
}

/// Split a body at zola's first `<!-- more -->` delimiter. Returns the summary
/// markdown above it (when non-empty) and the body with the marker removed.
/// Tolerates inner-whitespace variants (`<!--more-->`, `<!-- more -->`).
fn split_summary(body: &str) -> (Option<&str>, String) {
    let mut from = 0;
    while let Some(rel) = body[from..].find("<!--") {
        let start = from + rel;
        let Some(end_rel) = body[start..].find("-->") else {
            break;
        };
        let end = start + end_rel + "-->".len();
        if body[start + "<!--".len()..end - "-->".len()].trim() == "more" {
            let above = &body[..start];
            let rest = format!("{above}{}", &body[end..]);
            let summary = (!above.trim().is_empty()).then_some(above);
            return (summary, rest);
        }
        from = end;
    }
    (None, body.to_string())
}

// ---- frontmatter ---------------------------------------------------------

struct Meta {
    title: String,
    description: String,
    date: Option<(i64, u8, u8)>,
    draft: bool,
    /// The frontmatter `[extra]` table, carried verbatim onto
    /// `#set document(extra: ..)`. `None` when absent or empty.
    extra: Option<toml::value::Table>,
}

fn split_frontmatter(input: &str) -> Result<(&str, &str), String> {
    let input = input.trim_start_matches('\u{FEFF}');
    let rest = input
        .strip_prefix("+++")
        .ok_or_else(|| "expected `+++` frontmatter at start of file".to_string())?;
    let rest = rest.trim_start_matches('\n');
    let close = rest
        .find("\n+++")
        .ok_or_else(|| "no closing `+++` for frontmatter block".to_string())?;
    let fm = &rest[..close];
    let body = &rest[close + 4..];
    Ok((fm, body.trim_start_matches('\n')))
}

fn parse_frontmatter(fm: &str) -> Result<Meta, String> {
    let val: toml::Value = toml::from_str(fm).map_err(|e| format!("frontmatter TOML: {e}"))?;
    let title = val
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "frontmatter missing `title`".to_string())?
        .to_string();
    let description = val
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Zola frontmatter dates are TOML local dates (`YYYY-MM-DD`).
    let date = val.get("date").and_then(|v| {
        let s = v
            .as_datetime()
            .map(|d| d.to_string())
            .or_else(|| v.as_str().map(String::from))?;
        let mut parts = s.splitn(3, '-');
        let y = parts.next()?.parse().ok()?;
        let m = parts.next()?.parse().ok()?;
        let d = parts
            .next()?
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .ok()?;
        Some((y, m, d))
    });
    let draft = val.get("draft").and_then(|v| v.as_bool()).unwrap_or(false);
    let extra = val
        .get("extra")
        .and_then(|v| v.as_table())
        .filter(|t| !t.is_empty())
        .cloned();
    Ok(Meta {
        title,
        description,
        date,
        draft,
        extra,
    })
}

/// Render a TOML value as a typst value literal. Tables become typst
/// dictionaries (`(key: val)`), arrays become typst arrays, scalars map
/// directly; TOML datetimes are emitted as strings (a draft author can promote
/// them to `datetime(..)` if they want). `indent` is the nesting depth — the
/// level the value's *closing* delimiter sits at; children indent one deeper.
fn toml_to_typst(v: &toml::Value, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    let pad1 = "  ".repeat(indent + 1);
    match v {
        toml::Value::String(s) => format!("\"{}\"", ir::escape_typst_string(s)),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(dt) => format!("\"{dt}\""),
        toml::Value::Array(arr) if arr.is_empty() => "()".to_string(),
        toml::Value::Array(arr) => {
            // Inline arrays of scalars; break arrays that nest.
            let nested = arr
                .iter()
                .any(|x| matches!(x, toml::Value::Array(_) | toml::Value::Table(_)));
            if !nested {
                let items: Vec<_> = arr.iter().map(|x| toml_to_typst(x, indent)).collect();
                // A one-element typst array needs a trailing comma: `(x,)`.
                let trailing = if items.len() == 1 { "," } else { "" };
                format!("({}{trailing})", items.join(", "))
            } else {
                let items: Vec<_> = arr
                    .iter()
                    .map(|x| format!("{pad1}{}", toml_to_typst(x, indent + 1)))
                    .collect();
                format!("(\n{},\n{pad})", items.join(",\n"))
            }
        }
        toml::Value::Table(t) if t.is_empty() => "(:)".to_string(),
        toml::Value::Table(t) => {
            let items: Vec<_> = t
                .iter()
                .map(|(k, val)| format!("{pad1}{}: {}", typst_dict_key(k), toml_to_typst(val, indent + 1)))
                .collect();
            format!("(\n{},\n{pad})", items.join(",\n"))
        }
    }
}

/// A dictionary key as typst source: a bare identifier when it is one, else a
/// quoted string key (typst dicts accept both).
fn typst_dict_key(k: &str) -> String {
    let is_ident = !k.is_empty()
        && k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if is_ident {
        k.to_string()
    } else {
        format!("\"{}\"", ir::escape_typst_string(k))
    }
}

// ---- shortcode preprocessing --------------------------------------------
//
// Zola shortcodes (`{{ … }}` and `{% … %}`) aren't markdown syntax — pulldown
// sees them as text. We replace each occurrence with an HTML-comment marker so
// pulldown emits a discrete `Html`/`InlineHtml` event the builder intercepts.

const MARKER_OPEN: &str = "<!--TWYLA-SC-OPEN:";
const MARKER_CLOSE: &str = "<!--TWYLA-SC-CLOSE-->";
const MARKER_INLINE: &str = "<!--TWYLA-SC-INLINE:";
const MARKER_END: &str = "-->";

fn preprocess_shortcodes(body: &str) -> String {
    let mut out = String::new();
    let mut s = Scanner::new(body);
    loop {
        if s.eat_if("````") {
            out += "````";
            out += s.eat_until("````");
            if s.eat_if("```") {
                out += "````";
            }
        } else if s.eat_if('`') {
            out += "`";
            out += s.eat_until('`');
            if s.eat_if('`') {
                out += "`";
            }
        } else if s.eat_if('{') {
            if s.eat_if('{') {
                let inner = s.eat_until("}}");
                if !s.eat_if("}}") {
                    out += "{{";
                    out += inner;
                    return out;
                }
                let inner = inner.trim();
                write!(out, "{MARKER_INLINE}{inner}{MARKER_END}").unwrap();
            } else if s.eat_if('%') {
                let inner = s.eat_until("%}");
                if !s.eat_if("%}") {
                    out += "{%";
                    out += inner;
                    return out;
                }
                let inner = inner.trim();
                if inner == "end" {
                    write!(out, "\n\n{MARKER_CLOSE}\n\n").unwrap();
                } else {
                    write!(out, "\n\n{MARKER_OPEN}{inner}{MARKER_END}\n\n").unwrap();
                }
            } else {
                out += "{";
            }
        } else {
            match s.eat() {
                Some(c) => out.push(c),
                None => return out,
            }
        }
    }
}

// ---- markdown events → IR ------------------------------------------------

/// Parse preprocessed markdown into the IR block tree.
fn parse_blocks(md: &str) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let mut builder = Builder::new();
    for ev in Parser::new_ext(md, opts) {
        builder.event(ev);
    }
    builder.finish()
}

/// A partially-built container on the builder's stack. Each frame collects
/// either child blocks or child inlines; on the matching `End` event it's
/// converted to a node and attached to its parent.
enum Frame {
    // block containers
    Root(Vec<Block>),
    Quote(Vec<Block>),
    Item(Vec<Block>),
    Shortcode {
        name: String,
        args: String,
        body: Vec<Block>,
    },
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Table {
        align: Vec<Align>,
        head: Vec<Content>,
        rows: Vec<Vec<Content>>,
        row: Vec<Content>,
    },
    // inline containers
    Para(Content),
    Heading {
        level: u8,
        content: Content,
    },
    Emph(Content),
    Strong(Content),
    Strike(Content),
    Link {
        dest: String,
        content: Content,
    },
    HtmlElem {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Content,
    },
    TableCell(Content),
    // text accumulator
    Code {
        lang: String,
        text: String,
    },
}

/// Builds the IR tree from pulldown's flat event stream via a frame stack.
struct Builder<'a> {
    stack: Vec<Frame>,
    /// While inside a block-level HTML block, accumulates its raw text.
    html_block: Option<String>,
    /// While inside an unsupported container, collects its sub-events to
    /// re-serialize as a markdown TODO.
    unsupported: Option<Unsupported<'a>>,
}

struct Unsupported<'a> {
    depth: u32,
    inline: bool,
    events: Vec<Event<'a>>,
}

impl<'a> Builder<'a> {
    fn new() -> Self {
        Self {
            stack: vec![Frame::Root(Vec::new())],
            html_block: None,
            unsupported: None,
        }
    }

    fn finish(mut self) -> Vec<Block> {
        // Gracefully close any frames left open by malformed input, so a single
        // unbalanced tag degrades to partial output instead of losing the doc.
        while self.close_top() {}
        match self.stack.pop() {
            Some(Frame::Root(blocks)) => blocks,
            _ => Vec::new(),
        }
    }

    /// Force-close the top frame, attaching its accumulated content to its
    /// parent. Returns `false` once only `Root` remains.
    fn close_top(&mut self) -> bool {
        if self.stack.len() <= 1 {
            return false;
        }
        match self.stack.pop().unwrap() {
            Frame::Root(_) => return false,
            Frame::Para(c) => self.push_block(Block::Para(c)),
            Frame::Heading { level, content } => self.push_block(Block::Heading { level, content }),
            Frame::Quote(b) => self.push_block(Block::Quote(b)),
            Frame::Item(b) => {
                if let Some(Frame::List { items, .. }) = self.stack.last_mut() {
                    items.push(b);
                }
            }
            Frame::Shortcode { name, args, body } => {
                self.push_block(Block::Shortcode { name, args, body })
            }
            Frame::List { ordered, items } => self.push_block(Block::List { ordered, items }),
            Frame::Table {
                align, head, rows, ..
            } => self.push_block(Block::Table { align, head, rows }),
            Frame::Code { lang, text } => self.push_block(Block::Code { lang, text }),
            Frame::Emph(c) => self.push_inline(Inline::Emph(c)),
            Frame::Strong(c) => self.push_inline(Inline::Strong(c)),
            Frame::Strike(c) => self.push_inline(Inline::Strike(c)),
            Frame::Link { dest, content } => self.push_inline(Inline::Link { dest, content }),
            Frame::HtmlElem {
                tag,
                attrs,
                children,
            } => self.push_inline(Inline::Html {
                tag,
                attrs,
                children,
            }),
            Frame::TableCell(c) => {
                if let Some(Frame::Table { row, .. }) = self.stack.last_mut() {
                    row.push(c);
                }
            }
        }
        true
    }

    fn event(&mut self, ev: Event<'a>) {
        // Collecting an unsupported container: gather until depth returns to 0.
        if let Some(u) = &mut self.unsupported {
            match &ev {
                Event::Start(_) => u.depth += 1,
                Event::End(_) => u.depth -= 1,
                _ => {}
            }
            let done = u.depth == 0;
            u.events.push(ev);
            if done {
                let u = self.unsupported.take().unwrap();
                let mut md = String::new();
                let _ = pulldown_cmark_to_cmark::cmark(u.events.into_iter(), &mut md);
                if u.inline {
                    self.push_inline(Inline::Raw(md));
                } else {
                    self.push_block(Block::Raw(md));
                }
            }
            return;
        }

        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(s) => self.text(&s),
            Event::Code(s) => self.push_inline(Inline::Code(s.to_string())),
            Event::Html(s) => self.block_html(&s),
            Event::InlineHtml(s) => self.inline_html(&s),
            Event::SoftBreak => self.push_inline(Inline::SoftBreak),
            Event::HardBreak => self.push_inline(Inline::HardBreak),
            Event::Rule => self.push_block(Block::Rule),
            other => self.unsupported(other),
        }
    }

    fn start(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::Paragraph => self.stack.push(Frame::Para(Vec::new())),
            Tag::Heading { level, .. } => self.stack.push(Frame::Heading {
                level: level as u8,
                content: Vec::new(),
            }),
            Tag::BlockQuote(_) => self.stack.push(Frame::Quote(Vec::new())),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(s) => s.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.stack.push(Frame::Code {
                    lang,
                    text: String::new(),
                });
            }
            Tag::List(start) => self.stack.push(Frame::List {
                ordered: start.is_some(),
                items: Vec::new(),
            }),
            Tag::Item => self.stack.push(Frame::Item(Vec::new())),
            Tag::HtmlBlock => self.html_block = Some(String::new()),
            Tag::Table(aligns) => self.stack.push(Frame::Table {
                align: aligns.iter().map(|a| ir_align(*a)).collect(),
                head: Vec::new(),
                rows: Vec::new(),
                row: Vec::new(),
            }),
            // Head/row boundaries are handled at their `End`; cells accumulate
            // into the table frame's current `row`.
            Tag::TableHead | Tag::TableRow => {}
            Tag::TableCell => self.stack.push(Frame::TableCell(Vec::new())),
            Tag::Emphasis => self.stack.push(Frame::Emph(Vec::new())),
            Tag::Strong => self.stack.push(Frame::Strong(Vec::new())),
            Tag::Strikethrough => self.stack.push(Frame::Strike(Vec::new())),
            Tag::Link { dest_url, .. } => self.stack.push(Frame::Link {
                dest: dest_url.to_string(),
                content: Vec::new(),
            }),
            other => self.unsupported(Event::Start(other)),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if let Some(Frame::Para(c)) = self.stack.pop() {
                    self.push_block(Block::Para(c));
                }
            }
            TagEnd::Heading(_) => {
                if let Some(Frame::Heading { level, content }) = self.stack.pop() {
                    self.push_block(Block::Heading { level, content });
                }
            }
            TagEnd::BlockQuote(_) => {
                if let Some(Frame::Quote(b)) = self.stack.pop() {
                    self.push_block(Block::Quote(b));
                }
            }
            TagEnd::CodeBlock => {
                if let Some(Frame::Code { lang, text }) = self.stack.pop() {
                    self.push_block(Block::Code { lang, text });
                }
            }
            TagEnd::List(_) => {
                if let Some(Frame::List { ordered, items }) = self.stack.pop() {
                    self.push_block(Block::List { ordered, items });
                }
            }
            TagEnd::Item => {
                if let Some(Frame::Item(b)) = self.stack.pop() {
                    if let Some(Frame::List { items, .. }) = self.stack.last_mut() {
                        items.push(b);
                    }
                }
            }
            TagEnd::Emphasis => {
                if let Some(Frame::Emph(c)) = self.stack.pop() {
                    self.push_inline(Inline::Emph(c));
                }
            }
            TagEnd::Strong => {
                if let Some(Frame::Strong(c)) = self.stack.pop() {
                    self.push_inline(Inline::Strong(c));
                }
            }
            TagEnd::Strikethrough => {
                if let Some(Frame::Strike(c)) = self.stack.pop() {
                    self.push_inline(Inline::Strike(c));
                }
            }
            TagEnd::Link => {
                if let Some(Frame::Link { dest, content }) = self.stack.pop() {
                    self.push_inline(Inline::Link { dest, content });
                }
            }
            TagEnd::Table => {
                if let Some(Frame::Table {
                    align, head, rows, ..
                }) = self.stack.pop()
                {
                    self.push_block(Block::Table { align, head, rows });
                }
            }
            TagEnd::TableHead => {
                if let Some(Frame::Table { head, row, .. }) = self.stack.last_mut() {
                    *head = std::mem::take(row);
                }
            }
            TagEnd::TableRow => {
                if let Some(Frame::Table { rows, row, .. }) = self.stack.last_mut() {
                    rows.push(std::mem::take(row));
                }
            }
            TagEnd::TableCell => {
                if let Some(Frame::TableCell(c)) = self.stack.pop() {
                    if let Some(Frame::Table { row, .. }) = self.stack.last_mut() {
                        row.push(c);
                    }
                }
            }
            TagEnd::HtmlBlock => self.flush_html_block(),
            _ => {}
        }
    }

    fn text(&mut self, s: &str) {
        // Inside a fenced code block, accumulate raw (rendered verbatim).
        if let Some(Frame::Code { text, .. }) = self.stack.last_mut() {
            text.push_str(s);
            return;
        }
        self.push_inline(Inline::Text(s.to_string()));
    }

    /// Block-level HTML (`Event::Html`): a shortcode marker, or raw HTML text
    /// accumulated for conversion at `End(HtmlBlock)`.
    fn block_html(&mut self, s: &str) {
        if self.handle_marker(s.trim(), false) {
            return;
        }
        match &mut self.html_block {
            Some(buf) => buf.push_str(s),
            None => self.push_block(Block::Html(html::parse_html(s))),
        }
    }

    fn flush_html_block(&mut self) {
        let buf = self.html_block.take().unwrap_or_default();
        if buf.trim().is_empty() {
            return;
        }
        self.push_block(Block::Html(html::parse_html(&buf)));
    }

    /// Inline HTML (`Event::InlineHtml`): a single tag. An open tag pushes an
    /// `HtmlElem` frame (markdown content flows into it); the close pops it.
    fn inline_html(&mut self, s: &str) {
        if self.handle_marker(s.trim(), true) {
            return;
        }
        match parse_html_tag(s) {
            HtmlTag::Open {
                name,
                attrs,
                self_closing,
            } => {
                if self_closing || ir::is_void(&name) {
                    self.push_inline(Inline::Html {
                        tag: name,
                        attrs,
                        children: Vec::new(),
                    });
                } else {
                    self.stack.push(Frame::HtmlElem {
                        tag: name,
                        attrs,
                        children: Vec::new(),
                    });
                }
            }
            HtmlTag::Close => {
                if matches!(self.stack.last(), Some(Frame::HtmlElem { .. })) {
                    if let Some(Frame::HtmlElem {
                        tag,
                        attrs,
                        children,
                    }) = self.stack.pop()
                    {
                        self.push_inline(Inline::Html {
                            tag,
                            attrs,
                            children,
                        });
                    }
                }
            }
            HtmlTag::Comment => {}
            HtmlTag::Other(raw) => self.push_inline(Inline::Raw(format!("review raw html {raw}"))),
        }
    }

    /// Handle a preprocessed shortcode marker. Block markers push/pop a
    /// `Shortcode` frame; inline markers become an inline (or, on their own
    /// line via `Event::Html`, a one-shortcode paragraph). Returns `true` when
    /// `s` was a marker.
    fn handle_marker(&mut self, s: &str, inline: bool) -> bool {
        if let Some(inner) = s
            .strip_prefix(MARKER_OPEN)
            .and_then(|x| x.strip_suffix(MARKER_END))
        {
            match parse_shortcode(inner) {
                Some((name, args)) => self.stack.push(Frame::Shortcode {
                    name,
                    args,
                    body: Vec::new(),
                }),
                None => self.push_block(Block::Raw(format!("review block shortcode {inner}"))),
            }
            return true;
        }
        if s == MARKER_CLOSE {
            if let Some(Frame::Shortcode { name, args, body }) = self.stack.pop() {
                self.push_block(Block::Shortcode { name, args, body });
            }
            return true;
        }
        if let Some(inner) = s
            .strip_prefix(MARKER_INLINE)
            .and_then(|x| x.strip_suffix(MARKER_END))
        {
            match parse_shortcode(inner) {
                Some((name, args)) if inline => self.push_inline(Inline::Shortcode { name, args }),
                Some((name, args)) => {
                    self.push_block(Block::Para(vec![Inline::Shortcode { name, args }]))
                }
                None => self.push_block(Block::Raw(format!("review inline shortcode {inner}"))),
            }
            return true;
        }
        false
    }

    /// Begin (or, for a leaf, immediately emit) an untranslatable event,
    /// re-serialized to markdown inside a TODO comment.
    fn unsupported(&mut self, ev: Event<'a>) {
        if !matches!(ev, Event::Start(_)) {
            let mut md = String::new();
            let _ = pulldown_cmark_to_cmark::cmark([ev].into_iter(), &mut md);
            self.push_inline(Inline::Raw(md));
            return;
        }
        let inline = self.top_is_inline();
        self.unsupported = Some(Unsupported {
            depth: 1,
            inline,
            events: vec![ev],
        });
    }

    fn top_is_inline(&self) -> bool {
        matches!(
            self.stack.last(),
            Some(
                Frame::Para(_)
                    | Frame::Heading { .. }
                    | Frame::Emph(_)
                    | Frame::Strong(_)
                    | Frame::Strike(_)
                    | Frame::Link { .. }
                    | Frame::HtmlElem { .. }
                    | Frame::TableCell(_)
            )
        )
    }

    fn push_inline(&mut self, inl: Inline) {
        match self.stack.last_mut() {
            Some(
                Frame::Para(v)
                | Frame::Heading { content: v, .. }
                | Frame::Emph(v)
                | Frame::Strong(v)
                | Frame::Strike(v)
                | Frame::Link { content: v, .. }
                | Frame::HtmlElem { children: v, .. }
                | Frame::TableCell(v),
            ) => v.push(inl),
            // A block container with loose inline content — e.g. a *tight* list
            // item, whose inlines pulldown emits with no `Paragraph` wrapper.
            // Coalesce into a trailing paragraph instead of one para per inline.
            Some(
                Frame::Root(blocks)
                | Frame::Quote(blocks)
                | Frame::Item(blocks)
                | Frame::Shortcode { body: blocks, .. },
            ) => match blocks.last_mut() {
                Some(Block::Para(p)) => p.push(inl),
                _ => blocks.push(Block::Para(vec![inl])),
            },
            _ => {}
        }
    }

    fn push_block(&mut self, blk: Block) {
        match self.stack.last_mut() {
            Some(
                Frame::Root(v)
                | Frame::Quote(v)
                | Frame::Item(v)
                | Frame::Shortcode { body: v, .. },
            ) => {
                v.push(blk);
                return;
            }
            _ => {}
        }
        // Block content landing in an inline context (e.g. a code block inside
        // an inline `<a>`): render it now and embed it as verbatim inline so
        // nothing is lost. Drop only if there's no inline container either.
        if self.top_is_inline() {
            self.push_inline(Inline::Verbatim(ir::render(std::slice::from_ref(&blk))));
        }
    }
}

fn ir_align(a: Alignment) -> Align {
    match a {
        Alignment::Left => Align::Left,
        Alignment::Center => Align::Center,
        Alignment::Right => Align::Right,
        Alignment::None => Align::None,
    }
}

// ---- zola shortcode arg parsing ------------------------------------------

fn parse_shortcode(inner: &str) -> Option<(String, String)> {
    // `name(arg="val", ..)` — translate to typst `name(arg: "val", ..)`.
    let open = inner.find('(')?;
    let close = inner.rfind(')')?;
    if close < open {
        return None;
    }
    let name = inner[..open].trim().to_string();
    let args_raw = &inner[open + 1..close];
    let args = translate_args(args_raw);
    Some((name, args))
}

/// Translate `a="x", b="y"` to `a: "x", b: "y"`. Conservative — we
/// don't try to fix odd zola argument syntax (numbers without quotes,
/// arrays, etc.). The porter can clean up the few cases.
fn translate_args(s: &str) -> String {
    let mut out = String::new();
    for (i, part) in split_top_commas(s).iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let trimmed = part.trim();
        if let Some(eq) = trimmed.find('=') {
            let key = trimmed[..eq].trim();
            let val = trimmed[eq + 1..].trim();
            write!(out, "{key}: {val}").unwrap();
        } else {
            out.push_str(trimmed);
        }
    }
    out
}

fn split_top_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '(' | '[' if !in_str => depth += 1,
            ')' | ']' if !in_str => depth -= 1,
            ',' if !in_str && depth == 0 => {
                parts.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if start < s.len() {
        parts.push(&s[start..]);
    }
    parts
}

// ---- inline HTML tag parsing (html5ever tokenizer) -----------------------

/// A single inline HTML tag, as pulldown hands them to us one at a time.
enum HtmlTag {
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    Close,
    Comment,
    /// Not a recognizable tag (stray text, malformed) — passed through.
    Other(String),
}

/// Collects the first tag/comment token from the html5ever tokenizer, ignoring
/// the rest. `process_token` is `&self`, so the result rides a `RefCell`.
#[derive(Default)]
struct TagSink {
    tag: RefCell<Option<HtmlTag>>,
}

impl TokenSink for TagSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        if self.tag.borrow().is_some() {
            return TokenSinkResult::Continue;
        }
        let parsed = match token {
            TagToken(tag) => {
                let name = tag.name.to_string();
                let attrs = tag
                    .attrs
                    .iter()
                    .map(|a| (a.name.local.to_string(), a.value.to_string()))
                    .collect();
                match tag.kind {
                    StartTag => Some(HtmlTag::Open {
                        name,
                        attrs,
                        self_closing: tag.self_closing,
                    }),
                    EndTag => Some(HtmlTag::Close),
                }
            }
            CommentToken(_) => Some(HtmlTag::Comment),
            _ => None,
        };
        if parsed.is_some() {
            *self.tag.borrow_mut() = parsed;
        }
        TokenSinkResult::Continue
    }
}

/// Parse one inline HTML tag (`<col-s space="x">`, `</col-s>`, `<br/>`,
/// `<!--…-->`) with the html5ever tokenizer — spec-compliant attribute parsing
/// (quote styles, entity decoding) and the start/end/self-closing distinction,
/// without building a tree (which would erase that distinction).
fn parse_html_tag(raw: &str) -> HtmlTag {
    let tok = Tokenizer::new(TagSink::default(), TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(raw));
    let _ = tok.feed(&input);
    tok.end();
    tok.sink
        .tag
        .borrow_mut()
        .take()
        .unwrap_or_else(|| HtmlTag::Other(raw.to_string()))
}

// ---- assembly ------------------------------------------------------------

fn assemble(
    meta: &Meta,
    body: &str,
    summary: Option<&str>,
    kind: &str,
    output: Option<&str>,
) -> String {
    let esc = ir::escape_typst_string;
    let mut out = String::new();
    writeln!(out, "// twyla-convert draft. Manual cleanup expected!").unwrap();
    writeln!(out, "// Inspect any TODO markers below").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "#import \"/templates/lib.typ\": {kind}-template").unwrap();
    writeln!(out).unwrap();

    // Page metadata lives on the document, where twyla harvests it for
    // listings/feeds and the template reads it via `#context document.*`.
    writeln!(out, "#set document(").unwrap();
    writeln!(out, "  title: \"{}\",", esc(&meta.title)).unwrap();
    // A `<!-- more -->` summary (rendered typst content) wins over a frontmatter
    // `description` string; either lands on `document.description`.
    match summary {
        Some(s) => {
            let s = s.trim();
            if s.contains('\n') {
                writeln!(out, "  description: [").unwrap();
                for line in s.lines() {
                    writeln!(out, "    {line}").unwrap();
                }
                writeln!(out, "  ],").unwrap();
            } else {
                writeln!(out, "  description: [{s}],").unwrap();
            }
        }
        None if !meta.description.is_empty() => {
            writeln!(out, "  description: \"{}\",", esc(&meta.description)).unwrap();
        }
        None => {}
    }
    if let Some((y, m, d)) = meta.date {
        writeln!(out, "  date: datetime(year: {y}, month: {m}, day: {d}),").unwrap();
    }
    if meta.draft {
        writeln!(out, "  draft: true,").unwrap();
    }
    if let Some(output) = output {
        // Zola's route diverges from twyla's filename default — pin it.
        writeln!(out, "  output: \"{}\",", esc(output)).unwrap();
    }
    if let Some(extra) = &meta.extra {
        // Carry the frontmatter `[extra]` table onto the document verbatim.
        let dict = toml_to_typst(&toml::Value::Table(extra.clone()), 1);
        writeln!(out, "  extra: {dict},").unwrap();
    }
    writeln!(out, ")").unwrap();
    writeln!(out, "#show: {kind}-template").unwrap();
    writeln!(out).unwrap();
    out.push_str(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse markdown straight to a rendered typst body (no frontmatter).
    fn body(md: &str) -> String {
        ir::render(&parse_blocks(md))
    }

    /// The full shortcode path: preprocess (text → markers) then build + render.
    fn convert(md: &str) -> String {
        body(&preprocess_shortcodes(md))
    }

    #[test]
    fn frontmatter_split() {
        let src = "+++\ntitle = \"Hi\"\n+++\nbody\n";
        let (fm, b) = split_frontmatter(src).unwrap();
        assert_eq!(fm, "title = \"Hi\"");
        assert_eq!(b, "body\n");
    }

    #[test]
    fn extra_table_carried_onto_document() {
        let src = "+++\ntitle = \"T\"\n[extra]\nfeatured = true\ntags = [\"a\", \"b\"]\n\
                   weight = 2\n\"odd key\" = \"v\"\n[extra.nested]\nx = 1\n+++\nbody\n";
        let out = import_md(src, "page", None).unwrap();
        assert!(out.contains("extra: ("), "got: {out}");
        assert!(out.contains("featured: true"), "got: {out}");
        assert!(out.contains("tags: (\"a\", \"b\")"), "got: {out}");
        assert!(out.contains("weight: 2"), "got: {out}");
        assert!(out.contains("\"odd key\": \"v\""), "got: {out}");
        assert!(out.contains("nested: (") && out.contains("x: 1"), "got: {out}");
    }

    #[test]
    fn more_marker_sets_description_from_summary() {
        let src = "+++\ntitle = \"T\"\ndescription = \"fm\"\n+++\n\
                   The **lead** paragraph.\n\n<!-- more -->\n\nRest of the body.\n";
        let out = import_md(src, "page", None).unwrap();
        // Summary above `<!-- more -->` becomes a rich description, overriding fm.
        assert!(out.contains("description: [The *lead* paragraph.],"), "got:\n{out}");
        assert!(!out.contains("description: \"fm\""), "fm desc should be overridden:\n{out}");
        // Marker is gone; the lead still appears in the body.
        assert!(!out.contains("<!-- more -->"), "marker should be stripped:\n{out}");
        assert!(out.contains("Rest of the body."), "got:\n{out}");
    }

    #[test]
    fn no_more_marker_keeps_frontmatter_description() {
        let src = "+++\ntitle = \"T\"\ndescription = \"fm\"\n+++\nBody only.\n";
        let out = import_md(src, "page", None).unwrap();
        assert!(out.contains("description: \"fm\""), "got:\n{out}");
    }

    #[test]
    fn list_item_continuation_is_indented() {
        // A list item with a second paragraph: the continuation must be indented
        // under the marker, or typst splits the <li> out of the list.
        let out = body("- first para\n\n  second para\n");
        assert!(out.contains("- first para"), "got:\n{out}");
        assert!(out.contains("\n  second para"), "continuation not indented:\n{out}");
        assert!(!out.contains("\nsecond para"), "continuation at column 0:\n{out}");
    }

    #[test]
    fn no_extra_field_when_absent_or_empty() {
        let absent = import_md("+++\ntitle = \"T\"\n+++\nbody\n", "page", None).unwrap();
        assert!(!absent.contains("extra:"), "got: {absent}");
        let empty = import_md("+++\ntitle = \"T\"\n[extra]\n+++\nbody\n", "page", None).unwrap();
        assert!(!empty.contains("extra:"), "got: {empty}");
    }

    #[test]
    fn preprocess_inline() {
        let b = "before\n\n{{ svg(asset=\"x.svg\", size=\"l\") }}\n\nafter\n";
        assert!(preprocess_shortcodes(b).contains(MARKER_INLINE));
    }

    #[test]
    fn preprocess_block() {
        let out = preprocess_shortcodes("{% centered() %}\n[a](/b)\n{% end %}\n");
        assert!(out.contains(MARKER_OPEN));
        assert!(out.contains(MARKER_CLOSE));
    }

    #[test]
    fn escapes_markup_in_prose_but_not_code() {
        let out = body("issue #42, a < b, an @handle\n\n```\nlet x = #foo;\n```\n");
        assert!(out.contains(r"issue \#42"), "got: {out}");
        assert!(out.contains(r"a \< b"), "got: {out}");
        assert!(out.contains(r"an \@handle"), "got: {out}");
        assert!(out.contains("let x = #foo;"), "got: {out}");
        assert!(!out.contains(r"\#foo"), "code block was escaped: {out}");
    }

    #[test]
    fn converts_inline_html_to_html_elem() {
        let out = body("a <col-s space=\"bgr\">[0, 0, 1]</col-s> b");
        assert!(
            out.contains(r#"#html.elem("col-s", attrs: ("space": "bgr"))[\[0, 0, 1\]]"#),
            "got: {out}"
        );
    }

    #[test]
    fn converts_empty_inline_html_and_void() {
        let out = body("x <col-s value='656nm'></col-s> y <br> z");
        assert!(
            out.contains(r#"#html.elem("col-s", attrs: ("value": "656nm"))[]"#),
            "got: {out}"
        );
        assert!(out.contains(r#"#html.elem("br")"#), "got: {out}");
        assert!(
            !out.contains(r#"#html.elem("br")["#),
            "void br should have no body: {out}"
        );
    }

    #[test]
    fn converts_markdown_table() {
        let out = body("| a | b |\n|---|:-:|\n| 1 | 2 |\n");
        assert!(out.contains("#table("), "got: {out}");
        assert!(out.contains("columns: 2"), "got: {out}");
        assert!(out.contains("align: (auto, center)"), "got: {out}");
        assert!(out.contains("table.header([a], [b], )"), "got: {out}");
        assert!(out.contains("[1], [2], "), "got: {out}");
    }

    #[test]
    fn converts_strikethrough() {
        let out = body("~~struck~~ text");
        assert!(out.contains("#strike[struck]"), "got: {out}");
    }

    #[test]
    fn shortcode_inside_table_cell_converts() {
        let out = convert("| a | {{ diagram(asset=\"x_y.svg\") }} |\n|---|---|\n| 1 | 2 |\n");
        assert!(out.contains(r#"#diagram(asset: "x_y.svg")"#), "got: {out}");
        assert!(!out.contains(r"x\_y"), "got: {out}");
    }

    #[test]
    fn shortcode_mid_paragraph_converts() {
        let out = convert("before {{ var(name=\"x\") }} after\n");
        assert!(out.contains(r#"#var(name: "x")"#), "got: {out}");
    }

    #[test]
    fn block_shortcode_wraps_body() {
        // `*world*` is markdown emphasis → typst `_world_`.
        let out = convert("{% centered() %}\nhello *world*\n{% end %}\n");
        assert!(out.contains("#centered()["), "got: {out}");
        assert!(out.contains("hello _world_"), "got: {out}");
    }

    #[test]
    fn shortcode_inside_inline_code_is_left_literal() {
        let pre = preprocess_shortcodes("use `{{ foo() }}` here");
        assert!(!pre.contains(MARKER_INLINE), "got: {pre}");
        assert!(!convert("use `{{ foo() }}` here").contains("#foo()"));
    }
}
