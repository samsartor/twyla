#import "/templates/lib.typ": page-template
#show: page-template
#set document(title: "Reference")

= Reference
<reference>

#quote(block: true)[
This was written by claude. It is better than nothing, but
#html.span(style: "weight: bold; color: red;")[TODO] have a human re-write.
]

A minimal reference for Twyla's author-facing API. Expect this to grow.

- #link("/reference/document")[document] — per-page metadata and the
  `documents()` listing.
- #link("/reference/asset")[asset] — `asset.file` / `asset.sass` and asset
  URLs.
- #link("/reference/configuration")[configuration] — project layout and the
  knobs Twyla reads.

Beyond these, Twyla adds two small content builtins:

- `raw-html(markup)` — splice literal HTML into the output verbatim. Accepts a
  string or bytes (e.g. `asset.file("icon.svg").read(encoding: none)`), decoded
  as UTF-8.
- `plain-text(content)` — flatten content to a plain string (for slugs, alt
  text, meta descriptions, …).
