//! `twyla import` — convert a zola markdown post into a typst draft.
//!
//! Output is a `.typ.draft` the porter cleans up by hand; this is
//! scaffolding, not a maintained md↔typ sync. Coverage targets the
//! common shape of the personal-site corpus:
//!
//! - TOML frontmatter (`+++ … +++`) → `page-template` boilerplate.
//! - ATX headings → `h1`/`h2` helpers with a slug derived from the
//!   heading text.
//! - Paragraphs, blockquotes, ATX rules, ordered/unordered lists,
//!   fenced code, inline code, emphasis, strong, soft/hard breaks.
//! - Tables → `#table(columns:, align:, table.header(..), ..)` (with a
//!   TODO to check the styling).
//! - Links: external → `#link(..)` (the show rule handles `rel`/
//!   `target` in user code); internal anchor `#frag` → `#link(..)`;
//!   root-relative `/foo` → `#link(..)`.
//! - Inline and block HTML → `#html.elem("tag", attrs: (..))[..]`, parsed
//!   the same way the diff harness parses HTML.
//! - Zola shortcodes (`{{ name(args) }}` / `{% name(args) %}…{% end %}`)
//!   anywhere — mid-paragraph and inside table cells, not just whole-line —
//!   become `#name(args)` / `#name(args)[body]`. Args translate
//!   `k="v"` → `k: "v"`; the porter defines the `#name` helpers.
//!
//! What we *don't* try to do:
//!
//! - Heading slugification beyond the simple ascii-fold rule (zola's
//!   actual slugifier handles unicode; we'd diverge on accented chars).
//! - Render the shortcode helpers themselves — they're function calls the
//!   porter implements in `templates/`.

use std::cell::RefCell;
use std::fmt::Write as _;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, CommentToken, EndTag, StartTag, TagToken, Token, TokenSink, TokenSinkResult,
    Tokenizer, TokenizerOpts,
};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::html::{self, Node};
use crate::slug::slugify;

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
    let preprocessed = preprocess_shortcodes(body);
    let body_typst = render_body(&preprocessed);
    Ok(assemble(&meta, &body_typst, kind, output))
}

// ---- frontmatter ---------------------------------------------------------

struct Meta {
    title: String,
    description: String,
    date: Option<(i64, u8, u8)>,
    draft: bool,
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
    Ok(Meta {
        title,
        description,
        date,
        draft,
    })
}

// ---- shortcode preprocessing --------------------------------------------
//
// Zola shortcodes (`{{ … }}` and `{% … %}`) aren't markdown syntax —
// pulldown sees them as text and may fold them into adjacent
// paragraphs/links. We replace each occurrence with an HTML-comment
// marker on its own block so pulldown emits a discrete `Html` event we
// can intercept during the walk.

const MARKER_OPEN: &str = "<!--TWYLA-SC-OPEN:";
const MARKER_CLOSE: &str = "<!--TWYLA-SC-CLOSE-->";
const MARKER_INLINE: &str = "<!--TWYLA-SC-INLINE:";
const MARKER_END: &str = "-->";

fn preprocess_shortcodes(body: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        rewrite_line(line, &mut out);
    }
    out
}

/// Scan one (non-fenced) line, replacing shortcodes with marker comments
/// *in place* — so a `{{ … }}` mid-paragraph or inside a table cell is caught,
/// not just whole-line ones (mirroring how zola's grammar matches anywhere).
/// Inline `{{ … }}` becomes an inline marker; block `{% … %}` / `{% end %}` is
/// forced onto its own block with surrounding blank lines so its body stays
/// block-level markdown. Inline `` `code` `` spans are skipped verbatim.
fn rewrite_line(line: &str, out: &mut String) {
    let b = line.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n {
        match b[i] {
            // Inline code span: copy the backtick run, then everything up to
            // and including the matching close run, verbatim.
            b'`' => {
                let start = i;
                let mut ticks = 0;
                while i < n && b[i] == b'`' {
                    ticks += 1;
                    i += 1;
                }
                out.push_str(&line[start..i]);
                if let Some(close) = find_code_span_close(line, i, ticks) {
                    let end = close + ticks;
                    out.push_str(&line[i..end]);
                    i = end;
                }
                // No close on this line → not a code span; keep scanning.
            }
            // Inline shortcode `{{ name(args) }}` → in-place inline marker.
            b'{' if i + 1 < n && b[i + 1] == b'{' => match line[i..].find("}}") {
                Some(rel) => {
                    let inner = line[i + 2..i + rel].trim();
                    write!(out, "{MARKER_INLINE}{inner}{MARKER_END}").unwrap();
                    i += rel + 2;
                }
                None => {
                    out.push_str("{{");
                    i += 2;
                }
            },
            // Block shortcode `{% name(args) %}` / `{% end %}` → block marker.
            b'{' if i + 1 < n && b[i + 1] == b'%' => match line[i..].find("%}") {
                Some(rel) => {
                    let inner = line[i + 2..i + rel].trim();
                    if inner == "end" {
                        write!(out, "\n\n{MARKER_CLOSE}\n\n").unwrap();
                    } else {
                        write!(out, "\n\n{MARKER_OPEN}{inner}{MARKER_END}\n\n").unwrap();
                    }
                    i += rel + 2;
                }
                None => {
                    out.push_str("{%");
                    i += 2;
                }
            },
            _ => {
                let len = utf8_len(b[i]);
                out.push_str(&line[i..i + len]);
                i += len;
            }
        }
    }
}

/// Find the start index of the next run of exactly `ticks` backticks at or
/// after `from`, per CommonMark's same-length close rule. `None` if absent.
fn find_code_span_close(line: &str, from: usize, ticks: usize) -> Option<usize> {
    let b = line.as_bytes();
    let mut j = from;
    while j < b.len() {
        if b[j] == b'`' {
            let run_start = j;
            let mut run = 0;
            while j < b.len() && b[j] == b'`' {
                run += 1;
                j += 1;
            }
            if run == ticks {
                return Some(run_start);
            }
        } else {
            j += 1;
        }
    }
    None
}

/// Byte length of the UTF-8 sequence beginning with `first`.
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

// ---- markdown → typst walk ----------------------------------------------

fn render_body(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(md, opts);

    let mut s = String::new();
    let mut walker = Walker::new(&mut s);
    for event in parser {
        walker.handle(event);
    }
    s
}

struct Walker<'a> {
    out: &'a mut String,
    /// Tracks how to terminate the closest open block — used to know
    /// whether a `]]` or just `\n\n` is needed at `End(_)`.
    stack: Vec<Block>,
    /// True when the next text should be emitted inside a `link(..)[..]`
    /// content slot — disables shortcode inline replacement, since the
    /// porter wouldn't want a function call sitting inside link text.
    in_link_text: u32,
    /// Set when we're in a heading; collects text so we can emit the
    /// slug *and* the heading body together.
    heading: Option<HeadingBuf>,
    /// True if the currently-enclosing list is ordered. Pulldown emits
    /// `Tag::List(Some(_))` for ordered; we read it at `Start(List)`
    /// time and consult here at `Start(Item)` for the prefix.
    list_ordered: Vec<bool>,
    /// Depth within an unsupported tag, and all the events encountered therin.
    unsupported_depth: u32,
    unsupported_events: Vec<Event<'a>>,
    /// Open inline-HTML elements (`<col-s>…`), so a `</col-s>` knows to close
    /// the matching `#html.elem(..)[` content block.
    html_stack: Vec<String>,
    /// Accumulates the raw text of a block-level HTML block (`<div>…</div>`),
    /// parsed and converted to `#html.elem` calls at `End(HtmlBlock)`.
    html_block_buf: String,
}

#[derive(Clone, Copy)]
enum Block {
    Paragraph,
    Blockquote,
    CodeBlock,
    ListItem,
    Emphasis,
    Strong,
    Link,
    Heading,
}

struct HeadingBuf {
    level: HeadingLevel,
    text: String,
}

impl<'a> Walker<'a> {
    fn new(out: &'a mut String) -> Self {
        Self {
            out,
            stack: Vec::new(),
            in_link_text: 0,
            heading: None,
            list_ordered: Vec::new(),
            unsupported_depth: 0,
            unsupported_events: Vec::new(),
            html_stack: Vec::new(),
            html_block_buf: String::new(),
        }
    }

    fn handle(&mut self, ev: Event<'a>) {
        if self.unsupported_depth > 0 {
            match &ev {
                Event::Start(_) => self.unsupported_depth += 1,
                Event::End(_) => self.unsupported_depth -= 1,
                _ => (),
            }
            self.unsupported_events.push(ev);
            if self.unsupported_depth == 0 {
                pulldown_cmark_to_cmark::cmark(
                    self.unsupported_events.drain(..),
                    if let Some(h) = &mut self.heading {
                        &mut h.text
                    } else {
                        &mut self.out
                    },
                )
                .unwrap();
                self.push("*/");
            }
            return;
        }
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(s) => self.text(&s),
            Event::Code(s) => {
                if self.heading.is_some() {
                    write!(self.out, "`{s}`").unwrap();
                } else {
                    let buf = self.heading_or_out();
                    write!(buf, "`{s}`").unwrap();
                }
            }
            Event::Html(s) => self.block_html(&s),
            Event::InlineHtml(s) => self.inline_html(&s),
            Event::SoftBreak => self.push("\n"),
            Event::HardBreak => self.push(" \\\n"),
            Event::Rule => self.push("\n#html.hr()\n\n"),
            _ => {
                self.push("/* TODO ");
                pulldown_cmark_to_cmark::cmark([ev].into_iter(), self.push_to()).unwrap();
                self.push("*/");
            }
        }
    }

    fn start(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::Paragraph => {
                self.stack.push(Block::Paragraph);
            }
            Tag::Heading { level, .. } => {
                self.heading = Some(HeadingBuf {
                    level,
                    text: String::new(),
                });
                self.stack.push(Block::Heading);
            }
            Tag::BlockQuote(_) => {
                self.push("#html.blockquote[\n");
                self.stack.push(Block::Blockquote);
            }
            Tag::CodeBlock(kind) => {
                let lang = match &kind {
                    CodeBlockKind::Fenced(s) => s.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.push("\n```");
                self.push(&lang);
                self.push("\n");
                self.stack.push(Block::CodeBlock);
            }
            Tag::List(start) => {
                // Pulldown wraps `Tag::Item`s in `Tag::List`. `start`
                // is `Some(n)` for ordered (n is the first number),
                // `None` for unordered.
                self.list_ordered.push(start.is_some());
            }
            Tag::Item => {
                let ordered = self.list_ordered.last().copied().unwrap_or(false);
                self.push(if ordered { "+ " } else { "- " });
                self.stack.push(Block::ListItem);
            }
            Tag::HtmlBlock => {
                // Body arrives as `Event::Html` events accumulated in
                // `block_html`, converted at `End(HtmlBlock)`.
                self.html_block_buf.clear();
            }
            Tag::Table(aligns) => {
                self.push("\n// TODO twyla-convert: check table styling\n");
                self.push(&format!("#table(\n  columns: {},\n", aligns.len()));
                if aligns.iter().any(|a| *a != Alignment::None) {
                    let cols: Vec<&str> = aligns.iter().map(|a| align_name(*a)).collect();
                    self.push(&format!("  align: ({}),\n", cols.join(", ")));
                }
            }
            Tag::TableHead => self.push("  table.header("),
            Tag::TableRow => self.push("  "),
            Tag::TableCell => self.push("["),
            Tag::Emphasis => {
                self.push("_");
                self.stack.push(Block::Emphasis);
            }
            Tag::Strong => {
                self.push("*");
                self.stack.push(Block::Strong);
            }
            Tag::Link { dest_url, .. } => {
                // Anchor-only links (`[t](#frag)`) become label links
                // (`#link(<frag>)[t]`) — typst-html resolves them
                // natively via the bundle introspector, matching
                // pulldown's behavior without a show-rule hack.
                if let Some(frag) = dest_url.strip_prefix('#') {
                    write!(self.out, "#link(<{frag}>)[").unwrap();
                } else {
                    let url = escape_typst_string(&dest_url);
                    write!(self.out, "#link(\"{url}\")[").unwrap();
                }
                self.stack.push(Block::Link);
                self.in_link_text += 1;
            }
            _ => {
                self.push("/* TODO ");
                assert_eq!(self.unsupported_depth, 0);
                self.unsupported_depth = 1;
                assert_eq!(self.unsupported_events.len(), 0);
                self.unsupported_events.push(Event::Start(tag));
            }
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.stack.pop();
                self.push("\n\n");
            }
            TagEnd::Heading(_) => {
                self.stack.pop();
                if let Some(h) = self.heading.take() {
                    // `base.typ`'s `show heading` rule reads `it.label`
                    // and emits `<h{level} id="<label>">..</h{level}>`,
                    // matching pulldown's auto-id behavior. Emit an
                    // explicit `<slug>` label per heading so link
                    // targets are queryable via `#link(<slug>)`.
                    let prefix = "=".repeat(h.level as usize);
                    let slug = slugify(&h.text);
                    writeln!(self.out, "{prefix} {} <{}>\n", h.text, slug).unwrap();
                }
            }
            TagEnd::BlockQuote(_) => {
                self.stack.pop();
                self.push("]\n\n");
            }
            TagEnd::CodeBlock => {
                self.stack.pop();
                self.push("```\n\n");
            }
            TagEnd::List(_) => {
                self.list_ordered.pop();
                self.push("\n");
            }
            TagEnd::Item => {
                self.stack.pop();
                self.push("\n");
            }
            TagEnd::Emphasis => {
                self.stack.pop();
                self.push("_");
            }
            TagEnd::Strong => {
                self.stack.pop();
                self.push("*");
            }
            TagEnd::Link => {
                self.stack.pop();
                self.in_link_text -= 1;
                self.push("]");
            }
            TagEnd::Table => self.push(")\n\n"),
            TagEnd::TableHead => self.push("),\n"),
            TagEnd::TableRow => self.push("\n"),
            TagEnd::TableCell => self.push("], "),
            TagEnd::HtmlBlock => self.flush_html_block(),
            _ => {}
        }
    }

    fn text(&mut self, s: &str) {
        // Inside fenced code blocks, dump verbatim — the ```` ``` ```` block
        // is raw typst, so markup escaping would corrupt it.
        if matches!(self.stack.last(), Some(Block::CodeBlock)) {
            self.out.push_str(s);
            return;
        }
        // Everywhere else `s` is literal prose (pulldown already turned
        // markdown structure into events), so escape typst-markup specials so
        // a stray `#`, `*`, `<`, etc. doesn't start a function/emphasis/label.
        // Routes to the heading buffer when inside a heading, else the body.
        let escaped = escape_markup(s);
        if let Some(h) = &mut self.heading {
            h.text.push_str(&escaped);
        } else {
            self.out.push_str(&escaped);
        }
    }

    /// Block-level HTML (`Event::Html`): a shortcode marker, or raw HTML text
    /// accumulated for conversion at `End(HtmlBlock)`.
    fn block_html(&mut self, s: &str) {
        if self.handle_marker(s.trim(), false) {
            return;
        }
        self.html_block_buf.push_str(s);
    }

    /// Convert the accumulated HTML block into `#html.elem` calls by parsing it
    /// the same way the diff harness does, then walking the tree.
    fn flush_html_block(&mut self) {
        if self.html_block_buf.trim().is_empty() {
            self.html_block_buf.clear();
            return;
        }
        let buf = std::mem::take(&mut self.html_block_buf);
        let mut converted = String::from("\n");
        convert_html_node(&html::parse_html(&buf), &mut converted);
        converted.push_str("\n\n");
        self.push(&converted);
    }

    /// Inline HTML (`Event::InlineHtml`): a single open/close/void tag. Open
    /// tags become `#html.elem("name", ..)[`, the markdown content between
    /// flows in normally, and the close tag emits the matching `]`.
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
                let dict = typst_attrs(attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                if self_closing || is_void(&name) {
                    self.push(&format!("#html.elem(\"{name}\"{dict})"));
                } else {
                    self.push(&format!("#html.elem(\"{name}\"{dict})["));
                    self.html_stack.push(name);
                }
            }
            HtmlTag::Close => {
                if self.html_stack.pop().is_some() {
                    self.push("]");
                }
            }
            HtmlTag::Comment => {}
            HtmlTag::Other(raw) => {
                self.push(&format!("/* TODO twyla-convert: review raw html {raw} */"));
            }
        }
    }

    /// Handle a preprocessed shortcode marker comment. Returns `true` if `s`
    /// was a marker (and was emitted), `false` otherwise. `inline` is set when
    /// the marker came from `Event::InlineHtml` (mid-paragraph / table cell),
    /// where the call must be emitted tight rather than as its own block.
    fn handle_marker(&mut self, s: &str, inline: bool) -> bool {
        if let Some(inner) = s.strip_prefix(MARKER_OPEN).and_then(|x| x.strip_suffix(MARKER_END)) {
            if let Some((name, args)) = parse_shortcode(inner) {
                writeln!(self.out, "\n#{}({})[", name, args).unwrap();
            } else {
                writeln!(self.out, "\n// TODO twyla-convert: review block shortcode {inner}")
                    .unwrap();
            }
            return true;
        }
        if s == MARKER_CLOSE {
            // Strip trailing whitespace before closing the block, so a
            // paragraph break inside the body doesn't split the wrapped
            // content into sibling `<p>`s.
            while self.out.ends_with(|c: char| c.is_whitespace()) {
                self.out.pop();
            }
            self.push("]\n\n");
            return true;
        }
        if let Some(inner) = s.strip_prefix(MARKER_INLINE).and_then(|x| x.strip_suffix(MARKER_END)) {
            match parse_shortcode(inner) {
                Some((name, args)) if inline => {
                    write!(self.push_to(), "#{}({})", name, args).unwrap();
                }
                Some((name, args)) => {
                    writeln!(self.out, "\n#{}({})\n", name, args).unwrap();
                }
                None => {
                    writeln!(self.out, "\n// TODO twyla-convert: review inline shortcode {inner}")
                        .unwrap();
                }
            }
            return true;
        }
        false
    }

    fn push_to(&mut self) -> &mut String {
        if let Some(h) = &mut self.heading {
            &mut h.text
        } else {
            self.out
        }
    }

    fn push(&mut self, s: &str) {
        self.push_to().push_str(s)
    }

    fn heading_or_out(&mut self) -> &mut String {
        if let Some(h) = &mut self.heading {
            &mut h.text
        } else {
            &mut *self.out
        }
    }
}

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

// ---- html → html.elem ----------------------------------------------------

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
/// the rest. The `process_token` signature is `&self`, so the result rides a
/// `RefCell`.
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

/// Render an attribute set as the `, attrs: ("k": "v", ..)` part of an
/// `html.elem` call (empty string when there are no attributes). String keys
/// keep hyphenated names like `data-foo` valid.
fn typst_attrs<'a>(attrs: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let parts: Vec<String> = attrs
        .map(|(k, v)| format!("\"{}\": \"{}\"", escape_typst_string(k), escape_typst_string(v)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(", attrs: ({})", parts.join(", "))
    }
}

/// Recursively convert a parsed HTML node into `#html.elem` calls. The
/// document/html/head/body wrappers html5ever inserts are unwrapped.
fn convert_html_node(node: &Node, out: &mut String) {
    match node {
        Node::Document(children) => {
            for c in children {
                convert_html_node(c, out);
            }
        }
        Node::Doctype(_) | Node::Comment(_) => {}
        Node::Text(t) => out.push_str(&escape_markup(t)),
        Node::Element(el) => {
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

/// HTML5 void elements — emitted without a body.
fn is_void(name: &str) -> bool {
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

/// Map a pulldown table-column alignment to a typst alignment keyword.
fn align_name(a: Alignment) -> &'static str {
    match a {
        Alignment::Left => "left",
        Alignment::Center => "center",
        Alignment::Right => "right",
        Alignment::None => "auto",
    }
}

// ---- helpers -------------------------------------------------------------

fn escape_typst_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Backslash-escape characters that start typst markup syntax, so literal
/// prose (a `#`, `C++`/`a < b`, snake_case, an `@handle`, …) renders as text
/// instead of triggering a function call, label, emphasis, or math. Applied to
/// body/heading/link text — never to fenced code blocks.
fn escape_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '#' | '$' | '*' | '_' | '`' | '<' | '@' | '~' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ---- assembly ------------------------------------------------------------

fn assemble(meta: &Meta, body: &str, kind: &str, output: Option<&str>) -> String {
    let mut out = String::new();
    writeln!(out, "// twyla-convert draft. Manual cleanup expected!").unwrap();
    writeln!(out, "// Inspect any TODO markers below").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "#import \"/templates/lib.typ\": {kind}-template").unwrap();
    writeln!(out).unwrap();

    // Page metadata lives on the document, where twyla harvests it for
    // listings/feeds and the template reads it via `#context document.*`.
    writeln!(out, "#set document(").unwrap();
    writeln!(out, "  title: \"{}\",", escape_typst_string(&meta.title)).unwrap();
    if !meta.description.is_empty() {
        writeln!(
            out,
            "  description: \"{}\",",
            escape_typst_string(&meta.description)
        )
        .unwrap();
    }
    if let Some((y, m, d)) = meta.date {
        writeln!(out, "  date: datetime(year: {y}, month: {m}, day: {d}),").unwrap();
    }
    if meta.draft {
        writeln!(out, "  draft: true,").unwrap();
    }
    if let Some(output) = output {
        // Zola's route diverges from twyla's filename default — pin it.
        writeln!(out, "  output: \"{}\",", escape_typst_string(output)).unwrap();
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

    #[test]
    fn frontmatter_split() {
        let src = "+++\ntitle = \"Hi\"\n+++\nbody\n";
        let (fm, body) = split_frontmatter(src).unwrap();
        assert_eq!(fm, "title = \"Hi\"");
        assert_eq!(body, "body\n");
    }

    #[test]
    fn preprocess_inline() {
        let body = "before\n\n{{ svg(asset=\"x.svg\", size=\"l\") }}\n\nafter\n";
        let out = preprocess_shortcodes(body);
        assert!(out.contains(MARKER_INLINE));
    }

    #[test]
    fn preprocess_block() {
        let body = "{% centered() %}\n[a](/b)\n{% end %}\n";
        let out = preprocess_shortcodes(body);
        assert!(out.contains(MARKER_OPEN));
        assert!(out.contains(MARKER_CLOSE));
    }

    #[test]
    fn escapes_markup_in_prose_but_not_code() {
        let out = render_body("issue #42, a < b, an @handle\n\n```\nlet x = #foo;\n```\n");
        assert!(out.contains(r"issue \#42"), "got: {out}");
        assert!(out.contains(r"a \< b"), "got: {out}");
        assert!(out.contains(r"an \@handle"), "got: {out}");
        // Fenced code is emitted verbatim — not escaped.
        assert!(out.contains("let x = #foo;"), "got: {out}");
        assert!(!out.contains(r"\#foo"), "code block was escaped: {out}");
    }

    #[test]
    fn converts_inline_html_to_html_elem() {
        let out = render_body("a <col-s space=\"bgr\">[0, 0, 1]</col-s> b");
        assert!(
            out.contains(r#"#html.elem("col-s", attrs: ("space": "bgr"))[\[0, 0, 1\]]"#),
            "got: {out}"
        );
    }

    #[test]
    fn converts_empty_inline_html_and_void() {
        // Empty element keeps an (empty) body; void elements get none.
        let out = render_body("x <col-s value='656nm'></col-s> y <br> z");
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
        let out = render_body("| a | b |\n|---|:-:|\n| 1 | 2 |\n");
        assert!(out.contains("#table("), "got: {out}");
        assert!(out.contains("columns: 2"), "got: {out}");
        assert!(out.contains("align: (auto, center)"), "got: {out}");
        assert!(out.contains("table.header([a], [b], )"), "got: {out}");
        assert!(out.contains("[1], [2], "), "got: {out}");
    }

    /// The full shortcode path: preprocess (text → markers) then render.
    fn convert(md: &str) -> String {
        render_body(&preprocess_shortcodes(md))
    }

    #[test]
    fn shortcode_inside_table_cell_converts() {
        let out = convert("| a | {{ diagram(asset=\"x_y.svg\") }} |\n|---|---|\n| 1 | 2 |\n");
        assert!(out.contains(r#"#diagram(asset: "x_y.svg")"#), "got: {out}");
        // arg underscores must NOT be markup-escaped (they go via parse_shortcode)
        assert!(!out.contains(r"x\_y"), "got: {out}");
    }

    #[test]
    fn shortcode_mid_paragraph_converts() {
        let out = convert("before {{ var(name=\"x\") }} after\n");
        assert!(out.contains(r#"#var(name: "x")"#), "got: {out}");
    }

    #[test]
    fn shortcode_inside_inline_code_is_left_literal() {
        let pre = preprocess_shortcodes("use `{{ foo() }}` here");
        assert!(
            !pre.contains(MARKER_INLINE),
            "shortcode in a code span should not be marked: {pre}"
        );
        let out = convert("use `{{ foo() }}` here");
        assert!(!out.contains("#foo()"), "got: {out}");
    }
}
