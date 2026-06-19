// =========================================================
// BoxDraw library from https://github.com/samsartor/boxdraw
// =========================================================

#import "@preview/cetz:0.5.2"

// Adapted from https://github.com/MarkLodato/js-boxdrawing
#let CHARS =  (
  // Left, Up, Right, Down
  // _ long blank
  // . short blank
  // t/T short/long regular
  // b/B short/long bold
  // d/D short/long double
  // l/L short/long left-side only
  // r/R short/long right-side only
  " ":"____",
  "╴":"T___", "╸":"B___",
  "╵":"_T__", "╹":"_B__",
  "╶":"__T_", "╺":"__B_",
  "╷":"___T", "╻":"___B",
  "┘":"TT__", "┛":"BB__", "┙":"BT__", "┚":"TB__",
  "─":"T_T_", "━":"B_B_", "╾":"B_T_", "╼":"T_B_",
  "┐":"T__T", "┓":"B__B", "┑":"B__T", "┒":"T__B",
  "└":"_TT_", "┗":"_BB_", "┖":"_BT_", "┕":"_TB_",
  "│":"_T_T", "┃":"_B_B", "╿":"_B_T", "╽":"_T_B",
  "┌":"__TT", "┏":"__BB", "┍":"__BT", "┎":"__TB",
  "┴":"TTT_", "┻":"BBB_", "┵":"BTT_", "┸":"TBT_", "┶":"TTB_",
  "┺":"TBB_", "┷":"BTB_", "┹":"BBT_",
  "┤":"TT_T", "┫":"BB_B", "┥":"BT_T", "┦":"TB_T", "┧":"TT_B",
  "┨":"TB_B", "┪":"BT_B", "┩":"BB_T",
  "┬":"T_TT", "┳":"B_BB", "┭":"B_TT", "┮":"T_BT", "┰":"T_TB",
  "┲":"T_BB", "┱":"B_TB", "┯":"B_BT",
  "├":"_TTT", "┣":"_BBB", "┞":"_BTT", "┝":"_TBT", "┟":"_TTB",
  "┢":"_TBB", "┠":"_BTB", "┡":"_BBT",
  "┼":"TTTT", "╋":"BBBB", "┽":"BTTT", "╀":"TBTT", "┾":"TTBT", "╁":"TTTB",
  "╊":"TBBB", "╈":"BTBB", "╉":"BBTB", "╇":"BBBT",
  "╂":"TBTB", "┿":"BTBT", "╃":"BBTT", "╄":"TBBT", "╆":"TTBB", "╅":"BTTB",
  "═":"D.D.", "║":".D.D",
  "╝":"dd..", "╚":".dd.", "╔":"..dd", "╗":"d..d",
  "╜":"TD._", "╘":"_TD.", "╓":"._TD", "╕":"D._T",
  "╛":"DT_.", "╙":".DT_", "╒":"_.DT", "╖":"T_.D",
  "╩":"ddd.", "╠":".ddd", "╦":"d.dd", "╣":"dd.d",
  "╨":"TdT_", "╞":"_TdT", "╥":"T_Td", "╡":"dT_T",
  "╧":"dtd.", "╟":".dtd", "╤":"d.dt", "╢":"td.d",
  "╬":"dddd", "╪":"dtdt", "╫":"tdtd"

)

#let DIRS = (
  ( 0, -1, 2),
  (-1,  0, 3),
  ( 0,  1, 0),
  ( 1, 0,  1),
)

#let WSREGEX = regex("^\s$")

#let _swaprl(style) = {
  if style == "R" { "L" }
  else if style == "L" { "R" }
  else { style }
}

#let _tryconsume(found, expected, inn) = {
  if found == none { return none }
  let long = found == upper(found)
  let rel_expected = if inn { _swaprl(expected) } else { expected }
  if upper(found) == "D" {
    let remaining = if rel_expected == "R" { "l" } 
      else if rel_expected == "L" { "r" }
      else { return none }
    return if long { upper(remaining) } else { remaining }
  } else if upper(found) == rel_expected {
    "_"
  } else {
    none
  }
}

#let _consumetoend(
  // The current grid
  x,
  // The current coordinate
  i, j,
  // The current line style
  style,
  // The direction to move in, index of LURD
  out,
  // How much space between double lines
  sep,
  // The grid cell sizes
  cellw, cellh,
  // Whether to start with the next cell, or at the current cell
  next,
) = {
  let (di, dj, inn) = DIRS.at(out)

  let offset(out) = {
    let (di, dj, inn) = DIRS.at(out)
    if style == "R" { (0.5*dj*sep, -0.5*di*sep) }
    else if style == "L" { (-0.5*dj*sep, 0.5*di*sep) }
    else { (0.0*sep, 0.0*sep) }
  }

  let nodes = ()
  while true {
    // Step forward
    if next {
      i += di
      j += dj
    } else {
      next = true
    }

    // Look at the incoming edge (if any)
    let inn_cell = if i < 0 or i >= x.len() or j < 0 or j >= x.at(i).len() {
      none
    } else {
      x.at(i).at(j)
    }
    let inn_style;
    if type(inn_cell) == array {
      inn_style = inn_cell.at(inn)
    } else {
      none
    }

    // Try to consume the incoming edge
    let consumed = _tryconsume(inn_style, style, true)
    if consumed == none {
      // We can't connect to the incoming edge, end at the halfway point.
      let (oi, oj) = offset(out)
      nodes.push((i*cellh - 0.5*di*cellh + oi, j*cellw - 0.5*dj*cellw + oj))
      return (x, nodes);
    } else {
      x.at(i).at(j).at(inn) = consumed
    }

    // The relative directions we'd like to try
    let rel_outs = if style == "R" { (1, 0, -1) }
      else if style == "L" { (-1, 0, 1) }
      else { (0, 1, -1) }

    // Try to consume each outgoing edge
    let consumed
    let out_style
    let next_out
    for rel_out in rel_outs {
      // Absolute direction
      next_out = calc.rem-euclid(rel_out + out, 4)
      // Look at the outgoing edge.
      out_style = x.at(i).at(j).at(next_out)
      consumed = _tryconsume(out_style, style, false)
      if consumed != none {
        // Found the direction we'd like to take
        break
      }
    }

    if consumed == none {
      // There was no outgoing edge to connect to, end at the center.
      let (oi, oj) = offset(out)
      if inn_style == upper(inn_style) {
        nodes.push((i*cellh + oi, j*cellw + oj))
      } else {
        // Short edge, don't connect all the way
        nodes.push((i*cellh - 0.5*di*sep + oi, j*cellw - 0.5*dj*sep + oj))
      }
      return (x, nodes);
    }

    // Mark the edge as consumed
    x.at(i).at(j).at(next_out) = consumed

    // We changed direction
    if out != next_out {
      // Figure out the corner offset
      let (oi0, oj0) = offset(out)
      let (oi1, oj1) = offset(next_out)
      let oi = if calc.abs(oi0) > calc.abs(oi1) { oi0 } else { oi1 }
      let oj = if calc.abs(oj0) > calc.abs(oj1) { oj0 } else { oj1 }

      // Recalculate stuff
      out = next_out
      (di, dj, inn) = DIRS.at(out)

      // Leave a corner node
      nodes.push((i*cellh + oi, j*cellw + oj))
    }
  }
}

#let _consumeline(
  // The current grid
  x,
  // The current draw instructions
  w, 
  // The current coordinate
  i, j,
  // How much space between double lines
  sep,
  // The grid cell sizes
  cellw, cellh,
) = {
  for d in range(4).rev() {
    // Load the current cell
    let cell = x.at(i).at(j)
    if type(cell) != array {
      return (x, w)
    }
    let style = upper(cell.at(d))
    
    // Check if there is a line to make in this direction.
    let styles = if style == "_" or style == "." {
      ()
    } else if style == "D" {
      ("R", "L")
    } else {
      (style,)
    }
    
    // Produce and store contiguous lines
    for style in styles {
      let tail
      let head
      (x, tail) = _consumetoend(x, i, j, style, d, sep, cellw, cellh, true)
      (x, head) = _consumetoend(x, i, j, _swaprl(style), calc.rem(d + 2, 4), sep, cellw, cellh, false)
      w.push((style, ..head.rev(), ..tail))
    }
  }
  return (x, w)
}
  
#let boxdraw-parse(
  // The text to parse
  txt,
  // How much space between double lines
  sep,
  // The grid cell sizes
  cellw, cellh,
) = {
  let x = txt.split("\n").map(l => { 
    l.clusters().map(c => {
      if c.match(WSREGEX) != none {
        return none
      }
      let r = CHARS.at(c, default: none)
      if r == none {
        return c
      } else {
        return r.codepoints()
      }
    })
  })
  let w = ()

  let i = 0
  while i < x.len() {
    let j = 0
    while j < x.at(i).len() {
      let c = x.at(i).at(j)
      if type(c) == array {
          (x, w) = _consumeline(x, w, i, j, sep, cellw, cellh)
      } else if type(c) == str {
        let j_txt = j + 1
        while j_txt < x.at(i).len() {
          let next_txt = x.at(i).at(j_txt)
          if type(next_txt) == str {
            x.at(i).at(j) += next_txt
            x.at(i).at(j_txt) = none
          } else {
            break
          }
          j_txt += 1
        }
      }
      j += 1
    }
    i += 1
  }

  return (grid: x, draw: w)
}

#let DEFAULT_STROKES = (
  T: (paint: black, thickness: 1pt),
  R: (paint: black, thickness: 1pt),
  L: (paint: black, thickness: 1pt),
  B: (paint: black, thickness: 2pt),
)

#let DEFAULT_HTML_STROKES = (
  T: 0.12,
  R: 0.12,
  L: 0.12,
  B: 0.24,
)

#let boxdraw-cetz(
  // The parsed data to draw
  (grid: x, draw: w),
  // The grid cell sizes
  cellw, cellh,
  // Styling for each line type
  strokes: DEFAULT_STROKES,
  // Fill for closed rectangles
  fill: none,
) = cetz.canvas({
  for (style, ..points) in w {
    let stroke = strokes.at(style)
    let closed = points.last() == points.first()
    cetz.draw.line(
      ..points.map(((i, j)) => (j, -i)),
      stroke: stroke,
      close: closed,
      fill: if closed { fill } else { none },
    )
  }
  
  for (i, l) in x.enumerate() {
    for (j, c) in l.enumerate() {
      if type(c) == str {
        cetz.draw.content(
          ((j - 0.5)*cellw, -i*cellh),
          raw(c),
          anchor: "west",
        )
      }
    }
  }
})

#let _boxdraw-html-text(text) = text.split("\n").map(line => {
  line.clusters().map(c => {
    if CHARS.at(c, default: none) == none { c } else { " " }
  }).join("")
}).join("\n")

#let _boxdraw-svg-num(n) = {
  // Typst formats negative numbers with a typographic minus, but SVG path data
  // and view boxes need ASCII hyphens.
  if n < 0 { "-" + str(-n) } else { str(n) }
}

#let _boxdraw-svg-path(points) = {
  points.enumerate().map(((idx, point)) => {
    let (i, j) = point
    (if idx == 0 { "M " } else { "L " }) + _boxdraw-svg-num(j) + " " + _boxdraw-svg-num(i)
  }).join(" ")
}

#let boxdraw-html(
  // The original text. Drawing characters are replaced by spaces in the raw
  // layer so that the raw block keeps the same footprint as the source.
  text,
  // The parsed data to draw
  (grid: x, draw: w),
  // CSS line height for the raw text and SVG grid
  line-height,
  // Styling for each line type, expressed in SVG user units. Strokes use
  // `currentColor`, so they follow the surrounding text color.
  strokes: DEFAULT_HTML_STROKES,
) = {
  let rows = x.len()
  let cols = if rows == 0 { 0 } else { x.map(l => l.len()).fold(0, calc.max) }
  let paths = {
    for (style, ..points) in w {
      let closed = points.last() == points.first()
      html.elem("path", attrs: (
        d: _boxdraw-svg-path(points),
        fill: "none",
        stroke: "currentColor",
        "stroke-width": _boxdraw-svg-num(strokes.at(style, default: 0.07)),
        "stroke-linecap": "butt",
        "stroke-linejoin": "miter",
      ))
    }
  }

  html.elem("div", attrs: (
    class: "boxdraw",
    style: "--boxdraw-cols: " + str(cols) + "; --boxdraw-rows: " + str(rows) + "; --boxdraw-line-height: " + str(line-height) + ";",
  ), {
    raw(_boxdraw-html-text(text), block: true)
    html.elem("svg", attrs: (
      class: "boxdraw-svg",
      xmlns: "http://www.w3.org/2000/svg",
      viewBox: _boxdraw-svg-num(-0.5) + " " + _boxdraw-svg-num(-0.5 * line-height) + " " + str(cols) + " " + _boxdraw-svg-num(rows * line-height),
      preserveAspectRatio: "none",
      "aria-hidden": "true",
      focusable: "false",
    ), paths)
  })
}

#let boxdraw(
  text,
  with: auto,
  line-height: 1.5,
  sep: auto,
  strokes: DEFAULT_STROKES,
  html-strokes: DEFAULT_HTML_STROKES,
  fill: none,
) = {
  if with == auto {
    return context boxdraw(
      text,
      with: if target() == "html" { "html" } else { "cetz" },
      line-height: line-height,
      sep: sep,
      strokes: strokes,
      html-strokes: html-strokes,
      fill: fill,
    )
  } else if with == "html" {
    let cellw = 1
    let cellh = line-height
    let html-sep = if sep == auto { 0.14 } else { sep }
    let parsed = boxdraw-parse(text, html-sep, cellw, cellh)
    boxdraw-html(text, parsed, line-height, strokes: html-strokes)
  } else if with == "cetz" {
    let measured = measure(raw("oheartns"))
    let cellw = measured.width / 8
    let cellh = measured.height * line-height
    let cetz-sep = if sep == auto { 2pt } else { sep }
    let parsed = boxdraw-parse(text, cetz-sep, cellw, cellh)
    boxdraw-cetz(parsed, cellw, cellh, strokes: strokes, fill: fill)
  } else {
    assert(false, "Unknown drawing option: " + with)
  }
}

#show raw.where(lang: "boxdraw"): it => boxdraw(
  it.text,
  fill: luma(90%),
  strokes: (
    T: (paint: black, thickness: 1pt),
    R: (paint: red, thickness: 1pt),
    L: (paint: blue, thickness: 1pt),
    B: (paint: black, thickness: 2pt),
  )
)
#set text(size: 15pt)

```boxdraw
═════
```

```boxdraw
┌───────┐
│ Hello │
└───────┘
```

```boxdraw
══┐┏━━┓╠═╗
  ╽┃┏┓║║ ║
┏━╋┻┫┃┃║ ║
╿ ┗┳╋┻┛║ ║
├┐ ┞┸─╖╠═╣
╵└─┘══╝╚═╝
```

```boxdraw
┌───────────────────────────────────────┐
│hoearsothoearoheahoeasnoheaanranhoheaea│
└───────────────────────────────────────┘
```

```boxdraw
abcd
e╬╬f
g╬╬h
ijkl
```
