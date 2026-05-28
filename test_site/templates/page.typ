// test_site post template. Apply at the top of a content page:
//
//   #import "/templates/page.typ": *
//   #show: page-template.with(
//     path: "hello",
//     title: "Hello, twyla",
//     description: "...",
//     date: datetime(year: 2026, month: 5, day: 28),
//   )
//
// Emits the full `<html>` document; the remainder of the file is the body.

#import "/templates/lib.typ": (
  base-url,
  format-date,
  mark-as-post,
  apply-base-rules,
)

#let page-template(
  path: none,
  title: none,
  description: none,
  date: none,
  draft: false,
  body,
) = apply-base-rules({
  mark-as-post((
    path: path,
    title: title,
    description: description,
    date: date,
    draft: draft,
  ))

  html.elem("html", attrs: (lang: "en"), {
    html.elem("head", {
      html.elem("meta", attrs: (charset: "utf-8"))
      html.elem("title", title)
      if description != none {
        html.elem("meta", attrs: (name: "description", content: description))
      }
      html.elem("link", attrs: (rel: "stylesheet", href: base-url + "/style.css"))
    })
    html.elem("body", {
      html.elem("header", html.a(href: base-url + "/", [home]))
      html.elem("article", {
        html.h1(title)
        if date != none {
          html.elem("time", attrs: (datetime: format-date(date)), format-date(date))
        }
        if draft {
          html.div(class: "draft-banner", [This page is a draft.])
        }
        body
      })
    })
  })
})
