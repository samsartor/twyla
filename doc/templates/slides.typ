#import "./theme.typ": site-head, content-rules

#let frame = state("frame", 1)

#let slide(body, n: 1) = for i in range(1, n+1) {
  frame.update(i)
  html.div(
    if type(body) == content { body } else { body(i) },
    class: "slide",
  )
}

// The deck shell: a full-screen `<div class="deck">` holding the stage (every
// `.slide` stacked, one shown at a time), the bottom progress bar, and a HUD
// with the slide counter and prev/next buttons. `templates/slides.js` drives
// it; the counter text is filled in by JS.
#let page-shell(body) = html.elem("html", attrs: (lang: "en"), {
  site-head(slides: true)
  html.elem("body", {
    html.div(class: "deck", {
      html.div(class: "deck-stage prose", content-rules(body))
      html.div(class: "deck-progress", html.div(class: "deck-progress-fill"))
      html.div(class: "deck-hud", {
        html.elem("button", attrs: (class: "deck-nav deck-prev", type: "button", "aria-label": "Previous slide"), [‹])
        html.span(class: "deck-counter", "")
        html.elem("button", attrs: (class: "deck-nav deck-next", type: "button", "aria-label": "Next slide"), [›])
      })
    })
    context html.elem("script", attrs: (src: asset.file("/templates/slides.js").url(), defer: ""))
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

// A small, muted line pinned to the bottom of a slide — for sources, asides,
// and attributions.
#let postscript(body) = html.div(class: "postscript", body)

#let center-thought(body) = html.div(
  class: "centering",
  html.div(
    class: "thought",
    body
  )
)

