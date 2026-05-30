#import "/templates/page.typ": *
#import "/templates/lib.typ": inline-svg
#set document(
  title: "Inline SVG via the raw-html pass",
  description: "Exercises read() of a colocated asset + the raw-html resolution pass.",
  date: datetime(year: 2026, month: 5, day: 24),
  kind: "post",
)
#show: page-template

The figure below is an SVG file colocated under `content/`, read at
compile time and spliced inline through twyla's raw-html resolution
pass. The `<script type="x-twyla-raw-html">` wrapper never reaches the
final HTML.

#inline-svg("diagram-demo.svg")
