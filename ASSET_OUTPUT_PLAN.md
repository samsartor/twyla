# Plan: per-asset `output` control

## Context

Twyla assets currently emit to a single project-default path: `assets/{stem}-{hash}.{ext}`,
derived purely locally from the asset's own content hash. Users have no way to control where an
asset lands on disk. We want a per-asset `output` field:

```
output: auto | str | (sha256-hash-hex: str, ext: str, stem: str) -> str
```

- `auto` (default) — current behavior, with a new reuse rule (below).
- a string — an explicit output path.
- a closure — receives the asset's sha256 hex hash, output extension, and filename stem; returns a path.

These flow through as `OutputReq` variants; the resolver computes the concrete output path(s).
**Auto-reuse rule:** if exactly one *distinct explicit* path is requested for an asset, `auto`
usages reuse it; with zero or ≥2 distinct explicit paths, `auto` falls back to the project default.
Explicit `output` also **forces emission** (the file is written even if `.url()`/`.read()` is never
called), and a bare asset element sitting in content emits like a document.

This is feasible without touching the vendored introspection machinery: the `Introspect` bound is
`PartialEq + Hash` (not `Eq`), and typst's `Func` is `#[derive(Clone, Hash)]` + manual `PartialEq`,
so a `Func` can ride inside an `OutputReq` through the introspection record — once twyla drops its
own non-load-bearing `Eq`/`Ord` derives. Because `OutputReq` stays `PartialEq`, requests dedup, so a
template that stamps the same computed-path closure onto every output evaluates the closure once.

## Design

### `OutputReq` (`src/asset/mod.rs`)

Drop `Eq`/`Ord`; keep `PartialEq + Hash`. Same for `AssetReq` and `AssetReqIntrospect`.

```rust
#[derive(Clone, PartialEq, Hash, Debug)]
pub enum OutputReq { Read, Auto, Fixed(EcoString), Derive(Func) }
```

- `.url()` produces Auto, Fixed, or Derive depending on the element's `output` field
- `.read()` → `Read` (does not force emission).
- Bare asset SHOW_RULE → Auto, Fixed, or Derive depending on output, and return empty content.

`AssetReq.outputs` becomes `Vec<OutputReq>`, deduped by `PartialEq` (linear scan? sets are usually tiny).

### Per-usage path lookup (`ResolvedAsset`)

A `.url()` usage must get back *its own* policy's path. Add a resolution table; `.url()` linear-scans
it for the entry whose `policy == self.policy`.

The resolutions is conceptually an `outputs: HashMap<OutputReq, String>`, but
in practice may need to be a Vec sorted by the `output_path` String. Included
in `ResolvedAsset`'s manual `PartialEq`/`Hash` so the convergence constraint
re-spins when a new policy/path is discovered, exactly as it already does for
`outputs`. Keep `output_path` as the `Auto`-resolved convenience (used by
the native image rule + render key), but url should be derivable from ctx.

### `output` field on elements + macro

Add a settable, `auto`-default `output` field to `FileAsset`, `SassAsset`, `ImageAsset`, `TypstAsset`.
Field type is a single enum (reuse `OutputReq` if possible) with a `cast!` block mirroring `TypstSource`
(`typst_doc.rs:133`): `auto → Auto`, `EcoString → Fixed`, `Func → Derive`. **Not** part of any
`spec()` builder — `output` must never enter `AssetSpec` (two usages of one file with different
`output` must still build one asset and share bytes). Thread it through `asset_methods!`
(`mod.rs:19`) via a new `$output` macro param (an accessor `fn(&Packed<Elem>, StyleChain) -> OutputReq`),
parallel to the existing `$spec`. `resolve_or_request`/`resolve_image_or_request` gain an `OutputReqOutputReq
param.

### Resolver (`src/resolver.rs`)

- Split `build_asset` to produce `Built` (bytes + hashes) **only** — move path/url derivation out.
- New `resolve_outputs(world, eval)` run once per `discover` pass *after* all `resolve_asset` calls
  have unioned each spec's outputs. Per asset: dedup outputs; resolve each `Fixed`/`Derive` to a
  no-slash path (closures via `eval`, evaluated once per distinct policy); compute the set of distinct
  explicit paths; apply the auto-reuse rule for `Auto`; build `resolutions`; record the multi-path
  emission set. `Arc::make_mut` per mutated asset. `clear_asset_usage` (`resolver.rs:112`) also clears
  the output_path resolutions.
- Replace `emittable_assets` (`resolver.rs:87`) with an iterator yielding `(path, Arc<ResolvedAsset>)`
  — one per distinct emit path (multi-copy). `resolved_assets()` (watcher deps) unchanged.

### Closure evaluation + borrow safety (`src/compile.rs`)

`Func::call` needs a full `Engine` + `Tracked<Context>`. The iteration `engine` borrows `subsink`
mutably and is unusable when `discover` holds `subsink.introspections()`. So `discover`:
1. Loops introspections calling `resolve_asset` (accumulate outputs only) + `collect_document` — the
   only part needing the `subsink` borrow.
2. After that borrow ends, builds a **fresh eval-Engine** (world, library, `traced`, a new local
   `Sink`, `Route::default()`, `EmptyIntrospector`) and calls `resolver.resolve_outputs(world, eval)`,
   where `eval` calls `f.call(engine, Context::none().track(), (sha256_hex, ext, stem))`. Propagate
   closure `SourceResult` errors (spanned at the asset's `span`); surface warnings into the main sink.

Pathological closures that return a different path each pass simply don't converge — same failure mode
as an unstable counter, already diagnosed by `AssetReqIntrospect::diagnose`. Explicit `Fixed` adds no
extra spin.

### SHA256 (`Built` + paths)

Add `sha256: [u8;32]` to `Built` (`mod.rs:362`) + `sha256_hex() -> EcoString` (full 64-char). Compute
over output bytes in `file.rs`, `sass.rs`, `image.rs`, `typst_doc.rs`. Hash `Built` by `sha256`; drop
the u128 `content_hash`/`hash128` naming use (verify no test depends on it first). `default_asset_output`
(`project.rs:312`) uses the **first 32 hex chars** of sha256 (length parity with today). Closure
receives the **full** hex. New deps: `sha2 = "0.10"`, `hex = "0.4"` (sha2 already compiled transitively;
idiomatic and pure-Rust — preferred over the clunky, transitively-available `openssl`).

### Output paths / URL (`src/project.rs`)

`Fixed`/`Derive` strings are bundle-root-relative output paths normalized via the existing
`resolve_output` + `build_document_url` logic: `stuff/index.html` → written to
`public/stuff/index.html`, URL `$BASE/stuff` (index.html stripping + slash convention + base_url
folding, reused from documents). The closure/string returns a path, not a full URL.

### Render (`src/render.rs`)

`Output::Asset { path: String, asset: Arc<ResolvedAsset> }`; `key()` returns `path`; `write_to` still
writes `asset.built.emit`. `CompiledBundle.assets` becomes `Vec<(String, Arc<ResolvedAsset>)>`. Emit
loops (`compile.rs:127`, `render.rs:469`) iterate `(path, asset)` pairs — multiple distinct paths emit
multiple copies; identical paths still collide via `insert_unique` (existing conflict error). Per-path
shape leaves room for a future hard/soft-link cache keyed on `built.emit`/sha256.

### Bare-element SHOW_RULE (all four types)

Replace the four `show_unresolved` SHOW_RULEs (`file.rs:48`, `sass.rs:65`, `image.rs:161`,
`typst_doc.rs:119`) with a document-style rule (mirror `document::RENDER_INTROSPECTION`,
`document.rs:221`): build `spec` + `output` policy, `engine.introspect(AssetReqIntrospect(AssetReq{
spec, span, outputs: vec![OutputReq::Emit(policy)] }))`, return `Content::empty()`. Delete
`show_unresolved` (`mod.rs:328`); update `rules.rs:55-66` comments.

## Files to change

- `Cargo.toml` — add `sha2`, `hex`.
- `src/asset/mod.rs` — enums, derives, `outputs: Vec`, `cast!`, macro `$output`, `ResolvedAsset` +
  `Resolution`, resolve helpers' signatures, remove `show_unresolved`.
- `src/asset/{file,sass,image,typst_doc}.rs` — `output` field, sha256, SHOW_RULE rewrite.
- `src/resolver.rs` — split build, `resolve_outputs`, dedup, auto-reuse, `emittable_assets`,
  `clear_asset_usage`.
- `src/compile.rs` — `discover` restructure + eval-Engine, `CompiledBundle.assets` type, emit loop.
- `src/project.rs` — `default_asset_output` (sha256), asset-path resolver reusing `build_document_url`.
- `src/render.rs` — `Output::Asset { path, asset }`, `key`, emit loop, dup error.
- `src/rules.rs` — comment updates (registration count unchanged).
- `src/asset/tests.rs` + `src/compile.rs` tests — update + add.

## Task order

1. Deps.
2. `Built` sha256.
3. `default_asset_output` + asset-path resolver.
4. Enums/derives/casts.
5. `outputs: Vec`.
6. Resolve helper signatures + callers (`mod.rs:37`, `rules.rs:215/225`).
7. `ResolvedAsset` + `Resolution`.
8. `output` field + macro.
9. SHOW_RULEs + rules.rs comments.
10. Resolver split + `resolve_outputs` + emit iterator.
11. `discover` + eval-Engine.
12. Render multi-copy.
13. Tests.

## Verification

- `cargo build`, `cargo clippy`, `cargo test` (existing compile/document/asset tests still pass; update
  any asserting the old bare-asset error).
- New tests (tempdir harness, `compile.rs:594`; `asset/tests.rs`):
  1. `auto` default unchanged.
  2. `output: "css/site.css"` → `public/css/site.css`, URL `$BASE/css/site.css`.
  3. `output: "stuff/index.html"` → `public/stuff/index.html`, URL `$BASE/stuff`.
  4. Closure path; same closure across two usages evaluates once (assert single emission).
  5. Auto-reuse: one explicit + one `Auto` → both that path; add a 2nd distinct explicit → `Auto`
     falls back to default; correct copies emit.
  6. Bare `#asset.file("x.css", output: "x.css")` (no `.url()`) emits and vanishes (no error).
  7. Multi-copy: two distinct explicit paths → two identical files.
  8. All compile without hitting `MAX_ITERS` / the "did not stabilize" warning.
- Smoke: `twyla build` against the real site — no regression in default asset emission.
- The doc/ site has the cetz-generated favicon at /favicon.svg instead of generated path.

## Open items (resolve during implementation, non-blocking)

- Exact import for the `auto`-cast unit type in the `cast!` (the mechanism `Smart`/`auto` uses).
- Confirm no test references `content_hash` before deleting it.
- Confirm eval-Engine warning routing into the main sink.
- Can the resolved output paths be a map, or just a sorted vec.
- Have OutputReq as the type for the `output` field, or some other typst-aware enum without Read.
