//! SPIKE — twyla's native prelude.
//!
//! Proves the load-bearing mechanism for the all-Rust-builtins direction:
//! a `#[func]`-defined Rust function injected into typst's *global* scope,
//! callable from a plain `.typ` file with **zero imports and zero
//! `#show`** — exactly the "content dir full of plain typ files" target.
//!
//! How it works:
//! - `#[func]` (re-exported from typst-macros via `typst::foundations`)
//!   turns the annotated fn into a zero-sized type implementing
//!   `NativeFunc`. The macro emits `::typst_library::foundations::*`
//!   absolute paths, and twyla depends on `typst-library` directly, so it
//!   compiles outside the typst crates unmodified.
//! - `Library::builder().build()` hands back a `Library` whose `global`
//!   field is a `pub Module`. `module.scope_mut()` is `&mut Scope`, and
//!   `Scope::define_func::<T>()` binds a native func. We mutate the library
//!   *after* `build()` — no fork, no patched stdlib.
//!
//! This is a spike: `asset-url` here is a placeholder that fingerprints the
//! path string deterministically (so you can see Rust ran). The real impl
//! will read the asset bytes, optimize, emit into the bundle, and rewrite
//! the reference — i.e. become the first entry in the resolution-pass
//! registry. `raw-html` would follow as entry #2.

use typst::foundations::{Binding, NativeElement, Str, func};
use typst_library::Library;

/// Spike placeholder for the asset-url primitive. Returns a
/// content-addressed-looking path so the test can prove the native fn
/// executed (the output differs from the input by an inserted digest).
///
/// Real impl: resolve `path` against the project, hash the *bytes*,
/// optimize, emit `assets/<stem>.<hash>.<ext>` into the bundle, return
/// that URL. Native because it must cooperate with the asset pipeline.
#[func]
pub fn asset_url(
    /// Project-relative path of the asset to resolve.
    path: Str,
) -> Str {
    let digest = fnv6(path.as_str());
    let (stem, ext) = match path.as_str().rsplit_once('.') {
        Some((s, e)) => (s, e),
        None => (path.as_str(), ""),
    };
    let url = if ext.is_empty() {
        format!("/assets/{stem}.{digest}")
    } else {
        format!("/assets/{stem}.{digest}.{ext}")
    };
    Str::from(url)
}

/// Tiny deterministic 6-hex-char digest. Spike-only stand-in for a real
/// content hash; keeps the test stable across runs without pulling in a
/// hashing dependency.
fn fnv6(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:06x}", (h & 0xff_ffff))
}

/// Install twyla's native customizations into a freshly built library.
/// Call after `Library::builder().build()`, before wrapping in
/// `LazyHash`. Two seams:
/// - global-scope builtins (mechanism 1) — new verbs callable with zero
///   imports;
/// - native HTML rules (mechanism 2, [`crate::rules`]) — engine-level
///   element→HTML behavior that every site inherits.
///
/// Additive on the scope side — does not remove or shadow stdlib
/// bindings (yet; shadowing `document` to capture twyla metadata is the
/// planned next step — see render.rs notes).
pub fn install(library: &mut Library) {
    crate::rules::install(&mut library.rules);
    let global = library.global.scope_mut();
    global.define_func::<asset_url>();

    // SPIKE — overload `document`. Grab the native binding *first* (so the
    // routing primitive stays reachable), rebind it as `__std_document` for
    // `generate_main`, then shadow `document` with twyla's element. Bundle
    // routing keys on the native `DocumentElem` *type*, not this binding, so
    // `#__std_document(path)[..]` still routes. `bind` overwrites in place
    // (unlike `define`/`define_elem`, which dedup-panic on an existing name).
    let native_document = global
        .get("document")
        .expect("native `document` binding present after build")
        .read()
        .clone();
    global.define("__std_document", native_document);
    global.bind(
        "document".into(),
        Binding::detached(crate::twyla_doc::TwylaDocument::ELEM),
    );
}
