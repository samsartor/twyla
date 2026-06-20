#import "/templates/slides.typ": *
#import "@preview/cmarker:0.1.8"
#import "@preview/metalogo:1.2.0"
#import "/templates/theme.typ": hero

#let LaTeX = html.span(html.frame(metalogo.LaTeX), style: "display: inline-block")
#let TeX = html.span(html.frame(metalogo.TeX), style: "display: inline-block")

#set document(kind: "slides")
#show: slides-template

#slide[
  #hero

  #html.div(class: "centering")[
    Sam Sartor

    Show & Tell -- June 20th, 2026
  ]
]

#slide(n: 3)[
  = Zola

  #context if frame.get() == 1 [
    My website was built using Zola.

    #image("presentation/samsartorcom_color_screenshot.png")
  ]
  #context if frame.get() >= 2 [
    ... which means it was written in Markdown

    ```md
      +++
      title = "What is color?"
      date = 2024-09-27
      +++

      # Physics
      > **Color is a property of light**

      {{ svg(asset="what-is-color/electromagnetic_spectrum.svg") }}
      For example, light with a wavelength of <col-s>460nm</col-s> is blue.
    ```
  ]
  #context if frame.get() >= 3 [
    ... and templated/themed using Tera

    ```html
    <div class="image --{{ size }}">
        {{ load_data(path=asset) | safe }}
        {%- if caption -%}
            <div class="caption" align="center">
            {{ caption }}
            </div>
        {%- endif -%}
    </div>
    ```
  ]
]

#slide(n: 3)[
  = Twyla

  #context if frame.get() == 1 [
    Now my website is built using Twyla

    #image("presentation/samsartorcom_color_screenshot.png")
  ]
  #context if frame.get() >= 2 [
    ... which means it is written in Typst

    ```typst
    #set document(
      title: "What is color?",
      date: datetime(year: 2024, month: 9, day: 27),
    )

    = Physics <physics>

    #html.blockquote[*Color is a property of light.*]

    #svg(asset.file("electromagnetic_spectrum.svg"))
    For example, light with a wavelength of #cols[460nm] is blue.
    ```
  ]
  #context if frame.get() >= 3 [
    ... and templated/themed using Typst

    ```typst
    #let svg(asset, size: "m", caption: none) = html.div(
      class: "image --" + size,
      {
        context raw-html(asset.read())
        if caption != none {
          html.div(class: "caption", align: "center", caption)
        }
     },
    )
    ```
  ]
]

#slide[
  = Markdown

  Created in 2004 as a Perl script by Aaron Swartz and John Gruber,
  as an alternative to writing HTML for the blog https://daringfireball.net.

  #let mdexample = ```markdown
  # Heading

  _italic_ **bold** `monospace`

  [link](example.com)

  > Some sorta quote

  - this
    - is a
  - list

  <ul>Some HTML also works</ul>

  |   I    | Think |
  | ------ | ----- |
  | Tables | Suck  |
  ```
  #side-by-side[
    #mdexample
  ][
    #html.div(cmarker.render(
      mdexample.text,
      html: (ul: (a, b) => underline(b)),
    ), class: "rendered")
  ]
]

#slide[
  = Math Sucks (in Markdown)

  #side-by-side[
    #image("presentation/mathfail_no.png")
  ][
    #image("presentation/mathfail_yes.png")
    #html.div(image("presentation/noscript_logo.png"), style: "width: 128px; margin: auto")
  ]
]

#slide[
  = #LaTeX

  #TeX was developed by Donald Knuth in 1978 to typset "The Art of Computer Programming".
  Leslie Lamport packaged #TeX together with his own macros as #LaTeX in the 1980s.

  #image("presentation/artofprogramming_screenshot.png")
]

#slide(n: 2)[
  = Programming Sucks (in #LaTeX)

  ```latex
    \newenvironment{rSection}[2]{
      {\large\noindent\ignorespaces \textbf{#1}}
      \ifthenelse{\equal{\detokenize{#1}}{\detokenize{PROJECTS}}}
      {\href{https://#2}{\large\faGithub \textbf{ GitHub:\hspace{2pt}#2}}} % TRUE
      {{}} % FALSE
       \ifthenelse{\equal{\detokenize{#1}}{\detokenize{INTERESTS}}}
      {\href{https://#2}{\large\textbf{#2}}} % TRUE
      {{}} % FALSE
      \large\noindent\ignorespaces
    }{\noindent\ignorespacesafterend}
  ```

  #context if frame.get() == 2 [ 
    ```typst
      #let r-section(type: str, address: str, body) = [
        #set par(first-line-indent: 0em)

        #text(size: 12pt)[*#type*]
        #if type == "PROJECTS" {
          link("https://" + address)[#fa-icon("github") *GitHub: #h(2pt)#address*]
        } else if type == "INTERESTS" {
          link("https://" + address)[*#address*]
        }

        #body
      ]
    ```
  ]

  #postscript[From https://www.the-fractal-world.com/tyst-vs-latex-is-latex-just-bad/]
]

#slide[
  = Typst

  Developed since 2019 by Martin Haug and Laurenz Mädje, to write their master's
  theses. Open-source compiler, development funded by the company making a
  proprietary Overleaf competitor.

  #let fibexample = ```typst
  = Fibonacci sequence
  The Fibonacci sequence is defined through the recurrence
  relation $F_n = F_(n - 1) + F_(n - 2)$. It can also be
  expressed in _closed form:_
  $ F_n = round(1 / sqrt(5) phi.alt^n), quad
      phi.alt = (1 + sqrt(5)) / 2 $

  #let count = 8
  #let fib(n) = {
    if n <= 2 { 1 }
    else { fib(n - 1) + fib(n - 2) }
  }

  The first #count numbers are:
  #align(center, table(
    columns: count,
    ..range(count).map(n => $F_#(n+1)$),
    ..range(count).map(n => str(fib(n+1))),
  ))
  ```

  #side-by-side[
    #fibexample
  ][
    #context html.img(
      src: asset.typst(eval(
        "#set page(width: 5.5in, height: auto)\n" + fibexample.text,
        mode: "markup",
      ), format: "svg").url(),
      style: "width: 100%",
    )
  ]
]

#slide(n: 9, i => [
  #side-by-side[
    = Markdown

    #if i < 2 [
      - Some obsidian notes
    ]
    #if i < 9 [
      - My website
      #image("presentation/samsartorcom_color_screenshot.png")
    ]
  ][
    = #LaTeX
    #if i < 2 [
      - Some other notes
    ]
    - #if i < 7 [All] else [Most] of my research papers
    #image("presentation/contentawaretiles.png")
    #if i < 7 [ #image("presentation/latex_plot.png") ]
  ][
    = Typst
    #if i == 1 [
      - Nothing! We found out about it
    ]
    #if i >= 2 [
      - The new notes
    ]
    #if i == 2 [
      #image("presentation/brushverse_notes.png")
    ]
    #if i >= 3 [
      - A bunch of diagrams
    ]
    #if i == 3 [
      #image("presentation/brushverse_diagram.png")
    ]
    #if i >= 4 [
      - My resume (on my website)
    ]
    #if i == 4 [
      #image("presentation/resume_screenshot.png")
    ]
    #if i >= 5 [
      - Sumner's resume
    ]
    #if i == 5 [
      #image("presentation/sumner_resume.png")
    ]
    #if i == 6 [
      #image("presentation/the_beginning.png")
    ]
    #if i >= 7 [
      - The figures in a paper
    ]
    #if i == 7 [
      #image("presentation/overpainting_cetz_screenshot.png")
      #image("presentation/overpainting_teapot_screenshot.png")
    ]
    #if i >= 8 [
      - My entire dissertation
    ]
    #if i == 8 [
      #image("presentation/dissertation_flow_screenshot.png")
    ]
    #if i >= 9 [
      - My website
      #image("presentation/samsartorcom_color_screenshot.png")
    ]
  ]
])

/*
#slide[
  #center-thought[Maybe I really should build my website in Typst...]
]
*/

#slide[
  = #LaTeX HTML Export

  #html.div(class: "centering", style: "font-size: 3em")[Pandoc!]

  #image("presentation/pandoc-cartoon.svg")
]

#slide(n: 4, i => [
  = Typst HTML Export

  #image("presentation/html_history" + str(i) + ".png")
])

/*
#slide(n: 2)[
  = Twyla

  Created by #context if frame.get() == 1 [ Sam Sartor ] else [ #strike[Sam
  Sartor] Claude ] in 2026 because he got hit really hard in the head while
  writing his dissertation.
]
*/

#slide(n: 14, i => [
  = Twyla

  #{
    list.item[
      "Typst" + "Zola" = "Tyla" #if i >= 2 [ #sym.approx "Twyla" ]
      #if i == 2 [ #image("presentation/hogfather_twyla.jpg") ]
    ]
    if i >= 3 { list.item[Compiles all `*.typ` files in your website source
      #if i== 3 [  
      ```tree
      content/
      ├ main.typ
      ├ rewriting-my-blog.typ
      ├ oops-i-vibecoded-my-blog.typ
      └ how-to-bake-bread.typ
      sass/
      └ main.sass
      templates/
      ├ theme.typ
      └ components.typ
      ```
      ]
    ] }
    if i >= 4 { list.item[Built on the `typst-*` crates, published by the Typst compiler team
      #if i == 4 [ #image("presentation/typst_html_crate.png") ]
      #if i >= 5 { list.item[Also used by the Typst Language Server and VS Code plugin] }
      #if i == 5 [ #image("presentation/helix_lsp_workflow.png") ]
    ] }
    if i >= 6 { list.item[`twyla serve` + hot reloading] }
    if i >= 7 { list.item[Adds some stdlib features to the Typst language:
      #if i >= 8 { list.item[List all the pages/posts/documents with the `documents()` function] }
      #if i == 8 [
        ```typst
        #context for doc in documents() {
          if not doc.draft and doc.kind == "post" [
            html.div(
              html.div(
                [#doc.title],
                class: "post-title",
                style: "color: " + doc.extra.color,
              ),
              class: "post-header",
            )
    
            #doc.description
          ]
        }
        ```
      ]
      #if i >= 9 { list.item[Declare new pages inside other pages with `#document[Hello!]`] }
      #if i >= 10 { list.item[`asset.file` -- read a file, or reference it by URL] } 
      #if i >= 11 { list.item[`asset.sass` -- compile SASS/SCSS to CSS] } 
      #if i >= 12 { list.item[`asset.image` -- convert/resize images] } 
      #if i >= 13 { list.item[`asset.typst` -- invoke the Typst compiler to create images/pdfs] } 
    ] }
    if i >= 14 { list.item[`twyla convert` to port Zola/Hugo sites -> Twyla] }
  }
])

#slide[
  #section[Dumb Stuff with Twyla]
]

#slide[
  = Procedurally Generate your Favicon

  #html.img(src: "/favicon.svg")

  ```typst
  #let grabber-grad = gradient.linear(rgb("5cc6e8"), rgb("e8a76c"), relative: "parent", space: oklch)
  #html.link(rel: "icon", type: "image/x-icon", href: asset.typst([
    #set page(width: auto, height: auto, fill: none, margin: 0mm)
    #let thing = cetz.canvas({
      for s in (1, -1) {
        group({
          translate((0, s*py))
          rotate(s*phi, origin: (0, 0))
          line((0.1, 0), p0, stroke: (paint: grabber-grad, thickness: st, cap: "round"))
          ...
        })
      }
    })
    #rect(fill: black, height: auto, width: auto, inset: 5mm, radius: 20mm, thing)
  ], format: "svg", output: "/favicon.svg").url())
  ```
]

#slide[
  = Resume and Website Together

  #side-by-side[
    Common `data.typ` file:
    ```typst
    #let adobe = (
      kind: "work",
      title: [Graduate Research Scientist],
      employer: [Adobe Research],
      where: [London UK],
      start: vaugedate(year: 2026, month: "Jun"),
      text: [Ongoing work with image generation and editing diffusion models.],
    )

    #let twyla = (
      kind: "side",
      icon: "github",
      title: [Twyla],
      start: vaugedate(year: 2026, month: "May"),
      url: "https://github.com/samsartor/twyla",
      text: context [
        My own SSG like Hugo or Zola, but based on Typst.
        #if version.get() == "site" [ Used for this website! ]
      ],
    )
    ```
  ][
    Resume:
    ```typst
    #import "resume-template.typ": entries, entry, template
    #show: doc => template(
      doc,
      name: "Sam Sartor",
      email: "me@samsartor.com",
    )

    = Work
    #entries(adobe, sketchup)

    = Other
    #entries(twya, hornpipe, hypar_map, disjoint_captures)
    ```

    Homepage:
    ```typst
    #html.div(class: "cards", {
      project-card(resume.twyla)
      project-card(resume.hornpipe)
      project-card(resume.disjoint_captures)
      project-card(resume.hypar_map)
      html.div(class: "link-card", context [
        #html.a(href: asset.typst("/resume/resume.typ").url())[Resume.pdf]
        #html.a(href: asset.typst("/resume/cv.typ").url())[CV.pdf]
      ])
    }),
    ```
  ]
]

#let quine = [```typst
#let side-by-side(..pair) = pair.at(0)
#let code = QUINE
#side-by-side[
  #code
][
  #context html.img(
    src: asset.typst(
      eval(code.text, mode: "markup"),
      format: "svg",
    ).url(),
    style: "width: 100%",
  )
]
```]
#let quine_outer = (
"#let code = ```typst\n" + quine.text.replace("QUINE", "[
  #set page(width: auto, height: auto)
  = Hello, World
]") + "\n```",
  ..quine.text.split("\n").slice(2),
).join("\n")

#slide[
  = Self-Referential Slide

  #eval(
    quine_outer,
    mode: "markup",
    scope: (side-by-side: side-by-side)
  )
  /*
  #let quine_code = eval(
    quine_src,
    mode: "markup",
  )
  #side-by-side[
    #quine_code
  ][
    #eval(
      quine_code.text,
      mode: "markup",
      scope: (side-by-side: side-by-side)
    )
  ]
  */
]
