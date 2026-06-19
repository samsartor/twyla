// A deliberately bare content file: no imports, no `#show`, no template.
// It uses `asset.file(..)` — a twyla *native builtin* injected into the
// global scope from Rust (see src/asset.rs) — with zero ceremony. This
// is the "content dir full of plain typ files" target: the engine, not
// the author, supplies the prelude.

= Plain page

A bare typst file. The link below resolves through twyla's native `asset`
builtin (no import needed). `.url()` is contextual, so it sits in `#context`:

#context link(asset.file("logo.svg").url())[the logo]

An #link("https://typst.app")[external link] gets `rel`/`target` from
twyla's native link rule, with no `#show` rule in sight.

#let is-twyla = "twyla_version" in dictionary(sys)
Twyla runtime detected: #is-twyla (#sys.twyla_version)
