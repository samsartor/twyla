use std::collections::HashMap;
use std::time::Duration;

use crossbeam_channel::unbounded;
use typst::foundations::{Style, Value};
use typst::syntax::{FileId, RootedPath, VirtualPath, VirtualRoot};
use typst::utils::LazyHash;
use typst_utils::hash128;

use super::*;
use crate::project::TwylaContext;
use crate::render::RenderWorld;

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
    let mut resolver = AssetResolver::new(&world.ctx);
    let (docs, _harvested, assets) = world.compile_bundle_with_meta(&mut resolver).unwrap();
    (docs[0].html.clone(), assets)
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
    let mut resolver = AssetResolver::new(&ctx);

    let (_docs1, _h1, assets1) = world.compile_bundle_with_meta(&mut resolver).unwrap();
    let url1 = assets1[0].output_path.clone();

    // Edit the source (distinct mtime), then recompile the same world the way
    // `serve` does: reset the FileStore + age comemo.
    std::thread::sleep(Duration::from_millis(10));
    std::fs::write(dir.path().join("content/main.scss"), ".a { color: blue; }").unwrap();
    comemo::evict(0);
    world.files.reset();

    let (docs2, _h2, assets2) = world.compile_bundle_with_meta(&mut resolver).unwrap();
    let html2 = docs2[0].html.clone();
    let url2 = assets2[0].output_path.clone();

    assert_ne!(
        url1, url2,
        "fingerprint did not change after editing the source"
    );
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
    };

    let make = |pairs: &[(&AssetSpec, &str)]| {
        let mut m = HashMap::new();
        for (k, v) in pairs {
            m.insert((*k).clone(), String::from(*v));
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
    let (tx1, _r1) = unbounded::<AssetRequest>();
    let (tx2, _r2) = unbounded::<AssetRequest>();
    let (tx3, _r3) = unbounded::<AssetRequest>();
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
