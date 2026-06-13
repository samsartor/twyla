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
tags, "recents", your own personal #strike[Twitter] #strike[X] BlueSky clone ...
well that is what a for loop is good for!

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

```typst
#image("./pretty.png")

#context html.img(src: asset.image("./pretty.png", format: "webp", resize: 1024).url)
```

#quote(block: true)[
This is aspirational, and some features documented here,
like `asset.image` do not yet exist. If you want to help
out, message #link("https://samsartor.com/")[Sam] or
#link("https://sumnerevans.com/")[Sumner].
]

Assets are pretty powerful, you can use them to do all kinds of stuff!

```typst
Check out this pretty #html.img(src: asset("icon.svg").data-url) icon.

#figure(
  context raw-html(asset("./_diagram.typ", format: "svg").content),
  caption: [A diagram of some sort],
)

#figure(
  context html.img(src: asset(circle(), format: "svg").url, style: "width: 100%"),
  caption: [A big cirle],
)
```

= Customization
<customization>
Twyla customization and theming is mainly accomplished using Typt's
usual
#link("https://typst.app/docs/reference/styling#show-rules")[show rules];.
For example, you can replace Twyla's default theme and build your own
HTML from scratch:

```typst
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
      html.elem("meta", attrs: (
        name: "description",
        content: if document.description == none { "" }
          else { document.description },
      ))
      html.elem("title", document.title)
      html.script(src: asset.rolldown("/scripts/main.ts").url(), "")
      html.link(rel: "stylesheet", href: asset.sass("/sass/main.sass").url())
      html.link(rel: "icon", href: asset.image(circle(fill: blue), format: "png").url())
    })
    html.elem("body", body)
  })
}

= My Blog
...
```

Notice the uses of `asset.url()`. Twyla's
#link("/reference/asset")[asset system] can automatically convert assets
such as SASS to CSS (using the
#link("https://github.com/connorskees/grass")[grass] library), TS to JS
(using #link("https://rolldown.rs")[rolldown];), and Typst
#emph[content] into PNG/SVG/PDF (using Typst itself).

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
#link("https://raw.githubusercontent.com/samsartor/twyla/refs/heads/main/TWYLA_S
KILL.md")[`TWYLA_SKILL.md`] and throw your agent of choice at the problem.

Feel free to simultaneously iterate on the `twyla convert` utility itself too,
especially if you use a currently-unsupported SSG (or unusual features thereof).
Each site converted so far has required quite a bit of this (mainly via Claude),
but we hope there will be less of it each time.
