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
  apply-base-rules,
)

// Cross-document enumeration of every `mark-as-post`-emitting page,
// newest first, drafts filtered out. Pure author-side typst over the
// `<twyla-post>` introspector — the engine isn't involved.
#let post-list() = context {
  let posts = query(<twyla-post>).map(m => m.value)
  let live = posts.filter(p => not p.at("draft", default: false))
  let sorted = live.sorted(key: p => p.at(
    "date",
    default: datetime(year: 1970, month: 1, day: 1),
  ))
  let sorted = sorted.rev()
  if sorted.len() == 0 {
    html.p([No posts yet.])
  } else {
    html.elem("ul", attrs: (class: "post-list"), {
      for p in sorted {
        html.elem("li", {
          let permalink = base-url + "/" + p.path + "/"
          html.a(href: permalink, class: "post-link", p.title)
          let desc = p.at("description", default: none)
          if desc != none and desc != "" {
            html.span(class: "post-desc", desc)
          }
        })
      }
    })
  }
}

#let home-template(
  title: none,
  description: none,
  body,
) = apply-base-rules({
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
      html.elem("header", html.h1(title))
      html.elem("main", body)
    })
  })
})
