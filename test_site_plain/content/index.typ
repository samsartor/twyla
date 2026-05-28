// A deliberately bare content file: no imports, no `#show`, no template.
// It calls `asset-url(..)` — a twyla *native builtin* injected into the
// global scope from Rust (see src/prelude.rs) — with zero ceremony. This
// is the "content dir full of plain typ files" target: the engine, not
// the author, supplies the prelude.

= Plain page

A bare typst file. The link below resolves through twyla's native
`asset-url` builtin (no import needed):

#link(asset-url("logo.svg"))[the logo]
