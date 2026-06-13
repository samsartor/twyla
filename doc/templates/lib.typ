// Theme entry point. Pages do `#import "/templates/lib.typ": *` and get the
// page templates plus the reference-rendering helpers. The shells compose the
// chrome from `theme.typ` and the reference rendering from `components.typ`.

#import "@preview/frame-it:2.0.0": *
#import "@preview/dtree:0.1.1": dtree

#import "theme.typ": *
#import "components.typ": *

#let note = frame("Note", blue)
#let horizontalrule = context { if target() == "html" { html.hr() } else { line(length: 100%) } }

// Content show rules shared by every page: callout quotes, the `tree` and
// `example` code fences, and the (always-dark) syntax theme for code blocks.
//
// TODO: code-block colors are baked in as inline `<span style="color:…">` by
// typst's HTML export (see typst-html html_span_filled), so they can't follow
// the light/dark toggle. We pin code panels to a dark theme for now; revisit
// once twyla ships its own CSS-class-based `raw` show rule.
#let content-rules(body) = {
  show quote.where(block: true): it => note(it.body)
  show: frame-style(styles.hint)
  show raw.where(lang: "tree"): it => if target() == "html" { it } else { dtree(raw(it.text.replace("├", " ").replace("└", " "))) }
  // `--test-examples`: compile-check each `example` block and highlight as typst.
  show raw.where(lang: "example"): twyla-examples.compile-example
  set raw(theme: "/templates/Dracula.tmTheme")
  body
}

// The page shell: `<html>` + head + header + `<main>` + footer. With a
// `sidebar`, `<main>` becomes a two-column [toc | article] grid.
#let page-shell(body, sidebar: none) = html.elem("html", attrs: (lang: "en"), {
  site-head
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
