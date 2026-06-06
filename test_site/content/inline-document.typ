// Inline `#document(..)[body]` constructor.
//
// The page's own prose is "before ... after"; the inline document in the
// middle vanishes here and is emitted as its own bundle output
// `inline-child/index.html`, carrying the child body + plumbed metadata.

= Inline Document

before #document(
  output: "inline-child/index.html",
  title: [Hoisted Child],
  extra: (marker: "hoisted"),
)[
== Child Heading

child-body-text

child-extra=#context document.extra.marker
] after
