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
//! - Links: external → `#link(..)` (the show rule handles `rel`/
//!   `target` in user code); internal anchor `#frag` → `#link(..)`;
//!   root-relative `/foo` → `#link(..)`.
//! - Zola shortcodes (`{{ name(args) }}` / `{% name(args) %}…{% end %}`)
//!   for the four block helpers we've ported so far: `centered`, `svg`,
//!   `image`, `diagram`. Unknown shortcodes pass through unchanged with
//!   a `// TODO twyla-convert: review` comment.
//!
//! What we *don't* try to do:
//!
//! - Inline raw HTML beyond pass-through (the porter wraps it in
//!   `raw-html(..)` where needed).
//! - Heading slugification beyond the simple ascii-fold rule (zola's
//!   actual slugifier handles unicode; we'd diverge on accented chars).
//! - Custom shortcodes (`tagline`, `var`, `math`) — added when a page
//!   forces them.

use std::fmt::Write as _;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

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

fn rewrite_line(line: &str, out: &mut String) {
    let stripped = line.trim_matches(['\n', '\r']);
    let leading_ws = &line[..line.len() - line.trim_start().len()];

    // Whole-line block-open: `{% name(args) %}` (no body on same line).
    if let Some(rest) = stripped.trim().strip_prefix("{%") {
        if let Some(inner) = rest.strip_suffix("%}") {
            let inner = inner.trim();
            if inner == "end" {
                writeln!(out, "{leading_ws}\n{MARKER_CLOSE}\n").unwrap();
                return;
            }
            writeln!(out, "{leading_ws}\n{MARKER_OPEN}{inner}-->\n").unwrap();
            return;
        }
    }

    // Whole-line inline shortcode: `{{ name(args) }}`.
    if let Some(rest) = stripped.trim().strip_prefix("{{") {
        if let Some(inner) = rest.strip_suffix("}}") {
            let inner = inner.trim();
            writeln!(out, "{leading_ws}\n{MARKER_INLINE}{inner}-->\n").unwrap();
            return;
        }
    }

    // Otherwise — pass through. Inline shortcodes mid-paragraph (rare in
    // this corpus) won't be matched; the porter cleans those up by hand.
    out.push_str(line);
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
            Event::Html(s) | Event::InlineHtml(s) => self.html(&s),
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
                // The body is one or more `Event::Html` events we
                // handle in `html()`. No fence to emit here.
            }
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

    fn html(&mut self, s: &str) {
        let trimmed = s.trim();
        if trimmed.starts_with(MARKER_OPEN) {
            let inner = trimmed
                .trim_start_matches(MARKER_OPEN)
                .trim_end_matches(MARKER_END);
            if let Some((name, args)) = parse_shortcode(inner) {
                writeln!(self.out, "\n#{}({})[", name, args).unwrap();
            } else {
                writeln!(
                    self.out,
                    "\n// TODO twyla-convert: review block shortcode {inner}"
                )
                .unwrap();
            }
            return;
        }
        if trimmed == MARKER_CLOSE {
            // Strip trailing whitespace from accumulated output before
            // closing the block. Pulldown emits `End(Paragraph)` as
            // `\n\n`; left in place inside a block shortcode that
            // becomes a paragraph break, which makes typst wrap the
            // body in nested `<p>`s — html5ever then splits them into
            // siblings and the centered/etc. div ends up with > 1
            // child. Trim restores the single-paragraph shape.
            while self.out.ends_with(|c: char| c.is_whitespace()) {
                self.out.pop();
            }
            self.push("]\n\n");
            return;
        }
        if trimmed.starts_with(MARKER_INLINE) {
            let inner = trimmed
                .trim_start_matches(MARKER_INLINE)
                .trim_end_matches(MARKER_END);
            if let Some((name, args)) = parse_shortcode(inner) {
                writeln!(self.out, "\n#{}({})\n", name, args).unwrap();
            } else {
                writeln!(
                    self.out,
                    "\n// TODO twyla-convert: review inline shortcode {inner}"
                )
                .unwrap();
            }
            return;
        }
        // Real inline/block HTML the porter has to look at — pass
        // through unchanged with a comment flag.
        self.out.push_str("/* TODO ");
        self.out.push_str(s);
        self.out.push_str("*/");
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
        if matches!(c, '\\' | '#' | '$' | '*' | '_' | '`' | '<' | '@' | '~') {
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
}
