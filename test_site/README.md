# test_site

twyla's integration fixture — a small, self-contained, turn-key twyla
project used by `tests/site.rs`. It is **not** samsartor.com: its
templates are deliberately independent so the tests prove twyla's
engine primitives (bundle routing, `sys.inputs.base_url`, the
`x-twyla-raw-html` resolution pass) aren't hardcoded to one site.

What each page exercises:

- `content/main.typ` — home page; cross-document post enumeration via
  `query(<twyla-post>)`.
- `content/hello.typ` — headings + auto-slug ids, an intra-document
  anchor link (label → fragment-only href), lists, fenced code.
- `content/diagram-demo.typ` — `read()` of a colocated asset spliced
  through the raw-html resolution pass.
- `content/_draft.typ` — underscore-prefixed; must be skipped by the
  scanner (no `/draft/` route).
- `static/style.css` — verbatim static-file copy.

Run with `cargo test`. Pages whose features twyla doesn't implement yet
(nested sections, asset fingerprinting, feeds) have `#[ignore]`d tests
in `tests/site.rs` that serve as the executable to-do list.
