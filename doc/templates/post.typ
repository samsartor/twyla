#import "@preview/frame-it:2.0.0": *

#let post-template(body, theme-color: blue) = [
  #show: frame-style(styles.hint)
  #show heading.where(level: 2): body => html.h2({
    html.span(sym.bullet, style: "margin-right: 0.5em; color: " + theme-color.to-hex())
    body.body
  })

  = #context document.title

  #context frame("Note", theme-color)(document.description)

  #body
]
