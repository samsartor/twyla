#import "/templates/lib.typ": page-template
#show: page-template
#set document(title: "configuration")

= Configuration
<configuration>

Twyla is directory-driven; most "configuration" is just where you put files.
A few knobs come from the CLI.

== Project layout
<project-layout>

```tree
content/    pages (*.typ) + colocated assets (everything else)
static/     copied verbatim to the output root
templates/  your theme (Typst, and optionally .tmTheme / partials)
sass/       stylesheets, referenced via asset.sass
```

- Every `content/**/*.typ` whose name doesn't start with `_` is a page.
  `_`-prefixed files are private partials (import them; they don't route).
- `content/main.typ` is the site index; `content/<dir>/main.typ` is a section
  index. Routes mirror the path: `foo/bar.typ → /foo/bar/`,
  `foo/main.typ → /foo/`. Override per page with `#set document(output: …)`.
- Non-`.typ` files under `content/` are copied next to their page (zola-style
  colocated assets); `static/` is copied to the output root.

== Base URL
<base-url>

Pass `--base-url <url>` (or set `TWYLA_BASE_URL`) when doing the production build.
It is exposed to your Typst as `sys.inputs.base_url` (empty string if unset),
and used for absolute links and the `convert` link audit.

```typ
#let base-url = sys.inputs.at("base_url", default: "")
```

== Commands
<commands>

/ `twyla serve`: dev server on `http://localhost:1111`, live-reloading.
/ `twyla build [-o <dir>]`: write the static site (default `./public/`).
/ `twyla convert --from zola --base-url <url>`: scaffold `.typ` drafts from a
  Markdown site and diff the result against the source SSG's built `public/`.
  `--verify` is the read-only gate; `--only <slug>` scopes to one route.
/ `twyla md2typ <file.md>`: translate one Markdown file to a `.typ` draft on
  stdout.

== Theming
<theming>

A page selects its layout with a show rule, conventionally a `KIND-template`
that matches the page's `kind`:

```typ
#import "/templates/lib.typ": *
#show: page-template
```

Templates are ordinary Typst that build HTML with `html.elem` (and the
`html.div` / `html.a` / … constructors), read `document.*` for the current
page, and `documents()` for listings. See
#link("/reference/document")[document] and #link("/reference/asset")[asset].
