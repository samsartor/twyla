// test_site home template. Apply at the top of `content/_index.typ`:
//
//   #import "/templates/home.typ": *
//   #show: home-template.with(title: "test_site", description: "...")
//
//   Intro prose here.
//
//   #post-list()

#import "/templates/lib.typ": (
  base-url,
  format-date,
  text-of,
  apply-base-rules,
)

// Cross-document enumeration via the engine's `documents()` builtin: every
// page that set `kind: "post"`, newest first, drafts filtered out. No
// author-side metadata duplication — the data is harvested from each page's
// `#set document(..)`.
#let post-list() = context {
  let posts = documents().filter(p => p.kind == "post" and not p.draft)
  let epoch = datetime(year: 1970, month: 1, day: 1)
  let sorted = posts.sorted(key: p => if p.date == none { epoch } else { p.date }).rev()
  if sorted.len() == 0 {
    html.p([No posts yet.])
  } else {
    html.elem("ul", attrs: (class: "post-list"), {
      for p in sorted {
        html.elem("li", {
          html.a(href: base-url + p.url, class: "post-link", p.title)
          if p.description != none {
            html.span(class: "post-desc", p.description)
          }
        })
      }
    })
  }
}

#let home-template(body) = apply-base-rules(context {
  html.elem("html", attrs: (lang: "en"), {
    html.elem("head", {
      html.elem("meta", attrs: (charset: "utf-8"))
      html.elem("title", document.title)
      if document.description != none {
        html.elem(
          "meta",
          attrs: (name: "description", content: text-of(document.description)),
        )
      }
      html.elem("link", attrs: (rel: "stylesheet", href: base-url + "/style.css"))
    })
    html.elem("body", {
      html.elem("header", html.h1(document.title))
      html.elem("main", body)
    })
  })
})
