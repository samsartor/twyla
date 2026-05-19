// twyla — design notes & project tracker
//
// Living design doc. Reflects current understanding; expect rewrites as
// decisions land. Historical context (experiment 1, experiment 2,
// breaking-changes-from-0.14, detailed gotcha lists) lives in git
// history, not here.

#set document(title: "twyla — design & plan")
#set page(margin: 2cm, numbering: "1")
#set text(font: "Iosevka", size: 10pt)
#show heading.where(level: 1): set text(size: 18pt)
#show heading.where(level: 2): set text(size: 13pt)
#show link: set text(fill: blue)

= twyla

A static-site generator whose only authoring language is typst.

Twyla embeds the typst compiler as a Rust library (the `typst` crate), not
the CLI, so it can route typst files to URLs, intercept asset resolution
to fingerprint/optimize images, walk the compiled document for metadata,
emit RSS/Atom, and (eventually) do incremental rebuilds.

The driving target is porting
#link("https://samsartor.com")[samsartor.com] (currently zola + tera +
markdown, at `~/Src/site`) to twyla + typst, with HTML AST equivalence as
the porting harness.

== Why pure typst (no template layer)

Considered: a tera-like template engine layered on top of typst, by
analogy with zola layering tera on top of markdown. Rejected — typst
already provides the moving parts a template language would re-invent:

- *Inheritance* — `base.html` + `{% block %}` becomes a typst function
  taking a `body` parameter, with show/set rules supplying the defaults.
- *Filters* — typst functions compose better than pipeline syntax.
- *Shortcodes* — already just typst functions, no separate registry.
- *HTML output* — `html.elem(..)` and the typed `html.div(..)` family
  let us emit arbitrary tag + class + attribute structures.

Cost of *adding* a template language: two scoping models, two escape
hatches, two contexts for the author to switch between. Cost of *not*
adding one: typst's HTML export is younger than tera, so ergonomic rough
edges land on us. That's where twyla earns its keep — wrapping the crate
gives us room to shape output rather than living with defaults.

== Architecture

=== Routing & bundle

`#document(path, ..)` (typst PR #7964) is the routing primitive: one call
per emitted HTML file. Cross-document links use labels; typst computes
relative paths automatically. `#asset(path, bytes)` writes raw files into
the same bundle output.

Twyla's job, eventually: scan `content/*.typ`, generate a `main.typ` in
memory that emits one `#document(..)` per source file (path derived from
filesystem layout with `metadata()` override), compile the bundle,
post-process. Today the render binary handles a single user-supplied
entrypoint at a time, no scan-and-generate yet.

=== Asset model

Two orthogonal axes for asset primitives — *input source* and *output kind*:

#table(
  columns: (auto, 1fr, 1fr),
  align: (left, left, left),
  table.header[][*Output: bytes*][*Output: url*],

  [*Input: path*],
  [`read(path) -> bytes` \
   _Already typst's built-in._],
  [`asset-url(path) -> str` \
   — emit URL pointing at file. \
   Eventually fingerprinted.],

  [*Input: bytes*],
  [_byte transforms (font rewrite, \
    webp encode); rust or typst._],
  [`content-url(bytes, name) -> str` \
   — hash, write to bundle, URL.],

  [*Input: content* \
   _(typst Content)_],
  [`render(content, format) -> bytes` \
   — sub-compile. Overlaps with \
   typst's `#document(path,..)`.],
  [`asset.from-content(...).url` \
   — render + register + URL.],
)

Principle: byte transforms run in Rust (or in user typst where
ergonomic); presentation decisions (inline `<svg>` vs `<img>` vs
`<picture>` vs `<use href>` vs iframe) run in user typst templates.
URL-returning primitives are the seam: above them typst owns layout and
emission; below them Rust owns paths, fingerprinting, and the on-disk
bundle.

Phase 1 implements only `read` (free from typst). URL primitives are
deferred until a second use case forces the API surface.

`#document` is the page-level expression of "content input → URL." Open
question whether the eventual `asset.from-content(..)` API subsumes it,
or whether they stay separate (pages routed by content layout vs assets
routed by hash).

=== Resolution pass

Twyla owns a post-compile *resolution pass* — a walk over the bundle's
serialized output that finds placeholder markers and substitutes their
final form. Inspired by the future/promise pattern: typst emits a
deferred value; Rust resolves it later. This is the seam between "things
typst can express" and "things we want in the output."

Phase 1 has one placeholder: *raw HTML*. Typst has no `html.raw`; only
`<script>` and `<style>` bodies are emitted unescaped (verified:
`typst-html::tag::is_raw`). We exploit that: user typst calls
`raw-html(content)` which emits
`<script type="x-twyla-raw-html">..bytes..</script>`; the resolution pass
replaces each match with the inner content spliced inline. Used by the
`svg`/`diagram` shortcodes to embed external SVG files.

Anticipated placeholders, each earned by a use case:

- *Image optimization* — `<twyla-image data-src=".." data-sizes="..">`
  resolves to `<picture>`/`<img srcset>` after webp/avif encoding.
- *Asset-as-content* — sub-compile registered typst content to SVG,
  splice inline.
- *Feed expansion* — `<twyla-feed-entries data-query="..">` resolves to N
  siblings driven by introspector query results.

Implementation strategy: keep the str-replace pattern until we have ≥ 2
marker kinds, then generalize to a typed handler registry.

The resolution pass is *the* reason twyla doesn't need to fork typst —
it's pure post-processing of typst output, no internals touched.

== Tooling

=== Porting binaries

Three CLI binaries + a shell driver, all in this repo:

- *`twyla-render-page [--root <dir>] <entrypoint.typ>`* — compile as
  bundle, run resolution pass, print the single document's HTML.
- *`twyla-extract <selector> <input.html>`* — print inner HTML of the
  first matching element. Selectors: `class:<name>`, `tag:<name>`. Was
  used to scope the diff to a body subtree; unused now that the diff
  runs full-page. Kept until the second-page port confirms we don't need
  it again.
- *`twyla-diff [--textonly-pre] [--ignore-attr <tag>:<attr>]... <expected> <actual>`*
  — structural AST diff with optional porting relaxations.
- *`port-page.sh`* — render + full-page diff for a single page.
  Hardcoded to guis-2; generalize when we port the second page.

=== AST diff harness (`twyla::diff`)

Library + CLI. Parses both sides via html5ever, normalizes (lowercase
tags/attrs, sort attrs and class tokens, collapse inter-block whitespace,
preserve `<pre>`/`<code>`/`<script>`/`<style>`/`<textarea>` text
verbatim, drop comments), walks both trees in lockstep, reports the
first structural divergence.

Two parser quirks applied symmetrically to both sides — not opt-in
relaxations, since they're tool artifacts on the typst side that
correspond to no authorial intent on either:

- `<noscript>` content is parsed as HTML, not text. html5ever defaults
  to "scripting on," which turns noscript bodies into text nodes; we
  flip `scripting_enabled: false`. Otherwise zola's pretty-formatted
  `<link>` siblings diverge from typst's tightly-packed ones.
- Pure-whitespace text inside `<script>`/`<style>` is dropped. Typst's
  pretty-printer wraps even an empty `<script src="...">` body in a
  newline + indent; zola emits `<script src="..."></script>`. Significant
  text inside the same tags is left untouched. Doesn't apply to `<pre>`/
  `<code>`/`<textarea>`, where inter-token whitespace inside highlighted
  code is meaningful.

Relaxations are opt-in `(Matcher, RelaxationRule)` pairs, first-match
wins. Defaults to zero — porting starts strict.

#table(
  columns: (auto, 1fr),
  table.header[*`Matcher`*][*Matches*],
  [`Tag("p")`], [every `<p>` element],
  [`TagAttr { tag, attr, value }`], [matching tag with `attr == value`],
  [`TagAttrExists { tag, attr }`], [matching tag with `attr` set],
)

#table(
  columns: (auto, 1fr),
  table.header[*`RelaxationRule`*][*Effect*],
  [`IgnoreEntirely`], [subtree considered equal regardless],
  [`IgnoreAttribute(name)`], [skip one attribute on matched element],
  [`TextOnly`], [compare concatenated text only; ignore structure],
)

*Standing rule:* every new relaxation is a place twyla and zola
diverge. Run them by Sam first.

*Deliberately deferred:* per-section relaxations (different rules under
different ancestors), multi-divergence reporting (today: bails on first
divergence), attribute-value matchers ("ignore `style` only when value =
`color: red`").

== Status

+ #strike[*HTML-export prototype*] — done (experiment 1).
+ #strike[*Multi-document bundle output*] — done (experiment 2).
+ #strike[*AST-diff harness*] — done (`twyla::diff`).
+ #strike[*Port one page, body only (guis-2)*] — done. Two relaxations:
  `--textonly-pre` (code blocks) and `--ignore-attr td:style --ignore-attr
   th:style` (typst tables don't emit alignment as CSS).
+ #strike[*Port guis-2 full page*] — done. `templates/page.typ` in the
  site repo ports `base.html` + `page.html` + `components.html::mainpills`;
  `guis-2.typ` opens with `#show: page-template.with(url-path, title,
  description, date, ...)`. Same two relaxations as the body milestone.
+ *Second page* — port `guis-1` or `guis-3` to validate the workflow
  generalizes. Next.
+ *Generalize* — routing, asset pipeline, feed, dev server. After.

== Roadmap

Working assumption: walk from "one page, body" outward, adding twyla
infrastructure only when a porting case forces it.

=== Near term

+ *Second page.* Port `guis-1` or `guis-3` to validate the workflow
  generalizes. Will surface what was guis-2-specific in `port-page.sh`,
  `shortcodes.typ`, and `page.typ` — drives the first round of
  generalization. `_mainpills` in `page.typ` is already in place but
  untested; a paper page will exercise it.
+ *Open: who applies the page template?* Today `guis-2.typ` opens with
  `#show: page-template.with(..)` — explicit, one line of boilerplate
  per page. Alternative: twyla's eventual generated `main.typ` auto-
  applies a template per content file (less magic visible to the
  author). Defer until enough pages exist that the boilerplate hurts.

=== Medium term — twyla becomes an SSG

+ *Multi-entrypoint routing.* Replace the
  one-`#document`-per-render-call constraint with twyla scanning
  `content/*.typ`, generating an in-memory `main.typ` that emits one
  `#document(..)` per source. Path derives from filesystem layout, with
  per-page `metadata()` override.
+ *Real `asset-url` primitive.* Drop the hardcoded `base_url` in
  `templates/shortcodes.typ`. Comes from a twyla config layer (TOML or
  typst-side). This is the moment we promote our one resolution-pass
  placeholder into a registry, because the URL primitive needs hooks for
  fingerprinting/dedup.
+ *Image optimization pipeline.* Webp/avif encoding, srcset generation,
  in Rust as a resolution-pass handler. User typst calls one function;
  the registered handler does the byte transforms and emits the
  resolved `<picture>`.
+ *Feed generation.* `#document("atom.xml", ..)` in twyla's library,
  querying `bundle.introspector` for `kind: "post"` entries.

=== Longer term

+ *Dev server with live reload.* Open question: do we own it (twyla
  watches sources, serves with WS reload script) or shell out to
  `vite dev` for now and revisit?
+ *Incremental rebuilds.* Hold one `World` across watch cycles; lean on
  comemo memoization. Revisit if cold-start becomes a perf wall.
+ *Sass / JS bundling.* Probably shell out (vite or esbuild) rather than
  reimplement. Twyla orchestrates, doesn't compile.

=== Validation

+ *EXAMPLES-corpus test.* Run a port-page-style diff against
  representative pages from
  #link("https://github.com/getzola/zola/blob/master/EXAMPLES.md")[getzola's
  EXAMPLES]. Catches feature breadth the personal site doesn't exercise.
  Should produce a corpus of `(zola-html, twyla-html, relaxations)`
  triples that we keep green on CI.

== Lessons (from porting guis-2)

Compact gotcha list; details in commits.

- *Smart quotes* — leave typst's `smartquote` on; matches zola's
  `smart_punctuation = true` character-for-character.
- *Show rules fire in HTML mode* (verified de6f400). Folk claim
  otherwise; not true for content-transformation rules. Layout-specific
  rules (positioning, frame production) may not, untested.
- *Pulldown auto-wraps inline content in `<p>` inside containers*
  (`<div>`, `<blockquote>`, `<td>`); typst's HTML export doesn't. Wrap
  manually with `#html.p[..]` until we find a cleaner pattern.
- *Native `#table`* works for HTML. Only divergence: `align: (..)`
  doesn't reflect into per-cell `style="text-align:.."` (typst treats
  it as layout-only). Relax `--ignore-attr td:style --ignore-attr
  th:style` to absorb.
- *Show rules can't replace typst's auto-generated wrapper.* A `show
  table.cell.where(x: 0): it => html.elem("td", ..)` produces
  `<td><td ..>..</td></td>`. Same hazard expected on `list.item`,
  `enum.item`, etc.
- *`html.a(rel: ..)`* takes an array of enum tokens, not a
  space-joined string: `rel: ("noopener", "external")`.
- *External-link auto-attrs* (`rel="noopener external" target="_blank"`)
  via `show link: it => …` checking `type(it.dest) == str` and prefix.
- *`html.elem` for non-typed attrs.* `html.div` doesn't expose `align`;
  use `html.elem("div", attrs: (align: "center"), ..)`.
- *Inline SVG via the resolution pass.* `<script type="x-twyla-raw-html">`
  falls into `RawMode::Keep` because the type attribute isn't
  `text/javascript`.
- *Typst `#let` chain-across-newlines hazard.* `#let f(s) = s\n
  .replace(..)` parses as identity-of-`s` with the dot-calls as orphan
  markup, no error. Wrap multi-line chains in `(…)`.
- *Bypass typst's auto-`<head>`/`<body>` by emitting your own `<html>`.*
  `finalize_dom` (typst-html/document.rs:252) short-circuits when the
  single top-level element is `<html>` — uses ours verbatim, skipping
  the default head. Cost: footnotes are unsupported in that mode.
- *Reserved-word attrs need string keys.* `attrs: (as: "style", ...)`
  is a parse error; `as` is reserved, `type` is a builtin. Quote them:
  `("as": "style", "type": "...")`.
- *`[#]` in markup is a parse error.* `#` starts a code expression, so
  `[#]` opens an expression with no content. Escape as `[\#]` to emit a
  literal hash character (used in the `description__hash` span).

== Upstream-watch list

Worth raising upstream (or watching for) on typst:

+ `html.raw(content)` — first-class verbatim HTML. Today the resolution
  pass abuses `<script>`. Likely controversial (typst values structured
  output) but every HTML consumer hits this.
+ `#table(align: ..)` reflecting into `style="text-align: .."` on
  `<th>`/`<td>` in HTML mode. Same for `colspan`/`rowspan` attributes.
+ Show rules on `table.cell`/`list.item`/`enum.item` that can *replace*
  typst's auto-generated wrapper in HTML, not just decorate its body.
+ Hook on `image()` to emit external `src` instead of data URLs in HTML
  mode — would benefit anyone doing HTML export at scale.
+ Per-document defaults override (e.g. document-level
  `smartquote(enabled: false)` declared once).
+ Introspector exposure of cross-document link resolution data — useful
  for fingerprint-then-rewrite passes.

== Fork vs library

Standing assumption: *do not fork typst.* Twyla is a Rust binary that
consumes typst as a library, supplies a custom `World`, ships a small
typst library of helpers, and post-processes the bundle. If something is
genuinely better as an upstream change, *upstream it* before forking.
Fork threshold: library + upstreaming both proven inadequate.

Why: typst's HTML/bundle work is actively evolving in main. A fork puts
us permanently downstream of grammar, library, and HTML-output changes —
a real maintenance tax paid forever for benefits mostly reachable from
outside.

Decisions to date (every feature considered has landed on "library"):
asset fingerprinting (post-export DOM walk), image optimization (twyla
function bypassing `image()`), pretty URLs (`#document(..)` paths), path
computation, hot reload (WebSocket-inject during dev), RSS/sitemap,
per-document smartquote/lang (already configurable), incremental compile
(comemo), file mtime via `sys.inputs`, async/HTTP at compile (pre-fetch
in Rust). No fork: HTML-syntax-mode (the JSX-style sugar isn't worth
shadowing typst's grammar).

If we ever hit a real wall, document what we tried and why it didn't
work — that's the evidence base for reconsidering.

== Notes

- VCS: both `~/Src/twyla` and `~/Src/site` are managed with jj. *Rule:*
  always `jj new` before editing — jj has no staging area, so edits
  land in whatever revision the working copy points at.
- Typst pinned to main `de6f400` (2026-04-11) for the bundle export
  feature (PR #7964). Switch back to crates.io once 0.15.x ships.
- Tera shortcodes in scope for porting: `centered`, `diagram`, `image`,
  `math`, `svg`, `tagline`, `var` (`~/Src/site/templates/shortcodes/`).
  Phase 1 covered the first four; `math`/`tagline`/`var` come with
  pages that use them.
- Tera templates in scope: `base.html`, `page.html`, `section.html`,
  `index.html`, `components.html`, `404.html`, `atom.xml`,
  `paperpills.html`. Phase 1 covered `base.html` + `page.html` +
  `components.html::mainpills` (the last untested until a paper page).
- Known porting hazards: ROT13-encoded email obfuscation in footer,
  conditional asset loading (`page.extra.tilings`), SVG inlining with
  font-family rewriting (handled by the `svg`/`diagram` shortcodes).
