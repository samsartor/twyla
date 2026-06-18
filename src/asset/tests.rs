// See the same allow in `mod.rs`: `AssetSpec` is a content-hashed key whose
// `Bytes` only *looks* interior-mutable to clippy (`Arc` refcount).
#![allow(clippy::mutable_key_type)]

use std::collections::HashMap;
use std::time::Duration;

use crossbeam_channel::unbounded;
use typst::foundations::{Bytes, Style, Value};
use typst::syntax::{FileId, RootedPath, VirtualPath, VirtualRoot};
use typst::utils::LazyHash;
use typst_utils::hash128;

use super::*;
use crate::project::TwylaContext;
use crate::render::{Emit, RenderWorld};
use crate::resolver::Resolver;

fn site(files: &[(&str, &str)]) -> (TwylaContext, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("content")).unwrap();
    for (rel, contents) in files {
        std::fs::write(dir.path().join(rel), contents).unwrap();
    }
    let ctx = TwylaContext::new(dir.path(), Some("https://example.com".into())).unwrap();
    (ctx, dir)
}

/// First-page HTML + resolved assets from one compile of `world` (fresh
/// resolver — for tests that don't exercise cross-compile persistence).
fn compile(world: &RenderWorld) -> (String, Vec<ResolvedAsset>) {
    let mut resolver = Resolver::new(&world.ctx);
    let outputs = world.compile_bundle(&mut resolver).unwrap();
    let html = outputs.docs().next().unwrap().html.clone();
    (html, outputs.assets().cloned().collect())
}

fn fid(path: &str) -> FileId {
    FileId::new(RootedPath::new(
        VirtualRoot::Project,
        VirtualPath::new(path).unwrap(),
    ))
}

/// The core of Phase 0/1: an `asset.*().url()` call resolves to a fingerprinted
/// URL through the relayout loop, with no placeholder left behind.
#[test]
fn cold_build_resolves_asset_url_through_the_loop() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.file(\"logo.svg\").url()",
        ),
        ("content/logo.svg", "<svg/>"),
    ]);
    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(
        assets.len(),
        1,
        "expected one resolved asset, got {assets:?}"
    );
    let url = assets[0].output_path.as_str();
    assert!(
        url.starts_with("assets/logo-") && url.ends_with(".svg"),
        "unexpected auto-named url: {url}",
    );
    assert!(
        html.contains(url),
        "page missing resolved url {url}:\n{html}"
    );
    assert!(
        !html.contains("__twyla-asset-pending__"),
        "placeholder leaked into output:\n{html}",
    );
}

/// Two assets on one page both resolve, and `sass` fingerprints to `.css`.
#[test]
fn multiple_assets_resolve_and_sass_renames_to_css() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.file(\"a.txt\").url() #context asset.sass(\"b.scss\").url()",
        ),
        ("content/a.txt", "hello"),
        ("content/b.scss", "a{b:c}"),
    ]);
    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(assets.len(), 2, "got {assets:?}");
    assert!(
        assets
            .iter()
            .any(|a| a.output_path.starts_with("assets/a-") && a.output_path.ends_with(".txt")),
        "missing a.txt asset: {assets:?}",
    );
    assert!(
        assets
            .iter()
            .any(|a| a.output_path.starts_with("assets/b-") && a.output_path.ends_with(".css")),
        "sass asset not renamed to .css: {assets:?}",
    );
    assert!(
        !html.contains("__twyla-asset-pending__"),
        "placeholder leaked:\n{html}"
    );
}

/// Encode a solid-color `width`×`height` PNG (test source images).
fn png(width: u32, height: u32) -> Vec<u8> {
    let img = ::image::RgbImage::from_pixel(width, height, ::image::Rgb([10, 120, 200]));
    let mut buf = Vec::new();
    ::image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), ::image::ImageFormat::Png)
        .unwrap();
    buf
}

/// `asset.image` resizes aspect-preserving *and* transcodes: a 100×50 source
/// asked for `width: 40, format: "webp"` resolves to a `.webp` url whose bytes
/// decode to a 40×20 WebP (the height follows from the aspect ratio).
#[test]
fn image_resizes_and_transcodes() {
    let (ctx, dir) = site(&[(
        "content/main.typ",
        "#context asset.image(\"photo.png\", width: 40, format: \"webp\").url()",
    )]);
    std::fs::write(dir.path().join("content/photo.png"), png(100, 50)).unwrap();

    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert_eq!(assets.len(), 1, "got {assets:?}");
    let asset = &assets[0];
    assert!(
        asset.output_path.starts_with("assets/photo-") && asset.output_path.ends_with(".webp"),
        "unexpected output path: {}",
        asset.output_path,
    );
    assert!(html.contains(&asset.url), "page missing resolved url:\n{html}");

    let bytes = asset.built.emit.read().unwrap();
    assert_eq!(
        ::image::guess_format(&bytes).unwrap(),
        ::image::ImageFormat::WebP,
        "output should be WebP",
    );
    let decoded = ::image::load_from_memory(&bytes).unwrap();
    assert_eq!(
        (decoded.width(), decoded.height()),
        (40, 20),
        "expected aspect-preserved 40×20",
    );
}

/// With no `format`, the source format is kept; with no dimensions, the image
/// is only transcoded. Here: keep PNG, resize to a 30px-wide box.
#[test]
fn image_keeps_source_format_when_unspecified() {
    let (ctx, dir) = site(&[(
        "content/main.typ",
        "#context asset.image(\"photo.png\", width: 30).url()",
    )]);
    std::fs::write(dir.path().join("content/photo.png"), png(60, 60)).unwrap();

    let (_html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    let asset = &assets[0];
    assert!(asset.output_path.ends_with(".png"), "kept source format: {}", asset.output_path);
    let decoded = ::image::load_from_memory(&asset.built.emit.read().unwrap()).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (30, 30));
}

/// `fit: "cover"` crops to fill, so it needs both dimensions — one alone is a
/// build error pointing at the call site.
#[test]
fn cover_requires_both_dimensions() {
    let (ctx, dir) = site(&[(
        "content/main.typ",
        "#context asset.image(\"photo.png\", width: 40, fit: \"cover\").url()",
    )]);
    std::fs::write(dir.path().join("content/photo.png"), png(100, 50)).unwrap();

    let err = RenderWorld::new(&ctx)
        .unwrap()
        .compile_bundle(&mut Resolver::new(&ctx))
        .err()
        .expect("cover with one dimension should fail");
    assert!(
        err.to_string().contains("needs both"),
        "expected a 'needs both' error, got:\n{err}"
    );
}

/// A native/markdown image (`#image("x.png")` / `![](x.png)`) routes through
/// the `asset.image` pipeline: the `<img>` gets a fingerprinted src *and* the
/// output's intrinsic `width`/`height` attrs (no `#context` needed — the rule
/// resolves off its live styles).
#[test]
fn native_image_routes_through_pipeline_with_dimensions() {
    let (ctx, dir) = site(&[("content/main.typ", "#image(\"photo.png\")")]);
    std::fs::write(dir.path().join("content/photo.png"), png(80, 40)).unwrap();

    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert_eq!(assets.len(), 1, "got {assets:?}");
    assert!(
        assets[0].output_path.ends_with(".png"),
        "kept source format: {}",
        assets[0].output_path
    );
    assert!(
        html.contains("width=\"80\"") && html.contains("height=\"40\""),
        "expected intrinsic dimensions on <img>:\n{html}"
    );
    assert!(html.contains(&assets[0].url), "page missing resolved url:\n{html}");
}

/// `#set asset.image(format: "webp")` reaches *native* images too: the rule
/// reads the processing defaults off the chain, so the whole scope's images
/// transcode without touching the markdown.
#[test]
fn set_rule_transcodes_native_images() {
    let (ctx, dir) = site(&[(
        "content/main.typ",
        "#set asset.image(format: \"webp\")\n#image(\"photo.png\")",
    )]);
    std::fs::write(dir.path().join("content/photo.png"), png(50, 50)).unwrap();

    let (_html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert!(
        assets[0].output_path.ends_with(".webp"),
        "native image should pick up the set-rule format: {}",
        assets[0].output_path
    );
}

/// Vector images can't be raster-processed, so a native SVG stays a verbatim
/// fingerprinted copy with no intrinsic-dimension attrs.
#[test]
fn native_vector_image_stays_verbatim() {
    let (ctx, dir) = site(&[("content/main.typ", "#image(\"logo.svg\")")]);
    std::fs::write(
        dir.path().join("content/logo.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"></svg>",
    )
    .unwrap();

    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert!(
        assets[0].output_path.ends_with(".svg"),
        "svg copied verbatim: {}",
        assets[0].output_path
    );
    assert!(
        !html.contains("width=\"10\""),
        "vector image should not get raster dimension attrs:\n{html}"
    );
}

/// A failed asset build blames the asset call site instead of `<detached>`:
/// the span threaded through [`AssetRequest`] reaches [`file::build`], so a
/// missing file's diagnostic names the source file it was referenced from.
#[test]
fn missing_asset_blames_the_call_site() {
    let (ctx, _dir) = site(&[(
        "content/main.typ",
        "#context asset.file(\"nope.svg\").url()",
    )]);
    let world = RenderWorld::new(&ctx).unwrap();
    let mut resolver = Resolver::new(&world.ctx);
    let err = world
        .compile_bundle(&mut resolver)
        .err()
        .expect("a missing asset file should fail the build");
    // The rendered diagnostic includes the source snippet header (the file name)
    // only when the span resolves — a detached span would omit it.
    let msg = err.to_string();
    assert!(
        msg.contains("main.typ"),
        "asset error should point at the call site's source file, got:\n{msg}"
    );
    // ...and the message names the absolute path it tried to read (`content/`
    // appears only in the joined path, not in the `"nope.svg"` source snippet).
    assert!(
        msg.contains("content/nope.svg"),
        "asset error should name the absolute path it tried, got:\n{msg}"
    );
}

/// `.read()` resolves an asset's output through the same loop: a file's
/// contents inline verbatim (default UTF-8 `str`, no wrapper), and a sass asset
/// inlines its *compiled* CSS as raw `bytes` (`encoding: none`). The asset is
/// still registered for emission, and no placeholder (empty miss) survives.
#[test]
fn read_inlines_asset_content_through_the_loop() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.file(\"snippet.txt\").read() \
             #context str(asset.sass(\"theme.scss\").read(encoding: none))",
        ),
        ("content/snippet.txt", "HELLO-INLINE-CONTENT"),
        ("content/theme.scss", "a { b { color: red; } }"),
    ]);
    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert!(
        html.contains("HELLO-INLINE-CONTENT"),
        "default-str read not inlined:\n{html}"
    );
    // grass expands the nested rule into a real selector — proof we inlined the
    // *compiled* CSS, not the raw scss source (which has no `a b` selector).
    assert!(
        html.contains("a b"),
        "encoding:none sass bytes not inlined:\n{html}"
    );
    assert_eq!(assets.len(), 2, "both assets still registered: {assets:?}");
    assert!(
        !html.contains("__twyla-asset-pending__"),
        "placeholder leaked:\n{html}"
    );
}

/// The predicted pairing: `raw-html(asset.*.read())` splices an asset's resolved
/// contents into the page unescaped. The default-`str` read feeds `raw-html`
/// with no wrapper; the raw-html post-pass strips its placeholder wrapper, so
/// the `<svg>` lands verbatim (not escaped, not a `<script>`).
#[test]
fn raw_html_inlines_a_read_asset() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context raw-html(asset.file(\"icon.svg\").read())",
        ),
        ("content/icon.svg", "<svg><circle/></svg>"),
    ]);
    let (html, _assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert!(
        html.contains("<svg><circle/></svg>"),
        "svg not spliced verbatim:\n{html}"
    );
    assert!(
        !html.contains("x-twyla-raw-html"),
        "raw-html wrapper not stripped:\n{html}"
    );
    assert!(
        !html.contains("__twyla-asset-pending__"),
        "placeholder leaked:\n{html}"
    );
}

/// `#set asset.sass(minify: false)` reaches the asset spec. The migrated
/// `asset.sass` element carries `minify` as a *settable* field, so a set rule
/// flips grass to its expanded style — proving set-rule support, the point of
/// making assets elements. (Compressed default would emit `color:red`.)
#[test]
fn set_rule_toggles_sass_minify() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#set asset.sass(minify: false)\n\
             #context str(asset.sass(\"theme.scss\").read(encoding: none))",
        ),
        ("content/theme.scss", "a { color: red; }"),
    ]);
    let (html, _assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert!(
        html.contains("color: red"),
        "set asset.sass(minify: false) did not produce expanded CSS:\n{html}"
    );
    assert!(
        !html.contains("__twyla-asset-pending__"),
        "placeholder leaked:\n{html}"
    );
}

/// A bare asset element left in markup (never resolved via `.url()`/`.read()`)
/// reaches realization and is refused by the default show rule with a helpful
/// error, rather than mis-rendering.
#[test]
fn bare_asset_in_markup_is_refused() {
    let (ctx, _dir) = site(&[
        ("content/main.typ", "#asset.file(\"logo.svg\")"),
        ("content/logo.svg", "<svg/>"),
    ]);
    let world = RenderWorld::new(&ctx).unwrap();
    let mut resolver = Resolver::new(&world.ctx);
    let err = world
        .compile_bundle(&mut resolver)
        .err()
        .expect("a bare asset in markup should error")
        .to_string();
    assert!(
        err.contains("cannot be shown directly"),
        "expected a show-refusal error, got: {err}"
    );
}

/// Two *separate* resolvers in one process (sharing comemo's global cache) must
/// each resolve independently — the per-resolver epoch base prevents one from
/// hitting the other's cached placeholder.
#[test]
fn separate_compiles_each_resolve() {
    let files: &[(&str, &str)] = &[
        (
            "content/index.typ",
            "#context link(asset.file(\"logo.svg\").url())[logo]",
        ),
        ("content/logo.svg", "<svg/>"),
    ];
    let (ctx_a, _a) = site(files);
    let (html_a, _) = compile(&RenderWorld::new(&ctx_a).unwrap());
    assert!(
        !html_a.contains("__twyla-asset-pending__"),
        "compile A leaked:\n{html_a}"
    );

    let (ctx_b, _b) = site(files);
    let (html_b, _) = compile(&RenderWorld::new(&ctx_b).unwrap());
    assert!(
        !html_b.contains("__twyla-asset-pending__"),
        "compile B leaked:\n{html_b}"
    );
}

/// Editing a sass source and recompiling the *same* (persistent) world must
/// re-resolve to a new fingerprint — proving mtime revalidation evicts the
/// stale entry and the epoch bump lets re-discovery through even when the map
/// returns to a previously-cached (empty) state.
#[test]
fn editing_source_revalidates_to_new_fingerprint() {
    let (ctx, dir) = site(&[
        (
            "content/main.typ",
            "#context asset.sass(\"main.scss\").url()",
        ),
        ("content/main.scss", ".a { color: red; }"),
    ]);
    let mut world = RenderWorld::new(&ctx).unwrap();
    // One resolver reused across both compiles — the persistent-store path.
    let mut resolver = Resolver::new(&ctx);

    let outputs1 = world.compile_bundle(&mut resolver).unwrap();
    let url1 = outputs1.assets().next().unwrap().output_path.clone();

    // Edit the source (distinct mtime), then recompile the same world the way
    // `serve` does: reset the FileStore + age comemo.
    std::thread::sleep(Duration::from_millis(10));
    std::fs::write(dir.path().join("content/main.scss"), ".a { color: blue; }").unwrap();
    comemo::evict(0);
    world.files.reset();

    let outputs2 = world.compile_bundle(&mut resolver).unwrap();
    let html2 = outputs2.docs().next().unwrap().html.clone();
    let url2 = outputs2.assets().next().unwrap().output_path.clone();

    assert_ne!(
        url1, url2,
        "fingerprint did not change after editing the source"
    );
    assert!(
        !html2.contains("__twyla-asset-pending__"),
        "placeholder leaked after edit:\n{html2}"
    );
}

/// The document half of the warm path: an inline `#document(..)` discovered via
/// the sink persists across rebuilds, and is re-collected with a *fresh body*
/// when its source file changes (resolver reused; FileStore reset + comemo aged,
/// exactly as `serve` does). Guards the per-source document eviction in
/// `revalidate`.
#[test]
fn editing_source_revalidates_inline_document() {
    let (ctx, dir) = site(&[(
        "content/main.typ",
        "before #document(output: \"child/index.html\")[v1-body] after",
    )]);
    let mut world = RenderWorld::new(&ctx).unwrap();
    // One resolver across both compiles — the persistent document-store path.
    let mut resolver = Resolver::new(&ctx);

    let outputs1 = world.compile_bundle(&mut resolver).unwrap();
    let child1 = outputs1
        .docs()
        .find(|d| d.output_path == "child/index.html")
        .expect("inline child emitted on first compile")
        .html
        .clone();
    assert!(child1.contains("v1-body"), "first child body missing:\n{child1}");

    // Edit the source (distinct mtime), then recompile the same world like serve.
    std::thread::sleep(Duration::from_millis(10));
    std::fs::write(
        dir.path().join("content/main.typ"),
        "before #document(output: \"child/index.html\")[v2-body] after",
    )
    .unwrap();
    comemo::evict(0);
    world.files.reset();

    let outputs2 = world.compile_bundle(&mut resolver).unwrap();
    let child2 = outputs2
        .docs()
        .find(|d| d.output_path == "child/index.html")
        .expect("inline child still emitted after edit")
        .html
        .clone();
    assert!(
        child2.contains("v2-body"),
        "inline document body did not update on warm rebuild:\n{child2}"
    );
    assert!(
        !child2.contains("v1-body"),
        "stale inline document body survived warm rebuild:\n{child2}"
    );
}

/// `asset.typst` with an inline content value compiles to SVG under a stock
/// library and inlines via `raw-html` — the README's `circle()` icon case. The
/// content is built in the page's (twyla) library but laid out as stock typst.
#[test]
fn typst_inline_content_compiles_to_svg() {
    let (ctx, _dir) = site(&[(
        "content/main.typ",
        "#context raw-html(asset.typst(circle(fill: blue, radius: 6pt), format: \"svg\").read())",
    )]);
    let (html, _assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert!(html.contains("<svg"), "inline content not rendered to svg:\n{html}");
    assert!(
        !html.contains("__twyla-asset-pending__"),
        "placeholder leaked:\n{html}"
    );
}

/// `asset.typst` with a path source compiles a standalone `.typ` file (here to
/// PDF). The source lives at the project root, *not* under `content/`, so it is
/// an asset, not a page. Proves the path branch: the real file compiles as
/// `main` through the delegating loader.
#[test]
fn typst_path_source_compiles_to_pdf() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.typst(\"/doc.typ\", format: \"pdf\").url()",
        ),
        ("doc.typ", "= A Standalone Document\n\nWith a paragraph of text."),
    ]);
    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(assets.len(), 1, "got {assets:?}");
    let asset = &assets[0];
    assert!(
        asset.output_path.starts_with("assets/doc-") && asset.output_path.ends_with(".pdf"),
        "unexpected output path: {}",
        asset.output_path,
    );
    assert!(html.contains(&asset.url), "page missing resolved url:\n{html}");
    let bytes = asset.built.emit.read().unwrap();
    assert!(
        bytes.starts_with(b"%PDF"),
        "output is not a PDF (starts with {:?})",
        &bytes[..bytes.len().min(8)],
    );
}

/// The `png` format renders to a raster image at the requested `ppi`.
#[test]
fn typst_inline_content_compiles_to_png() {
    let (ctx, _dir) = site(&[(
        "content/main.typ",
        "#context asset.typst(circle(fill: red, radius: 10pt), format: \"png\", ppi: 96).url()",
    )]);
    let (_html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    let asset = &assets[0];
    assert!(asset.output_path.ends_with(".png"), "not png: {}", asset.output_path);
    assert_eq!(
        ::image::guess_format(&asset.built.emit.read().unwrap()).unwrap(),
        ::image::ImageFormat::Png,
        "output should be a PNG",
    );
}

/// The `html` format compiles the document like a page and emits an `.html`
/// asset.
#[test]
fn typst_path_source_compiles_to_html() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.typst(\"/frag.typ\", format: \"html\").url()",
        ),
        ("frag.typ", "= Heading\n\nbody text"),
    ]);
    let (_html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    let asset = &assets[0];
    assert!(asset.output_path.ends_with(".html"), "not html: {}", asset.output_path);
    let body = String::from_utf8(asset.built.emit.read().unwrap().to_vec()).unwrap();
    assert!(body.contains("body text"), "html missing body:\n{body}");
}

/// A bare `asset.typst(..)` left in markup is refused by the default show rule
/// (consumed via `.url()`/`.read()` in normal use), like the other assets.
#[test]
fn bare_typst_asset_is_refused() {
    let (ctx, _dir) = site(&[(
        "content/main.typ",
        "#asset.typst(circle(), format: \"svg\")",
    )]);
    let err = RenderWorld::new(&ctx)
        .unwrap()
        .compile_bundle(&mut Resolver::new(&ctx))
        .err()
        .expect("a bare typst asset in markup should error")
        .to_string();
    assert!(
        err.contains("cannot be shown directly"),
        "expected a show-refusal error, got: {err}"
    );
}

/// Editing a typst asset's source file (or an import) revalidates it to a new
/// fingerprint on a warm rebuild — proving the sub-compile's file reads are
/// harvested into `upstream` and drive invalidation, like sass partials.
#[test]
fn editing_typst_source_revalidates_to_new_fingerprint() {
    let (ctx, dir) = site(&[
        (
            "content/main.typ",
            "#context asset.typst(\"/doc.typ\", format: \"svg\").url()",
        ),
        ("doc.typ", "= First Version"),
    ]);
    let mut world = RenderWorld::new(&ctx).unwrap();
    let mut resolver = Resolver::new(&ctx);

    let url1 = world
        .compile_bundle(&mut resolver)
        .unwrap()
        .assets()
        .next()
        .unwrap()
        .output_path
        .clone();

    std::thread::sleep(Duration::from_millis(10));
    std::fs::write(dir.path().join("doc.typ"), "= A Different Version").unwrap();
    comemo::evict(0);
    world.files.reset();

    let outputs2 = world.compile_bundle(&mut resolver).unwrap();
    let url2 = outputs2.assets().next().unwrap().output_path.clone();
    let html2 = outputs2.docs().next().unwrap().html.clone();

    assert_ne!(url1, url2, "fingerprint did not change after editing the typst source");
    assert!(
        !html2.contains("__twyla-asset-pending__"),
        "placeholder leaked after edit:\n{html2}"
    );
}

/// Hash discipline (the comemo split): the resolved-map hashes by *content* and
/// is order-independent; the sink hashes on *epoch* (constant within a
/// generation, distinct across them).
#[test]
fn map_hashes_by_content_sink_keys_on_epoch() {
    let a = AssetSpec::File {
        file: fid("content/a.css"),
    };
    let b = AssetSpec::Sass {
        file: fid("content/b.scss"),
        minify: false,
    };

    // A minimal resolved record; the map's hash only looks at `(spec,
    // content_hash, url)`, so the emit/upstream fields can be empty here.
    let resolved = |spec: &AssetSpec, url: &str| ResolvedAsset {
        spec: spec.clone(),
        built: Built {
            emit: Emit::Bytes(Bytes::new(Vec::<u8>::new())),
            upstream: Vec::new(),
            content_hash: 0,
            ext: None,
            stem: None,
            dimensions: None,
        },
        output_path: String::from(url),
        url: String::from(url),
    };
    let make = |pairs: &[(&AssetSpec, &str)]| {
        let mut m = HashMap::new();
        for (k, v) in pairs {
            m.insert((*k).clone(), resolved(k, v));
        }
        ResolvedAssets(m)
    };

    let m1 = make(&[(&a, "ua"), (&b, "ub")]);
    let m2 = make(&[(&b, "ub"), (&a, "ua")]);
    assert_eq!(
        hash128(&m1),
        hash128(&m2),
        "resolved-map hash must be order-independent"
    );

    let m3 = make(&[(&a, "CHANGED"), (&b, "ub")]);
    assert_ne!(hash128(&m1), hash128(&m3), "map must track its contents");

    let sink = |epoch, tx| {
        TwylaAssetSink::sink
            .set(Value::dynamic(AssetSink { epoch, tx }))
            .wrap()
    };
    let (tx1, _r1) = unbounded::<AssetReq>();
    let (tx2, _r2) = unbounded::<AssetReq>();
    let (tx3, _r3) = unbounded::<AssetReq>();
    let same_a: LazyHash<Style> = sink(7, tx1);
    let same_b: LazyHash<Style> = sink(7, tx2);
    let other: LazyHash<Style> = sink(8, tx3);
    assert_eq!(
        hash128(&same_a),
        hash128(&same_b),
        "same epoch must hash equal regardless of channel"
    );
    assert_ne!(
        hash128(&same_a),
        hash128(&other),
        "distinct epochs must hash differently"
    );
}
