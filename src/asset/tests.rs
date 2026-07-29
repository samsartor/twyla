// See the same allow in `mod.rs`: `AssetSpec` is a content-hashed key whose
// `Bytes` only *looks* interior-mutable to clippy (`Arc` refcount).
#![allow(clippy::mutable_key_type)]

use std::time::Duration;

use super::*;
use crate::project::TwylaContext;
use crate::render::{Output, RenderWorld};
use crate::resolver::Resolver;
use typst::syntax::{RootedPath, VirtualPath, VirtualRoot};

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

fn compile_asset_keys(world: &RenderWorld) -> (String, Vec<String>) {
    let mut resolver = Resolver::new(&world.ctx);
    let outputs = world.compile_bundle(&mut resolver).unwrap();
    let html = outputs.docs().next().unwrap().html.clone();
    let mut keys: Vec<String> = outputs
        .iter()
        .filter_map(|out| match out {
            Output::Asset { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    keys.sort();
    (html, keys)
}

fn compile_warnings(world: &RenderWorld) -> Vec<SourceDiagnostic> {
    let sources: Vec<_> = world
        .ctx
        .scan_pages()
        .unwrap()
        .into_iter()
        .map(|path| {
            FileId::new(RootedPath::new(
                VirtualRoot::Project,
                VirtualPath::virtualize(&world.ctx.root, &path).unwrap(),
            ))
        })
        .collect();
    let mut resolver = Resolver::new(&world.ctx);
    crate::compile::compile_bundle(&world.ctx, world, &sources, &mut resolver)
        .warnings
        .into_iter()
        .collect()
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

#[test]
fn explicit_output_path_controls_url_and_emission() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.sass(\"site.scss\", output: \"css/site.css\").url()",
        ),
        ("content/site.scss", "a{b:c}"),
    ]);
    let (html, keys) = compile_asset_keys(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(keys, vec!["css/site.css"]);
    assert!(
        html.contains("https://example.com/css/site.css"),
        "page missing explicit asset URL:\n{html}"
    );
}

#[test]
fn asset_output_index_html_uses_document_url_convention() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.file(\"thing.txt\", output: \"stuff/index.html\").url()",
        ),
        ("content/thing.txt", "hello"),
    ]);
    let (html, keys) = compile_asset_keys(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(keys, vec!["stuff/index.html"]);
    assert!(
        html.contains("https://example.com/stuff/"),
        "index.html output should strip to directory URL:\n{html}"
    );
}

#[test]
fn closure_output_path_controls_url_and_emission() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.file(\"thing.txt\", output: (hash, ext, stem) => \"derived/\" + stem + \".\" + ext).url()",
        ),
        ("content/thing.txt", "hello"),
    ]);
    let (html, keys) = compile_asset_keys(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(keys, vec!["derived/thing.txt"]);
    assert!(
        html.contains("https://example.com/derived/thing.txt"),
        "page missing closure-derived asset URL:\n{html}"
    );
}

#[test]
fn auto_reuses_single_explicit_output_path() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.file(\"logo.svg\", output: \"logo.svg\").url()\n\
             #context asset.file(\"logo.svg\").url()",
        ),
        ("content/logo.svg", "<svg/>"),
    ]);
    let (html, keys) = compile_asset_keys(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(keys, vec!["logo.svg"]);
    assert_eq!(
        html.matches("https://example.com/logo.svg").count(),
        2,
        "auto URL should reuse the one explicit path:\n{html}"
    );
}

#[test]
fn auto_falls_back_when_two_explicit_paths_exist() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.file(\"logo.svg\", output: \"a.svg\").url()\n\
             #context asset.file(\"logo.svg\", output: \"b.svg\").url()\n\
             #context asset.file(\"logo.svg\").url()",
        ),
        ("content/logo.svg", "<svg/>"),
    ]);
    let (html, keys) = compile_asset_keys(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(keys.len(), 3, "got keys {keys:?}");
    assert!(keys.contains(&"a.svg".to_string()), "got keys {keys:?}");
    assert!(keys.contains(&"b.svg".to_string()), "got keys {keys:?}");
    let default = keys
        .iter()
        .find(|key| key.starts_with("assets/logo-") && key.ends_with(".svg"))
        .expect("missing default auto path");
    assert!(html.contains("https://example.com/a.svg"), "{html}");
    assert!(html.contains("https://example.com/b.svg"), "{html}");
    assert!(
        html.contains(&format!("https://example.com/{default}")),
        "{html}"
    );
}

#[test]
fn two_explicit_outputs_emit_two_copies() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.file(\"logo.svg\", output: \"a.svg\").url()\n\
             #context asset.file(\"logo.svg\", output: \"b.svg\").url()",
        ),
        ("content/logo.svg", "<svg/>"),
    ]);
    let (_html, keys) = compile_asset_keys(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(keys, vec!["a.svg", "b.svg"]);
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
        .write_to(
            &mut std::io::Cursor::new(&mut buf),
            ::image::ImageFormat::Png,
        )
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
    assert!(
        html.contains(&asset.url),
        "page missing resolved url:\n{html}"
    );

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

/// Processing constructors treat `bytes` as the source itself, while strings
/// remain paths. Reading the PNG here is only how the Typst test obtains a
/// byte value; `asset.image` receives no path and therefore has no source stem.
#[test]
fn image_accepts_inline_bytes() {
    let (ctx, dir) = site(&[(
        "content/main.typ",
        "#let data = read(\"photo.png\", encoding: none)\n\
         #context asset.image(data, width: 20).url()",
    )]);
    std::fs::write(dir.path().join("content/photo.png"), png(40, 20)).unwrap();

    let (_html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert_eq!(assets.len(), 1, "got {assets:?}");
    let asset = &assets[0];
    assert_eq!(asset.built.stem, None);
    assert!(asset.output_path.ends_with(".png"));
    let decoded = ::image::load_from_memory(&asset.built.emit.read().unwrap()).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (20, 10));
    assert!(
        asset.built.upstream.is_empty(),
        "inline image bytes should not be watched as an asset file"
    );
}

#[test]
fn sass_and_svg_accept_inline_bytes() {
    let (ctx, _dir) = site(&[(
        "content/main.typ",
        "#context str(asset.sass(bytes(\"a { b { color: red; } }\"), format: \"scss\").read(encoding: none))\n\
         #context raw-html(asset.svg(bytes(\"<svg> <circle cx='1'/> </svg>\")).read())",
    )]);

    let (html, _assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert!(
        html.contains("a b"),
        "inline SCSS was not compiled:\n{html}"
    );
    assert!(
        html.contains("<svg"),
        "inline SVG was not processed:\n{html}"
    );
}

#[test]
fn sass_byte_source_requires_a_format() {
    let (ctx, _dir) = site(&[(
        "content/main.typ",
        "#context asset.sass(bytes(\"a { color: red }\")).url()",
    )]);
    let err = RenderWorld::new(&ctx)
        .unwrap()
        .compile_bundle(&mut Resolver::new(&ctx))
        .err()
        .expect("a Sass byte source without a format should fail");
    let message = err.to_string();
    assert!(
        message.contains("require an explicit `format`"),
        "unexpected diagnostic:\n{message}"
    );
}

#[test]
fn sass_format_auto_detects_file_syntax() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.sass(\"theme.sass\", format: auto).read()",
        ),
        ("content/theme.sass", "a\n  color: red"),
    ]);

    let (html, _assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert!(
        html.contains("a{color:red}"),
        "explicit auto did not detect indented Sass:\n{html}"
    );
}

#[test]
fn raw_emits_inline_text_with_an_extension() {
    let (ctx, _dir) = site(&[(
        "content/main.typ",
        "#context asset.raw(\"a{color:red}\", extension: \"css\").url()",
    )]);

    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert_eq!(assets.len(), 1, "got {assets:?}");
    let asset = &assets[0];
    assert!(asset.output_path.ends_with(".css"));
    assert_eq!(asset.built.emit.read().unwrap().as_slice(), b"a{color:red}");
    assert!(asset.built.upstream.is_empty());
    assert!(html.contains(&asset.url));
}

#[test]
fn raw_defaults_to_bin_extension() {
    let (ctx, _dir) = site(&[(
        "content/main.typ",
        "#context asset.raw(bytes((0, 1, 2))).url()",
    )]);

    let (_html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert_eq!(assets.len(), 1, "got {assets:?}");
    assert!(assets[0].output_path.ends_with(".bin"));
    assert_eq!(assets[0].built.ext.as_deref(), Some("bin"));
}

/// `format: auto` keeps the detected source format. Here: keep PNG and resize
/// to a 30px-wide box.
#[test]
fn image_auto_keeps_source_format() {
    let (ctx, dir) = site(&[(
        "content/main.typ",
        "#context asset.image(\"photo.png\", width: 30, format: auto).url()",
    )]);
    std::fs::write(dir.path().join("content/photo.png"), png(60, 60)).unwrap();

    let (_html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    let asset = &assets[0];
    assert!(
        asset.output_path.ends_with(".png"),
        "kept source format: {}",
        asset.output_path
    );
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
    assert!(
        html.contains(&assets[0].url),
        "page missing resolved url:\n{html}"
    );
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

/// A native SVG image routes through the `asset.svg` minify pipeline (not the
/// raster one): the output is minified and gets no intrinsic-dimension attrs.
#[test]
fn native_svg_image_routes_through_svg_pipeline() {
    let (ctx, dir) = site(&[("content/main.typ", "#image(\"logo.svg\")")]);
    std::fs::write(
        dir.path().join("content/logo.svg"),
        "<!-- strip me -->\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"></svg>",
    )
    .unwrap();

    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert!(
        assets[0].output_path.ends_with(".svg"),
        "svg output: {}",
        assets[0].output_path
    );
    let bytes = assets[0].built.emit.read().unwrap();
    assert!(
        !String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("strip me"),
        "native svg should be minified (comment stripped)"
    );
    assert!(
        !html.contains("width=\"10\""),
        "vector image should not get raster dimension attrs:\n{html}"
    );
}

/// `#set asset.svg(minify: false)` reaches native images too: the markdown-style
/// `#image("x.svg")` keeps its bytes verbatim under the set rule.
#[test]
fn set_rule_disables_native_svg_minify() {
    let src = "<!-- keep me -->\n<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
    let (ctx, dir) = site(&[(
        "content/main.typ",
        "#set asset.svg(minify: false)\n#image(\"logo.svg\")",
    )]);
    std::fs::write(dir.path().join("content/logo.svg"), src).unwrap();

    let (_html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    let bytes = assets[0].built.emit.read().unwrap();
    assert_eq!(
        String::from_utf8(bytes.to_vec()).unwrap(),
        src,
        "minify: false should pass the source through verbatim"
    );
}

/// `asset.svg` minifies and injects a fixed root `id`, inlinable via `.read()`.
#[test]
fn svg_asset_minifies_and_sets_root_id() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context raw-html(asset.svg(\"icon.svg\", id: \"logo\").read())",
        ),
        (
            "content/icon.svg",
            "<!-- gone -->\n<svg xmlns=\"http://www.w3.org/2000/svg\">\n  <circle r=\"5\"/>\n</svg>",
        ),
    ]);
    let (html, _assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert!(
        html.contains("id=\"logo\""),
        "root id not injected:\n{html}"
    );
    assert!(!html.contains("gone"), "comment survived minify:\n{html}");
    assert!(
        !html.contains("__twyla-asset-pending__") && !html.contains("__twyla-svg-id-pending__"),
        "placeholder leaked:\n{html}"
    );
}

/// `id: auto` derives a stable id and `.elem-id()` resolves it, so a template
/// can build `<use href="url#id">` — the cross-document `<use>` pattern.
#[test]
fn svg_auto_id_resolves_through_elem_id() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context {\n\
               let icon = asset.svg(\"icon.svg\", id: auto)\n\
               raw-html(\"<use href=\\\"\" + icon.url() + \"#\" + icon.elem-id() + \"\\\"/>\")\n\
             }",
        ),
        (
            "content/icon.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"5\"/></svg>",
        ),
    ]);
    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(assets.len(), 1, "got {assets:?}");
    let id = assets[0]
        .built
        .elem_id
        .clone()
        .expect("auto id recorded on the built asset");
    assert!(id.starts_with("svg-"), "unexpected auto id: {id}");
    assert!(
        html.contains(&format!("#{id}\"")),
        "page missing use fragment #{id}:\n{html}"
    );
    let bytes = String::from_utf8(assets[0].built.emit.read().unwrap().to_vec()).unwrap();
    assert!(
        bytes.contains(&format!("id=\"{id}\"")),
        "emitted svg missing injected id {id}:\n{bytes}"
    );
    assert!(
        !html.contains("__twyla-svg-id-pending__"),
        "id placeholder leaked:\n{html}"
    );
}

/// Minification is best-effort: a source svgm can't parse passes through
/// verbatim (with a warning) instead of failing the build.
#[test]
fn unparsable_svg_passes_through_verbatim() {
    let src = "<svg><open></svg>";
    let (ctx, _dir) = site(&[
        ("content/main.typ", "#context asset.svg(\"bad.svg\").url()"),
        ("content/bad.svg", src),
    ]);
    let (_html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    let bytes = assets[0].built.emit.read().unwrap();
    assert_eq!(
        String::from_utf8(bytes.to_vec()).unwrap(),
        src,
        "unparsable svg should pass through verbatim"
    );
}

/// ...but an `id` request on an unparsable svg is a hard error: the id can't
/// be injected, and passing through silently would break `<use>` references.
#[test]
fn unparsable_svg_with_id_request_errors() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context asset.svg(\"bad.svg\", id: \"x\").url()",
        ),
        ("content/bad.svg", "<svg><open></svg>"),
    ]);
    let err = RenderWorld::new(&ctx)
        .unwrap()
        .compile_bundle(&mut Resolver::new(&ctx))
        .err()
        .expect("id on an unparsable svg should fail the build");
    assert!(
        err.to_string().contains("failed to parse"),
        "expected a parse error, got:\n{err}"
    );
}

/// `asset.typst(format: "svg", minify: true)` runs the compiled SVG through
/// svgm — the output shrinks but is still an SVG.
#[test]
fn typst_svg_minify_shrinks_output() {
    let snippet = |minify: &str| {
        format!(
            "#context asset.typst(circle(fill: blue, radius: 6pt), format: \"svg\"{minify}).url()"
        )
    };

    let (ctx_plain, _a) = site(&[("content/main.typ", snippet("").leak())]);
    let (_html, plain) = compile(&RenderWorld::new(&ctx_plain).unwrap());
    let plain_len = plain[0].built.emit.read().unwrap().len();

    let (ctx_min, _b) = site(&[("content/main.typ", snippet(", minify: true").leak())]);
    let (_html, minified) = compile(&RenderWorld::new(&ctx_min).unwrap());
    let min_bytes = minified[0].built.emit.read().unwrap();

    assert!(
        min_bytes.starts_with(b"<svg"),
        "minified output should still be an svg"
    );
    assert!(
        min_bytes.len() < plain_len,
        "minify should shrink the svg: {} !< {plain_len}",
        min_bytes.len()
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
    // Both assets are *only* `.read()` (inlined above), never `.url()`'d, so
    // neither is emitted as a file — the bytes already live in the HTML.
    assert_eq!(
        assets.len(),
        0,
        "read-only assets should be inlined, not written as files: {assets:?}"
    );
    assert!(
        !html.contains("__twyla-asset-pending__"),
        "placeholder leaked:\n{html}"
    );
}

/// An asset both linked (`.url()`) and inlined (`.read()`) is emitted exactly
/// once: the URL usage makes it a file, and unioning the read usage neither
/// suppresses nor duplicates it.
#[test]
fn asset_used_for_url_and_read_is_emitted_once() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#context html.elem(\"link\", attrs: (href: asset.file(\"s.css\").url()))\n\
             #context raw-html(asset.file(\"s.css\").read())",
        ),
        ("content/s.css", "a{b:c}"),
    ]);
    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(
        assets.len(),
        1,
        "url+read of one spec should emit exactly one file: {assets:?}"
    );
    assert!(html.contains("a{b:c}"), "read content not inlined:\n{html}");
    assert!(
        html.contains(&format!("href=\"{}\"", assets[0].url)),
        "link did not point at the emitted asset URL:\n{html}"
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

/// A bare asset element left in markup emits the asset and renders nothing.
#[test]
fn bare_asset_in_markup_emits_and_vanishes() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "before #asset.file(\"logo.svg\", output: \"logo.svg\") after",
        ),
        ("content/logo.svg", "<svg/>"),
    ]);
    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert_eq!(assets.len(), 1, "got {assets:?}");
    assert_eq!(assets[0].output_path, "logo.svg");
    assert!(
        html.contains("before") && html.contains("after"),
        "missing text: {html}"
    );
    assert!(
        !html.contains("logo.svg"),
        "bare asset should render nothing: {html}"
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

/// The document half of the warm path: editing the source of an inline
/// `#document(..)` and recompiling the *same* resolver (FileStore reset + comemo
/// aged, exactly as `serve` does) re-discovers it with a *fresh body*. Documents
/// are cleared and rediscovered each compile, so the stale v1 body neither
/// lingers nor trips the duplicate-output conflict check against v2.
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
    assert!(
        child1.contains("v1-body"),
        "first child body missing:\n{child1}"
    );

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

    assert!(
        html.contains("<svg"),
        "inline content not rendered to svg:\n{html}"
    );
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
        (
            "doc.typ",
            "= A Standalone Document\n\nWith a paragraph of text.",
        ),
    ]);
    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());

    assert_eq!(assets.len(), 1, "got {assets:?}");
    let asset = &assets[0];
    assert!(
        asset.output_path.starts_with("assets/doc-") && asset.output_path.ends_with(".pdf"),
        "unexpected output path: {}",
        asset.output_path,
    );
    assert!(
        html.contains(&asset.url),
        "page missing resolved url:\n{html}"
    );
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
    assert!(
        asset.output_path.ends_with(".png"),
        "not png: {}",
        asset.output_path
    );
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
    assert!(
        asset.output_path.ends_with(".html"),
        "not html: {}",
        asset.output_path
    );
    let body = String::from_utf8(asset.built.emit.read().unwrap().to_vec()).unwrap();
    assert!(body.contains("body text"), "html missing body:\n{body}");
}

/// Inline content is constructed under Twyla's library before `asset.typst`
/// lays it out with stock Typst. If it carries a Twyla asset call into that
/// subcompile, the nested request cannot reach the outer discovery loop. The
/// escaped placeholder is diagnosed instead of failing silently.
#[test]
fn typst_html_warns_when_outer_content_carries_a_nested_twyla_asset() {
    let (ctx, _dir) = site(&[
        (
            "content/main.typ",
            "#let child = context html.img(src: asset.file(\"nested.svg\").url())\n\
             #context asset.typst(child, format: \"html\").url()",
        ),
        ("content/nested.svg", "<svg/>"),
    ]);
    let warnings = compile_warnings(&RenderWorld::new(&ctx).unwrap());

    assert!(
        warnings.iter().any(|warning| warning
            .message
            .contains("inside this `asset.typst` document could not be resolved")),
        "missing nested-asset warning: {warnings:#?}"
    );
}

/// A bare `asset.typst(..)` left in markup emits and renders nothing.
#[test]
fn bare_typst_asset_emits_and_vanishes() {
    let (ctx, _dir) = site(&[(
        "content/main.typ",
        "before #asset.typst(circle(), format: \"svg\", output: \"icon.svg\") after",
    )]);
    let (html, assets) = compile(&RenderWorld::new(&ctx).unwrap());
    assert_eq!(assets.len(), 1, "got {assets:?}");
    assert_eq!(assets[0].output_path, "icon.svg");
    assert!(
        html.contains("before") && html.contains("after"),
        "missing text: {html}"
    );
    assert!(
        !html.contains("icon.svg"),
        "bare asset should render nothing: {html}"
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

    assert_ne!(
        url1, url2,
        "fingerprint did not change after editing the typst source"
    );
    assert!(
        !html2.contains("__twyla-asset-pending__"),
        "placeholder leaked after edit:\n{html2}"
    );
}
