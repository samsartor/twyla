#import "/templates/journey.typ": *

#let horizontalrule = context { if target() == "html" { html.hr() } else { line(length: 100%) } }

<twyla>
Twyla is a static site generator (SSG) similar to #link("https://gohugo.io/")[Hugo] or
#link("https://www.getzola.org")[Zola], but built entirely around the
#link("https://typst.app")[Typst] typesetting and scripting language.

Content is written in Typst. Your templates are written in Typst. Your
themes are written in Typst (and in
#link("https://sass-lang.com/")[SASS];). Everything is Typst! Except
Twyla itself, which is written in Rust.

For an example, check out my own
#link("https://samsartor.com")[personal website]
(#link("https://gitlab.com/samsartor/site")[Source];).

#quote(block: true)[
Twyla is still in early development, and mostly vibe-coded. Use for your
personal blog, not your company homepage.
]

= Installing
<installing>
Your best option (for now) is to compile Twyla from source:

```
cargo install --git https://github.com/samsartor/twyla
```

= Getting Started
<getting-started>
We will grow one small site from a single unstyled file into a complete,
programmable theme. Each step changes only a little; the preview shows the
cumulative result.

#journey-step(
  "01",
  title: [Start with one file],
  kicker: [A page],
  files: ("content/main.typ",),
  code: [
```example
#set document(title: "Field Notes")

= Things worth keeping

Hello! This is my new site.
```
  ],
  preview: preview-one,
)[
  A Twyla site needs no configuration file. Run `twyla serve`, open
  #link("http://localhost:1111")[localhost:1111], and this Typst document is
  already HTML—with live reload while you write.
]

#journey-step(
  "02",
  title: [Let the site grow],
  kicker: [More pages],
  files: (
    "content/rewriting-my-blog.typ",
    "content/oops-i-vibecoded-my-blog.typ",
    "content/how-to-bake-bread.typ",
  ),
  code: [
```tree
content/
├ main.typ
├ rewriting-my-blog.typ
├ oops-i-vibecoded-my-blog.typ
└ how-to-bake-bread.typ
```
  ],
  preview: preview-pages,
)[
  Additional pages are just additional files. Twyla maps the content tree to
  clean URLs, so the project structure remains obvious as the site expands.
]

#journey-step(
  "03",
  title: [Turn pages into data],
  kicker: [Metadata + listings],
  files: ("content/main.typ", "content/rewriting-my-blog.typ"),
  code: [
```example
= Recent notes

#context for doc in documents() {
  if not doc.draft and doc.kind == "post" [
    == #link(doc.url, doc.title)
    #doc.date.display()

    #doc.description
  ]
}
```
  ],
  preview: preview-listing,
)[
  Add `title`, `date`, and `description` with
  #link("https://twyla.dev/reference#document")[document()], then use the
  #link("https://twyla.dev/reference/#documents")[documents()] iterator to
  build the home page. There is no listing configuration—just a for loop.
]

#journey-step(
  "04",
  title: [Give it a visual system],
  kicker: [Templates + Sass],
  files: ("templates/page.typ", "sass/main.sass"),
  code: [
```sass
.post
  display: grid
  grid-template-columns: 2rem 1fr auto
  gap: 1rem
  padding: 1rem 0
  border-top: 1px solid #d8d2c5

.post-date
  color: #a9562e
```
  ],
  preview: preview-styled,
)[
  Typst show rules control the markup; Sass controls its presentation. Both
  are ordinary source files, so a theme can be as small or as ambitious as
  the site needs.
]

#journey-step(
  "05",
  title: [Bring in real assets],
  kicker: [Images],
  files: ("content/audrey.jpg", "content/audrey.typ"),
  code: [
```example
#image("./audrey.jpg")

#context html.img(
  src: asset.image(
    "./audrey.jpg",
    format: "webp",
    width: 512,
  ).url(),
)
```
  ],
  preview: preview-image,
)[
  Use Typst's built-in image function, or ask Twyla's
  #link("https://twyla.dev/reference/assets")[asset system] to resize and
  convert the source. The generated URL is fingerprinted and ready to publish.
]

#journey-step(
  "06",
  title: [Make the theme programmable],
  kicker: [Generated artwork],
  files: ("grabber.typ", "templates/theme.typ"),
  code: [
```example
#import "/grabber.typ": grabber-canvas

#context raw-html(
  asset.typst(
    grabber-canvas(phi: 35deg),
    format: "svg",
  ).read(),
)
```
  ],
  preview: preview-advanced,
)[
  Assets do not have to start as files. These fireplace tongs are drawn
  procedurally with #link("https://cetz-package.github.io")[Cetz], rendered
  by Typst, and emitted by Twyla as SVG. The theme really is part of the
  program.
]

#html.div(class: "journey-finish", [
  #html.span(class: "finish-prompt", "$")
  `twyla build --base-url https://example.com`
  #html.span(class: "finish-result", [Your complete static site is in `public/`.])
])

= Customization
<customization>
Twyla customization and theming is mainly accomplished using Typst's
usual
#link("https://typst.app/docs/reference/styling#show-rules")[show rules];.
For example, you can replace Twyla's default theme and build your own
HTML from scratch:

```example
#show: body => {
  set smartquote(enabled: true)
  set raw(theme: "/templates/Dracula.tmTheme")

  show heading.where(level: 1): it => {
    html.h1(smallcaps(it.body), class: "title")
  }  

  html.elem("html", attrs: (lang: "en"), {
    html.elem("head", {
      html.elem("meta", attrs: (charset: "UTF-8"))
      html.elem("meta", attrs: (
        name: "viewport",
        content: "width=device-width, initial-scale=1",
      ))
      context html.elem("meta", attrs: (
        name: "description",
        content: if document.description == none { "" }
          else { plain-text(document.description) },
      ))
      context html.elem("title", document.title)
      context html.link(rel: "stylesheet", href: asset.sass("/sass/main.sass").url())
      context html.link(rel: "icon", href: asset.typst(circle(fill: blue), format: "png").url())
    })
    html.elem("body", body)
  })
}

= My Blog
...
```

Again, notice the uses of `asset.url()`. Twyla's #link("/reference/asset")[asset
system] can automatically convert assets such as SASS to CSS (using the
#link("https://github.com/connorskees/grass")[grass] library) and Typst
#emph[content] into PNG/SVG/PDF (using Typst itself). Soon we'll include TS to
JS conversion using #link("https://rolldown.rs")[rolldown] as well.

Instead of stylizing every post separately, you probably want to create
a common set of templates:

```tree
content/
├ main.typ
├ rewriting-my-blog.typ
├ oops-i-vibecoded-my-blog.typ
└ how-to-bake-bread.typ
sass/
└ main.sass
templates/
├ base.typ
├ home.typ
└ post.typ
```

```typst
#import "/templates/post.typ": post-template

#show: post-template.with(theme-color: blue)
#set document(
  title: "How to Bake Bread",
  description: "The recipe for bread I learned from that talking rat",
)

== Step 1: Finding Grinded Wheat
...
```

Twyla themes can also be arbitrary typst packages. Conventionally, a Twyla theme
should expose `KIND-template` functions and/or `KIND-default` constants for each
supported page `KIND` (eg `"page"`, `"root"`, `"dir"`, `"draft"`).

= Migrating

Twyla provides a pretty capable `twyla convert` utility for existing
Hugo/Zola sites. It will attempt to parse every `*.md` file in your existing
website and create a sibling `*.typ` file. Once done, your site gets compiled
but instead of overwriting `public/` like `twyla build`, the `twyla convert
--verify` command will #strong[display a diff against the existing HTML there].

You can iterate on the Typst version of your source personally: replacing
placeholders, writing templates, recreating your theme, etc until the diff
is small enough that you are satisfied. Or alternatively, you can download
#link("https://raw.githubusercontent.com/samsartor/twyla/refs/heads/main/TWYLA_SKILL.md")[TWYLA_SKILL.md]
and throw your agent of choice at the problem.

Feel free to simultaneously iterate on the `twyla convert` utility itself too,
especially if you use a currently-unsupported SSG (or unusual features thereof).
Each site converted so far has required quite a bit of this (mainly via Claude),
but we hope there will be less of it each time.
