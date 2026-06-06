#import "/templates/lib.typ": page-template
#show: page-template
#set document(title: "asset")

= asset
<asset>

The `asset` module turns project files into fingerprinted outputs emitted under
`/assets/…`. Two constructors exist today; both return an `Asset` whose
resolved URL you read with `.url()`.

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

== .url() is contextual
<url-is-contextual>

An asset's URL is resolved during compilation, so `.url()` must be called
inside a `#context` block (or inside a function the template wraps in
`#context`):

```typ
#context html.elem("img", attrs: (src: asset.file("./photo.jpg").url()))
```

The native image rule uses this machinery automatically: a plain
`#image("photo.jpg")` or a Markdown `![](photo.jpg)` is emitted as a
fingerprinted file, not an inline base64 blob.
