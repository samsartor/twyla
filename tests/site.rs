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
use twyla::diff::{Matcher, RelaxConfig, RelaxationRule, diff};
use twyla::html::parse_html;
use twyla::project::TwylaContext;
use twyla::render::{Outputs, render_site};

const BASE_URL: &str = "https://example.com";

fn test_site() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test_site")
}

fn ctx() -> TwylaContext {
    TwylaContext::new(test_site(), Some(BASE_URL.to_string()))
        .expect("construct TwylaContext for test_site")
}

fn render() -> Outputs {
    render_site(&ctx()).expect("render test_site")
}

/// HTML of the doc routed to `key` (e.g. `hello/index.html`).
fn page<'a>(outputs: &'a Outputs, key: &str) -> &'a str {
    outputs
        .docs()
        .find(|d| d.output_path == key)
        .unwrap_or_else(|| {
            let have: Vec<_> = outputs.docs().map(|d| d.output_path.clone()).collect();
            panic!("no doc at {key:?}; have {have:?}")
        })
        .html
        .as_str()
}

// --- routing -------------------------------------------------------------

#[test]
fn routes_every_content_page_and_skips_underscores() {
    let outputs = render();
    let paths: Vec<String> = outputs.docs().map(|d| d.output_path.clone()).collect();

    for expected in ["index.html", "hello/index.html", "diagram-demo/index.html"] {
        assert!(
            paths.iter().any(|p| p == expected),
            "missing route {expected:?}; have {paths:?}",
        );
    }
    // `_picture.typ` is underscore-prefixed → never scanned, never routed.
    assert!(
        !paths.iter().any(|p| p.starts_with("picture")),
        "underscore-prefixed file leaked into routes: {paths:?}",
    );
}

// --- home page: cross-document enumeration -------------------------------

#[test]
fn home_enumerates_posts_newest_first() {
    let docs = render();
    let home = page(&docs, "index.html");
    println!("{}", home);

    // Both posts appear as links to their permalinks.
    assert!(
        home.contains(&format!("href=\"{BASE_URL}/hello/\"")),
        "home missing hello link"
    );
    assert!(
        home.contains(&format!("href=\"{BASE_URL}/diagram-demo/\"")),
        "home missing diagram-demo link",
    );

    // Sorted by date descending: diagram-demo (05-24) before hello (05-20).
    let diag = home
        .find("/diagram-demo/")
        .expect("diagram-demo link present");
    let hello = home.find("/hello/").expect("hello link present");
    assert!(
        diag < hello,
        "posts not newest-first (diagram-demo should precede hello)"
    );
}

// --- headings, slugs, intra-doc anchors ----------------------------------

#[test]
fn headings_get_slug_ids() {
    let docs = render();
    let hello = page(&docs, "hello/index.html");
    assert!(
        hello.contains("<h2 id=\"prose-and-marks\">"),
        "missing slugified h2 id"
    );
    assert!(
        hello.contains("<h2 id=\"anchors\">"),
        "missing anchors h2 id"
    );
}

#[test]
fn intra_doc_anchor_is_fragment_only() {
    let docs = render();
    let hello = page(&docs, "hello/index.html");
    // Label-based `#link(<anchors>)` resolves to a fragment, not an
    // absolutized URL — the cross-doc-leak regression guard from the
    // render smoke test, here on an independent site.
    assert!(
        hello.contains("href=\"#anchors\""),
        "intra-doc link not fragment-only"
    );
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
    assert!(
        diag.contains("viewBox=\"0 0 120 60\""),
        "svg attributes missing"
    );
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
    let cfg = RelaxConfig::new().relax(Matcher::Tag("pre".to_string()), RelaxationRule::TextOnly);

    let expected = cfg.normalize(&parse_html(&golden_html));
    let actual = cfg.normalize(&parse_html(actual_html));
    if let Err(d) = diff(&expected, &actual) {
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
fn build_emits_pages_and_static() {
    let out = tempfile::tempdir().expect("tempdir");
    let summary: BuildSummary = build_run(Build {
        ctx: ctx(),
        output_dir: out.path().to_path_buf(),
    })
    .expect("build test_site");

    // 3 base fixtures + 2 spike-doc pages + inline-document page & its hoisted
    // child + context-documents page & one generated page per post (hello +
    // diagram-demo) = 10.
    assert_eq!(summary.pages, 10, "expected 10 routed pages");

    let exists = |rel: &str| out.path().join(rel).is_file();
    assert!(exists("index.html"), "home not written");
    assert!(exists("hello/index.html"), "hello not written");
    assert!(
        exists("diagram-demo/index.html"),
        "diagram-demo not written"
    );
    // static/ copied verbatim.
    assert!(exists("style.css"), "static/style.css not copied");
    // Colocated content (non-source under content/) is not shipped — files
    // belong in static/. The svg under content/ is *not* emitted at root.
    assert!(
        !exists("diagram-demo.svg"),
        "colocated content svg should not be emitted"
    );
    // underscore-prefixed draft never compiled → no route written.
    assert!(!exists("draft/index.html"), "draft page was written");
}

/// End-to-end asset pipeline: `asset.sass` compiles SCSS → CSS via grass,
/// `asset.file` copies verbatim, both fingerprinted, and `twyla build` emits
/// them under `assets/`.
#[test]
fn build_emits_fingerprinted_sass_and_file_assets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir(root.join("content")).unwrap();
    std::fs::write(
        root.join("content/main.typ"),
        "#context asset.sass(\"main.scss\").url() \
         #context asset.file(\"robots.txt\").url()",
    )
    .unwrap();
    // Nested SCSS — proves grass actually compiled (nesting gets flattened).
    std::fs::write(
        root.join("content/main.scss"),
        ".card { color: red; .title { font-weight: bold; } }",
    )
    .unwrap();
    std::fs::write(root.join("content/robots.txt"), "User-agent: *\n").unwrap();

    let out = tempfile::tempdir().expect("out");
    let ctx = TwylaContext::new(root, Some(BASE_URL.to_string())).expect("ctx");
    let summary = build_run(Build {
        ctx,
        output_dir: out.path().to_path_buf(),
    })
    .expect("build");

    assert_eq!(summary.assets, 2, "expected 2 processed assets");

    let assets_dir = out.path().join("assets");
    let files: Vec<_> = std::fs::read_dir(&assets_dir)
        .expect("assets/ dir written")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();

    let css = files
        .iter()
        .find(|f| f.starts_with("main-") && f.ends_with(".css"))
        .unwrap_or_else(|| panic!("no fingerprinted main-*.css in {files:?}"));
    let css_body = std::fs::read_to_string(assets_dir.join(css)).unwrap();
    assert!(
        css_body.contains(".card .title"),
        "SCSS nesting not compiled by grass; got:\n{css_body}",
    );

    assert!(
        files
            .iter()
            .any(|f| f.starts_with("robots-") && f.ends_with(".txt")),
        "verbatim file asset not emitted: {files:?}",
    );
}

// =========================================================================
// Pending — features twyla doesn't implement yet. Each is `#[ignore]`d and
// fails today; un-ignore (and add the fixture content noted) as the
// feature lands. This is the asset-work to-do list in executable form.
// =========================================================================

/// Nested sections: `content/<section>/<page>.typ` should route to
/// `<section>/<page>/index.html`. Blocked on `scan_pages` recursion
/// (`project.rs` — "no subdirectory recursion yet"). To enable: add
/// `test_site/content/notes/main.typ` + `notes/first.typ`, then
/// un-ignore.
#[test]
#[ignore = "nested-section routing not implemented (scan_pages is top-level only)"]
fn nested_section_routes_under_subdir() {
    let outputs = render();
    let paths: Vec<String> = outputs.docs().map(|d| d.output_path.clone()).collect();
    assert!(
        paths.iter().any(|p| p == "notes/first/index.html"),
        "nested section not routed; have {paths:?}",
    );
}

/// The native image rule: a plain `#image("pic.svg")` (no `#context`) emits a
/// fingerprinted asset and rewrites `<img src>` to its base_url-prefixed URL —
/// not upstream's base64 inline, not a verbatim path, not a leaked placeholder.
#[test]
fn referenced_image_is_fingerprinted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir(root.join("content")).unwrap();
    // Bare markup (renders to index.html); the rule fires without `#context`.
    std::fs::write(
        root.join("content/main.typ"),
        "#image(\"pic.svg\", alt: \"a red square\")",
    )
    .unwrap();
    // A minimal valid SVG (usvg needs intrinsic size); path-source → File asset.
    std::fs::write(
        root.join("content/pic.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\">\
         <rect width=\"4\" height=\"4\" fill=\"red\"/></svg>",
    )
    .unwrap();

    let out = tempfile::tempdir().expect("out");
    let ctx = TwylaContext::new(root, Some(BASE_URL.to_string())).expect("ctx");
    let summary = build_run(Build {
        ctx,
        output_dir: out.path().to_path_buf(),
    })
    .expect("build");

    assert_eq!(summary.assets, 1, "expected the image to be processed once");

    // Emitted as a fingerprinted file: assets/pic-<hash>.svg.
    let assets_dir = out.path().join("assets");
    let files: Vec<_> = std::fs::read_dir(&assets_dir)
        .expect("assets/ dir written")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert!(
        files
            .iter()
            .any(|f| f.starts_with("pic-") && f.ends_with(".svg")),
        "no fingerprinted pic-*.svg emitted: {files:?}",
    );

    // <img src> points at that asset's base_url-prefixed URL; alt passes
    // through; no base64 inline and no unresolved placeholder.
    let html = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");
    assert!(
        html.contains(&format!("{BASE_URL}/assets/pic-")) && html.contains(".svg\""),
        "img src not rewritten to a fingerprinted asset URL:\n{html}",
    );
    assert!(
        html.contains("alt=\"a red square\""),
        "alt not passed through:\n{html}"
    );
    assert!(
        !html.contains("data:image"),
        "image was base64-inlined:\n{html}"
    );
    assert!(
        !html.contains("__twyla-asset-pending__"),
        "asset placeholder leaked (non-convergence):\n{html}",
    );
}

/// Feed generation: `twyla build` should emit an Atom/RSS feed listing
/// the same `<twyla-post>` entries the home page enumerates. Blocked on
/// feed generation (needs base_url, which test_site already provides).
#[test]
#[ignore = "feed generation not implemented yet"]
fn build_emits_feed_with_post_entries() {
    let out = tempfile::tempdir().expect("tempdir");
    build_run(Build {
        ctx: ctx(),
        output_dir: out.path().to_path_buf(),
    })
    .expect("build");
    let feed = out.path().join("atom.xml");
    assert!(feed.is_file(), "no atom.xml emitted");
    let body = std::fs::read_to_string(&feed).expect("read feed");
    assert!(body.contains("Hello, twyla"), "feed missing post entry");
}

// --- SPIKE: overloaded `document` element --------------------------------

/// The overloaded `document` binding: `#set document(extra: ..)` targets
/// twyla's element, and `#context document.extra` reads it back off the
/// style chain (access_field → field_from_styles). Also confirms routing
/// still works through the rebound native `__std_document`.
#[test]
fn spike_overloaded_document_contextual_read() {
    let docs = render();
    let html = page(&docs, "spike-doc/index.html");
    assert!(
        html.contains("extra-color=blue"),
        "contextual extra.color not read: {html}"
    );
    assert!(
        html.contains("extra-tag=spike"),
        "contextual extra.tag not read: {html}"
    );
    assert!(
        html.contains("draft=false"),
        "contextual draft default not read: {html}"
    );
}

/// An inline `#document(output:)[body]` constructor. The body is hoisted into
/// its own bundle output and the surrounding prose renders without it
/// (`before ... after`, not the child text). Metadata passed to the call is
/// plumbed onto the child so `#context document.*` resolves there.
#[test]
fn inline_document_hoists_to_own_output() {
    let docs = render();

    // Parent page: prose survives, child content gone (rendered to nothing).
    let parent = page(&docs, "inline-document/index.html");
    assert!(parent.contains("before"), "parent prose missing: {parent}");
    assert!(parent.contains("after"), "parent prose missing: {parent}");
    assert!(
        !parent.contains("child-body-text"),
        "inline document body leaked into the parent page: {parent}"
    );
    assert!(
        !parent.contains("Child Heading"),
        "inline document heading leaked into the parent page: {parent}"
    );

    // Child page: emitted as its own output, carrying the hoisted body and the
    // metadata plumbed through the call (contextual `document.extra`). Its
    // relative `output: "inline-child/index.html"` resolves under the parent
    // page's output dir (`inline-document/`).
    let child = page(&docs, "inline-document/inline-child/index.html");
    assert!(
        child.contains("child-body-text"),
        "hoisted child body missing: {child}"
    );
    assert!(
        child.contains("<h2 id=\"child-heading\">"),
        "hoisted child heading missing/not realized as a real document: {child}"
    );
    assert!(
        child.contains("child-extra=hoisted"),
        "metadata not plumbed onto hoisted child (contextual extra): {child}"
    );
}

/// `#context`-generated documents: a page paginating over `documents()` emits
/// extra bundle outputs that don't exist at eval time. Twyla's discovery loop
/// must surface them, route each to its own output, and converge.
#[test]
fn context_generated_documents_are_discovered_and_routed() {
    let docs = render();

    // hello.typ is the one `kind: "post"` fixture → one generated page, keyed
    // by the post's url: `ctx` + `/hello/` + `index.html`. The relative output
    // resolves under the generator page's output dir (`context-documents/`).
    let generated = page(&docs, "context-documents/ctx/hello/index.html");
    assert!(
        generated.contains("ctx-summary-for"),
        "context-generated document body missing: {generated}"
    );
    assert!(
        generated.contains("Hello, twyla"),
        "generated doc did not receive the post title via documents(): {generated}"
    );
    assert!(
        generated.contains("<h2 id=\"context-summary\">"),
        "generated doc not realized as a real document (no heading slug): {generated}"
    );

    // The generator page itself still renders (its `#context` block lays out as
    // nothing, since each `document(..)` is hoisted away).
    let generator = page(&docs, "context-documents/index.html");
    assert!(
        generator.contains("Context Documents"),
        "generator page prose missing: {generator}"
    );
    assert!(
        !generator.contains("ctx-summary-for"),
        "generated child leaked into the generator page: {generator}"
    );
}

/// The decisive harvest test: read each page's `document.extra`/`draft`
/// per-document from its body's resolved style chain — even though the
/// `#set document(..)` lives inside the included file. Proves the
/// `documents` array can be built without forking `bundle_impl` and
/// without the redundant explicit-field metadata the old site needed.
#[test]
fn spike_harvest_reads_per_doc_extra() {
    use twyla::document::ResolvedDocument;
    use twyla::render::RenderWorld;
    use twyla::resolver::Resolver;

    let ctx = ctx();
    let world = RenderWorld::new(&ctx).expect("world");
    // Harvest shares the single eval/compile with the rendered HTML; the
    // metadata now rides on each `OutputDoc.meta`.
    let outputs = world
        .compile_bundle(&mut Resolver::new(&ctx))
        .expect("compile+harvest");
    let docs: Vec<&ResolvedDocument> = outputs.docs().filter_map(|d| d.meta.as_ref()).collect();
    let spike = docs
        .iter()
        .find(|d| d.output == "spike-doc/index.html")
        .unwrap_or_else(|| panic!("spike-doc not harvested; got {docs:#?}"));
    let extra = format!("{:?}", spike.extra);
    assert!(
        extra.contains("blue"),
        "extra.color not harvested: {spike:#?}"
    );
    assert!(
        extra.contains("spike"),
        "extra.tag not harvested: {spike:#?}"
    );
    assert_eq!(spike.draft, false, "draft default wrong: {spike:#?}");

    // Per-doc isolation: a second page sets a different extra + draft:true.
    let spike2 = docs
        .iter()
        .find(|d| d.output == "spike-doc-2/index.html")
        .unwrap_or_else(|| panic!("spike-doc-2 not harvested; got {docs:#?}"));
    assert!(
        format!("{:?}", spike2.extra).contains("red"),
        "doc2 extra bled: {spike2:#?}"
    );
    assert!(
        !format!("{:?}", spike2.extra).contains("blue"),
        "doc1 extra bled into doc2: {spike2:#?}"
    );
    assert_eq!(spike2.draft, true, "doc2 draft not harvested: {spike2:#?}");

    // A post page harvests its standard fields + derived kind.
    let hello = docs
        .iter()
        .find(|d| d.output == "hello/index.html")
        .unwrap_or_else(|| panic!("hello not harvested; got {docs:#?}"));
    assert_eq!(hello.kind, "post", "hello kind not harvested: {hello:#?}");
    assert!(hello.date.is_some(), "hello date not harvested: {hello:#?}");
    assert_eq!(
        format!("{:?}", hello.extra),
        "None",
        "unset extra not default: {hello:#?}"
    );
    assert_eq!(hello.draft, false, "unset draft not default: {hello:#?}");
}
