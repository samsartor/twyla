#import "@preview/cetz:0.5.2"

#let grabber(
  theta: -60deg,
  phi: 35deg,
  p: 1,
  hl: 1.7,
  gl: 1.7,
  rad: 0.4,
  hw: 0.3,
  ht: 0.7mm,
  st: 2mm,
  gt: 1mm,
  color: black,
) = cetz.draw.group({
  import cetz.draw: *
  rotate(theta)

  let py = calc.sin(phi)*p
  for s in (1, -1) {
    group({
      translate((0, s*py))
      rotate(s*phi, origin: (0, 0))
      let p0 = (-hl, 0)
      line((0.1, 0), p0, stroke: (paint: color, thickness: st, cap: "round"))
      merge-path(
        {
          let p1 = (p0.at(0) - 0.3, -s*hw)
          let p2 = (p1.at(0) - 0.8, p1.at(1))
          line(p0, p1, p2)
          let p3 = (p2.at(0) - 0.3*hw, -s*hw*0.5)
          let p4 = (p2.at(0), 0.0)
          arc-through(p2, p3, p4)
          line(p4, (p0.at(0) - 0.2, 0))
        },
        stroke: (paint: color, thickness: ht, cap: "butt")
      )
    })
  }
  for s in (1, -1) {
    group({
      translate((0, s*py))
      rotate(-s*phi, origin: (0, 0))
      line((-0.1, 0), (gl, 0), stroke: (paint: color, thickness: st, cap: "round"))
      merge-path(
        {
          let grabend = (gl+rad*2, s*rad)
          arc-through((gl, 0), (gl+rad, -s*rad*0.2), grabend)
          line(grabend, (rel: (0.3, s*0.1)))
        },
        stroke: (paint: color, thickness: gt, cap: "butt")
      )
    })
  }  
})

#let grabber-canvas(..stuff) = cetz.canvas(grabber(..stuff))

#let grabber-grad = gradient.linear(rgb("e8a76c"), rgb("5cc6e8"), relative: "parent", space: oklch)
