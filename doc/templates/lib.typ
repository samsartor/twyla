// Theme entry point. Pages do `#import "/templates/lib.typ": *` and get the
// page templates plus the reference-rendering helpers. The shells compose the
// chrome from `theme.typ` and the reference rendering from `components.typ`.

#import "theme.typ": *
#import "components.typ": *
#import "boxdraw.typ": boxdraw

#let horizontalrule = context { if target() == "html" { html.hr() } else { line(length: 100%) } }

// The page shell: `<html>` + head + header + `<main>` + footer. With a
// `sidebar`, `<main>` becomes a two-column [toc | article] grid.
#let page-shell(body, sidebar: none) = html.elem("html", attrs: (lang: "en"), {
  site-head()
  html.elem("body", {
    site-header
    html.elem("main", attrs: (class: "content" + if sidebar != none { " with-sidebar" } else { "" }), {
      if sidebar != none {
        html.elem("aside", attrs: (class: "toc"), sidebar)
      }
      html.elem("article", attrs: (class: "prose"), content-rules(body))
    })
    site-footer
  })
})

// Default template: `#show: page-template` wraps a normal page.
#let page-template(body) = page-shell(body)
