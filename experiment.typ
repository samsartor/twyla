// Experiment 2 — bundle mode (typst main, post-PR #7964).
//
// Verifies: multi-document output, asset emission, cross-document linking,
// per-document metadata extraction from the bundle introspector.

#set document(author: "Sam Sartor")

#document("index.html", title: "Home")[
  #metadata((kind: "page", slug: "/", date: datetime(year: 2026, month: 5, day: 17))) <home-meta>

  = Home

  This is the landing page. Jump to the #link(<blog>)[blog].
]

#document("blog/index.html", title: "Blog")[
  #metadata((kind: "page", slug: "/blog/", date: datetime(year: 2026, month: 5, day: 16))) <blog>

  = Blog

  Welcome. Back to #link(<home-meta>)[home].

  #html.div(class: "post-list")[
    A post: #link(<post-1>)[GUIs Part 2]
  ]
]

#document("blog/guis-2.html", title: "GUIs Part 2")[
  #metadata((
    kind: "post",
    slug: "/blog/guis-2/",
    title: "Trees Aren't All You Need",
    date: datetime(year: 2023, month: 2, day: 7),
  )) <post-1>

  = Trees Aren't All You Need

  Body content goes here.
]

#asset("style.css", bytes("body { font-family: Iosevka; }"))
