#import "/templates/slides.typ": *

#set document(kind: "slides")
#show: slides-template

#slide[
  = Twyla

  An SSG where everything is Typst.  
]

#slide(n: 3)[
  = Zola

  #context if frame.get() == 1 [
    My website was written in Zola.

    #image("presentation/samsartorcom_color_screenshot.png")
  ]
  #context if frame.get() >= 2 [
    ... which means it was written in markdown

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
    ... and in Tera templates

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

#slide[
  = Math Sucks (in Markdown)
]

#slide[
  = LaTeX
]

#slide[
  = Programming Sucks (in LaTeX)
]

#slide[
  = Typst
]

#slide(n: 4, i => [
  = Typst HTML Export

  #image("presentation/html_history" + str(i) + ".png")
])

#slide(n: 8, i => [
  #side-by-side[
    = Markdown

    - My website
    #if i <= 2 [
      #image("presentation/samsartorcom_color_screenshot.png")
    ]
  ][
    = LaTeX
    - All my research papers
    #if i <= 1 [
      - Some notes
    ]
  ][
    = Typst
    #if i == 1 [
      - Nothing! Just found out about it
    ]
    #if i >= 2 [
      - Some notes
    ]
    #if i >= 3 [
      - A bunch of diagrams
    ]
    #if i >= 4 [
      - My resume (on my website)
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
      - My entire dissertation
    ]
    #if i >= 8 [
      - All my research papers
    ]
  ]
])
