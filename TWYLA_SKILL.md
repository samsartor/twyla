# Porting a site to Twyla

This is a field guide for an agent porting an existing static site (today:
[Zola](https://www.getzola.org)) to Twyla. It assumes you can read the target
site's templates and run shell commands. It was written from a real port of a
~17-page Zola blog; every gotcha below is one we actually hit.

The goal is **not** a line-by-line translation of the old templates. A naive
conversion of Tera/Liquid templates produces Typst that reads like
"templating-but-worse." Instead you build a small, composable **library of
components** in Typst and assemble each page template from a few lines of it.

---

## The loop

```
zola build --drafts            # 1. produce the ground truth in public/
twyla convert --from zola --base-url https://example.com   # 2. scaffold .typ drafts
#                               3. write templates/ (a component library)
twyla convert --verify --base-url https://example.com      # 4. diff vs ground truth; fix; repeat
```

`twyla convert` compiles the whole site in memory, **diffs every page against
the ground-truth `public/`** (structure, whitespace-relaxed), and runs a link
audit. `--verify` is the read-only gate: it never writes, just reports. Drive
it to `RESULT: OK`.

### 1. Ground truth

Twyla diffs against the source SSG's built output. Build it first:

```
zola build --drafts            # --drafts so draft pages are included
```

This populates `public/`. If `public/` is stale or missing, `convert` will tell
you. (Override the location with `--ground-truth <dir>`.)

### 2. Scaffold drafts

```
twyla convert --from zola --base-url <url>
```

For every `content/**/*.md` this writes a sibling `.typ` draft (e.g.
`guis-1.md → guis-1.typ`, `_index.md → main.typ`) and a placeholder
`templates/lib.typ`. The drafts carry the markdown body translated to Typst,
frontmatter as `#set document(...)`, and shortcode calls as `#name(...)`.

- It **never clobbers** an existing `.typ` (generate mode). To regenerate after
  the importer improves, use `--overwrite`, or run `twyla md2typ <file.md>` to
  re-translate a single page to stdout.
- The drafts say "Manual cleanup expected!" at the top. Expect to hand-fix a
  few per-page issues (see gotchas).

### 3. Build the component library

This is the real work. See "Architecture" below.

### 4. Iterate

```
twyla convert --verify --base-url <url>             # all pages
twyla convert --verify --base-url <url> --only guis-1   # scope to one route
```

Read the diff (see "Reading the diff"), fix a template or a draft, re-run. The
site must **compile as a whole** — one broken page fails the run for all — so
clear compile errors first, then chase diffs.

---

## The Twyla API you build against

Twyla registers these on top of stock Typst. This is your whole toolbox.

**Per-page metadata** — set near the top of a page, read back contextually:

```typ
#set document(
  title: "My Post",
  date: datetime(year: 2024, month: 1, day: 2),
  description: [A short blurb.],
  kind: "post",            // optional; defaults from filename (see kinds)
  draft: false,
  extra: (any: "dict"),    // arbitrary author data
  output: "custom/index.html",   // override the route if needed
)
#context document.title    // readable on this page inside #context
```

**Cross-page listing** — `documents()` returns one dict per page:

```typ
#context for doc in documents() {
  // doc.url, doc.output, doc.title, doc.date, doc.description,
  // doc.kind, doc.draft, doc.extra
}
```

**Assets** — fingerprinted, emitted to `/assets/...`, resolved contextually:

```typ
#context asset.sass("/sass/site.sass").url()    // compile SCSS/Sass → CSS
#context asset.file("/static/logo.png").url()   // copy a file verbatim
#context asset.image("/img/hero.jpg", width: 1024, format: "webp").url()  // resize + transcode
#context asset.typst("/resume/cv.typ", format: "pdf").url()  // compile a typst doc (svg/png/pdf/html)
```

Paths are project-root-relative when they start with `/`. `.url()` is
contextual — call it inside `#context` (or inside a function the template wraps
in `#context`).

**Raw HTML & text:**

```typ
#raw-html("<span class=\"x\">literal HTML</span>")  // spliced in verbatim
#plain-text(some-content)                            // flatten content → string
```

**`sys.inputs.base_url`** — the `--base-url` value (empty if unset).

**Native rules already applied for you** (don't reimplement):
- headings get auto-slug `id`s (zola parity); an explicit `<label>` wins.
- external `http(s)` links via `#link` get `rel="noopener external" target="_blank"`.
- `image()` / markdown `![]()` are emitted as fingerprinted asset files, not base64.

**Page kinds** (selects the `{kind}-template`, and the default if `kind` unset):
- `content/main.typ` → `root` (the site index)
- `content/<dir>/main.typ` → `dir` (a section index)
- anything else → `page`

**Static & colocated files:** everything under `static/` is copied to the
output root; non-`.typ` files under `content/` are copied alongside their page
(zola "colocated assets"). So `/scripts/site.js` (from `static/scripts/`) and
`/resume.pdf` (from `content/`) just work as literal URLs.

---

## Architecture: a component library, not templates

Split the theme into small modules and let one entry point compose them. The
port that produced this guide used:

```
templates/
├ theme.typ        # site chrome: <head>, header, footer, scripts, <html> wrapper
├ components.typ   # reusable pieces: pills, listing cards, the page header
├ shortcodes.typ   # the {{ }} / {% %} helpers, as plain functions
└ lib.typ          # imports the above; defines {root,dir,page}-template
```

Each page can use a single show rule:

```typ
#import "/templates/lib.typ": *
#show: page-template
```

A page template is then tiny — it just stitches components:

```typ
#let page-template(body) = context page-shell(
  title: "Site - " + plain-text(document.title),
  description: meta-desc(document),
  {
    description-header(document)      // the title block
    content-body(document.extra, body)
  },
)
#let dir-template = page-template     // same layout for now
```

Build the HTML with `html.elem(tag, attrs: (..), body)` and the convenience
constructors `html.div`, `html.a`, `html.span`, etc. Use `documents()` for
listings, `document.*` for the current page.

---

## CommonMark → Typst gotchas

These are the impedance mismatches between the source SSG's CommonMark and
Typst's HTML export. **They are the bulk of the work.** Each has a one-line fix.

### Inline custom elements split the paragraph → `box()`
An unknown tag via `html.elem` defaults to **block**, so a custom inline
element (`<col-s>`, a swatch, an icon component) splits the surrounding
paragraph into `…<p>x</p><custom/><p>y</p>…`. Wrap it in `box()` to force inline:

```typ
box(html.elem("col-s", attrs: (value: "460nm"), [460nm]))
```

The importer auto-boxes custom elements it finds **mid-paragraph**, so a clean
conversion usually handles this for you. You still reach for `box()` by hand
when you author a custom-element helper yourself, or for a custom element on its
**own line** that CommonMark wraps in `<p>` (an open+close pair like
`<tiling-canvas …></tiling-canvas>`) — once boxed-inline, Typst's own
paragraph-wrapping then matches.

### A bare block element gets `<p>`-wrapped → `raw-html()`
Typst wraps a lone inline/void element (a `<span class=tagline>`, a standalone
`<br>`) in `<p>`, but CommonMark emits it as a bare HTML block. You can't relax
this away (`<p>` is usually meaningful). `block()` removes the `<p>` but injects
`style="display:block"`. The clean fix is `raw-html`, which lands as a bare
block:

```typ
#let tagline(body) = raw-html("<span class=\"tagline\">" + plain-text(body) + "</span>")
// a stray <br> in content:  #raw-html("<br>")
```

### Smart quotes
Typst applies smart quotes to **markup** (string literals are left alone), and
differs from zola in a couple of spots. To opt a region out entirely, use the
`smartquote` set rule:

```typ
#set smartquote(enabled: false)   // straight quotes in this scope
```

That's the clean fix for your own chrome (a literal `That's all`). Note one
remaining divergence to watch in body text: an apostrophe after a digit becomes
a **prime** `′` in Typst (e.g. `P3's` → `P3′s`); fix the source text (`P3’s`)
on the affected page.

### The `asset` module gets shadowed
If a shortcode takes an `asset:` parameter (a path string), it shadows the
global `asset` module inside that function. Capture it at module scope:

```typ
#let asset-url(path) = asset.file(path).url()   // top level, before the shortcode
```

### Unpadded dates
Zola prints `2023-2-6`, not `2023-02-06`:

```typ
#d.display("[year]-[month padding:none]-[day padding:none]")
```

### Syntax highlighting
`<pre>` is diffed **text-only**, so token-level highlight differences are
tolerated (you may see an `attr-drift` *warning*; ignore it). Two real
differences from zola/syntect to handle:
- Typst doesn't emit inline `color`/`background-color` on the `<pre>` the way
  syntect does — so a code block renders unstyled until you give `pre` a
  `color`/`background` in your **CSS/SASS**.
- Typst picks the highlight theme from a `set raw(theme: ".../Some.tmTheme")`
  rule (a TextMate `.tmTheme` file in the project), applied on the body — it
  does not read zola's `highlight_theme` config. Drop a `.tmTheme` in
  `templates/` and point `set raw` at it.

---

## Reading the diff

- `-` is **expected** (the ground truth); `+` is what **Twyla produced**.
- **Whitespace is fully relaxed** — never chase indentation or blank lines.
- `<pre>` compares **text-only**; `<td>/<th>` ignore `style`.
- URL-bearing attributes (`src`, `srcset`, sub-resource `href`) are
  **value-relaxed** — the link audit proves they resolve, so you don't have to
  match fingerprinted asset URLs. `<a href>` to a *page* stays strict.
- Scope with `--only <slug>` while iterating on one page.
- The audit reports `broken-link` / `unreachable` for internal URLs Twyla
  doesn't produce, and `feed-todo` for feeds (not yet implemented).

---

## Treat the drafts as yours

`twyla convert` gives you a **starting point**, not a maintained source. Once a
page is drafted, the `.typ` is yours: iterate on it directly, the same way you
iterate on the templates. Don't get stuck trying to make the importer emit
exactly the right thing — fixing a stray construct in one draft is usually
faster and is what an end user would actually do.

Reserve escalation for diffs that seem **unreasonable to resolve** — where the
ground truth itself looks wrong, or matching it would mean fighting Typst. The
canonical example: zola putting rendered HTML back into a `<meta name=description>`
attribute that should be plain text. That's a case to flag to the project owner
(relax the diff, or accept it) rather than engineer around. When in doubt about
whether a diff is worth chasing, ask.

> If you do regenerate drafts after the converter improves, note that
> `convert --overwrite` rewrites **all** of them and discards hand-edits; use
> `twyla md2typ <file.md>` to re-translate a single page to stdout instead.
