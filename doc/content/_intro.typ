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
cargo install https://github.com/samsartor/twyla
```

= Getting Started
<getting-started>
All you need to start using twyla is a single file!

```tree
content/
└ main.typ
```

If you `twyla serve` and point your browser at
#link("http://localhost:1111") you will see your `main.typ` as HTML. Go
ahead and add something, possibly your name? The webpage reloads
automatically.

To deploy your website, simply run
`twyla build --base-url https://example.com` and copy the `public` dir
to the provider of your choice.

Additional pages are just additional files:

```tree
content/
├ main.typ
├ rewriting-my-blog.typ
├ oops-i-vibecoded-my-blog.typ
└ how-to-bake-bread.typ
```

For a blog you will probably want to list your other pages on your home
page. To do that, use the
#link("https://twyla.dev/reference/#documents")[documents()] iterator:

```example
= My Blog

#context for doc in documents() {
  if not doc.draft and doc.kind == "post" [
    == #link(doc.url, doc.title)
    #doc.date.display()
    
    #doc.description    
  ]
}

== Other Stuff

I make bread!
```

You can see Twyla's main idea in action: _there is no configuration, only code._
Typst is a real programming language. If you want to create sidebars, listings,
tags, "recents", whatever ... well that is what for loops are for!

Before you get carried away, please include basic information like `title` and `date`:

```example
#set document(
  title: "Rewriting My Blog",
  date: datetime(year: 2026, month: 4, day: 12),
  description: [
    My blog was written in normal everyday Markdown, but as a
    tech hipster I found that unacceptable...
  ],
)
```

Twyla's #link("https://twyla.dev/reference#document")[document] function
supports a number of additional features, beyond what are available in normal
Typst, including an `extra` field you can fill with whatever data you want.

For the purpose of theming you can also add a SCSS file and
#link("https://typst.app/docs/reference/html")[some HTML];:

```tree
content/
├ main.typ
├ rewriting-my-blog.typ
├ oops-i-vibecoded-my-blog.typ
└ how-to-bake-bread.typ
sass/
└ main.sass
```

#horizontalrule

```sass
.post-header
  display: flex
  border: 1px black

.post-title
  flex: grow
  size: 2em

.post-date:
  color: grey  
```

#horizontalrule

```example
#context for doc in documents() {
  if not doc.draft and doc.kind == "post" [
    html.div(
      html.div(
        [#doc.title],
        class: "post-title",
        style: "color: " + doc.extra.color,
      ),
      html.div(    
        [#doc.date.display()],
        class: "post-date",
      ),
      class: "post-header",
    )
    
    #doc.description
  ]
}
```

If you would like to include an image, you can use either the built-in
image function or handle it explicitly with Twyla's
#link("https://twyla.dev/reference/assets")[asset system];:

```example
#image("./audrey.jpg")

#context html.img(src: asset.image("./audrey.jpg", format: "webp", width: 512).url())
```

Assets are pretty powerful, you can use them to do all kinds of stuff!

```example

Check out this pretty #context html.img(src: asset.file("icon.svg").url()) icon.

#figure(
  context raw-html(asset.typst("./_diagram.typ", format: "svg").read()),
  caption: [A diagram of some sort],
)

#figure(
  context html.img(src: asset.typst(circle(), format: "svg").url(), style: "width: 100%"),
  caption: [A big cirle],
)
```

Ok, so what is happening there in that last example? Twyla is rendering a
`circle()` as an SVG, writing that SVG to a generated path in your `public/`
folder, and then providing the URL to reference as the src of an image _OR_
inline as an `<svg>` element.

You can find an even cooler example here on Twyla's own website. See the
little fireplace tongs we use as an icon? Those are drawn procedurally in a
#link("https://cetz-package.github.io")[Cetz] canvas, and then rendered by Twyla
as part of the theme, in order to generate the actual favicon URL!

= Customization
<customization>
Twyla customization and theming is mainly accomplished using Typt's
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
