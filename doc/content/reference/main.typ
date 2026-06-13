#import "/templates/lib.typ": *
#set document(
  title: "Reference",
  description: "The functions and types Twyla adds to Typst's standard library.",
)

// Auto-generated reference. Each item below is reflected out of the binary via
// `twyla-reflect.describe` (see src/reflect.rs) — its name, the baked `///`
// docs, parameters, and return type. The rendering (sidebar, signature, type
// pills, parameter list) lives in `/templates/components.typ`. Requires the
// `--reflect` opt-in, otherwise `twyla-reflect` is not in scope.

// (binding name, reflected value) pairs — the name drives the section heading
// and anchor, since modules (e.g. `asset`) carry no reflected name.
#let items = (
  ("document", document),
  ("documents", documents),
  ("asset", asset),
  ("raw-html", raw-html),
  ("plain-text", plain-text),
)

#page-shell(
  sidebar: reference-toc(items),
  reference-body(items),
)
