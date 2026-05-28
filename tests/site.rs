//! Integration tests over `test_site/` — a self-contained twyla project
//! checked into the repo (see `test_site/README.md`).
//!
//! Two assertion styles, per the agreed model:
//!
//! - **Property-based** (most tests): parse the rendered HTML and assert
//!   specific structure/attributes/links exist. Robust to typst's
//!   cosmetic output churn while we track a git pin.
//! - **Golden** (a few pages): diff the full render against a checked-in
//!   `tests/golden/<page>.html` through the existing relaxation harness.
//!   `<pre>` is relaxed to text-only so syntax-highlighting color churn
//!   in syntect doesn't break the golden.
//!
//! Pages whose features twyla doesn't implement yet — nested sections,
//! asset fingerprinting, feeds — have `#[ignore]`d tests at the bottom.
//! They are the executable to-do list: un-ignore as each feature lands.

use std::path::{Path, PathBuf};

use twyla::build::{Build, BuildSummary, run as build_run};
use twyla::diff::{
    Matcher, RelaxConfig, RelaxationRule, diff, parse_html,
};
use twyla::project::TwylaContext;
use twyla::render::{RoutedDoc, render_site};

const BASE_URL: &str = "https://example.com";

fn test_site() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test_site")
}

fn ctx() -> TwylaContext {
    TwylaContext::new(test_site(), Some(BASE_URL.to_string()))
        .expect("construct TwylaContext for test_site")
}

fn render() -> Vec<RoutedDoc> {
    render_site(&ctx()).expect("render test_site")
}

/// HTML of the doc routed to `bundle_path` (e.g. `hello/index.html`).
fn page<'a>(docs: &'a [RoutedDoc], bundle_path: &str) -> &'a str {
    docs.iter()
        .find(|d| d.path == PathBuf::from(bundle_path))
        .unwrap_or_else(|| {
            let have: Vec<_> = docs.iter().map(|d| d.path.display().to_string()).collect();
            panic!("no doc at {bundle_path:?}; have {have:?}")
        })
        .html
        .as_str()
}

// --- routing -------------------------------------------------------------

#[test]
fn routes_every_content_page_and_skips_underscore_drafts() {
    let docs = render();
    let paths: Vec<_> = docs.iter().map(|d| d.path.clone()).collect();

    for expected in ["index.html", "hello/index.html", "diagram-demo/index.html"] {
        assert!(
            paths.contains(&PathBuf::from(expected)),
            "missing route {expected:?}; have {paths:?}",
        );
    }
    // `_draft.typ` is underscore-prefixed → never scanned, never routed.
    assert!(
        !paths.iter().any(|p| p.starts_with("draft")),
        "underscore-prefixed draft leaked into routes: {paths:?}",
    );
}

// --- home page: cross-document enumeration -------------------------------

#[test]
fn home_enumerates_posts_newest_first() {
    let docs = render();
    let home = page(&docs, "index.html");

    // Both posts appear as links to their permalinks.
    assert!(home.contains(&format!("href=\"{BASE_URL}/hello/\"")), "home missing hello link");
    assert!(
        home.contains(&format!("href=\"{BASE_URL}/diagram-demo/\"")),
        "home missing diagram-demo link",
    );

    // Sorted by date descending: diagram-demo (05-24) before hello (05-20).
    let diag = home.find("/diagram-demo/").expect("diagram-demo link present");
    let hello = home.find("/hello/").expect("hello link present");
    assert!(diag < hello, "posts not newest-first (diagram-demo should precede hello)");
}

// --- headings, slugs, intra-doc anchors ----------------------------------

#[test]
fn headings_get_slug_ids() {
    let docs = render();
    let hello = page(&docs, "hello/index.html");
    assert!(hello.contains("<h2 id=\"prose-and-marks\">"), "missing slugified h2 id");
    assert!(hello.contains("<h2 id=\"anchors\">"), "missing anchors h2 id");
}

#[test]
fn intra_doc_anchor_is_fragment_only() {
    let docs = render();
    let hello = page(&docs, "hello/index.html");
    // Label-based `#link(<anchors>)` resolves to a fragment, not an
    // absolutized URL — the cross-doc-leak regression guard from the
    // render smoke test, here on an independent site.
    assert!(hello.contains("href=\"#anchors\""), "intra-doc link not fragment-only");
    assert!(
        !hello.contains(&format!("{BASE_URL}/hello/#")),
        "intra-doc anchor was absolutized against base URL",
    );
}

// --- raw-html resolution pass --------------------------------------------

#[test]
fn raw_html_marker_is_resolved_to_inline_svg() {
    let docs = render();
    let diag = page(&docs, "diagram-demo/index.html");

    // The SVG body is spliced inline...
    assert!(diag.contains("<svg"), "inline svg missing");
    assert!(diag.contains("viewBox=\"0 0 120 60\""), "svg attributes missing");
    // ...and the real marker tag is gone. (The *escaped* mention of the
    // marker inside a <code> span — `&lt;script type="x-twyla-raw-html">`
    // — is left untouched, so match the unescaped tag specifically.)
    assert!(
        !diag.contains("<script type=\"x-twyla-raw-html\">"),
        "raw-html marker survived into output",
    );
}

// --- base_url injection --------------------------------------------------

#[test]
fn base_url_is_injected_into_asset_urls() {
    let docs = render();
    let hello = page(&docs, "hello/index.html");
    assert!(
        hello.contains(&format!("href=\"{BASE_URL}/style.css\"")),
        "base_url not injected into stylesheet href",
    );
}

// --- golden diff (full structural fidelity, pre relaxed) ------------------

#[test]
fn hello_matches_golden() {
    let docs = render();
    let actual_html = page(&docs, "hello/index.html");
    let golden_html = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/hello.html"),
    )
    .expect("read tests/golden/hello.html");

    // `<pre>` text-only: syntect highlight spans are cosmetic and churn
    // across typst bumps. Everything else compared structurally.
    let cfg = RelaxConfig::new()
        .relax(Matcher::Tag("pre".to_string()), RelaxationRule::TextOnly);

    let expected = parse_html(&golden_html);
    let actual = parse_html(actual_html);
    if let Err(d) = diff(&expected, &actual, &cfg) {
        panic!(
            "hello/index.html diverged from golden:\n{d}\n\n\
             If this is an intentional change, regenerate with:\n  \
             cargo run -- render --root test_site --base-url {BASE_URL} hello \
             > tests/golden/hello.html",
        );
    }
}

// --- build to disk -------------------------------------------------------

#[test]
fn build_emits_pages_static_and_colocated_assets() {
    let out = tempfile::tempdir().expect("tempdir");
    let summary: BuildSummary = build_run(Build {
        ctx: ctx(),
        output_dir: out.path().to_path_buf(),
    })
    .expect("build test_site");

    assert_eq!(summary.pages, 3, "expected 3 routed pages");

    let exists = |rel: &str| out.path().join(rel).is_file();
    assert!(exists("index.html"), "home not written");
    assert!(exists("hello/index.html"), "hello not written");
    assert!(exists("diagram-demo/index.html"), "diagram-demo not written");
    // static/ copied verbatim.
    assert!(exists("style.css"), "static/style.css not copied");
    // colocated content asset (non-.typ under content/) emitted at root.
    assert!(exists("diagram-demo.svg"), "colocated svg not copied");
    // underscore-prefixed draft never compiled → no route written.
    assert!(!exists("draft/index.html"), "draft page was written");
}

// =========================================================================
// Pending — features twyla doesn't implement yet. Each is `#[ignore]`d and
// fails today; un-ignore (and add the fixture content noted) as the
// feature lands. This is the asset-work to-do list in executable form.
// =========================================================================

/// Nested sections: `content/<section>/<page>.typ` should route to
/// `<section>/<page>/index.html`. Blocked on `scan_pages` recursion
/// (`project.rs` — "no subdirectory recursion yet"). To enable: add
/// `test_site/content/notes/_index.typ` + `notes/first.typ`, then
/// un-ignore.
#[test]
#[ignore = "nested-section routing not implemented (scan_pages is top-level only)"]
fn nested_section_routes_under_subdir() {
    let docs = render();
    let paths: Vec<_> = docs.iter().map(|d| d.path.clone()).collect();
    assert!(
        paths.contains(&PathBuf::from("notes/first/index.html")),
        "nested section not routed; have {paths:?}",
    );
}

/// Asset fingerprinting: an `asset-url(..)` primitive should emit images
/// at content-hashed paths and rewrite references to match. Blocked on
/// the asset-url primitive + resolution-pass registry.
#[test]
#[ignore = "asset fingerprinting not implemented (no asset-url primitive yet)"]
fn referenced_image_is_fingerprinted() {
    let out = tempfile::tempdir().expect("tempdir");
    build_run(Build { ctx: ctx(), output_dir: out.path().to_path_buf() })
        .expect("build");
    // Expect e.g. assets/diagram-demo.<hash>.svg rather than a verbatim copy.
    let assets = out.path().join("assets");
    assert!(assets.is_dir(), "no assets/ output dir — fingerprinting not wired");
}

/// Feed generation: `twyla build` should emit an Atom/RSS feed listing
/// the same `<twyla-post>` entries the home page enumerates. Blocked on
/// feed generation (needs base_url, which test_site already provides).
#[test]
#[ignore = "feed generation not implemented yet"]
fn build_emits_feed_with_post_entries() {
    let out = tempfile::tempdir().expect("tempdir");
    build_run(Build { ctx: ctx(), output_dir: out.path().to_path_buf() })
        .expect("build");
    let feed = out.path().join("atom.xml");
    assert!(feed.is_file(), "no atom.xml emitted");
    let body = std::fs::read_to_string(&feed).expect("read feed");
    assert!(body.contains("Hello, twyla"), "feed missing post entry");
}
