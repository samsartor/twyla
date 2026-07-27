#import "@preview/cetz:0.5.2"

#set page(
  width: 150mm,
  height: 88mm,
  margin: 0mm,
  fill: rgb("f4f0e4"),
)
#set text(font: "Iosevka Aile", size: 8pt, fill: rgb("59615b"))

#let ink = rgb("252824")
#let teal = rgb("167d81")
#let cyan = rgb("65cdd0")
#let copper = rgb("e69361")
#let grid-color = rgb("d6d2c8")

// A damped signal: enough structure to demonstrate that the illustration is
// computed, while remaining a decorative example rather than a data claim.
#let samples = range(0, 49).map(i => {
  let x = i / 4
  let y = calc.sin(x * 62deg) * calc.exp(-x / 8)
  (x, y)
})

#align(center + horizon, cetz.canvas({
  import cetz.draw: *

  let left = 1.4
  let bottom = 1.15
  let width = 11.5
  let height = 4.8
  let map-point(point) = (
    left + point.at(0) / 12 * width,
    bottom + (point.at(1) + 1) / 2 * height,
  )
  let points = samples.map(map-point)
  let baseline = bottom + height / 2

  // Quiet graph-paper grid.
  for i in range(0, 7) {
    let y = bottom + i * height / 6
    line(
      (left, y),
      (left + width, y),
      stroke: (paint: grid-color, thickness: .45pt),
    )
  }
  for i in range(0, 13) {
    let x = left + i * width / 12
    line(
      (x, bottom),
      (x, bottom + height),
      stroke: (paint: grid-color, thickness: .45pt),
    )
  }

  // A translucent area and its computed curve.
  line(
    (left, baseline),
    ..points,
    (left + width, baseline),
    close: true,
    fill: gradient.linear(
      cyan.lighten(35%),
      copper.lighten(34%),
      relative: "parent",
      space: oklch,
    ),
    stroke: none,
  )
  line(
    ..points,
    stroke: (paint: teal, thickness: 1.7pt, cap: "round", join: "round"),
  )

  // Mark every sixth computed sample.
  for (i, point) in points.enumerate() {
    if calc.rem(i, 6) == 0 {
      circle(
        point,
        radius: .095,
        fill: if i == 0 { copper } else { cyan },
        stroke: (paint: ink, thickness: .6pt),
      )
    }
  }

  // Axes, labels, and a compact title block.
  line(
    (left, baseline),
    (left + width + .2, baseline),
    stroke: (paint: ink, thickness: .8pt),
  )
  line(
    (left, bottom - .1),
    (left, bottom + height + .15),
    stroke: (paint: ink, thickness: .8pt),
  )

  content(
    (left, bottom + height + .7),
    text(size: 14pt, weight: 650, fill: ink)[A small signal],
    anchor: "west",
  )
  content(
    (left, bottom + height + .28),
    text(size: 7pt, fill: teal)[computed in Typst · drawn with Cetz],
    anchor: "west",
  )
  content(
    (left + width, bottom - .42),
    text(size: 7pt)[input →],
    anchor: "east",
  )
  content(
    (left - .35, bottom + height),
    text(size: 7pt)[output],
    anchor: "south",
  )
}))
