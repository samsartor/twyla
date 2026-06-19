//! Integration test over `test_site_plain/` — the "minimal site is just a
//! content dir of plain `.typ` files" contract.
//!
//! Unlike `test_site/` (which exercises full author-side templating), this
//! fixture's content has **no imports, no `#show`, no template**. It proves
//! twyla's native prelude: builtins injected into typst's global scope from
//! Rust (`src/prelude.rs`) are callable with zero ceremony, and a bare file
//! still renders to a complete HTML document (typst-html's default shell —
//! which a twyla default template will later replace with something nicer).

use std::path::Path;

use twyla::project::TwylaContext;
use twyla::render::{Outputs, render_site};

fn render_plain() -> Outputs {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_site_plain");
    // No base_url: asset URLs come out root-relative (`/assets/..`), which the
    // native link rule treats as internal (no rel/target).
    let ctx = TwylaContext::new(root, None).expect("ctx");
    render_site(&ctx).expect("render test_site_plain")
}

#[test]
fn native_asset_builtin_resolves_with_zero_imports() {
    let outputs = render_plain();
    let page = &outputs
        .docs()
        .find(|d| d.output_path == "index/index.html")
        .expect("index page rendered")
        .html;

    // The page imports nothing and applies no show rule, yet `asset.file(..)`
    // resolved through the compile loop — it lives in the global scope,
    // injected from Rust. The asset is fingerprinted by its content hash:
    // logo.svg -> /assets/logo-<hash>.svg.
    assert!(
        page.contains("href=\"/assets/logo-") && page.contains(".svg\""),
        "native asset builtin did not resolve; got:\n{page}",
    );
    // Twyla extends Typst's existing `sys` module so templates can detect that
    // they are running inside Twyla and inspect the Twyla version.
    assert!(
        page.contains("Twyla runtime detected:")
            && page.contains(">true<")
            && page.contains(env!("CARGO_PKG_VERSION")),
        "sys.twyla_version was not visible through sys; got:\n{page}",
    );

    // And a bare content file still produced a full document shell.
    assert!(page.contains("<html"), "no document shell for bare content");
    assert!(page.contains("<body>"), "no body for bare content");
}

#[test]
fn native_html_rules_apply_without_show_rules() {
    let outputs = render_plain();
    let page = &outputs
        .docs()
        .find(|d| d.output_path == "index/index.html")
        .expect("index page rendered")
        .html;

    // Native heading rule (mechanism 2): a source-level `=` becomes `<h1>`
    // (no +1 offset) and every heading gets an auto-slug `id` — here from
    // the text "Plain page", with no label and no `show heading` rule.
    assert!(
        page.contains("<h1 id=\"plain-page\">"),
        "native heading rule did not produce `= -> <h1 id=slug>`; got:\n{page}",
    );

    // Native link rule: external http(s) links get noopener/external/_blank.
    assert!(
        page.contains("rel=\"noopener external\"") && page.contains("target=\"_blank\""),
        "external link missing native rel/target; got:\n{page}",
    );

    // ...but the internal asset-url link (a relative `/assets/..` href) is
    // left alone — no rel/target leaks onto non-external links.
    let asset_anchor = page
        .split("<a ")
        .find(|frag| frag.contains("/assets/logo-"))
        .expect("asset anchor present");
    let asset_anchor = &asset_anchor[..asset_anchor.find('>').unwrap_or(asset_anchor.len())];
    assert!(
        !asset_anchor.contains("rel=") && !asset_anchor.contains("target="),
        "internal asset link wrongly got rel/target: {asset_anchor}",
    );
}
