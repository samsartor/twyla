<h1>Twyla</h1>

Twyla is a static site generator (SSG) similar to [Hugo](https://gohugo.io/) or [Zola](https://www.getzola.org), but built entirely around the [Typst](https://typst.app) typesetting and scripting language.

Content is written in Typst. Your templates are written in Typst. Your themes are written in Typst (and in [SASS](https://sass-lang.com/)). Everything is Typst! Except Twyla itself, which is written in Rust.

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

<hr />

#### content/main.typ

```example
#set document(title: "My Blog")

= My Blog

I make bread!
```

If you `twyla serve` and point your browser at [http://localhost:1111](http://localhost:1111) you will see your `main.typ` as HTML. Go ahead and add something, possibly your name? The webpage reloads automatically.

To deploy your website, run `twyla build --base-url https://example.com` and copy the `public` dir to the provider of your choice.

Additional pages are additional files:

```tree
content/
├ main.typ
├ rewriting-my-blog.typ
├ oops-i-vibecoded-my-blog.typ
└ how-to-bake-bread.typ
```

<hr />

#### content/main.typ

```example
= My Blog

I make bread! And wrote these posts:

#context for doc in documents() {
  if not doc.draft and doc.kind == "post" [
    == #link(doc.url, doc.title)
    #doc.date.display()

    #doc.description
  ]
}
```

For a blog you will probably want to list your other pages on your home page. To do that, use the [documents()](https://twyla.dev/reference/#documents) iterator:

You can see Twyla’s main idea in action: _there is no configuration, only code._ Typst is a real programming language. If you want to create sidebars, listings, tags, “recents”, whatever … well that is what for loops are for!

Before you get carried away, please include basic information like `title` and `date`:

```example
#set document(
  title: "Rewriting My Blog",
  date: datetime(year: 2026, month: 4, day: 12),
  description: [
    My blog was written in normal everyday Markdown, but as a
    tech hipster I found that unacceptable...
  ],
  extra: (
    color: blue,
  ),
)
```

Twyla’s [document](https://twyla.dev/reference#document) function supports a number of additional features, beyond what are available in normal Typst, including an `extra` field you can fill with whatever data you want.

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

#### sass/main.sass

```sass
.post-header
  display: flex
  align-items: end
  border-bottom: 1px solid grey

.post-title
  flex-grow: 1
  font-size: 1.2em

.post-date
  color: grey
```

<hr />

#### content/main.typ

```example
#context for doc in documents() {
  if not doc.draft and doc.kind == "post" [
    #html.div({
      html.div(
        [#doc.title],
        class: "post-title",
        style: "color: " + doc.extra.color,
      )
      html.div(
        [#doc.date.display()],
        class: "post-date",
      )
    }, class: "post-header")

    #doc.description
  ]
}
```

If you would like to include an image, you can use either the built-in image function or handle it explicitly with Twyla’s [asset system](https://twyla.dev/reference/assets):

```example
Don't talk to me
#image("./audrey.jpg")

Or my son
#context html.img(src: asset.image(
  "./audrey.jpg",
  format: "webp",
  width: 32,
).url())

#html.style("img {
  max-width: 256px;
  width: auto;
  height: auto;
}")
```

Assets are pretty powerful, you can use them to do all kinds of stuff!

```example
#let icon = context html.img(
  src: asset.file("icon.svg").url(),
  style: "height: 1.5em; baseline-shift: bottom;"
)

Check out this pretty icon: #icon

#figure(
  context raw-html(asset.typst(
    "./_diagram.typ",
    format: "svg",
  ).read()),
  caption: [File as an inline diagram],
)

#figure(
  context block(html.img(
    src: asset.typst([
      #set page(width: auto, height: auto, margin: 2mm)
      #circle(fill: red, stroke: 1mm + blue)
    ], format: "svg").url(),
    style: "width: 5em; margin: auto",
  )),
  caption: [Link to this circle],
)
```

Ok, so what is happening there in that last example? Twyla is rendering a `circle()` as an SVG, writing that SVG to a generated path in your `public/` folder, and then providing the URL to reference as the src of an image _OR_ inline as an `<svg>` element.

You can find an even cooler example here on Twyla’s own website. See the little fireplace tongs we use as an icon? Those are drawn procedurally in a [Cetz](https://cetz-package.github.io) canvas, and then rendered by Twyla as part of the theme, in order to generate the actual favicon URL!

## Customization

Twyla customization and theming is mainly accomplished using Typst’s usual [show rules](https://typst.app/docs/reference/styling#show-rules). For example, you can replace Twyla’s default theme and build your own HTML from scratch:

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

Again, notice the uses of `asset.url()`. Twyla’s [asset system](/reference/asset) can automatically convert assets such as SASS to CSS (using the [grass](https://github.com/connorskees/grass) library) and Typst _content_ into PNG/SVG/PDF (using Typst itself). Soon we’ll include TS to JS conversion using [rolldown](https://rolldown.rs) as well.

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

<hr />

```typst
#import "/templates/post.typ": post-template
#set document(
  title: "How to Bake Bread",
  description: "The recipe for bread I learned from that talking rat",
)

#show: post-template.with(theme-color: blue)

== Step 1: Finding Grinded Wheat
...
```

Twyla themes can also be arbitrary typst packages. Conventionally, a Twyla theme should expose `KIND-template` functions and/or `KIND-default` constants for each supported page `KIND` (eg `"page"`, `"root"`, `"dir"`, `"draft"`).

## Migrating

Twyla provides a pretty capable `twyla convert` utility for existing Hugo/Zola sites. It will attempt to parse every `*.md` file in your existing website and create a sibling `*.typ` file. Once done, your site gets compiled but instead of overwriting `public/` like `twyla build`, the `twyla convert
--verify` command will **display a diff against the existing HTML there**.

You can iterate on the Typst version of your source personally: replacing placeholders, writing templates, recreating your theme, etc until the diff is small enough that you are satisfied. Or alternatively, you can download [TWYLA\_SKILL.md](https://raw.githubusercontent.com/samsartor/twyla/refs/heads/main/TWYLA_SKILL.md) and throw your agent of choice at the problem.

Feel free to simultaneously iterate on the `twyla convert` utility itself too, especially if you use a currently-unsupported SSG (or unusual features thereof). Each site converted so far has required quite a bit of this (mainly via Claude), but we hope there will be less of it each time.
