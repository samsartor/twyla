// Source → result components for the guide. Executable examples bind one raw
// value and use that same value both for syntax-highlighted source and eval,
// so the displayed code is exactly what produced the preview.

#let browser-frame(body, class: "") = html.div(class: "journey-browser " + class, {
  html.div(class: "browser-bar", {
    html.span(class: "browser-dots", [● ● ●])
    html.span(class: "browser-url", [localhost:1111])
  })
  html.div(class: "browser-page", body)
})

#let source-result(
  source,
  result,
  source-label: "content/main.typ",
  result-label: "Rendered page",
  class: "",
) = html.div(class: "example-workspace journey-workspace " + class, {
  html.div(class: "journey-source", {
    html.div(class: "journey-panel-bar", {
      html.span([Source])
      html.span(class: "panel-meta", source-label)
    })
    html.div(class: "journey-code", source)
  })
  html.div(class: "journey-result", {
    html.div(class: "journey-panel-bar", {
      html.span([Result])
      html.span(class: "panel-meta", result-label)
    })
    browser-frame(result)
  })
})

#let project-example(
  tree,
  source,
  result,
  source-label: "content/main.typ",
  result-label: "Rendered home page",
) = html.div(class: "project-example", {
  html.div(class: "project-tree", {
    html.div(class: "project-tree-label", [Project files])
    tree
  })
  source-result(
    source,
    result,
    source-label: source-label,
    result-label: result-label,
    class: "project-listing",
  )
})

// A small deterministic site used only as the `documents()` input while
// evaluating the guide's listing example.
#let guide-documents() = (
  (
    title: [Rewriting My Blog],
    date: datetime(year: 2026, month: 4, day: 12),
    description: [My blog was written in normal everyday Markdown.],
    url: "#rewriting-my-blog",
    kind: "post",
    draft: false,
  ),
  (
    title: [Oops, I Vibecoded My Blog],
    date: datetime(year: 2026, month: 4, day: 7),
    description: [A few notes from building the first version.],
    url: "#oops-i-vibecoded-my-blog",
    kind: "post",
    draft: false,
  ),
  (
    title: [How to Bake Bread],
    date: datetime(year: 2026, month: 4, day: 3),
    description: [The recipe I learned from that talking rat.],
    url: "#how-to-bake-bread",
    kind: "post",
    draft: false,
  ),
)
