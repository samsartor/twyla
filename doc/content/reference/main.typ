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
//
// Every asset type exposes the same two contextual methods (`.url()`/`.read()`),
// so `render-item` promotes them to the head of the `asset` section and renders
// each type without its (now-shared) scope, rather than repeating them under
// every type — see the `asset` handling in `/templates/components.typ`.
#let items = (
  ("document", document),
  ("documents", documents),
  ("asset", asset),
  ("raw-html", raw-html),
  ("plain-text", plain-text),
  ("sys.twyla-version", sys.twyla-version, [
    The version of Twyla currently compiling the document, as a `version` value. The
    field can be used to detect if Twyla is being used:
    ```example
    #if "twyla-version" in dictionary(sys) [ Hello from Twyla ] else [ Hello from Typst ]
    ```
  ]),
)

#page-shell(
  sidebar: reference-toc(items),
  reference-body(items),
)
