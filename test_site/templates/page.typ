// test_site post template. Apply at the top of a content page:
//
//   #import "/templates/page.typ": *
//   #set document(
//     title: "Hello, twyla",
//     description: "...",
//     date: datetime(year: 2026, month: 5, day: 28),
//     kind: "post",
//   )
//   #show: page-template
//
// Emits the full `<html>` document; the remainder of the file is the body.
// Metadata comes from `#set document(..)` (twyla's overloaded element) — the
// template reads it back contextually, and twyla harvests the same fields into
// `documents()`. No more redundant per-page metadata dict.

#import "/templates/lib.typ": (
  base-url,
  format-date,
  text-of,
  apply-base-rules,
)

#let page-template(body) = apply-base-rules(context {
  let title = document.title
  let date = document.date
  let description = document.description

  html.elem("html", attrs: (lang: "en"), {
    html.elem("head", {
      html.elem("meta", attrs: (charset: "utf-8"))
      html.elem("title", title)
      if description != none {
        html.elem("meta", attrs: (name: "description", content: text-of(description)))
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
        if document.draft {
          html.div(class: "draft-banner", [This page is a draft.])
        }
        body
      })
    })
  })
})
