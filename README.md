> This README is aspirational, and not all features here are currently implemented. See doc/planning.typ for the current state of the project.

<h1>Twyla</h1>

Twyla is a static site generator (SSG) similar to [Zola](https://www.getzola.org) but built entirely around the [Typst](https://typst.app) typesetting and scripting language.

Content is written in Typst. Your templates are written in Typst. Your themes are written in Typst (and in [SASS](https://sass-lang.com/)). Everything is Typst! Except Twyla itself, which is written in Rust.

For an example, check out my own [personal website](https://samsartor.com) ([Source](https://gitlab.com/samsartor/site)).

> Twyla is still in early development, and mostly vibe-coded. Use for your personal blog, not your company homepage.

## Installing

Your best option (for now) is to compile Twyla from source:

```
cargo install --git https://github.com/samsartor/twyla
```

## Getting Started

All you need to start using twyla is a single file!

```tree
content/
└ main.typ
```

If you `twyla serve` and point your browser at [http://localhost:1111](http://localhost:1111) you will see your `main.typ` as HTML. Go ahead and add something, possibly your name? The webpage reloads automatically.

To deploy your website, simply run `twyla build --base-url https://example.com` and copy the `public` dir to the provider of your choice.

Additional pages are just additional files:

```tree
content/
├ main.typ
├ rewriting-my-blog.typ
├ oops-i-vibecoded-my-blog.typ
└ how-to-bake-bread.typ
```

For a blog you will probably want to list your other pages on your home page. To do that, use the [documents()](/reference/document#documents) iterator:

```typst
= My Blog

#context for doc in documents() {
  if not doc.draft and doc.kind == "page" [
    == #link(doc.url, doc.title)
    #doc.date.display()
    
    #doc.description    
  ]
}

== Other Stuff

I make bread!
```

Your other pages should include basic information like `title`, `date`, `kind`, and a `description` as so:

```typst
#set document(
  title: "Rewriting My Blog",
  date: datetime(year: 2026, month: 4, day: 12),
  description: [
    My blog was written in normal everyday Markdown, but as a
    tech hipster I found that unacceptable...
  ],
)
```

Twyla’s [document](reference/document) function supports a number of additional features, beyond what are available in normal Typst, including an `extra` field you can fill with whatever data you want.

For the purpose of theming you can also add a SCSS file and [some HTML](https://typst.app/docs/reference/html):

```tree
content/
├ main.typ
├ rewriting-my-blog.typ
├ oops-i-vibecoded-my-blog.typ
└ how-to-bake-bread.typ
sass/
└ main.sass
```

<hr />

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

<hr />

```typst
#context for doc in documents() {
  if !doc.draft and doc.kind == "post" [
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

If you would like to include an image, you can use either the built-in image function or handle it explicitly with Twyla’s [asset system](/reference/asset):

```typst
#image("./pretty.png")

#context html.img(src: asset("./pretty.png", format: "webp", resize: 1024).url)
```

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

If you would like to port your existing website over to Twyla, you can follow the guide provided by `typst init`, or download the [`TWYLA_SKILL.md`](https://raw.githubusercontent.com/samsartor/twyla/refs/heads/main/TWYLA_SKILL.md) and throw your agent of choice at the problem.

## Customization

Twyla customization and theming is mainly accomplished using Typt’s usual [show rules](https://typst.app/docs/reference/styling#show-rules). For example, you can replace Twyla’s default theme and build your own HTML from scratch:

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

Notice the uses of `asset(...).url`. Twyla’s [asset system](/reference/asset) can automatically convert assets such as SASS to CSS (using the [grass](https://github.com/connorskees/grass) library), TS to JS (using [rolldown](https://rolldown.rs)), and Typst _content_ into PNG/SVG/PDF (using Typst itself). Such transformations can also be disabled with `asset(transform: none, ...)` or customized in [`Twyla.toml`](/reference/configuration).

Instead of stylizing every post separately, you probably want to create a common set of templates:

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

As a shorthand, you can also add the theme to your `Twyla.toml`:

```toml
theme = "/templates/theme.typ"
```

Twyla themes can also be arbitrary typst packages such as `@samsartor/twyla-pickles` or `@samsartor/twyla-book`. A theme need only expose `KIND-template` functions and/or `KIND-default` constants for each supported page `KIND` (eg `"page"`, `"root"`, `"dir"`, `"draft"`).
