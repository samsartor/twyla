# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Standard Rust project (Cargo). Twyla is a typst-based static site generator — authoring is all Typst; Twyla itself is Rust and consumes the typst compiler **as a library** (not the CLI).

**Read `doc/content/planning.typ` first when picking up new work.** It is the authoritative engineering plan and covers architecture (injection mechanisms, compile loop, routing, resolution pass, diff harness, dev server) in full. The aspirational user manual lives in `doc/content/main.typ` and `README.md`.

## Twyla-specific CLI invocations

```
cargo run -- serve --root test_site --base-url https://example.com
cargo run -- build --root test_site -o /tmp/out
cargo run -- diff <expected.html> <actual.html>
cargo run -- import path/to/post.md
```

`twyla serve` defaults to `127.0.0.1:1111` with SSE hot reload. `--base-url`/`TWYLA_BASE_URL` is required by `check` and recommended for tests. There are two integration test fixtures: `--test site` (`test_site/`) and `--test plain` (`test_site_plain/`). Some tests require `$SITE_ROOT` and are `#[ignore]`d by default.

Doc-site preview (Justfile): `just doc2readme`, `just doc2pdf`. The `typst` CLI is only used for these.

## Typst pin — do not bump casually

`Cargo.toml` pins **every** typst crate (`typst`, `typst-bundle`, `typst-eval`, `typst-html`, `typst-kit`, `typst-library`, `typst-utils`) to one specific git rev. Twyla depends on unreleased bundle-export work and on internal-ish APIs (`Engine`, `Sink`, `Traced`, `Route`, `TargetElem`, `EmptyIntrospector`, `analyze`, `typst_eval::eval`, `NativeRuleMap`, `Protected`). Never `cargo update -p typst` — bump deliberately and bump all seven crates together to the same rev. Switch back to crates.io only once typst 0.15 ships.

The vendored fixed-point loop in `src/compile.rs` mirrors typst's `compile_impl`; if you bump the pin, re-diff that file against upstream.

## Test fixtures

- `test_site/` — full author-side templating; exercises routing, cross-doc enumeration, heading slugs, raw-html, colocated assets, build-to-disk. One golden at `tests/golden/hello.html`. Pending features have `#[ignore]`d tests as the executable to-do list.
- `test_site_plain/` — minimal "plain `.typ` files" contract; no imports, no `#show`, no template. Proves native prelude and HTML rules work with zero ceremony.

Regenerate the golden: `cargo run -- render --root test_site --base-url https://example.com hello > tests/golden/hello.html` (once `render` subcommand is revived — currently `todo!()` in `src/main.rs::cmd_check`).

## Conventions

- Repo is git on `main`. Notes in `planning.typ` about `jj` and `~/Src/twyla` describe Sam's working tree, not this checkout.
- Module-level `//!` docs in `src/` carry implementation rationale; `///` docs are user-facing API docs. Preserve that split.
- `World::today()` returns a hard-coded date (`2026-05-17`) — do not change it; keeps AST diffs stable.

## Commit messages

Follow [Scoped Commits](https://scopedcommits.com/): `type(scope): subject`. Use the **folder** of the changed files as the scope — e.g. `fix(src/compile): …`, `feat(src/rules): …`, `test(tests): …`, `doc(doc/content): …`. For changes spanning multiple top-level folders, omit the scope.
