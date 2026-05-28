// twyla — design notes & project tracker
//
// Living design doc. Reflects current understanding; expect rewrites as
// decisions land. Historical detail (per-experiment logs, the long
// done-list, breaking-changes-from-0.14) lives in git history and in the
// `myr`-and-earlier revisions of this file, not here. This rewrite trims
// that history down to what still informs decisions.
//
// Companion: doc/index.typ is the *aspirational* user-facing manual and
// the basis for the dogfooded website. This file is the engineering plan
// for making index.typ true.

#set document(title: "twyla — design & plan")
#set page(margin: 2cm, numbering: "1")
#set text(font: "Iosevka", size: 10pt)
#show heading.where(level: 1): set text(size: 18pt)
#show heading.where(level: 2): set text(size: 13pt)
#show link: set text(fill: blue)

= twyla

A static-site generator whose only authoring language is typst. Twyla
embeds the typst compiler as a Rust *library* (not the CLI), so it can
route typst files to URLs, inject its own builtins and HTML rules,
intercept asset resolution to fingerprint/optimize/transcode, walk the
compiled bundle for metadata, emit feeds, and do incremental rebuilds.

Two driving targets, both gating "0.1":
+ Port #link("https://samsartor.com")[samsartor.com] (zola + tera +
  markdown, `~/Src/site`) onto twyla and *delete zola* — including its
  sass build, the last umbilical.
+ Ship a dogfooded website (doc/index.typ) that itself builds on twyla.

*0.1 is not "stable."* It is the point Sam is willing to announce "here
is this cool thing." Concretely: zola decommissioned, all posts live, a
feed present, and a linkable twyla site. The typst dependency is pinned
to an unreleased `main` rev (below), so 0.1 means "I run my site on it,"
not "others install it reproducibly."

== Why pure typst (no template layer)

Rejected a tera-like template engine on top of typst. Typst already
supplies the moving parts one would re-invent: inheritance (a function
taking `body`, with show/set rules for defaults), filters (function
composition), shortcodes (just functions), and arbitrary HTML output
(`html.elem`, the typed `html.div` family). Adding a second language
doubles the scoping models and escape hatches. The cost of *not* adding
one — typst's young HTML export has rough edges — is exactly where twyla
earns its keep: wrapping the crate lets us shape output instead of
living with defaults. See § Injection mechanisms.

= Architecture

== Injection mechanisms

Twyla customizes typst through three seams, *without forking*. All are
applied right after `Library::builder().build()`, mutating the returned
`Library` before it is frozen in the world's `LazyHash`. Pick the
lowest-surprise seam for each need.

#table(
  columns: (auto, 1fr),
  table.header[*Mechanism*][*What / when*],

  [*1. Global builtins* \ `global.scope_mut()` \ `.define_func::<F>()`],
  [Native Rust functions/values callable from any content file with
   *zero imports* — the "plain typ files" target. `#[func]`
   (`typst::foundations::func`) works outside the typst crates because
   its expansion uses absolute `::typst_library::` / `::typst_utils::`
   paths and we depend on both directly. *Spiked & proven:* a bare
   `content/index.typ` calling `asset-url("logo.svg")` rendered with no
   import. Use for *verbs* (asset, raw-html, …).],

  [*2. Native HTML rules* \ `library.rules` \ `.replace::<E>(Html, fn)`],
  [`Library.rules` is a public `NativeRuleMap`. `replace::<HeadingElem>`
   overrides how an element serializes to HTML *globally and
   invisibly*; `register` adds rules for new elements. `ShowFn<T>` is a
   plain `fn(elem, engine, styles) -> SourceResult<Content>`, and
   `HtmlElem`/`tag`/`attr` are public in typst-html, so we can author
   them. Use for *element-level HTML behavior* — h-level offset, link
   `rel="noopener external"`, etc. — instead of per-site `show` rules.
   This is strictly better than show-rule fixups: no ordering quirks,
   no zola/twyla incompatibility the author must paper over.],

  [*3. Default template* \ injected `#show:` \ in generated main],
  [The synthesized `main.typ` wraps each page's body in a default
   template (a typst show rule) so a bare file still gets a real
   `<html>` shell + theme. Overridable per-page (the file's own
   `#show:`) or globally via `Twyla.toml` `theme` / `theme-show`. Keep
   injected "before your code" typst *minimal* — most fixups belong in
   mechanism 2, not here.],
)

Division of labor: mechanism 2 handles *how typst elements become HTML*;
mechanism 1 handles *new verbs*; mechanism 3 handles *the page shell and
theme markup*. Markup (the shell, listings) stays in typst — authoring
HTML trees in Rust `Content` builders is the bad path; *logic* (hashing,
slugs that are engine concerns, transforms) goes in Rust.

== Routing & bundle

`#document(path, ..)` (typst PR #7964, `DocumentElem`: required
`path`/`body`, optional `title`/`date`/`description`/…) is the routing
primitive — one call per emitted HTML file. Cross-document links use
labels; typst-html computes relative hrefs per-bundle (same-doc →
`#frag`, cross-doc → `../other/index.html#frag`).

Every compile goes through a synthesized virtual entrypoint (in-memory
`Source` at `/__twyla_main.typ`, never on disk — distinct from the
user's `content/main.typ` below). Twyla scans `content/*.typ`, derives a
slug per stem (`foo.typ` → `/foo/`), and emits one `#document(..)`
wrapping a `#include` per slug. Bundle output paths *are* URLs. Content
files no longer call `#document` themselves — twyla owns routing.

*Home page is `content/main.typ` → `/`*, matching Typst's `main.typ`
entrypoint convention rather than Zola's `_index`. A user who genuinely
wants a page served at `/main/index.html` overrides the path in document
metadata. (Code reality: `scan_pages` currently special-cases `_index`
— migrate it to `main`.)

*Page vs library — the underscore rule.* Once the scanner walks
`content/` recursively, it needs to know which `.typ` files are pages
and which are libraries/partials (`include`d or `asset()`-compiled, never
routed). Classify *by name, not metadata*: *the search terminates at
any file or directory whose name starts with `_`.* So `_diagram.typ` is
a partial (e.g. `asset("./_diagram.typ", format: "svg")`), and
`_drafts/` is skipped wholesale without descending. The win is that
classification needs no compile — a metadata flag would force compiling
the file (and abusing the introspection fixed-point) just to learn it
shouldn't be a page. Precedent: Sass `_partial.scss`. The rule
generalizes cleanly: each directory's `main.typ` is its index
(`content/blog/main.typ` → `/blog/`), bare stems are pages
(`content/blog/post.typ` → `/blog/post/`), `_*` is private.

*Distinct from drafts.* `_`-prefix means "not a page at all" (a
library). A *draft* is a real page, built but excluded from
listings/feed — that's `#set document(draft: true)` metadata, an
orthogonal axis. Don't conflate them (earlier notes did).

Subdirectory recursion itself is *not yet* implemented — `scan_pages`
is top-level only.

= Builtins — the minimum for 0.1

The aspirational doc implies a handful of builtins. Scoped to a *minimum
version of that vision* (Sam's call: typst-content-as-asset is later):

#table(
  columns: (auto, 1fr, 1fr),
  table.header[*Builtin*][*Minimum surface*][*Rust work / mechanism*],

  [`document` \ (shadowed)],
  [`#set document(.., draft: bool, extra: any)` atop native
   title/date/description],
  [Mechanism 1: a `#[func]` overwriting the global `document` binding,
   capturing draft/extra/route, then constructing the native
   `DocumentElem`. Shadow confirmed feasible (scope is a map).],

  [`pages` \ (+ `.current`)],
  [iterable of `{url,title,date,description,draft,extra}`; `.current` is
   this page],
  [Aggregate per-page `document` metadata. *Contextual* (see Crux 1) —
   `query`-backed, `#context` required. `.current` needs context
   regardless (per-doc identity).],

  [`asset(path, format:)`],
  [`.url`, `.data-url`, `.content`; file input only; `format` covers
   sass→css + image/svg passthrough],
  [Mechanism 1 + resolution pass. Read bytes via the World, transform in
   Rust (grass for sass), fingerprint, emit. *See Crux 2 — the
   side-channel is the real unknown.*],

  [`raw-html(content)`],
  [inline verbatim HTML],
  [Already spiked as resolution-pass marker #1; promote from placeholder
   to a registry entry.],
)

Supporting infra (not builtins, but the minimum doc won't run without
them):

- *Package imports* (`@preview/...`). *Hard prerequisite:* doc/index.typ
  itself imports `@preview/frame-it` and `@preview/dtree`, so the
  dogfood site cannot compile without this. Today `render.rs` rejects
  `VirtualRoot::Package(_)` outright and typst-kit's download feature is
  off. Serving from memory is possible via the loader but needs
  versioned specs (`@ns/name:x.y.z`) + a `typst.toml` manifest; the full
  third-party story wants typst-kit's downloader/cache. *Not "easy" —
  but mandatory.*
- *Default template* (mechanism 3) + `Twyla.toml` `theme`/`theme-show`.
- *Sass via grass* — folds *into* `asset()`, not a separate builtin.
  Killing the sass dep is a stated 0.1 blocker.

Deferred (explicitly out of the minimum): asset from typst content/file
(`asset(circle(), ..)`, `asset("./_diagram.typ", format:"svg")`) — drops
the whole sub-compile path; TS→JS via rolldown; native `image()`
auto-routing through assets (require explicit `html.img(src:
asset(..).url)`); `resize`/srcset/`<picture>` raster optimization.

== Crux 1 — `pages` and context

Resolved. Top-level markup included into a document body is *not*
implicitly contextual (`#include` doesn't change that; `query`/`here`
need explicit `#context`). So whether `for page in pages` needs
`#context` depends on what `pages` *is*:

- *Contextual query* (today's `query(<twyla-post>)` shape) → needs
  `#context`, single compile. *This is the 0.1 choice* — the
  aspirational doc now writes `#context for page in pages [..]`, so it's
  settled, not a reluctant compromise.
- *Plain data* → no `#context`, but requires a metadata *pre-pass*
  (compile once to harvest metadata, inject `pages` as data, compile for
  real). A pure ergonomic upgrade, deferrable.

`pages.current` resists the plain-data trick regardless: a shared bundle
compile has one global scope, so "which page am I" needs the
introspector (context). Templates read it inside a show rule, which can
be made contextual — fine in practice.

== Asset model

Two orthogonal axes — *input source* × *output kind*:

#table(
  columns: (auto, 1fr, 1fr),
  align: (left, left, left),
  table.header[][*`.content`*][*`.url`*],

  [*Input: path*],
  [`asset(path).content` — bytes, \
   transformed; inline via raw-html \
   or data-url. `read` underneath.],
  [`asset(path).url` — transform + \
   fingerprint + emit; URL back.],

  [*Input: bytes*],
  [byte transforms (sass→css, webp \
   encode) in Rust → `.content`.],
  [`asset(bytes,name).url` — hash, \
   write to bundle, URL.],

  [*Input: content* \ _(typst Content)_ \ — *deferred*],
  [`render(content, fmt)` — sub- \
   compile to bytes.],
  [`asset(content,fmt).url` — render \
   + register + URL.],
)

Principle: byte transforms run in Rust; *presentation* decisions (inline
`<svg>` vs `<img>` vs `<picture>` vs iframe) run in user typst.
URL-returning primitives are the seam — above them typst owns layout and
emission, below them Rust owns paths, fingerprinting, and the on-disk
bundle.

== Resolution pass

Twyla owns a post-compile walk over the serialized bundle that finds
placeholder markers and substitutes their final form — the future/promise
pattern: typst emits a deferred value, Rust resolves it later. It is *the*
reason twyla needn't fork typst: pure post-processing, no internals
touched.

Today: one marker, *raw HTML*. Typst has no `html.raw`; only
`<script>`/`<style>` bodies emit unescaped (verified
`typst-html::tag::is_raw`). `raw-html(content)` emits `<script
type="x-twyla-raw-html">..</script>`; the pass splices the inner body
inline. Generalize the str-replace to a typed handler registry once a
second marker kind lands — which is exactly where `asset(..).url` /
`.data-url` go (Crux 2).

== Crux 2 — the asset side-channel (research)

*This is the deep item.* The tension: `asset(..).url/.data-url/.content`
are *strings*, which means asset processing either blocks typst rendering
or must be deferred — unlike a traditional *queued* pipeline. Three
candidate shapes, none free:

+ *Opaque placeholder.* `asset(p).url` returns a marker string carrying
  the spec; the resolution pass transforms + writes + rewrites after
  compile. No side channel, nothing blocks, fields stay cheap strings,
  and it reuses the raw-html machinery. *But* the URL is opaque until
  resolved — a user doing `.replace(".png",".webp")` on it gets garbage.
  Sam's objection: too surprising; would need API rethinking (e.g. make
  the surprising values methods, or a distinct type) to be honest.

+ *Eager + side channel.* The func reads via the World, runs the
  transform inline (grass, or even a subprocess — typst packages already
  shell out to WASM), computes the real URL, and *registers the bytes*
  for twyla to write. Clean UX (real URL immediately) but: (a) blocks
  compile; (b) the side channel is the hard part — a thread-local is
  fragile if typst parallelizes, and *comemo memoization means a cached
  func won't re-fire its side effect*, so an incremental rebuild loses
  the registration. `engine.sink` carries `values` out of a compile and
  *may* survive memoization (it replays warnings) — the most promising
  clean channel; needs verification.

+ *Second compile pass.* typst exposes no public hook to inject
  Rust-computed data into its introspection fixed-point mid-loop;
  `typst::compile` is all-or-nothing. The equivalent is to compile
  *twice*, feeding resolved assets in via `sys.inputs`/global on pass 2
  — the same machinery `pages`-as-plain-data would use. Necessary only
  if an asset result must feed back into typst *layout* (real pixel
  dims, `measure()`); none of the doc's URL/data-url-in-attribute cases
  do, so the minimum avoids it.

*Sleeper problem (applies to placeholder & eager alike):* asset *source*
files are read in twyla's Rust pass, not by typst, so they are invisible
to `World::dependencies()`. `twyla serve`'s watcher won't know
`main.sass` is a dep of a page. The resolution pass knows the source
paths — feed them into the watcher's dep set explicitly.

Open sub-problems to work through incrementally:
+ Placeholder *format & forgery/collision safety* across attribute /
  text / CSS contexts.
+ *Dependency tracking* of asset sources → watcher (the sleeper).
+ *Transform caching* keyed by (source-hash, params).
+ The *fixed-point re-run* seam for layout-coupled / content-input
  assets (deferred, but design the hook).
+ `pages.current` per-document identity (shared with Crux 1).
+ Decide the *public API shape* that avoids the placeholder surprise:
  opaque-string vs methods vs a dedicated `asset` type.

= Tooling

Single `twyla` binary, clap-driven, two audiences.

== End-user (turn-key, cwd-driven, zero flags)

- *`twyla serve`* — dev server on `127.0.0.1:1111`. Shared `World`; a
  watcher thread recompiles incrementally and request handlers only
  clone last-compile HTML (ms regardless of size). SSE hot reload:
  `/__twyla/reload` reloads the browser on every recompile (errors
  included, so fixing broken typst auto-recovers); a second `notify`
  watcher on `static/` swaps single assets in place. Hand-rolled
  HTTP/1.1 over `std::net`.
- *`twyla build [-o <dir>]`* — compile every page, write
  `<dir>/<slug>/index.html`, copy `static/` verbatim and colocated
  `content/*.{!typ}`. Default `./public/` (zola-compatible). No
  cleaning; overwrites in place.

== Porting harness (flag-driven)

`render <slug>`, `check <slug>` (render + full-page AST diff vs
`public/<slug>/index.html`, zola layout as the manifest), `diff
<expected> <actual>`, `import <md>` (md→typ draft generator). Pipeline is
exposed as `twyla::render::{render_site, render_slug}` so all commands
run in-process.

== AST diff harness (`twyla::diff`)

html5ever parse → normalize (lowercase tags/attrs, sort attrs + class
tokens, collapse inter-block whitespace, preserve
`<pre>`/`<code>`/`<script>`/`<style>`/`<textarea>` verbatim, drop
comments) → lockstep walk → first divergence. Two parser quirks applied
symmetrically (`scripting_enabled:false` for `<noscript>`; drop
pure-whitespace inside `<script>`/`<style>`). Relaxations are opt-in
`(Matcher, RelaxationRule)` pairs, first-match-wins, default zero.
*Standing rule:* every relaxation is a place twyla and zola diverge —
run new ones by Sam.

== Test fixtures (`test_site/`, `test_site_plain/`)

Repo-local integration fixtures, decoupled from `~/Src/site` (the old
`render_site_smoke` depended on it). `test_site/` exercises full
author-side templating (routing, cross-doc `query` enumeration, heading
slugs, intra-doc anchors, raw-html, colocated assets, build-to-disk) via
property assertions plus one golden diffed through the relax harness.
`test_site_plain/` exercises the *plain-files + native prelude* contract
(zero imports, `asset-url` resolves from global scope). Features not yet
built (nested sections, asset fingerprinting, feeds) have `#[ignore]`d
pending tests that are the executable to-do list.

= Roadmap to 0.1

Built so far (detail in git): HTML/bundle prototype, AST-diff harness,
md→typ import, multi-document virtual-main routing, `serve` (shared
World + watcher + SSE), `build`, the guis-{1,2,3} + home-page port,
label-based internal links, the test-site fixtures, and a proven native
prelude spike (mechanism 1).

Planned order (each unblocks the next):

+ *Package imports* — unblocks the dogfood doc (which imports
  `@preview/*`). Loader serves `VirtualRoot::Package`; embedded `@twyla`
  bytes first, typst-kit downloader for the rest.
+ *`document` shadow + `pages`* (Crux 1: contextual). Replaces the
  per-site `mark-as-post` / `post-list` with engine builtins.
+ *Default template (mechanism 3) + native HTML rules (mechanism 2)* —
  nicer-than-bare shell; fold the h1/h2 offset and external-link `rel`
  into `rules.replace` instead of show-rule hacks.
+ *`asset()` — sass first* (Crux 2). Resolves the side-channel decision;
  grass integration *deletes the zola sass dependency* (0.1 blocker).
+ *Promote `raw-html`* to the resolution-pass registry (entry #1 of N).
+ *Feed generation* — query the `<twyla-post>`/`pages` metadata, emit
  `atom.xml`. Regression-not-to-have (zola had `generate_feeds`).
+ *Port the remaining ~13 pages* hardest-shape-first (nested section →
  needs `scan_pages` recursion; paper page → exercises untested
  `_mainpills`). This is the real feature-completeness test.
+ *Delete zola* — once sass + all pages are on twyla.
+ *Thin dogfood doc site* (doc/index.typ) + README. Deliberately small.

Deferred past 0.1: EXAMPLES-corpus verifier; image optimization
pipeline (webp/avif, srcset); asset-from-content; TS bundling.

= Lessons

Hard-won; prevents re-stepping rakes. Marked *[obsolete]* where a newer
mechanism supersedes the workaround.

- *Show rules fire in HTML mode* (content-transformation rules; verified
  de6f400). Layout-specific rules may not — untested.
- *typst-html shifts `=` to `<h2>`* (reserves `<h1>` for the doc title).
  *[obsolete workaround]* the per-site `show heading` fixup is replaced
  by `rules.replace::<HeadingElem>(Html, ..)` (mechanism 2) if we want
  `= → <h1>` globally. `set heading(offset: -1)` rejects negatives.
- *External-link auto-attrs* (`rel="noopener external" target="_blank"`).
  *[obsolete workaround]* was a `show link` rule checking `type(it.dest)
  == str`; now a `rules.replace::<LinkElem>` candidate. Note `html.a(rel:
  ..)` takes an array of tokens (`("noopener","external")`), not a
  joined string.
- *Native `#table`* works for HTML; only `align:` doesn't reflect into
  per-cell `style="text-align"` (relax `td/th:style`).
- *Show rules can't replace typst's auto wrapper* (`show
  table.cell: ..` → `<td><td>..</td></td>`). Same for `list.item`,
  `enum.item`. (A `rules.replace` on the *element* can, where one exists
  — distinct from a userland `show`.)
- *Pulldown auto-wraps inline content in `<p>` inside containers*; typst
  doesn't. Wrap manually or splice via `raw-html`. Same root cause:
  bare inline elements (`<br>`, empty `<a>`) get `<p>`-wrapped at block
  context, and a soft break injects `<span
  style="white-space:pre-wrap">`.
- *`query(<label>)` is BUNDLE-scope in bundle mode, not per-doc.* The
  early `<twyla-page>` design always returned the first doc's slug. Fixed
  by label-based internal links (typst-html resolves per-doc) + a `state`
  object for per-doc metadata. `here().page()` is useless as a doc
  discriminator in HTML mode (always 1).
- *Heading `id` is link-driven, not label-driven.* typst-html emits
  `id="label"` only with an incoming `#link(<label>)`. For zola parity
  (every heading gets an id) re-emit headings with explicit `id` — and
  this is itself a mechanism-2 candidate now.
- *Bypass typst's auto-`<head>`/`<body>` by emitting your own `<html>`*
  (`finalize_dom` short-circuits on a single top-level `<html>`). Cost:
  footnotes unsupported in that mode. (Mechanism 3's default template
  emits the shell, so authors normally needn't.)
- *Virtual `main.typ` is dead-cheap to inject* — the in-memory source
  map is consulted before disk. The same trick serves an embedded
  default template / `@twyla` package bytes.
- *Reserved-word attrs need string keys* (`("as":.., "type":..)`).
  *`[#]` is a parse error* — escape `[\#]`.
- *`#let` chain-across-newlines hazard* — `#let f(s) = s\n .replace(..)`
  parses as identity-of-`s` with orphan markup, no error. Wrap multi-line
  chains in `(…)`.

= Upstream-watch list

Worth raising upstream / watching. *Note:* mechanism 2 (`rules.replace`)
now lets us solve several of these *in-library* without an upstream
change — demoted accordingly.

+ `html.raw(content)` — first-class verbatim HTML. Still wanted; the
  resolution-pass `<script>` abuse is the standing workaround.
+ `#table(align:)` → `style="text-align"` on `<th>`/`<td>`; `colspan`/
  `rowspan` attrs. *(Could also be a `rules.replace::<TableCell>`.)*
+ Show rules that *replace* typst's auto wrapper on
  `table.cell`/`list.item`/`enum.item` in userland (mechanism 2 covers
  the engine side; this is about author ergonomics).
+ Hook on `image()` to emit external `src` instead of data URLs in HTML
  — benefits anyone exporting HTML at scale; also the seam for native
  `image()`-through-assets.
+ Suppress the `<span style="white-space:pre-wrap">` whitespace shim and
  the paragraph auto-wrap of bare inline elements in HTML output.
+ A public seam to inject data into the introspection fixed-point (would
  make Crux 2's "second compile pass" a single pass).

= Fork vs library

*Standing assumption: do not fork typst.* Twyla consumes typst as a
library, supplies a custom `World`, injects builtins + HTML rules +
a default template, and post-processes the bundle. If something is
genuinely better upstream, *upstream it* before forking. Fork threshold:
library + upstreaming both proven inadequate.

The native-rule discovery (mechanism 2) is fresh evidence for this
stance: things that looked like they'd need show-rule hacks or a fork
(global h-level / link-rel behavior) turn out to be supported
customization points. Every feature considered has landed on "library":
asset fingerprinting (post-export walk), image optimization (twyla
function bypassing `image()`), pretty URLs (`#document` paths), hot
reload (SSE inject), feeds, native HTML-element behavior (`rules`),
incremental compile (comemo). No fork even for HTML-syntax sugar.

Why: typst's HTML/bundle work evolves in `main`; a fork is a permanent
maintenance tax on grammar/library/output changes for benefits mostly
reachable from outside. If we ever hit a real wall, document what was
tried — that's the evidence base for reconsidering.

= Notes

- *VCS:* `~/Src/twyla` and `~/Src/site` use jj. *Rule:* always `jj new`
  before editing (no staging area — edits land in whatever revision the
  working copy points at). This doc lives on the `myr` line; the native-
  prelude spike lives on a separate change to be merged/rebased.
- *Typst pin:* `main` `de6f400` (2026-04-11) for bundle export (PR
  #7964). Adding native builtins required a direct `typst-utils` dep at
  the same rev (the `#[func]` expansion references it). Switch to
  crates.io once 0.15.x ships — that release is the real blocker for a
  reproducible install, and it's outside our control.
- *Packages now matter:* unlike earlier phases, the dogfood doc requires
  package-import support to compile at all (it imports `@preview/*`).
- *Dev-asset bootstrap (until `asset()`/grass lands):* `~/Src/site/
  static/` holds symlinks into zola/vite outputs in `public/`. Recompile
  CSS via `zola build --drafts` (or `sass sass/site.sass public/site.css`
  directly); JS via `yarn build`. This symlink hack is the bridge the
  sass-via-grass work removes.
- *Site porting scope:* tera shortcodes done (`centered`/`diagram`/
  `image`/`svg`/`tagline`); pending `math`/`var` (wait for a page that
  uses them). Known hazards: ROT13 email obfuscation in footer,
  conditional asset loading (`page.extra.tilings`), SVG font-family
  rewriting (handled by the `svg`/`diagram` shortcodes).
