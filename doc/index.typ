#import "@preview/frame-it:2.0.0": *
#import "@preview/dtree:0.1.1": dtree

#let note = frame("Note", blue)

#show: frame-style(styles.hint)

= Overview

Twyla is a static site generator (SSG) similar to
#link("https://www.getzola.org")[Zola] but built entirely around the
#link("https://typst.app")[Typst] typesetting and scripting language.

Content is written in Typst. Your templates are written in Typst. Your themes
are written in Typst (and in #link("https://sass-lang.com/")[SASS]). Everything
is Typst! Except Twyla itself, which is written in Rust.

For an example, check out my own personal website:
/ Source: https://gitlab.com/samsartor/site
/ Live: https://samsartor.com

#note[
  Twyla is still in early development, and mostly vibe-coded. Use for your personal blog, not
  your company homepage.
]

= Installing

Your best option (for now) is to compile Twyla from source:
```
cargo install https://github.com/samsartor/twyla
```

= Getting Started

All you need to start using twyla is a single file!
#dtree(```
content/
  main.typ
```)

If you `twyla serve` and point your browser at http://localhost:1111
you will see your `main.typ` as HTML. Go ahead and add something,
possibly your name? The webpage reloads automatically.

To deploy your website, simply run `twyla build --base-url https://example.com`
and copy the `public` dir to the provider of your choice.

Additional pages are just additional files:
#dtree(```
content/
  main.typ
  rewriting-my-blog.typ
  oops-i-vibecoded-my-blog.typ
  how-to-bake-bread.typ
```)

For a blog you will probably want to list your other pages on your home page.
To do that, use the #link("/reference/pages")[pages] iterator:
```typst
= My Blog

#for page in pages {
  if !page.draft [
    == #link(page.url, page.title)
    #page.date.display()
    
    #page.description    
  ]
}

== Other Stuff

I make cheese!
```

Your other pages should include basic information like `title`, `data`, `draft`
status, and a `description` like so:
```typst
#set document(
  title: "Rewriting My Blog",
  date: datetime(year: 2026, month: 4, day: 12),
  draft: false,
  description: [
    My blog was written in normal everyday Markdown, but as a
    tech hipster I found that unacceptable...
  ],
)
```

Twyla's #link("reference/document")[document] function supports a number of additional features,
beyond what are available in normal Typst, including an `extra` field you can fill with whatever
data you want.

For the purpose of theming you can also add a SCSS file and #link("https://typst.app/docs/reference/html")[some HTML]:
#dtree(```
content/
  main.typ
  rewriting-my-blog.typ
  oops-i-vibecoded-my-blog.typ
  how-to-bake-bread.typ
sass/
  main.sass
```)

#line(length: 100%)

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

#line(length: 100%)

```typst
#for page in pages {
  if !page.draft [
    html.div(
      html.div(
        [#page.title],
        class: "post-title",
        style: "color: " + page.extra.color,
      ),
      html.div(    
        [#page.date.display()],
        class: "post-date",
      ),
      class: "post-header",
    )
    
    #page.description
  ]
}
```

If you would like to include an image, you can use either the built-in image
function or handle it explicitly with Twyla's #link("/reference/asset")[asset
system]:
```typst
#image("./pretty.png")

#html.img(src: asset("./pretty.png", format: "webp", resize: 1024).url)
```

Assets are pretty powerful, you can use them to do all kinds of stuff!

```typst
Check out this pretty #html.img(src: asset("icon.svg").data-url) icon.

#figure(
  raw-html(asset("./_diagram.typ", format: "svg").content),
  caption: [A diagram of some sort],
)

#figure(
  html.img(src: asset(circle(), format: "svg").url, style: "width: 100%"),
  caption: [A big cirle],
)
```

If you would like to port your existing website over to Twyla,
you can follow the guide provided by `typst init`, or download the
#link("https://raw.githubusercontent.com/samsartor/twyla/refs/heads/main/TWYLA_S
KILL.md")[`TWYLA_SKILL.md`] and throw your agent of choice at the problem.

= Customization

Twyla customization and theming is mainly accomplished using Typt's usual
#link("https://typst.app/docs/reference/styling#show-rules")[show rules]. For
example, you can replace Twyla's default theme and build your own HTML from
scratch:

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
        content: if pages.current.description == none { "" }
          else { pages.current.description },
      ))
      html.elem("title", pages.current.title)
      html.script(src: asset("/scripts/main.ts").url, "")
      html.link(rel: "stylesheet", href: asset("/sass/main.sass").url)
      html.link(rel: "icon", href: asset(circle(fill: blue), format: "png").url)
    })
    html.elem("body", body)
  })
}

= My Blog
...
```

Notice the uses of `asset(...).url`. Twyla's #link("/reference/asset")[asset
system] can automatically convert assets such as SASS to CSS (using
the #link("https://github.com/connorskees/grass")[grass] library),
TS to JS (using #link("https://rolldown.rs")[rolldown]), and Typst
_content_ into PNG/SVG/PDF (using Typst itself). Such transformations
can also be disabled with `asset(transform: none, ...)` or customized in
#link("/reference/configuration")[`Twyla.toml`].

Instead of stylizing every post separately, you probably want to create
a common set of templates:
#dtree(```
content/
  main.typ
  rewriting-my-blog.typ
  oops-i-vibecoded-my-blog.typ
  how-to-bake-bread.typ
sass/
  main.sass
templates/
  base.typ
  home.typ
  post.typ
```)
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

As a shorthand for all pages, you can also add the theme to your `Twyla.toml`:
```toml
theme: "/templates/post.typ"
theme-show: "post-template.with(theme-color: blue)"
```
