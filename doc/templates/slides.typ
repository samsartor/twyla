#import "./theme.typ": site-head, content-rules

#let frame = state("frame", 1)

#let slide(body, n: 1) = for i in range(1, n+1) {
  frame.update(i)
  html.div(
    if type(body) == content { body } else { body(i) },
    class: "slide",
  )
}

#let page-shell(body, sidebar: none) = html.elem("html", attrs: (lang: "en"), {
  site-head(slides: true)
  html.elem("body", {
    html.elem("main", attrs: (class: "content" + if sidebar != none { " with-sidebar" } else { "" }), {
      if sidebar != none {
        html.elem("aside", attrs: (class: "toc"), sidebar)
      }
      html.elem("article", attrs: (class: "prose"), content-rules(body))
    })
  })
})

// Default template: `#show: page-template` wraps a normal page.
#let slides-template(body) = page-shell(body)

#let side-by-side(..args) = html.div(
  class: "side-by-side",
  style: "grid-template-columns: repeat(" + str(args.pos().len()) + ", 1fr)",
  for arg in args.pos() {
    html.div(arg)
  }
)

#let center-thought(body) = html.div(
  class: "centering",
  html.div(
    class: "thought",
    body
  )
)
