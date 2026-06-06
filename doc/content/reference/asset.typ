#import "/templates/lib.typ": page-template
#show: page-template
#set document(title: "asset")

= asset
<asset>

The `asset` module turns project files into fingerprinted outputs emitted under
`/assets/…`. Two constructors exist today; both return an `Asset` you either
*link* to with `.url()` or *inline* with `.read()`.

== asset.file
<asset-file>

```typ
#context asset.file("/static/logo.png").url()
```

Copies a project file verbatim and fingerprints it by content hash, so the URL
changes when the bytes do. Paths starting with `/` are project-root-relative;
otherwise they resolve relative to the calling file.

== asset.sass
<asset-sass>

```typ
#context html.elem("link", attrs: (
  rel: "stylesheet",
  href: asset.sass("/sass/main.sass").url(),
))
```

Compiles a Sass/SCSS file to CSS (via #link("https://github.com/connorskees/grass")[grass])
and fingerprints the result. The indented `.sass` syntax and `.scss` are both
supported, chosen by extension. `@use`/`@import` partials are tracked, so
editing a partial invalidates the compiled CSS.

== .read() for inlining
<asset-read>

Where `.url()` links to the emitted file, `.read()` returns its resolved output
*contents* — for inlining instead of linking. A file asset reads back its
bytes; a sass asset reads back its *compiled* CSS.

Like the built-in #link("https://typst.app/docs/reference/data-loading/read/")[`read`],
it returns a UTF-8 `str` by default and raw `bytes` with `encoding: none`:

```typ
// compiled CSS, inlined into a <style> (str, no wrapper needed):
#context html.elem("style", asset.sass("/sass/critical.sass").read())

// an SVG spliced in verbatim — pairs with raw-html:
#context raw-html(asset.file("/static/icon.svg").read())

// raw bytes, e.g. to hash or re-encode:
#context asset.file("/static/logo.png").read(encoding: none)
```

Path-backed assets are read from disk on demand, so `.read()` never holds the
bytes in memory longer than the call (fine for large files).

== .url() and .read() are contextual
<resolution-is-contextual>

An asset is resolved during compilation, so both `.url()` and `.read()` must be
called inside a `#context` block (or inside a function the template wraps in
`#context`):

```typ
#context html.elem("img", attrs: (src: asset.file("./photo.jpg").url()))
```

The native image rule uses this machinery automatically: a plain
`#image("photo.jpg")` or a Markdown `![](photo.jpg)` is emitted as a
fingerprinted file, not an inline base64 blob.
