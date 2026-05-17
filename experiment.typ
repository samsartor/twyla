// Throwaway: pokes typst 0.14's HTML export to see what shape the
// API gives us. Not part of the real twyla — used once, then deleted.

#set document(title: "Experiment", author: "Sam Sartor")

#metadata((
  kind: "page-meta",
  date: datetime(year: 2026, month: 5, day: 17),
  tags: ("demo", "test"),
)) <page-meta>

#metadata((kind: "summary", text: "First HTML-export experiment")) <summary>

#html.div(class: "container --large")[
  #html.h1[Hello, twyla]

  #html.p[
    This is a paragraph with #html.span(class: "highlight")[inline]
    markup, plus a literal less-than #raw("<") to test escaping.
  ]

  #html.div(class: "image_container")[
    Tag-only image (no asset load): #html.img(src: "raw.svg", alt: "tag")

    Real image() call (triggers World::file): #image("teaser.svg")
  ]
]
