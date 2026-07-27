// Components for the cumulative Getting Started example. Each stage shows the
// files touched, the relevant source, and the state of the same "Field Notes"
// site after that change.

#import "/grabber.typ": grabber-canvas, grabber-grad

#let browser-frame(body, class: "") = html.div(class: "journey-browser " + class, {
  html.div(class: "browser-bar", {
    html.span(class: "browser-dots", [● ● ●])
    html.span(class: "browser-url", [localhost:1111])
    html.span(class: "browser-live", [● live])
  })
  html.div(class: "browser-page", body)
})

#let journey-step(
  number,
  body,
  title: none,
  kicker: none,
  files: (),
  code: none,
  preview: none,
) = html.elem(
  "section",
  attrs: (class: "journey-step", id: "journey-" + number),
  {
    html.div(class: "journey-marker", number)
    html.div(class: "journey-heading", {
      html.div(class: "journey-kicker", kicker)
      html.elem("h3", attrs: (class: "journey-title"), title)
      html.div(class: "journey-copy", body)
      html.div(class: "journey-files", {
        html.span(class: "files-label", [Changed])
        for file in files {
          html.span(class: "file-chip", file)
        }
      })
    })
    html.div(class: "journey-workspace", {
      html.div(class: "journey-source", {
        html.div(class: "journey-panel-bar", {
          html.span([Source])
          html.span(class: "panel-meta", files.last())
        })
        html.div(class: "journey-code", code)
      })
      html.div(class: "journey-result", {
        html.div(class: "journey-panel-bar", {
          html.span([Result])
          html.span(class: "panel-meta", [auto-reloaded])
        })
        preview
      })
    })
  },
)

#let preview-one = browser-frame(
  html.div(class: "demo-plain demo-one", {
    html.elem("h1", [Things worth keeping])
    html.p([Hello! This is my new site.])
  }),
)

#let preview-pages = browser-frame(
  html.div(class: "demo-plain demo-pages", {
    html.elem("h1", [Field Notes])
    html.p([Things I have learned, made, or eaten.])
    html.elem("hr")
    html.a(href: "#", [Rewriting my blog])
    html.a(href: "#", [Oops, I vibe-coded my blog])
    html.a(href: "#", [How to bake bread])
  }),
)

#let preview-listing = browser-frame(
  html.div(class: "demo-plain demo-listing", {
    html.div(class: "demo-mini-nav", {
      html.b([Field Notes])
      html.span([About])
    })
    html.elem("h1", [Recent notes])
    html.div(class: "demo-post", {
      html.span(class: "demo-date", [12 Apr 2026])
      html.b([Rewriting my blog])
      html.p([Leaving Markdown behind for a pleasantly programmable blog.])
    })
    html.div(class: "demo-post", {
      html.span(class: "demo-date", [03 Apr 2026])
      html.b([How to bake bread])
      html.p([A suspiciously elaborate loaf, from starter to crust.])
    })
  }),
)

#let preview-styled = browser-frame(
  html.div(class: "demo-styled", {
    html.div(class: "demo-mini-nav", {
      html.b([Field / Notes])
      html.span([Archive  About])
    })
    html.div(class: "demo-rule")
    html.div(class: "demo-overline", [Notes from the workshop])
    html.elem("h1", [Recent things worth keeping])
    html.div(class: "styled-post", {
      html.span([01])
      html.div({
        html.b([Rewriting my blog])
        html.p([Leaving Markdown behind for a programmable publishing system.])
      })
      html.time([12.04.26])
    })
    html.div(class: "styled-post", {
      html.span([02])
      html.div({
        html.b([How to bake bread])
        html.p([A suspiciously elaborate loaf, from starter to crust.])
      })
      html.time([03.04.26])
    })
  }),
  class: "browser-styled",
)

#let preview-image = context browser-frame(
  html.div(class: "demo-styled demo-with-image", {
    html.div(class: "demo-mini-nav", {
      html.b([Field / Notes])
      html.span([Archive  About])
    })
    html.div(class: "demo-rule")
    html.div(class: "image-layout", {
      html.div({
        html.div(class: "demo-overline", [Featured note])
        html.elem("h1", [An excellent studio assistant])
        html.p([Audrey has strong opinions about static site generators.])
        html.span(class: "read-more", [Read note →])
      })
      html.img(
        src: asset.image("/content/audrey.jpg", format: "webp", width: 512).url(),
        alt: "Audrey",
      )
    })
  }),
  class: "browser-styled",
)

#let preview-advanced = browser-frame(
  html.div(class: "demo-advanced", {
    html.div(class: "advanced-orbit", {
      for (i, p) in (20deg, 35deg, 52deg, 28deg).enumerate() {
        html.div(
          class: "advanced-tool advanced-tool-" + str(i + 1),
          html.frame(grabber-canvas(
            theta: i * 47deg - 62deg,
            phi: p,
            color: grabber-grad,
          )),
        )
      }
    })
    html.div(class: "demo-mini-nav", {
      html.b([Field / Notes])
      html.span([Archive  About])
    })
    html.div(class: "advanced-copy", {
      html.div(class: "demo-overline", [Built from source])
      html.elem("h1", [The theme is part of the program.])
      html.p([Even this artwork was drawn in Typst and emitted as SVG.])
    })
  }),
  class: "browser-advanced",
)
