// Source → result components for the guide. Executable examples bind one raw
// value and use that same value both for syntax-highlighted source and eval,
// so the displayed code is exactly what produced the preview.

// A small deterministic site used only as the `documents()` input while
// evaluating the guide's listing example.
#let guide-documents() = (
  (
    title: [Rewriting My Blog],
    date: datetime(year: 2026, month: 4, day: 12),
    description: [It used to be written in Markdown.],
    url: "#rewriting-my-blog",
    kind: "post",
    draft: false,
    extra: (color: "blue"),
  ),
  (
    title: [Oops, I Vibecoded My Blog],
    date: datetime(year: 2026, month: 4, day: 7),
    description: [Where did all those em dashes come from?],
    url: "#oops-i-vibecoded-my-blog",
    kind: "post",
    draft: false,
    extra: (color: "red"),
  ),
  (
    title: [How to Bake Bread],
    date: datetime(year: 2026, month: 4, day: 3),
    description: [This recipe I learned from a talking rat.],
    url: "#how-to-bake-bread",
    kind: "post",
    draft: false,
    extra: (color: "green"),
  ),
)

#let project-example(
  ..sources,
  result: auto,
  result-label: "Rendered page",
  class: "",
) = if "twyla-version" in dictionary(sys) {
  sources = sources.pos()
  if result == auto {
    result = []
    for s in sources {
      if s.func() == raw and (s.lang == "example" or s.lang == "typst") {
        result += eval(
          s.text,
          mode: "markup",
          scope: (documents: guide-documents),
        )
      }
      if s.func() == raw and s.lang == "sass" {
        result += [ #context html.style(asset.sass(
          bytes(s.text),
          format: "sass",
        ).read()) ]
      }
    }
  }
  html.div(class: "example-workspace journey-workspace " + class, [
    #show heading.where(level: 3): body => html.div(
      class: "journey-panel-bar",
      html.span(class: "panel-meta", body.body)
    )
    #html.div(class: "journey-source", {
      for (i, s) in sources.enumerate() {
        if s.func() == raw and s.lang != "tree" {
          html.div(class: "journey-code", s)
        } else {
          s
        }
        if i < sources.len() - 1 {
          html.div(class: "journey-source-sep")
        }
      }
    })
    #html.div(class: "journey-result", {
      context html.div(
        class: "journey-browser " + class,
        html.iframe(src: document(output: auto)[
          #result
        ].url()),
      )
    })
  ])
} else {
  sources = sources.pos()
  for (i, s) in sources.enumerate() {
    s
    if s.func() == raw and i < sources.len() - 1 {
      html.hr()
    }
  }
}
