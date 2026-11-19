//! `asset.js` end to end: fixtures written by hand into a temp dir (no npm,
//! no network — packages are fake `node_modules/<pkg>/package.json`s).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use crate::project::TwylaContext;
use crate::render::{Output, Outputs, RenderWorld};
use crate::resolver::{Resolver, ResolverStats};
use crate::session::DevSession;

/// A project at `<tmp>/site` (so `<tmp>` is a writable "outside"), with
/// `files` written relative to it (`../x` lands outside).
fn project(files: &[(&str, &str)]) -> (TwylaContext, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("site");
    std::fs::create_dir_all(root.join("content")).unwrap();
    for (rel, contents) in files {
        write(&root, rel, contents);
    }
    let ctx = TwylaContext::new(&root, None).unwrap();
    (ctx, dir)
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A page that links `call` (an `asset.js(..)` expression) and lists its
/// preloads, one per line: `URL <url>` / `PRE <url>`.
fn page(call: &str) -> String {
    format!(
        "#let app = {call}\n\
         #context [\n\
           #for u in app.preload() [PRE #u\\ ]\n\
           URL #app.url()\n\
         ]\n"
    )
}

fn compile(ctx: &TwylaContext) -> Result<Outputs, String> {
    let world = RenderWorld::new(ctx).unwrap();
    world
        .compile_bundle(&mut Resolver::new(ctx))
        .map_err(|e| e.to_string())
}

/// The error text of a compile that must fail.
fn compile_err(ctx: &TwylaContext) -> String {
    match compile(ctx) {
        Ok(outputs) => panic!("expected a failed build, got {:?}", keys(&outputs)),
        Err(err) => err,
    }
}

fn html(outputs: &Outputs) -> String {
    outputs.docs().next().unwrap().html.clone()
}

/// The emitted bytes at `key`, as text.
fn text(outputs: &Outputs, key: &str) -> String {
    let bytes = match outputs.get(key) {
        Some(Output::Asset { asset, .. }) => asset.built.emit.read().unwrap(),
        Some(Output::Companion { emit, .. }) => emit.read().unwrap(),
        other => panic!("no asset output at {key}: {other:?}"),
    };
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Every asset/companion output key, sorted.
fn keys(outputs: &Outputs) -> Vec<String> {
    let mut keys: Vec<String> = outputs
        .iter()
        .filter(|o| matches!(o, Output::Asset { .. } | Output::Companion { .. }))
        .map(|o| o.key().to_string())
        .collect();
    keys.sort();
    keys
}

/// The one output key starting with `prefix` (e.g. `assets/main-`) and
/// ending `.js`.
fn key(outputs: &Outputs, prefix: &str) -> String {
    let found: Vec<String> = keys(outputs)
        .into_iter()
        .filter(|k| k.starts_with(prefix) && k.ends_with(".js"))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected one {prefix}*.js, got {:?}",
        keys(outputs)
    );
    found[0].clone()
}

fn entry(outputs: &Outputs) -> String {
    text(outputs, &key(outputs, "assets/main-"))
}

#[test]
fn bundles_tsx_with_tsconfig_jsx_and_paths() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/scripts/main.tsx\", minify: false)"),
        ),
        (
            "tsconfig.json",
            r#"{ "compilerOptions": {
                "jsx": "react-jsx", "jsxImportSource": "tinyjsx",
                "paths": { "@lib/*": ["./lib/*"] }
            } }"#,
        ),
        (
            "scripts/main.tsx",
            "import { greet } from \"@lib/greet\";\n\
             const name: string = \"twyla\";\n\
             console.log(<p class=\"x\">{greet(name)}</p>);\n",
        ),
        (
            "lib/greet.ts",
            "export function greet(who: string): string { return `GREET:${who}`; }\n",
        ),
        (
            "node_modules/tinyjsx/package.json",
            r#"{ "name": "tinyjsx", "exports": { "./jsx-runtime": "./runtime.js" } }"#,
        ),
        (
            "node_modules/tinyjsx/runtime.js",
            "export function jsx(t, p) { return 'TINYJSX:' + t; }\nexport const jsxs = jsx;\n",
        ),
    ]);
    let outputs = compile(&ctx).unwrap();
    let code = entry(&outputs);
    assert!(code.contains("GREET:"), "path alias not bundled:\n{code}");
    assert!(
        code.contains("TINYJSX:"),
        "jsx runtime from tsconfig not used:\n{code}"
    );
    assert!(
        !code.contains(": string"),
        "TypeScript not stripped:\n{code}"
    );
    assert!(!code.contains("<p"), "JSX not compiled:\n{code}");
    let url = key(&outputs, "assets/main-");
    assert!(
        html(&outputs).contains(&format!("URL /{url}")),
        "{}",
        html(&outputs)
    );
}

/// A classic-runtime pragma works without any tsconfig.
#[test]
fn jsx_classic_pragma_without_tsconfig() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.jsx\", minify: false)"),
        ),
        (
            "main.jsx",
            "/** @jsxRuntime classic */\n/** @jsx h */\n\
             function h(t) { return 'H:' + t; }\nconsole.log(<b />);\n",
        ),
    ]);
    let code = entry(&compile(&ctx).unwrap());
    assert!(
        code.contains("h(\"b\""),
        "classic pragma not applied:\n{code}"
    );
}

fn package_fixture(platform: &str) -> (TwylaContext, tempfile::TempDir) {
    project(&[
        (
            "content/main.typ",
            &page(&format!(
                "asset.js(\"/main.js\", minify: false, platform: \"{platform}\")"
            )),
        ),
        (
            "main.js",
            "import { where } from \"pkg\";\nconsole.log(where);\n",
        ),
        (
            "node_modules/pkg/package.json",
            r#"{ "name": "pkg", "exports": { ".": {
                "browser": "./browser.js", "node": "./node.js", "default": "./default.js"
            } } }"#,
        ),
        (
            "node_modules/pkg/browser.js",
            "export const where = 'IN_BROWSER';\n",
        ),
        (
            "node_modules/pkg/node.js",
            "export const where = 'IN_NODE';\n",
        ),
        (
            "node_modules/pkg/default.js",
            "export const where = 'IN_DEFAULT';\n",
        ),
    ])
}

#[test]
fn packages_resolve_with_platform_conditions() {
    for (platform, expected) in [
        ("browser", "IN_BROWSER"),
        ("node", "IN_NODE"),
        ("neutral", "IN_DEFAULT"),
    ] {
        let (ctx, _dir) = package_fixture(platform);
        let code = entry(&compile(&ctx).unwrap());
        assert!(
            code.contains(expected),
            "{platform}: expected {expected}:\n{code}"
        );
    }
}

/// `main` imports `common` statically and loads `lazy-a`/`lazy-b`
/// dynamically, both of which import `common` and `deep`: `common` is a
/// static chunk (preloaded), `deep` a chunk only dynamic imports reach (not
/// preloaded), and both lazies are dynamic chunks. All are emitted.
#[test]
fn dynamic_imports_become_companion_chunks_and_static_ones_preload() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/js/main.js\", minify: false)"),
        ),
        (
            "js/main.js",
            "import { common } from './common.js';\n\
             console.log(common);\n\
             export const a = () => import('./lazy-a.js');\n\
             export const b = () => import('./lazy-b.js');\n",
        ),
        ("js/common.js", "export const common = 'COMMON';\n"),
        ("js/deep.js", "export const deep = 'DEEP';\n"),
        (
            "js/lazy-a.js",
            "import { common } from './common.js';\nimport { deep } from './deep.js';\n\
             export const a = common + deep + 'A';\n",
        ),
        (
            "js/lazy-b.js",
            "import { common } from './common.js';\nimport { deep } from './deep.js';\n\
             export const b = common + deep + 'B';\n",
        ),
    ]);
    let outputs = compile(&ctx).unwrap();
    let all = keys(&outputs);
    let common = key(&outputs, "assets/common-");
    let deep = key(&outputs, "assets/deep-");
    let lazy_a = key(&outputs, "assets/lazy-a-");
    let lazy_b = key(&outputs, "assets/lazy-b-");
    assert_eq!(all.len(), 5, "entry + 4 chunks, got {all:?}");

    let main = entry(&outputs);
    let name = |k: &str| k.trim_start_matches("assets/").to_string();
    assert!(main.contains(&format!("./{}", name(&common))), "{main}");
    assert!(main.contains(&format!("./{}", name(&lazy_a))), "{main}");
    assert!(text(&outputs, &lazy_b).contains(&format!("./{}", name(&deep))));

    let page = html(&outputs);
    assert!(
        page.contains(&format!("PRE /{common}")),
        "common not preloaded:\n{page}"
    );
    for dynamic in [&deep, &lazy_a, &lazy_b] {
        assert!(
            !page.contains(dynamic.as_str()),
            "{dynamic} preloaded:\n{page}"
        );
    }
}

/// Two entries in separate builds (different `define`s, so different
/// groups) that load the same module dynamically both carry the same chunk
/// (same name, same bytes): it's emitted once, not a conflict.
#[test]
fn identical_chunks_from_two_entries_dedupe() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context [#asset.js(\"/a.js\", minify: false, define: (X: 1)).url() \
             #asset.js(\"/b.js\", minify: false, define: (X: 2)).url()]",
        ),
        ("a.js", "export const go = () => import('./lazy.js');\n"),
        ("b.js", "export const run = () => import('./lazy.js');\n"),
        ("lazy.js", "export const lazy = 'LAZY';\n"),
    ]);
    let outputs = compile(&ctx).unwrap();
    let all = keys(&outputs);
    assert_eq!(all.len(), 3, "a + b + one lazy chunk, got {all:?}");
    key(&outputs, "assets/lazy-");
}

#[test]
fn define_flattens_and_minify_removes_dead_branches() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page(
                "asset.js(\"/main.js\", define: (\
                   SITE: (debug: false, title: \"My Site\", tags: (\"a\", \"b\")),\
                   \"process.env.NODE_ENV\": \"production\"))",
            ),
        ),
        (
            "main.js",
            "if (SITE.debug) { console.log('DEBUG_ONLY'); }\n\
             if (process.env.NODE_ENV !== 'production') { console.log('DEV_ONLY'); }\n\
             console.log(SITE.title, SITE.tags);\n",
        ),
    ]);
    let code = entry(&compile(&ctx).unwrap());
    assert!(
        !code.contains("DEBUG_ONLY"),
        "dead branch survived:\n{code}"
    );
    assert!(
        !code.contains("DEV_ONLY"),
        "NODE_ENV branch survived:\n{code}"
    );
    assert!(code.contains("My Site"), "string leaf missing:\n{code}");
    assert!(
        code.contains("[`a`,`b`]") || code.contains("[\"a\",\"b\"]"),
        "array leaf missing:\n{code}"
    );
    assert!(!code.contains("SITE"), "define not applied:\n{code}");
}

/// rolldown's own default on the browser platform: `NODE_ENV` follows
/// `minify`.
#[test]
fn node_env_follows_minify_by_default() {
    for (minify, expected) in [("true", "production"), ("false", "development")] {
        let (ctx, _dir) = project(&[
            (
                "content/main.typ",
                &page(&format!("asset.js(\"/main.js\", minify: {minify})")),
            ),
            ("main.js", "console.log(process.env.NODE_ENV);\n"),
        ]);
        let code = entry(&compile(&ctx).unwrap());
        assert!(code.contains(expected), "minify {minify}:\n{code}");
    }
}

#[test]
fn virtual_modules_serve_typst_data() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#let posts = ((title: \"Hello\", n: 1), (title: \"World\", n: 2))\n\
             #context asset.js(\"/main.js\", minify: false, modules: (\"virtual:posts\": posts)).url()",
        ),
        (
            "main.js",
            "import posts from 'virtual:posts';\nconsole.log(posts.map(p => p.title));\n",
        ),
    ]);
    let code = entry(&compile(&ctx).unwrap());
    assert!(
        code.contains("\"Hello\"") && code.contains("\"World\""),
        "{code}"
    );
}

#[test]
fn inject_supplies_globals_by_import() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page(
                "asset.js(\"/main.js\", minify: false, \
                 inject: (h: \"./h.js\", q: (\"./q.js\", \"query\")))",
            ),
        ),
        ("main.js", "console.log(h('div'), q('#x'));\n"),
        (
            "h.js",
            "export default function h(t) { return 'H_DEFAULT:' + t; }\n",
        ),
        (
            "q.js",
            "export function query(s) { return 'Q_NAMED:' + s; }\n",
        ),
    ]);
    let code = entry(&compile(&ctx).unwrap());
    assert!(
        code.contains("H_DEFAULT:"),
        "default inject missing:\n{code}"
    );
    assert!(code.contains("Q_NAMED:"), "named inject missing:\n{code}");
}

#[test]
fn external_imports_are_left_alone() {
    let main = "import { lib } from 'runtime-lib';\nimport { cdn } from 'https://cdn.example/x.js';\n\
                console.log(lib, cdn);\n";
    // Without `external`, a bare specifier nothing provides is an error.
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false)"),
        ),
        ("main.js", main),
    ]);
    let err = compile_err(&ctx);
    assert!(err.contains("Could not resolve 'runtime-lib'"), "{err}");
    assert!(err.contains("`external`"), "{err}");

    // Listed, it's left as an import. The http URL is an implicit external.
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false, external: (\"runtime-lib\",))"),
        ),
        ("main.js", main),
    ]);
    let outputs = compile(&ctx).unwrap();
    let code = entry(&outputs);
    assert!(code.contains("from \"runtime-lib\""), "{code}");
    assert!(code.contains("from \"https://cdn.example/x.js\""), "{code}");
    assert_eq!(keys(&outputs).len(), 1);
    assert!(!html(&outputs).contains("PRE"), "an external isn't a chunk");
}

/// An `external` spelled exactly like one of the bundle's chunk files would
/// make rolldown's import lists ambiguous (preload could list a chunk that's
/// really only loaded dynamically): refused.
#[test]
fn external_named_like_a_chunk_is_refused() {
    let (ctx, _dir) = lazy_project("asset.js(\"/main.js\", minify: false)");
    let outputs = compile(&ctx).unwrap();
    let lazy = key(&outputs, "assets/lazy-");
    let lazy = lazy.trim_start_matches("assets/");
    // The lazy chunk's contents (and so its name) don't change.
    write(
        &ctx.root,
        "main.js",
        &format!("import '{lazy}';\nexport const go = () => import('./lazy.js');\n"),
    );
    write(
        &ctx.root,
        "content/main.typ",
        &page(&format!(
            "asset.js(\"/main.js\", minify: false, external: (\"{lazy}\",))"
        )),
    );
    let err = compile_err(&ctx);
    assert!(err.contains("same name as a chunk"), "{err}");
}

const LAZY_FIXTURE: &[(&str, &str)] = &[
    ("main.js", "export const go = () => import('./lazy.js');\n"),
    ("lazy.js", "export const lazy = 'LAZY_BODY';\n"),
];

fn lazy_project(call: &str) -> (TwylaContext, tempfile::TempDir) {
    let page = page(call);
    let mut files = vec![("content/main.typ", page.as_str())];
    files.extend_from_slice(LAZY_FIXTURE);
    project(&files)
}

#[test]
fn split_false_inlines_dynamic_imports() {
    let (ctx, _dir) = lazy_project("asset.js(\"/main.js\", minify: false, split: false)");
    let outputs = compile(&ctx).unwrap();
    assert_eq!(keys(&outputs).len(), 1, "{:?}", keys(&outputs));
    assert!(entry(&outputs).contains("LAZY_BODY"));
}

#[test]
fn iife_format_is_one_classic_script() {
    let (ctx, _dir) = lazy_project("asset.js(\"/main.js\", minify: false, format: \"iife\")");
    let outputs = compile(&ctx).unwrap();
    assert_eq!(keys(&outputs).len(), 1, "{:?}", keys(&outputs));
    let code = entry(&outputs);
    assert!(code.trim_start().starts_with("(function"), "{code}");
    assert!(
        code.contains("LAZY_BODY") && !code.contains("import("),
        "{code}"
    );
}

#[test]
fn iife_with_explicit_split_is_an_error() {
    let (ctx, _dir) = lazy_project("asset.js(\"/main.js\", format: \"iife\", split: true)");
    let err = compile_err(&ctx);
    assert!(err.contains("`split: true` can't be used"), "{err}");
}

#[test]
fn sourcemaps_are_emitted_next_to_every_chunk() {
    let (ctx, _dir) = lazy_project("asset.js(\"/main.js\", sourcemap: true)");
    let outputs = compile(&ctx).unwrap();
    let all = keys(&outputs);
    assert_eq!(all.len(), 4, "two chunks + two maps: {all:?}");
    for js in all.iter().filter(|k| k.ends_with(".js")) {
        let name = js.trim_start_matches("assets/");
        assert!(
            all.contains(&format!("{js}.map")),
            "no map for {js}: {all:?}"
        );
        let code = text(&outputs, js);
        assert!(
            code.contains(&format!("//# sourceMappingURL={name}.map")),
            "{js} doesn't reference its map:\n{code}"
        );
        assert!(text(&outputs, &format!("{js}.map")).contains("\"mappings\""));
    }
}

#[test]
fn read_inlines_a_chunkless_entry() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context html.elem(\"script\", asset.js(\"/main.js\").read())",
        ),
        ("main.js", "import { x } from './x.js';\nconsole.log(x);\n"),
        ("x.js", "export const x = 'INLINED_X';\n"),
    ]);
    let outputs = compile(&ctx).unwrap();
    assert!(html(&outputs).contains("INLINED_X"), "{}", html(&outputs));
    assert!(keys(&outputs).is_empty(), "read-only: nothing emitted");
}

#[test]
fn read_refuses_an_entry_with_chunks() {
    let page = "#context asset.js(\"/main.js\").read()";
    let mut files = vec![("content/main.typ", page)];
    files.extend_from_slice(LAZY_FIXTURE);
    let (ctx, _dir) = project(&files);
    let err = compile_err(&ctx);
    assert!(err.contains("imports a chunk"), "{err}");
    assert!(err.contains("split: false"), "{err}");
}

#[test]
fn bytes_input_is_refused() {
    let (ctx, _dir) = project(&[(
        "content/main.typ",
        "#context asset.js(bytes(\"console.log(1)\")).url()",
    )]);
    let err = compile_err(&ctx);
    assert!(err.contains("needs a file path"), "{err}");
}

#[test]
fn non_js_imports_are_refused() {
    for import in [
        "import './style.css';",
        "import pic from './pic.png'; console.log(pic);",
    ] {
        let (ctx, _dir) = project(&[
            ("content/main.typ", &page("asset.js(\"/main.js\")")),
            ("main.js", import),
            ("style.css", "a{}"),
            ("pic.png", "not really"),
        ]);
        let err = compile_err(&ctx);
        assert!(err.contains("only bundles JavaScript"), "{import}: {err}");
        assert!(
            err.contains("`?raw`") && err.contains("`?url`"),
            "no hint: {err}"
        );
    }
}

#[test]
fn syntax_errors_blame_the_call_and_name_the_module() {
    let (ctx, _dir) = project(&[
        ("content/main.typ", &page("asset.js(\"/main.js\")")),
        ("main.js", "import './broken.js';\n"),
        ("broken.js", "const = ;\n"),
    ]);
    let err = compile_err(&ctx);
    assert!(err.contains("main.typ"), "not blamed on the call:\n{err}");
    assert!(err.contains("broken.js"), "no module location:\n{err}");
}

// -- sandbox ----------------------------------------------------------------

#[test]
fn imports_outside_the_project_fail() {
    for import in ["/etc/hostname", "/etc/passwd", "../outside.js"] {
        let (ctx, _dir) = project(&[
            ("content/main.typ", &page("asset.js(\"/main.js\")")),
            (
                "main.js",
                &format!("import x from '{import}';\nconsole.log(x);\n"),
            ),
            ("../outside.js", "export default 'OUTSIDE';\n"),
        ]);
        let err = compile_err(&ctx);
        assert!(!err.contains("OUTSIDE"), "{err}");
    }
}

#[cfg(unix)]
#[test]
fn symlinks_escaping_the_project_fail() {
    let (ctx, dir) = project(&[
        ("content/main.typ", &page("asset.js(\"/main.js\")")),
        (
            "main.js",
            "import x from './link.js';\nimport y from 'escape';\nconsole.log(x, y);\n",
        ),
        ("../outside.js", "export default 'OUTSIDE';\n"),
        (
            "../pkg/package.json",
            r#"{ "name": "escape", "main": "index.js" }"#,
        ),
        ("../pkg/index.js", "export default 'OUTSIDE_PKG';\n"),
    ]);
    std::os::unix::fs::symlink(dir.path().join("outside.js"), ctx.root.join("link.js")).unwrap();
    std::fs::create_dir_all(ctx.root.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("pkg"), ctx.root.join("node_modules/escape"))
        .unwrap();
    let err = compile_err(&ctx);
    assert!(err.contains("link.js"), "{err}");

    // Just the package symlink.
    write(
        &ctx.root,
        "main.js",
        "import y from 'escape';\nconsole.log(y);\n",
    );
    let err = compile_err(&ctx);
    assert!(err.contains("Could not resolve 'escape'"), "{err}");
    assert!(err.contains("`external`"), "{err}");
}

/// rolldown never writes: building leaves the project tree untouched
/// (no `dist/`, no cache dirs).
#[test]
fn building_writes_nothing() {
    let (ctx, _dir) = lazy_project("asset.js(\"/main.js\", sourcemap: true)");
    let listing = |root: &Path| {
        let mut all = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                all.push(path);
            }
        }
        all.sort();
        all
    };
    let before = listing(&ctx.root);
    compile(&ctx).unwrap();
    assert_eq!(listing(&ctx.root), before);
}

// -- deps -------------------------------------------------------------------

/// Rebuild after `edit`, as `serve` does, and return the entry's code.
fn rebuild_entry(session: &mut DevSession) -> Result<String, String> {
    let outputs = session.rebuild().result.map_err(|e| e.to_string())?;
    Ok(entry(&outputs))
}

fn edit(root: &Path, rel: &str, contents: &str) {
    // A distinct mtime, so the stamp moves even on coarse filesystems.
    std::thread::sleep(Duration::from_millis(10));
    write(root, rel, contents);
}

#[test]
fn editing_an_imported_module_revalidates() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false)"),
        ),
        (
            "main.js",
            "import { v } from './dep.js';\nconsole.log(v);\n",
        ),
        ("dep.js", "export const v = 'VERSION_ONE';\n"),
    ]);
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("VERSION_ONE"));
    edit(&ctx.root, "dep.js", "export const v = 'VERSION_TWO';\n");
    assert!(rebuild_entry(&mut session).unwrap().contains("VERSION_TWO"));
}

#[test]
fn editing_package_json_revalidates() {
    let (ctx, _dir) = package_fixture("browser");
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("IN_BROWSER"));
    edit(
        &ctx.root,
        "node_modules/pkg/package.json",
        r#"{ "name": "pkg", "exports": { ".": "./default.js" } }"#,
    );
    assert!(rebuild_entry(&mut session).unwrap().contains("IN_DEFAULT"));
}

#[test]
fn editing_tsconfig_revalidates() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.ts\", minify: false)"),
        ),
        ("main.ts", "import { v } from '@x/v';\nconsole.log(v);\n"),
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@x/*": ["./one/*"] } } }"#,
        ),
        ("one/v.ts", "export const v = 'FROM_ONE';\n"),
        ("two/v.ts", "export const v = 'FROM_TWO';\n"),
    ]);
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("FROM_ONE"));
    edit(
        &ctx.root,
        "tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@x/*": ["./two/*"] } } }"#,
    );
    assert!(rebuild_entry(&mut session).unwrap().contains("FROM_TWO"));
}

/// A failed build still records what it read: fixing the broken module, or
/// creating the missing one, is noticed by the next rebuild.
#[test]
fn failed_builds_keep_their_deps() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false)"),
        ),
        (
            "main.js",
            "import { v } from './dep.js';\nimport './missing.js';\nconsole.log(v);\n",
        ),
        ("dep.js", "export const v = ;\n"),
    ]);
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).is_err());
    // Nothing changed: the cached failure is replayed, not rebuilt.
    assert!(rebuild_entry(&mut session).is_err());

    edit(&ctx.root, "dep.js", "export const v = 'FIXED';\n");
    assert!(
        rebuild_entry(&mut session).is_err(),
        "missing.js is still missing"
    );
    edit(&ctx.root, "missing.js", "console.log('NOW_HERE');\n");
    let code = rebuild_entry(&mut session).unwrap();
    assert!(
        code.contains("FIXED") && code.contains("NOW_HERE"),
        "{code}"
    );
}

/// The recorded deps include the resolver's probes and package.json reads,
/// not just module contents.
#[test]
fn deps_cover_modules_packages_and_tsconfig() {
    let (ctx, _dir) = package_fixture("browser");
    write(&ctx.root, "tsconfig.json", "{}");
    let world = RenderWorld::new(&ctx).unwrap();
    let mut resolver = Resolver::new(&ctx);
    world.compile_bundle(&mut resolver).unwrap();
    let deps: Vec<_> = resolver.asset_deps().map(|d| d.path.clone()).collect();
    for rel in [
        "main.js",
        "node_modules/pkg/package.json",
        "node_modules/pkg/browser.js",
        "tsconfig.json",
    ] {
        assert!(
            deps.contains(&ctx.root.join(rel)),
            "{rel} not a dep: {deps:#?}"
        );
    }
    assert!(
        deps.iter().all(|d| d.starts_with(&ctx.root)),
        "a dep outside the root: {deps:#?}"
    );
}

/// rolldown appends the `sourceMappingURL` comment after hashing, so the
/// sourcemap build of a module must not share the plain build's file names:
/// both variants of one entry (and its chunk) in one compile coexist.
#[test]
fn sourcemap_and_plain_builds_coexist() {
    let page = "#context [#asset.js(\"/main.js\").url() \
                #asset.js(\"/main.js\", sourcemap: true).url()]";
    let mut files = vec![("content/main.typ", page)];
    files.extend_from_slice(LAZY_FIXTURE);
    let (ctx, _dir) = project(&files);
    let all = keys(&compile(&ctx).unwrap());
    assert_eq!(all.len(), 6, "2 entries + 2 chunks + 2 maps: {all:?}");
    assert_eq!(
        all.iter().filter(|k| k.ends_with(".sm.js")).count(),
        2,
        "{all:?}"
    );
}

/// Every other option that changes the bytes is covered by rolldown's hash:
/// many variants of one entry in one compile never claim one name with two
/// contents (render would reject that).
#[test]
fn option_variants_never_collide() {
    let variants = [
        "",
        "minify: false",
        "format: \"iife\"",
        "split: false",
        "target: \"es2015\"",
        "define: (X: 1)",
        "define: (X: 2)",
        "inject: (h: \"./lazy.js\")",
        "platform: \"neutral\"",
        "sourcemap: true",
        "sourcemap: true, minify: false",
    ];
    let page = variants
        .iter()
        .map(|v| format!("#context asset.js(\"/main.js\", {v}).url()\n"))
        .collect::<String>();
    let mut files = vec![("content/main.typ", page.as_str())];
    files.push((
        "main.js",
        "console.log(typeof X, typeof h, 2 ** 3);\nexport const go = () => import('./lazy.js');\n",
    ));
    files.push(("lazy.js", "export default 'LAZY';\n"));
    let (ctx, _dir) = project(&files);
    compile(&ctx).unwrap();
}

#[test]
fn duplicate_define_keys_are_refused() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", define: (SITE: (debug: false), \"SITE.debug\": true))"),
        ),
        ("main.js", "console.log(SITE.debug);\n"),
    ]);
    let err = compile_err(&ctx);
    assert!(err.contains("sets `SITE.debug` twice"), "{err}");
}

#[test]
fn read_drops_the_sourcemap_directive() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context html.elem(\"script\", asset.js(\"/main.js\", sourcemap: true).read())",
        ),
        ("main.js", "console.log('INLINE');\n"),
    ]);
    let outputs = compile(&ctx).unwrap();
    let page = html(&outputs);
    assert!(page.contains("INLINE"), "{page}");
    assert!(!page.contains("sourceMappingURL"), "{page}");
    assert!(keys(&outputs).is_empty());
}

/// Compiling from inside a tokio runtime (an async handler, a
/// `#[tokio::test]`) must not panic in rolldown's `block_on`.
#[test]
fn builds_from_inside_a_tokio_runtime() {
    let (ctx, _dir) = lazy_project("asset.js(\"/main.js\")");
    let current = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert_eq!(
        current
            .block_on(async { compile(&ctx) })
            .unwrap()
            .iter()
            .count(),
        3
    );
    let multi = tokio::runtime::Builder::new_multi_thread().build().unwrap();
    assert_eq!(
        multi
            .block_on(async { compile(&ctx) })
            .unwrap()
            .iter()
            .count(),
        3
    );
}

/// Retargeting a symlink to another in-project file invalidates, even though
/// the old target (what rolldown actually read) is untouched.
#[cfg(unix)]
#[test]
fn retargeting_a_symlink_revalidates() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false)"),
        ),
        (
            "main.js",
            "import { v } from './helper.js';\nconsole.log(v);\n",
        ),
        ("a.js", "export const v = 'FROM_A';\n"),
        ("b.js", "export const v = 'FROM_B';\n"),
    ]);
    let link = ctx.root.join("helper.js");
    std::os::unix::fs::symlink(ctx.root.join("a.js"), &link).unwrap();
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("FROM_A"));
    std::thread::sleep(Duration::from_millis(10));
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(ctx.root.join("b.js"), &link).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("FROM_B"));
}

/// Replacing an escaping symlink (a cached failure) with one inside the
/// project is noticed.
#[cfg(unix)]
#[test]
fn fixing_an_escaping_symlink_revalidates() {
    let (ctx, dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false)"),
        ),
        (
            "main.js",
            "import { v } from './helper.js';\nconsole.log(v);\n",
        ),
        ("a.js", "export const v = 'INSIDE';\n"),
        ("../outside.js", "export const v = 'OUTSIDE';\n"),
    ]);
    let link = ctx.root.join("helper.js");
    std::os::unix::fs::symlink(dir.path().join("outside.js"), &link).unwrap();
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).is_err());
    std::thread::sleep(Duration::from_millis(10));
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(ctx.root.join("a.js"), &link).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("INSIDE"));
}

// -- groups: one rolldown build per compile -----------------------------------

const A_JS: &str = "import { shared } from './shared.js';\nconsole.log('A', shared);\n";
const B_JS: &str = "import { shared } from './shared.js';\nconsole.log('B', shared);\n";
const SHARED_JS: &str = "export const shared = 'SHARED_V1';\n";

/// Two pages, each with an `asset.js` entry; both entries import
/// `shared.js`. `a_page`/`b_page` are the pages' sources.
fn group_project(a_page: &str, b_page: &str) -> (TwylaContext, tempfile::TempDir) {
    project(&[
        ("content/a.typ", a_page),
        ("content/b.typ", b_page),
        ("a.js", A_JS),
        ("b.js", B_JS),
        ("shared.js", SHARED_JS),
    ])
}

fn a_page() -> String {
    page("asset.js(\"/a.js\", minify: false)")
}

fn b_page() -> String {
    page("asset.js(\"/b.js\", minify: false)")
}

/// A cold build, with the resolver's counters.
fn compile_stats(ctx: &TwylaContext) -> (Result<Outputs, String>, ResolverStats) {
    let world = RenderWorld::new(ctx).unwrap();
    let mut resolver = Resolver::new(ctx);
    let result = world
        .compile_bundle(&mut resolver)
        .map_err(|e| e.to_string());
    (result, resolver.stats)
}

/// The page whose output path starts with `prefix` (`a` for `content/a.typ`).
fn doc_html(outputs: &Outputs, prefix: &str) -> String {
    let docs: Vec<_> = outputs
        .docs()
        .filter(|d| d.output_path.starts_with(prefix))
        .collect();
    assert_eq!(docs.len(), 1, "one page at {prefix}*");
    docs[0].html.clone()
}

/// The whole output directory, as written by `twyla build`: relative path →
/// bytes.
fn tree(outputs: &Outputs) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path.strip_prefix(base).unwrap().display().to_string();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    outputs.emit_to_fs(dir.path()).unwrap();
    let mut out = BTreeMap::new();
    walk(dir.path(), dir.path(), &mut out);
    out
}

/// `tree`'s file names, for assertion messages.
fn names(tree: &BTreeMap<String, Vec<u8>>) -> Vec<&String> {
    tree.keys().collect()
}

#[test]
fn entries_on_two_pages_share_one_build() {
    let (ctx, _dir) = group_project(&a_page(), &b_page());
    let (outputs, stats) = compile_stats(&ctx);
    let outputs = outputs.unwrap();
    assert_eq!(stats.rolldown_builds, 1, "one rolldown build: {stats:?}");
    assert_eq!(stats.builds, 1);

    let all = keys(&outputs);
    assert_eq!(all.len(), 3, "a + b + one shared chunk, got {all:?}");
    let shared = key(&outputs, "assets/shared-");
    let a = text(&outputs, &key(&outputs, "assets/a-"));
    let b = text(&outputs, &key(&outputs, "assets/b-"));
    assert!(!a.contains("SHARED_V1") && !b.contains("SHARED_V1"));
    assert!(text(&outputs, &shared).contains("SHARED_V1"));
    for (prefix, page) in [("a", "assets/a-"), ("b", "assets/b-")] {
        let html = doc_html(&outputs, prefix);
        assert!(html.contains(&format!("PRE /{shared}")), "{html}");
        assert!(html.contains(&format!("URL /{page}")), "{html}");
    }
}

/// A page whose `b.js` entry is only requested once `a.js`'s URL is known
/// (a later fixed-point iteration): it still joins `a.js`'s group.
const LATE_PAGE: &str = "#let a = asset.js(\"/a.js\", minify: false)\n\
     #context [#for u in a.preload() [PRE #u\\ ] URL #a.url()]\n\
     #context if a.url().ends-with(\".js\") [\n\
       #let b = asset.js(\"/b.js\", minify: false)\n\
       #for u in b.preload() [PRE #u\\ ] LATE #b.url()\n\
     ]\n";

#[test]
fn an_entry_discovered_late_joins_the_group() {
    let (ctx, _dir) = group_project(LATE_PAGE, "no scripts here");
    let (outputs, stats) = compile_stats(&ctx);
    let outputs = outputs.unwrap();
    // `{a}`, then `{a, b}` once `b` turns up.
    assert_eq!(stats.rolldown_builds, 2, "{stats:?}");

    let all = keys(&outputs);
    assert_eq!(all.len(), 3, "a + b + one shared chunk, got {all:?}");
    let shared = key(&outputs, "assets/shared-");
    let html = doc_html(&outputs, "a");
    assert_eq!(
        html.matches(&format!("PRE /{shared}")).count(),
        2,
        "both entries preload the shared chunk:\n{html}"
    );
    assert!(html.contains("LATE /assets/b-"), "{html}");

    // The same bundles as when both are found at once.
    let (eager, _dir) = group_project(&a_page(), &b_page());
    let assets = |t: BTreeMap<String, Vec<u8>>| -> BTreeMap<String, Vec<u8>> {
        t.into_iter()
            .filter(|(k, _)| k.starts_with("assets/"))
            .collect()
    };
    let late = assets(tree(&outputs));
    let eager = assets(tree(&compile(&eager).unwrap()));
    assert_eq!(late, eager, "{:?} vs {:?}", names(&late), names(&eager));
}

/// Declaring the entries in the other order (the pages swapped, so the
/// first-discovered entry differs) changes nothing in the output but which
/// page is which.
#[test]
fn group_output_does_not_depend_on_discovery_order() {
    let (one, _d1) = group_project(&a_page(), &b_page());
    let (two, _d2) = group_project(&b_page(), &a_page());
    let one = compile(&one).unwrap();
    let two = compile(&two).unwrap();
    let page_path = |outputs: &Outputs, prefix: &str| {
        outputs
            .docs()
            .find(|d| d.output_path.starts_with(prefix))
            .unwrap()
            .output_path
            .clone()
    };
    let (a_path, b_path) = (page_path(&one, "a"), page_path(&one, "b"));
    let mut swapped = tree(&two);
    let a = swapped.remove(&a_path).unwrap();
    let b = swapped.remove(&b_path).unwrap();
    swapped.insert(a_path, b);
    swapped.insert(b_path, a);
    let one = tree(&one);
    assert_eq!(one, swapped, "{:?} vs {:?}", names(&one), names(&swapped));
}

#[test]
fn serve_rebuilds_a_group_only_when_it_changes() {
    let (ctx, _dir) = group_project(&a_page(), &b_page());
    write(&ctx.root, "content/other.typ", "Other page");
    // Out of the ledger's racy window, so a stamp alone vouches for each
    // module (a file written moments ago is re-hashed on every check).
    for f in ["a.js", "b.js", "shared.js"] {
        std::fs::File::options()
            .write(true)
            .open(ctx.root.join(f))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
    }
    std::thread::sleep(crate::ledger::FINE_RACY_WINDOW + Duration::from_millis(10));
    let mut session = DevSession::new(&ctx).unwrap();
    let rebuild = |session: &mut DevSession| session.rebuild().result.unwrap();
    let cold = |ctx: &TwylaContext| tree(&compile(ctx).unwrap());
    rebuild(&mut session);
    assert_eq!(session.resolver.stats.rolldown_builds, 1);

    // An unrelated page: no rolldown build, no module read.
    let before = session.resolver.stats;
    edit(&ctx.root, "content/other.typ", "Other page, edited");
    rebuild(&mut session);
    assert_eq!(
        session.resolver.stats.rolldown_builds,
        before.rolldown_builds
    );
    assert_eq!(session.resolver.stats.content_reads, before.content_reads);

    // The shared module: one group rebuild.
    edit(
        &ctx.root,
        "shared.js",
        "export const shared = 'SHARED_V2';\n",
    );
    let outputs = rebuild(&mut session);
    assert_eq!(
        session.resolver.stats.rolldown_builds,
        before.rolldown_builds + 1
    );
    assert!(text(&outputs, &key(&outputs, "assets/shared-")).contains("SHARED_V2"));
    assert_eq!(tree(&outputs), cold(&ctx));

    // `b` drops its entry: the group is rebuilt without it, as a cold
    // build of the new project would be (no shared chunk for one entry).
    edit(&ctx.root, "content/b.typ", "no scripts here");
    let outputs = rebuild(&mut session);
    let warm = tree(&outputs);
    assert_eq!(warm, cold(&ctx), "{:?}", names(&warm));
    assert_eq!(keys(&outputs).len(), 1, "{:?}", keys(&outputs));

    // And back.
    edit(&ctx.root, "content/b.typ", &b_page());
    let warm = tree(&rebuild(&mut session));
    assert_eq!(warm, cold(&ctx), "{:?}", names(&warm));
    assert_eq!(warm.keys().filter(|k| k.starts_with("assets/")).count(), 3);
}

/// Different options are different groups; without code splitting
/// (`split: false`, `iife`) every entry is a group of its own.
#[test]
fn groups_follow_options() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context [\n\
               #asset.js(\"/a.js\", minify: false).url()\n\
               #asset.js(\"/b.js\", minify: false).url()\n\
               #asset.js(\"/c.js\", minify: true).url()\n\
               #asset.js(\"/d.js\", minify: false, split: false).url()\n\
               #asset.js(\"/e.js\", minify: false, split: false).url()\n\
               #asset.js(\"/f.js\", minify: false, format: \"iife\").url()\n\
             ]",
        ),
        ("a.js", A_JS),
        ("b.js", B_JS),
        ("c.js", A_JS),
        ("d.js", A_JS),
        ("e.js", B_JS),
        ("f.js", B_JS),
        ("shared.js", SHARED_JS),
    ]);
    let (outputs, stats) = compile_stats(&ctx);
    let outputs = outputs.unwrap();
    // {a, b}, {c}, {d}, {e}, {f}.
    assert_eq!(stats.rolldown_builds, 5, "{stats:?}");
    assert_eq!(stats.builds, 5);
    // Only {a, b} has a chunk; the groups of one inline `shared.js`.
    let shared = key(&outputs, "assets/shared-");
    assert_eq!(keys(&outputs).len(), 7, "{:?}", keys(&outputs));
    for name in ["c", "d", "e", "f"] {
        let code = text(&outputs, &key(&outputs, &format!("assets/{name}-")));
        assert!(code.contains("SHARED_V1"), "{name} doesn't inline shared");
    }
    assert!(text(&outputs, &shared).contains("SHARED_V1"));
}

/// A broken shared module fails the group: each member's error is
/// reported at its own call site. Fixing it recovers.
#[test]
fn a_failed_group_blames_every_member() {
    let (ctx, _dir) = group_project(&a_page(), &b_page());
    write(&ctx.root, "shared.js", "export const shared = ;\n");
    let mut session = DevSession::new(&ctx).unwrap();
    let Err(err) = session.rebuild().result else {
        panic!("expected the group to fail");
    };
    let err = err.to_string();
    assert!(err.contains("a.typ") && err.contains("b.typ"), "{err}");
    assert_eq!(err.matches("asset.js:").count(), 2, "{err}");
    // Cached: replayed, not rebuilt.
    let builds = session.resolver.stats.rolldown_builds;
    assert!(session.rebuild().result.is_err());
    assert_eq!(session.resolver.stats.rolldown_builds, builds);

    edit(&ctx.root, "shared.js", SHARED_JS);
    let outputs = session.rebuild().result.unwrap();
    assert_eq!(keys(&outputs).len(), 3, "{:?}", keys(&outputs));
}

/// Entries governed by different `tsconfig.json` files are one group, but
/// can't share a rolldown build (one tsconfig applies to a whole build):
/// one rolldown build each, each with its own tsconfig.
#[test]
fn a_group_splits_its_builds_by_tsconfig() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context [#asset.js(\"/one/main.ts\", minify: false).url() \
             #asset.js(\"/two/main.ts\", minify: false).url()]",
        ),
        (
            "one/tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@v": ["./v-one.ts"] } } }"#,
        ),
        ("one/main.ts", "import { v } from '@v';\nconsole.log(v);\n"),
        ("one/v-one.ts", "export const v = 'FROM_ONE';\n"),
        (
            "two/tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@v": ["./v-two.ts"] } } }"#,
        ),
        ("two/main.ts", "import { v } from '@v';\nconsole.log(v);\n"),
        ("two/v-two.ts", "export const v = 'FROM_TWO';\n"),
    ]);
    let (outputs, stats) = compile_stats(&ctx);
    let outputs = outputs.unwrap();
    assert_eq!((stats.builds, stats.rolldown_builds), (1, 2), "{stats:?}");
    let codes: Vec<String> = keys(&outputs).iter().map(|k| text(&outputs, k)).collect();
    assert_eq!(codes.len(), 2, "{:?}", keys(&outputs));
    assert!(codes.iter().any(|c| c.contains("FROM_ONE")));
    assert!(codes.iter().any(|c| c.contains("FROM_TWO")));
}

// -- groups follow the live requests ------------------------------------------

/// The emitted `assets/` files of a build: name → bytes.
fn asset_tree(outputs: &Outputs) -> BTreeMap<String, Vec<u8>> {
    tree(outputs)
        .into_iter()
        .filter(|(k, _)| k.starts_with("assets/"))
        .collect()
}

const C_JS: &str = "console.log('C');\n";

/// `c.js` is requested only while `a.js` has no preloads — which holds
/// exactly while `a` is bundled without `b` (they share `shared.js`). With
/// `b` found later than `a`, an intermediate pass bundles `a` alone and
/// requests `c`; once `b` joins, `c` drops out again. The output must be
/// that of the final live set, `{a, b}`: no `c`, the same bundles as when
/// `b` is found right away, in any declaration order.
#[test]
fn a_member_that_stops_being_requested_leaves_the_group() {
    const A: &str = "#let a = asset.js(\"/a.js\", minify: false)\n";
    const C: &str = "#context if a.url().ends-with(\".js\") and a.preload() == () \
                     [C #asset.js(\"/c.js\", minify: false).url()]\n";
    const B_NOW: &str = "#context [B #asset.js(\"/b.js\", minify: false).url()]\n";
    const B_LATER: &str = "#context if a.url().ends-with(\".js\") \
                           [B #asset.js(\"/b.js\", minify: false).url()]\n";
    const A_URL: &str = "#context [A #a.url()]\n";
    let build = |page: String| {
        let (ctx, _dir) = project(&[
            ("content/main.typ", &page),
            ("a.js", A_JS),
            ("b.js", B_JS),
            ("c.js", C_JS),
            ("shared.js", SHARED_JS),
        ]);
        let (outputs, stats) = compile_stats(&ctx);
        let outputs = outputs.unwrap();
        let html = html(&outputs);
        assert!(!html.contains("/assets/c-"), "c requested:\n{html}");
        assert!(!html.contains("__twyla-asset-pending__"), "{html}");
        (asset_tree(&outputs), stats.rolldown_builds)
    };
    let (cold, builds) = build(format!("{A}{A_URL}{B_NOW}{C}"));
    assert_eq!(cold.len(), 3, "a + b + shared: {:?}", names(&cold));
    assert_eq!(builds, 1, "c is never requested");
    for (page, expected_builds) in [
        // `{a}`, then `{a, b, c}`, then `{a, b}`.
        (format!("{A}{A_URL}{B_LATER}{C}"), 3),
        (format!("{A}{C}{B_LATER}{A_URL}"), 3),
        (format!("{A}{C}{A_URL}{B_NOW}"), 1),
    ] {
        let (late, builds) = build(page.clone());
        assert_eq!(builds, expected_builds, "{page}");
        assert_eq!(
            late,
            cold,
            "{page}\n{:?} vs {:?}",
            names(&late),
            names(&cold)
        );
    }
}

/// An inline document whose body captures `a.url()` and requests `b.js`
/// (which regroups `a`, changing its URL): the document follows the final
/// URL instead of conflicting with its own earlier version.
#[test]
fn an_inline_document_follows_a_regrouped_url() {
    let (ctx, _dir) = group_project(
        "#let a = asset.js(\"/a.js\", minify: false)\n\
         #context if a.url().ends-with(\".js\") {\n\
           let u = a.url()\n\
           document(output: \"widget.html\")[A #u #context asset.js(\"/b.js\", minify: false).url()]\n\
         }\n",
        "no scripts here",
    );
    let outputs = compile(&ctx).unwrap();
    let a = key(&outputs, "assets/a-");
    key(&outputs, "assets/shared-");
    let widget = doc_html(&outputs, "a/widget");
    assert!(
        widget.contains(&format!("A /{a}")),
        "stale URL ({a}, {:?}):\n{widget}",
        keys(&outputs)
    );
    assert_eq!(outputs.docs().count(), 3, "index, b, widget");
}

/// The same for any asset: a document capturing a URL that was pending in
/// an earlier pass.
#[test]
fn an_inline_document_capturing_a_pending_url_doesnt_conflict() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context { let u = asset.sass(\"/s.scss\").url(); \
             document(output: \"w.html\")[URL #u] }",
        ),
        ("s.scss", "a { color: red; }"),
    ]);
    let outputs = compile(&ctx).unwrap();
    let w = doc_html(&outputs, "w");
    assert!(w.contains("URL /assets/s-"), "{w}");
}

/// A chain that reveals one more entry each pass (each waits for the
/// previous one's URL) never settles: at the iteration limit that's an
/// error, not a page shipping a pending URL.
#[test]
fn a_group_still_changing_at_the_iteration_limit_is_an_error() {
    // `if e1 resolved { if e2 resolved { … } }`: each entry is only
    // requested once the previous one has a URL.
    let mut body = String::from("[done]");
    let mut files = vec![("shared.js".to_string(), SHARED_JS.to_string())];
    for i in (1..=7).rev() {
        files.push((
            format!("e{i}.js"),
            format!("import {{ shared }} from './shared.js';\nconsole.log({i}, shared);\n"),
        ));
        body = format!(
            "if asset.js(\"/e{i}.js\", minify: false).url().ends-with(\".js\") {{ {body} }}"
        );
    }
    files.push((
        "content/main.typ".to_string(),
        format!("#context {{ {body} }}"),
    ));
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let (ctx, _dir) = project(&files);
    let err = compile_err(&ctx);
    assert!(err.contains("did not stabilize"), "{err}");
}

/// An `asset.js` called from inside a package is refused: bundling only
/// reads the project.
#[test]
fn entries_from_packages_are_refused() {
    let (mut ctx, dir) = project(&[
        (
            "content/main.typ",
            "#import \"@local/p:0.1.0\": app\n\
             #context [#asset.js(\"/main.js\").url() #app().url()]",
        ),
        ("main.js", "console.log('project');\n"),
        (
            "../packages/local/p/0.1.0/typst.toml",
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\nentrypoint = \"lib.typ\"\n",
        ),
        (
            "../packages/local/p/0.1.0/lib.typ",
            "#let app() = asset.js(\"/main.js\")",
        ),
        (
            "../packages/local/p/0.1.0/main.js",
            "console.log('package');\n",
        ),
    ]);
    ctx.package_dir = Some(dir.path().join("packages"));
    let err = compile_err(&ctx);
    assert!(err.contains("not a package file"), "{err}");
}

/// Failing compiles in a row (here: a broken stylesheet), each with a
/// different `define`, don't grow the group cache: each attempt keeps only
/// its own groups and the last successful compile's.
#[test]
fn failed_compiles_dont_grow_the_group_cache() {
    let page = |n: usize| {
        format!(
            "#context [#asset.js(\"/a.js\", minify: false, define: (N: {n})).url() \
             #asset.sass(\"/s.scss\").url()]"
        )
    };
    let (ctx, _dir) = project(&[
        ("content/main.typ", &page(0)),
        ("a.js", A_JS),
        ("shared.js", SHARED_JS),
        ("s.scss", "a { color: red; }"),
    ]);
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(session.rebuild().result.is_ok());
    assert_eq!(session.resolver.js_group_count(), 1);
    edit(&ctx.root, "s.scss", "a { color: ");
    for n in 1..6 {
        edit(&ctx.root, "content/main.typ", &page(n));
        assert!(session.rebuild().result.is_err());
        assert_eq!(
            session.resolver.js_group_count(),
            2,
            "attempt {n}: its own group and the last good one"
        );
    }
    edit(&ctx.root, "s.scss", "a { color: red; }");
    assert!(session.rebuild().result.is_ok());
    assert_eq!(session.resolver.js_group_count(), 1);
}

/// A failing tsconfig lookup for one entry doesn't keep the others from
/// being built (and their deps recorded), and every error is reported.
#[test]
fn a_tsconfig_error_doesnt_stop_the_other_partitions() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context [#asset.js(\"/one/main.ts\", minify: false).url() \
             #asset.js(\"/two/main.ts\", minify: false).url()]",
        ),
        ("one/tsconfig.json", "{ not json"),
        ("one/main.ts", "console.log(1);\n"),
        ("two/tsconfig.json", "{}"),
        ("two/main.ts", "import { v } from './v';\nconsole.log(v);\n"),
        ("two/v.ts", "export const v = ;\n"),
    ]);
    let (result, stats) = compile_stats(&ctx);
    let Err(err) = result else {
        panic!("expected the group to fail");
    };
    assert_eq!(stats.rolldown_builds, 1, "two/ still bundled: {stats:?}");
    assert!(err.contains("tsconfig"), "{err}");
    assert!(err.contains("v.ts"), "two/'s error missing: {err}");
}

// -- import queries: ?raw, ?url, ?inline ---------------------------------------

/// A project whose `main.js` is `main` and whose page links it (`URL ..`)
/// and links `asset.file("/logo.png")` (`FILE ..`), plus `files`.
fn query_project(main: &str, files: &[(&str, &[u8])]) -> (TwylaContext, tempfile::TempDir) {
    let (ctx, dir) = project(&[
        (
            "content/main.typ",
            "#context [URL #asset.js(\"/main.js\", minify: false).url()]",
        ),
        ("main.js", main),
    ]);
    for (rel, bytes) in files {
        let path = ctx.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    (ctx, dir)
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really a png";

#[test]
fn raw_imports_the_file_as_a_string() {
    let (ctx, _dir) = query_project(
        "import shader from './shader.glsl?raw';\n\
         import source from './typed.ts?raw';\n\
         console.log(shader, source);\n",
        &[
            ("shader.glsl", b"void main() { SHADER_BODY; }\n"),
            ("typed.ts", b"const typed: number = 1;\n"),
        ],
    );
    let outputs = compile(&ctx).unwrap();
    let code = entry(&outputs);
    assert!(code.contains("void main() { SHADER_BODY; }"), "{code}");
    assert!(
        code.contains("const typed: number = 1;"),
        "raw .ts must be untransformed:\n{code}"
    );
    assert_eq!(keys(&outputs).len(), 1, "nothing else emitted");
}

#[test]
fn raw_of_invalid_utf8_is_an_error() {
    let (ctx, _dir) = query_project(
        "import b from './bin.dat?raw';\nconsole.log(b);\n",
        &[("bin.dat", &[0xff, 0xfe, 0x00])],
    );
    let err = compile_err(&ctx);
    assert!(err.contains("bin.dat?raw` isn't valid UTF-8"), "{err}");
}

/// The URL in the bundle is byte-for-byte `asset.file(..).url()`, and with
/// both used, the file is emitted once.
#[test]
fn url_matches_asset_file() {
    for base_url in [None, Some("https://example.com/site")] {
        let (mut ctx, _dir) = project(&[
            (
                "content/main.typ",
                "#context [URL #asset.js(\"/main.js\", minify: false).url()]\n\n\
                 #context [FILE #asset.file(\"/img/logo.png\").url()]",
            ),
            (
                "main.js",
                "import logo from './img/logo.png?url';\nconsole.log(logo);\n",
            ),
        ]);
        std::fs::create_dir_all(ctx.root.join("img")).unwrap();
        std::fs::write(ctx.root.join("img/logo.png"), PNG).unwrap();
        ctx.base_url = base_url.map(str::to_string);
        let outputs = compile(&ctx).unwrap();
        let html = html(&outputs);
        let file_url = html
            .split("FILE ")
            .nth(1)
            .and_then(|rest| rest.split('<').next())
            .unwrap()
            .trim()
            .to_string();
        assert!(file_url.contains("/assets/logo-"), "{html}");
        let code = entry(&outputs);
        assert!(
            code.contains(&format!("\"{file_url}\"")),
            "{file_url} not in:\n{code}"
        );
        let logos: Vec<String> = keys(&outputs)
            .into_iter()
            .filter(|k| k.contains("logo-"))
            .collect();
        assert_eq!(logos.len(), 1, "{logos:?}");
    }
}

/// A `?url` file is emitted with the entry that imports it, alone; and
/// with an entry that's only `.read()` (inlined), since the inlined code
/// references it by its absolute URL.
#[test]
fn url_files_are_emitted_with_their_entry() {
    for page in [
        "#context [URL #asset.js(\"/main.js\").url()]",
        "#context html.elem(\"script\", asset.js(\"/main.js\").read())",
    ] {
        let (ctx, _dir) = project(&[
            ("content/main.typ", page),
            (
                "main.js",
                "import logo from './logo.png?url';\nconsole.log(logo);\n",
            ),
        ]);
        std::fs::write(ctx.root.join("logo.png"), PNG).unwrap();
        let outputs = compile(&ctx).unwrap();
        let logos: Vec<String> = keys(&outputs)
            .into_iter()
            .filter(|k| k.starts_with("assets/logo-") && k.ends_with(".png"))
            .collect();
        assert_eq!(logos.len(), 1, "{page}: {:?}", keys(&outputs));
        let bytes = match outputs.get(&logos[0]) {
            Some(Output::Companion { emit, .. }) => emit.read().unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(bytes.as_slice(), PNG);
        assert!(html(&outputs).contains(&format!("/{}", logos[0])) || page.contains(".url()"));
    }
}

/// Two entries of one group importing one `?url` file: emitted once, a
/// companion of exactly the entries that reach it (here through a shared
/// chunk), not of a third that doesn't.
#[test]
fn url_files_follow_exactly_the_entries_that_reach_them() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context [#asset.js(\"/a.js\", minify: false).url() \
             #asset.js(\"/b.js\", minify: false).url() \
             #asset.js(\"/c.js\", minify: false).url()]",
        ),
        (
            "a.js",
            "import { logo } from './shared.js';\nconsole.log('A', logo);\n",
        ),
        (
            "b.js",
            "import { logo } from './shared.js';\nconsole.log('B', logo);\n",
        ),
        ("c.js", "console.log('C');\n"),
        (
            "shared.js",
            "import logo from './logo.png?url';\nexport { logo };\n",
        ),
    ]);
    std::fs::write(ctx.root.join("logo.png"), PNG).unwrap();
    let world = RenderWorld::new(&ctx).unwrap();
    let mut resolver = Resolver::new(&ctx);
    let outputs = world.compile_bundle(&mut resolver).unwrap();
    let logos: Vec<String> = keys(&outputs)
        .into_iter()
        .filter(|k| k.ends_with(".png"))
        .collect();
    assert_eq!(logos.len(), 1, "{:?}", keys(&outputs));
    for asset in resolver.resolved_assets() {
        let stem = asset.built.stem.clone().unwrap();
        let has = asset.built.companions.iter().any(|c| c.path == logos[0]);
        assert_eq!(has, stem != "c", "{stem}");
        assert!(
            !asset.preload.iter().any(|p| p.ends_with(".png")),
            "?url files aren't preloads"
        );
    }
}

#[test]
fn inline_is_a_data_url_with_the_files_mime_type() {
    use base64::Engine as _;
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
    let (ctx, _dir) = query_project(
        "import icon from './icon.svg?inline';\n\
         import blob from './blob.xyz?inline';\n\
         console.log(icon, blob);\n",
        &[("icon.svg", svg), ("blob.xyz", b"\x00\x01")],
    );
    let outputs = compile(&ctx).unwrap();
    let code = entry(&outputs);
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    assert!(
        code.contains(&format!("data:image/svg+xml;base64,{}", b64(svg))),
        "{code}"
    );
    assert!(
        code.contains(&format!(
            "data:application/octet-stream;base64,{}",
            b64(b"\x00\x01")
        )),
        "{code}"
    );
    assert_eq!(keys(&outputs).len(), 1, "nothing else emitted");
}

#[test]
fn unknown_or_combined_queries_are_errors() {
    for (import, shown) in [
        ("./x.txt?foo", "`?foo`"),
        ("./x.txt?raw&url", "`?raw&url`"),
        ("./x.txt?url&worker", "`?url&worker`"),
    ] {
        let (ctx, _dir) = query_project(
            &format!("import x from '{import}';\nconsole.log(x);\n"),
            &[("x.txt", b"x")],
        );
        let err = compile_err(&ctx);
        assert!(
            err.contains(&format!("unsupported import query {shown}"))
                && err.contains("`?raw`, `?url`, `?inline`, `?worker` and `?worker&url`"),
            "{import}: {err}"
        );
    }
}

#[test]
fn queried_files_outside_the_project_fail() {
    let (ctx, _dir) = project(&[
        ("content/main.typ", &page("asset.js(\"/main.js\")")),
        (
            "main.js",
            "import x from '../outside.txt?raw';\nconsole.log(x);\n",
        ),
        ("../outside.txt", "OUTSIDE_SECRET"),
    ]);
    let err = compile_err(&ctx);
    assert!(err.contains("can't resolve `../outside.txt?raw`"), "{err}");
    assert!(!err.contains("OUTSIDE_SECRET"));
}

/// A queried import is its own module: distinct from the plain import of
/// the same file, and one module for every specifier of the same file with
/// the same query.
#[test]
fn queried_imports_are_distinct_modules_and_dedupe() {
    let (ctx, _dir) = query_project(
        "import { v } from './lib/dep.js';\n\
         import one from './lib/dep.js?raw';\n\
         import two from './lib/../lib/dep.js?raw';\n\
         console.log(v, one, two);\n",
        &[("lib/dep.js", b"export const v = 'DEP' + 'VALUE';\n")],
    );
    let code = entry(&outputs_of(&ctx));
    // The plain import is bundled as code; the raw one as a string, once.
    assert_eq!(
        code.matches("export const v = 'DEP' + 'VALUE';").count(),
        1,
        "{code}"
    );
    assert!(
        code.contains("\"DEPVALUE\""),
        "the plain import is code:\n{code}"
    );
    // Module ids are root-relative: the output doesn't depend on where the
    // project is.
    assert!(
        !code.contains(&ctx.root.display().to_string()),
        "absolute path in:\n{code}"
    );
}

fn outputs_of(ctx: &TwylaContext) -> Outputs {
    compile(ctx).unwrap()
}

#[test]
fn editing_a_queried_file_revalidates() {
    let (ctx, _dir) = query_project(
        "import t from './note.txt?raw';\n\
         import u from './logo.png?url';\n\
         import d from './icon.svg?inline';\n\
         console.log(t, u, d);\n",
        &[
            ("note.txt", b"NOTE_ONE"),
            ("logo.png", PNG),
            ("icon.svg", b"<svg/>"),
        ],
    );
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("NOTE_ONE"));
    for (file, contents) in [
        ("note.txt", "NOTE_TWO"),
        ("logo.png", "another png"),
        ("icon.svg", "<svg><g/></svg>"),
    ] {
        edit(&ctx.root, file, contents);
        let warm = tree(&session.rebuild().result.unwrap());
        assert_eq!(warm, tree(&compile(&ctx).unwrap()), "after editing {file}");
    }
}

#[test]
fn queries_write_nothing() {
    let (ctx, _dir) = query_project(
        "import t from './note.txt?raw';\n\
         import u from './logo.png?url';\n\
         import d from './icon.svg?inline';\n\
         console.log(t, u, d);\n",
        &[
            ("note.txt", b"NOTE"),
            ("logo.png", PNG),
            ("icon.svg", b"<svg/>"),
        ],
    );
    let listing = |root: &Path| -> Vec<std::path::PathBuf> {
        let mut all = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                all.push(path);
            }
        }
        all.sort();
        all
    };
    let before = listing(&ctx.root);
    compile(&ctx).unwrap();
    assert_eq!(listing(&ctx.root), before);
}

// -- queried imports resolve through the sandbox ---------------------------------

/// A serve rebuild, which must equal a cold build of the project as it is
/// now (both by output tree).
fn assert_warm_is_cold(session: &mut DevSession, ctx: &TwylaContext, what: &str) {
    let warm = session.rebuild().result.map(|o| tree(&o));
    let cold = compile(ctx).map(|o| tree(&o));
    match (warm, cold) {
        (Ok(warm), Ok(cold)) => assert_eq!(warm, cold, "{what}: {:?}", names(&warm)),
        (Err(warm), Err(_)) => panic!("{what}: both failed: {warm}"),
        (warm, cold) => panic!(
            "{what}: warm ok = {}, cold ok = {}",
            warm.is_ok(),
            cold.is_ok()
        ),
    }
}

/// Resolving a queried import never reads outside the root: a
/// `package.json` above the project whose `browser` field would remap the
/// import is never seen, and every dep the build records is inside the
/// root, including the resolver's probes for the queried file.
#[test]
fn query_resolution_stays_in_the_sandbox() {
    let (ctx, dir) = query_project(
        "import t from './note.txt?raw';\nconsole.log(t);\n",
        &[("note.txt", b"INSIDE_NOTE"), ("evil.txt", b"EVIL_NOTE")],
    );
    std::fs::write(
        dir.path().join("package.json"),
        r#"{ "name": "outside", "browser": { "./site/note.txt": "./site/evil.txt" } }"#,
    )
    .unwrap();
    let world = RenderWorld::new(&ctx).unwrap();
    let mut resolver = Resolver::new(&ctx);
    let outputs = world.compile_bundle(&mut resolver).unwrap();
    let code = entry(&outputs);
    assert!(
        code.contains("INSIDE_NOTE") && !code.contains("EVIL"),
        "{code}"
    );
    let deps: Vec<_> = resolver.asset_deps().map(|d| d.path.clone()).collect();
    for dep in &deps {
        assert!(
            dep.starts_with(&ctx.root),
            "dep outside the root: {}",
            dep.display()
        );
    }
    let note = ctx.root.join("note.txt");
    assert!(deps.contains(&note), "no dep on note.txt: {deps:?}");
}

#[test]
fn creating_a_missing_queried_file_is_noticed() {
    let (ctx, _dir) = query_project("import t from './note.txt?raw';\nconsole.log(t);\n", &[]);
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(session.rebuild().result.is_err());
    edit(&ctx.root, "note.txt", "NOW_HERE");
    assert_warm_is_cold(&mut session, &ctx, "after creating note.txt");
}

/// `?url` through a symlink in the project: named after the link (like
/// `asset.file`), and retargeting the link (old target untouched) is
/// noticed.
#[cfg(unix)]
#[test]
fn url_through_a_symlink() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context [URL #asset.js(\"/main.js\", minify: false).url()]\n\n\
             #context [FILE #asset.file(\"/logo.png\").url()]",
        ),
        (
            "main.js",
            "import logo from './logo.png?url';\nconsole.log(logo);\n",
        ),
        ("one.png", "first png"),
        ("two.png", "second png"),
    ]);
    let link = ctx.root.join("logo.png");
    std::os::unix::fs::symlink(ctx.root.join("one.png"), &link).unwrap();
    let outputs = compile(&ctx).unwrap();
    let pngs: Vec<String> = keys(&outputs)
        .into_iter()
        .filter(|k| k.ends_with(".png"))
        .collect();
    assert_eq!(pngs.len(), 1, "one name for both: {pngs:?}");
    assert!(pngs[0].starts_with("assets/logo-"), "{pngs:?}");
    assert!(entry(&outputs).contains(&format!("/{}", pngs[0])));

    let mut session = DevSession::new(&ctx).unwrap();
    session.rebuild().result.unwrap();
    std::thread::sleep(Duration::from_millis(10));
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(ctx.root.join("two.png"), &link).unwrap();
    assert_warm_is_cold(&mut session, &ctx, "after retargeting logo.png");
}

#[test]
fn package_exports_changes_reach_queried_imports() {
    let (ctx, _dir) = query_project(
        "import t from 'pkg/data?raw';\nconsole.log(t);\n",
        &[
            (
                "node_modules/pkg/package.json",
                br#"{ "name": "pkg", "exports": { "./data": "./one.txt" } }"#,
            ),
            ("node_modules/pkg/one.txt", b"DATA_ONE"),
            ("node_modules/pkg/two.txt", b"DATA_TWO"),
        ],
    );
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("DATA_ONE"));
    edit(
        &ctx.root,
        "node_modules/pkg/package.json",
        r#"{ "name": "pkg", "exports": { "./data": "./two.txt" } }"#,
    );
    assert_warm_is_cold(&mut session, &ctx, "after editing exports");
    assert!(rebuild_entry(&mut session).unwrap().contains("DATA_TWO"));
}

#[test]
fn tsconfig_paths_apply_to_queried_imports() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context [URL #asset.js(\"/main.ts\", minify: false).url()]",
        ),
        (
            "main.ts",
            "import t from '@text/note.txt?raw';\nconsole.log(t);\n",
        ),
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@text/*": ["./one/*"] } } }"#,
        ),
        ("one/note.txt", "FROM_ONE"),
        ("two/note.txt", "FROM_TWO"),
    ]);
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("FROM_ONE"));
    edit(
        &ctx.root,
        "tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@text/*": ["./two/*"] } } }"#,
    );
    assert_warm_is_cold(&mut session, &ctx, "after editing tsconfig");
    assert!(rebuild_entry(&mut session).unwrap().contains("FROM_TWO"));
}

// -- the query splitter ------------------------------------------------------------

#[test]
fn query_specifier_forms() {
    let ok = |import: &str, files: &[(&str, &[u8])], expect: &str| {
        let (ctx, _dir) = query_project(
            &format!("import t from '{import}';\nconsole.log(t);\n"),
            files,
        );
        let code = entry(&compile(&ctx).unwrap());
        assert!(code.contains(expect), "{import}:\n{code}");
    };
    // A `:` inside a file name isn't a scheme.
    ok("./a:b.txt?raw", &[("a:b.txt", b"COLON_FILE")], "COLON_FILE");
    // Percent escapes stay literal in relative paths.
    ok(
        "./a%20b.txt?raw",
        &[("a%20b.txt", b"PERCENT_FILE"), ("a b.txt", b"SPACE_FILE")],
        "PERCENT_FILE",
    );
    // A bare package subpath goes through `exports`.
    ok(
        "pkg/data?raw",
        &[
            (
                "node_modules/pkg/package.json",
                br#"{ "name": "pkg", "exports": { "./data": "./real/data.txt" } }"#,
            ),
            ("node_modules/pkg/real/data.txt", b"EXPORTED_DATA"),
        ],
        "EXPORTED_DATA",
    );
    // A leading `#` is a package import, not a fragment.
    ok(
        "#internal/x.txt?raw",
        &[
            (
                "package.json",
                br##"{ "name": "site", "imports": { "#internal/*": "./internal/*" } }"##,
            ),
            ("internal/x.txt", b"PACKAGE_IMPORT"),
        ],
        "PACKAGE_IMPORT",
    );

    let err = |import: &str, expect: &str| {
        let (ctx, _dir) = query_project(
            &format!("import t from '{import}';\nconsole.log(t);\n"),
            &[("a.txt", b"A")],
        );
        let err = compile_err(&ctx);
        assert!(err.contains(expect), "{import}: {err}");
    };
    err("./a.txt?raw=", "unsupported import query `?raw=`");
    err("./a.txt?raw=value", "unsupported import query `?raw=value`");
    err("./a.txt?raw#part", "can't have a `#` fragment");
    err("./a.txt#part?raw", "can't have a `#` fragment");
}

#[test]
fn inline_mime_types_ignore_extension_case() {
    let (ctx, _dir) = query_project(
        "import a from './icon.SVG?inline';\nimport b from './note.txt?inline';\n\
         console.log(a, b);\n",
        &[("icon.SVG", b"<svg/>"), ("note.txt", b"hi")],
    );
    let code = entry(&compile(&ctx).unwrap());
    assert!(code.contains("\"data:image/svg+xml;base64,"), "{code}");
    assert!(
        code.contains("\"data:text/plain;charset=utf-8;base64,aGk=\""),
        "{code}"
    );
}

// Root-absolute imports, `import.meta.env`, workers.

/// `/x` in an import is relative to the project root, in every form an
/// import takes: plain, queried, dynamic, and in `inject`.
#[test]
fn root_absolute_imports_resolve_against_the_project_root() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page(
                "asset.js(\"/scripts/app/main.js\", minify: false, \
                 inject: (h: \"/lib/h.js\"))",
            ),
        ),
        (
            "scripts/app/main.js",
            "import { util } from '/lib/util.ts';\n\
             import note from '/data/note.txt?raw';\n\
             console.log(util, note, h('x'));\n\
             import('/lib/lazy.js').then((m) => console.log(m.lazy));\n",
        ),
        ("lib/util.ts", "export const util: string = 'ROOT_UTIL';\n"),
        ("lib/h.js", "export default (t) => 'ROOT_H:' + t;\n"),
        ("lib/lazy.js", "export const lazy = 'ROOT_LAZY';\n"),
        ("data/note.txt", "ROOT_NOTE"),
    ]);
    let outputs = compile(&ctx).unwrap();
    let code = entry(&outputs);
    for marker in ["ROOT_UTIL", "ROOT_NOTE", "ROOT_H:"] {
        assert!(code.contains(marker), "{marker} missing:\n{code}");
    }
    let lazy = key(&outputs, "assets/lazy-");
    assert!(text(&outputs, &lazy).contains("ROOT_LAZY"));
    // The queried import's module id names it root-relatively.
    assert!(!code.contains(&*ctx.root.to_string_lossy()), "{code}");
}

/// A root-absolute import can't climb out of the root, and every dep it
/// records stays inside it.
#[test]
fn root_absolute_imports_stay_in_the_root() {
    for import in ["/../secret.js", "/../secret.txt?raw"] {
        let (ctx, dir) = project(&[
            ("content/main.typ", &page("asset.js(\"/main.js\")")),
            (
                "main.js",
                &format!("import s from '{import}';\nconsole.log(s);\n"),
            ),
        ]);
        std::fs::write(dir.path().join("secret.js"), "export default 'SECRET';\n").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "SECRET").unwrap();
        let world = RenderWorld::new(&ctx).unwrap();
        let mut resolver = Resolver::new(&ctx);
        let err = world
            .compile_bundle(&mut resolver)
            .map(|o| keys(&o))
            .expect_err(import);
        let err = err.to_string();
        assert!(!err.contains("SECRET"), "{import}: {err}");
        assert!(
            err.contains("outside the twyla project root"),
            "{import}: {err}"
        );
        for dep in resolver.asset_deps() {
            assert!(
                dep.path.starts_with(&ctx.root),
                "{import}: {}",
                dep.path.display()
            );
        }
    }
}

#[test]
fn root_absolute_imports_warm_equals_cold() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false)"),
        ),
        (
            "main.js",
            "import { util } from '/lib/util.js';\nimport n from '/note.txt?raw';\n\
             console.log(util, n);\n",
        ),
        ("lib/util.js", "export const util = 'UTIL_ONE';\n"),
        ("note.txt", "NOTE_ONE"),
    ]);
    let mut session = DevSession::new(&ctx).unwrap();
    assert!(rebuild_entry(&mut session).unwrap().contains("UTIL_ONE"));
    edit(
        &ctx.root,
        "lib/util.js",
        "export const util = 'UTIL_TWO';\n",
    );
    assert_warm_is_cold(&mut session, &ctx, "after editing util.js");
    edit(&ctx.root, "note.txt", "NOTE_TWO");
    assert_warm_is_cold(&mut session, &ctx, "after editing note.txt");
    assert!(entry(&compile(&ctx).unwrap()).contains("NOTE_TWO"));
}

/// The `import.meta.env` recipe: flattened by `define`, and dead branches
/// on it removed.
#[test]
fn import_meta_env_defines_fold() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page(
                "asset.js(\"/main.js\", define: (\"import.meta.env\": \
                 (MODE: \"production\", DEV: false, PROD: true)))",
            ),
        ),
        (
            "main.js",
            "if (import.meta.env.DEV) console.log('DEV_ONLY');\n\
             if (import.meta.env.PROD) console.log('PROD_ONLY');\n\
             console.log(import.meta.env.MODE);\n",
        ),
    ]);
    let code = entry(&compile(&ctx).unwrap());
    assert!(!code.contains("DEV_ONLY"), "{code}");
    assert!(code.contains("PROD_ONLY"), "{code}");
    assert!(code.contains("production"), "{code}");
    assert!(!code.contains("import.meta.env"), "{code}");
}

/// `import.meta.env.BASE_URL` is twyla's URL prefix (with a trailing
/// slash) unless `define` sets it.
#[test]
fn import_meta_env_base_url_defaults_to_the_base_url() {
    let cases = [
        (None, "", "\"/\""),
        (
            Some("https://example.com/site"),
            "",
            "\"https://example.com/site/\"",
        ),
        (
            Some("https://example.com/site/"),
            "",
            "\"https://example.com/site/\"",
        ),
        (
            None,
            ", define: (\"import.meta.env.BASE_URL\": \"/x/\")",
            "\"/x/\"",
        ),
        (
            Some("https://example.com/site"),
            ", define: (\"import.meta.env\": (BASE_URL: \"/y/\"))",
            "\"/y/\"",
        ),
    ];
    for (base_url, define, expected) in cases {
        let (mut ctx, _dir) = project(&[
            (
                "content/main.typ",
                &page(&format!("asset.js(\"/main.js\", minify: false{define})")),
            ),
            ("main.js", "console.log(import.meta.env.BASE_URL);\n"),
        ]);
        ctx.base_url = base_url.map(str::to_string);
        let code = entry(&compile(&ctx).unwrap());
        assert!(code.contains(expected), "{base_url:?}{define}: {code}");
    }
}

/// `new URL("<local path>", import.meta.url)` anywhere but as a module
/// worker's URL would resolve next to the bundled chunk, where the file
/// isn't: an error naming the file and line, blamed on the call, with the
/// `?url` hint. That includes a script (a download link isn't a worker), a
/// path without `./` (relative in a browser too), and a missing file.
#[test]
fn new_url_of_a_local_file_is_an_error() {
    for (arg, file) in [
        ("'./pic.png'", "pic.png"),
        ("`./pic.png`", "pic.png"),
        ("'/pic.png'", "pic.png"),
        ("'pic.png'", "pic.png"),
        ("'pkg/x.png'", "pkg/x.png"),
        ("'./missing.png'", "missing.png"),
        ("'./download.js'", "download.js"),
    ] {
        let (ctx, _dir) = project(&[
            (
                "content/main.typ",
                &page("asset.js(\"/main.js\", minify: false)"),
            ),
            (
                "main.js",
                // The literal on line 1 (in a comment) isn't the call.
                &format!("// see {arg}\nconsole.log(new URL({arg}, import.meta.url));\n"),
            ),
            ("pic.png", "not really"),
            ("download.js", "export {};\n"),
        ]);
        let err = compile_err(&ctx);
        assert!(err.contains("main.js:2:"), "{arg}: no line: {err}");
        assert!(err.contains(&format!("{file}?url")), "{arg}: {err}");
    }
}

#[test]
fn new_url_of_non_literals_and_urls_is_left_alone() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false)"),
        ),
        (
            "main.js",
            "export function at(name) { return new URL(name, import.meta.url); }\n\
             console.log(new URL('https://example.com/x.png', import.meta.url));\n\
             console.log(new URL('//cdn.example.com/x.png', import.meta.url));\n\
             console.log(import.meta.url);\n",
        ),
    ]);
    let outputs = compile(&ctx).unwrap();
    let code = entry(&outputs);
    assert_eq!(keys(&outputs).len(), 1, "{:?}", keys(&outputs));
    assert!(code.contains("new URL(name, import.meta.url)"), "{code}");
    assert!(
        code.contains("new URL(\"https://example.com/x.png\""),
        "{code}"
    );
    assert!(code.contains("\"//cdn.example.com/x.png\""), "{code}");
    assert!(code.contains("console.log(import.meta.url)"), "{code}");
}

const WORKER_TS: &str = "import { shared } from './shared.ts';\n\
     self.onmessage = (e: MessageEvent) => postMessage(shared + e.data);\n";
const WORKER_SHARED_TS: &str = "export const shared: string = 'WORKER_SHARED';\n";

fn worker_project(main: &str, call: &str) -> (TwylaContext, tempfile::TempDir) {
    project(&[
        ("content/main.typ", &page(call)),
        ("main.js", main),
        ("w.ts", WORKER_TS),
        ("shared.ts", WORKER_SHARED_TS),
    ])
}

/// The one worker chunk among `outputs`: its key and code.
fn worker_chunk(outputs: &Outputs) -> (String, String) {
    let key = key(outputs, "assets/w-");
    let code = text(outputs, &key);
    (key, code)
}

#[test]
fn worker_query_bundles_a_worker_entry() {
    for base_url in [None, Some("https://example.com/site")] {
        let (mut ctx, _dir) = worker_project(
            "import W from './w.ts?worker';\nconst w = new W();\nw.postMessage('hi');\n",
            "asset.js(\"/main.js\", minify: false)",
        );
        ctx.base_url = base_url.map(str::to_string);
        let outputs = compile(&ctx).unwrap();
        let code = entry(&outputs);
        let (worker, worker_code) = worker_chunk(&outputs);
        assert!(worker_code.contains("WORKER_SHARED"), "{worker_code}");
        let url = ctx.asset_url(&worker);
        assert!(
            code.contains(&format!("\"{url}\"")),
            "{url} not in:\n{code}"
        );
        assert!(code.contains("type: \"module\""), "{code}");
        // Not a preload: a worker starts only when the code starts it.
        assert!(!html(&outputs).contains("PRE"), "{}", html(&outputs));
    }
}

#[test]
fn worker_url_query_is_the_url() {
    let (ctx, _dir) = worker_project(
        "import u from './w.ts?worker&url';\nnew Worker(u, { type: 'module' });\n",
        "asset.js(\"/main.js\", minify: false)",
    );
    let outputs = compile(&ctx).unwrap();
    let (worker, _) = worker_chunk(&outputs);
    let code = entry(&outputs);
    assert!(code.contains(&format!("\"/{worker}\"")), "{code}");
    assert!(!code.contains("WorkerWrapper"), "{code}");
}

#[test]
fn new_worker_new_url_bundles_a_worker_entry() {
    for arg in ["'./w.ts'", "'/w.ts'"] {
        let (ctx, _dir) = worker_project(
            &format!(
                "const w = new Worker(new URL({arg}, import.meta.url), {{ type: 'module' }});\nw.postMessage(1);\n"
            ),
            "asset.js(\"/main.js\", minify: false)",
        );
        let outputs = compile(&ctx).unwrap();
        let (worker, worker_code) = worker_chunk(&outputs);
        assert!(worker_code.contains("WORKER_SHARED"), "{worker_code}");
        let code = entry(&outputs);
        let name = worker.strip_prefix("assets/").unwrap();
        // Relative to the chunk (rolldown's rewrite): next to it in `assets/`.
        assert!(
            code.contains(&format!("new URL(\"{name}\", import.meta.url)")),
            "{arg}: {code}"
        );
    }
}

#[test]
fn workers_need_code_splitting() {
    for (main, call) in [
        (
            "import W from './w.ts?worker';\nnew W();\n",
            "asset.js(\"/main.js\", split: false)",
        ),
        (
            "import W from './w.ts?worker';\nnew W();\n",
            "asset.js(\"/main.js\", format: \"iife\")",
        ),
        (
            "new Worker(new URL('./w.ts', import.meta.url), { type: 'module' });\n",
            "asset.js(\"/main.js\", split: false)",
        ),
    ] {
        let (ctx, _dir) = worker_project(main, call);
        let err = compile_err(&ctx);
        assert!(err.contains("code splitting"), "{call}: {err}");
    }
}

/// A worker chunk (and the chunks it imports) go with every entry that
/// reaches the importing module, even through a dynamic import, and with
/// no other.
#[test]
fn worker_chunks_follow_the_entries_that_reach_them() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context [A #asset.js(\"/a.js\", minify: false).url()]\n\n\
             #context [B #asset.js(\"/b.js\", minify: false).url()]\n\n\
             #context [C #asset.js(\"/c.js\", minify: false).url()]",
        ),
        ("a.js", "import('./lazy.js').then((m) => m.start());\n"),
        ("b.js", "import { start } from './lazy.js';\nstart();\n"),
        ("c.js", "console.log('C');\n"),
        (
            "lazy.js",
            "import W from './w.ts?worker';\nexport function start() { return new W(); }\n",
        ),
        ("w.ts", WORKER_TS),
        ("shared.ts", WORKER_SHARED_TS),
    ]);
    let outputs = compile(&ctx).unwrap();
    let (worker, _) = worker_chunk(&outputs);
    let companions_of = |prefix: &str| -> Vec<String> {
        let key = key(&outputs, prefix);
        match outputs.get(&key) {
            Some(Output::Asset { asset, .. }) => asset
                .built
                .companions
                .iter()
                .map(|c| c.path.clone())
                .collect(),
            other => panic!("{other:?}"),
        }
    };
    assert!(companions_of("assets/a-").contains(&worker));
    assert!(companions_of("assets/b-").contains(&worker));
    assert!(!companions_of("assets/c-").contains(&worker));
    let preload = |prefix: &str| -> Vec<String> {
        match outputs.get(&key(&outputs, prefix)) {
            Some(Output::Asset { asset, .. }) => asset.built.preload.clone(),
            other => panic!("{other:?}"),
        }
    };
    assert!(!preload("assets/b-").contains(&worker));
}

#[test]
fn workers_warm_equals_cold() {
    let (ctx, _dir) = worker_project(
        "import W from './w.ts?worker';\nnew W();\nnew Worker(new URL('./w.ts', import.meta.url), { type: 'module' });\n",
        "asset.js(\"/main.js\", minify: false)",
    );
    let mut session = DevSession::new(&ctx).unwrap();
    rebuild_entry(&mut session).unwrap();
    edit(
        &ctx.root,
        "shared.ts",
        "export const shared: string = 'WORKER_SHARED_TWO';\n",
    );
    assert_warm_is_cold(&mut session, &ctx, "after editing the worker's import");
    let outputs = compile(&ctx).unwrap();
    assert!(worker_chunk(&outputs).1.contains("WORKER_SHARED_TWO"));
}

/// A classic script has no `import.meta`: rolldown would replace it with
/// `{}` and the code would see `undefined`. An error instead, unless
/// `define` sets the value (as twyla does `import.meta.env.BASE_URL`).
#[test]
fn import_meta_in_an_iife_is_an_error() {
    for code in [
        "console.log(import.meta.url);\n",
        "console.log(import.meta.env.MODE);\n",
    ] {
        let (ctx, _dir) = project(&[
            (
                "content/main.typ",
                &page("asset.js(\"/main.js\", format: \"iife\")"),
            ),
            ("main.js", code),
        ]);
        let err = compile_err(&ctx);
        assert!(
            err.contains("main.js") && err.contains("classic"),
            "{code}: {err}"
        );
    }
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page(
                "asset.js(\"/main.js\", minify: false, format: \"iife\", \
                 define: (\"import.meta.env.MODE\": \"production\"))",
            ),
        ),
        (
            "main.js",
            "console.log(import.meta.env.MODE, import.meta.env.BASE_URL);\n",
        ),
    ]);
    let code = entry(&compile(&ctx).unwrap());
    assert!(code.contains("\"production\", \"/\""), "{code}");
}

/// `new Worker(new URL(..))` must be started as a module worker, with the
/// options written out: the worker is bundled as a module.
#[test]
fn new_worker_must_say_type_module() {
    for options in [
        "",
        ", {}",
        ", { type: 'classic' }",
        ", opts",
        ", { ...opts }",
    ] {
        let (ctx, _dir) = worker_project(
            &format!(
                "const opts = {{ type: 'module' }};\n\
                 new Worker(new URL('./w.ts', import.meta.url){options});\n"
            ),
            "asset.js(\"/main.js\", minify: false)",
        );
        let err = compile_err(&ctx);
        assert!(
            err.contains("main.js:2:") && err.contains("type: \"module\""),
            "{options}: {err}"
        );
    }
    for start in [
        "new SharedWorker(new URL('./w.ts', import.meta.url), { type: 'module' })",
        "new Worker(new URL('./w.ts', import.meta.url), { name: 'w', \"type\": \"module\" })",
    ] {
        let (ctx, _dir) = worker_project(
            &format!("console.log({start});\n"),
            "asset.js(\"/main.js\", minify: false)",
        );
        let outputs = compile(&ctx).unwrap();
        assert!(
            worker_chunk(&outputs).1.contains("WORKER_SHARED"),
            "{start}"
        );
    }
}

/// A worker whose start was tree-shaken away isn't shipped.
#[test]
fn a_tree_shaken_worker_start_ships_nothing() {
    for start in [
        "new Worker(new URL('./w.ts', import.meta.url), { type: 'module' })",
        "new W()",
    ] {
        let (ctx, _dir) = worker_project(
            &format!(
                "import W from './w.ts?worker';\n\
                 function unused() {{ return {start}; }}\n\
                 console.log('live');\n"
            ),
            "asset.js(\"/main.js\", minify: false)",
        );
        let outputs = compile(&ctx).unwrap();
        assert!(
            !keys(&outputs).iter().any(|k| k.starts_with("assets/w-")),
            "{start}: {:?}",
            keys(&outputs)
        );
    }
}

/// One file both an entry (on one page) and a worker (started on another),
/// with the same options: each page gets the file its URL names.
#[test]
fn an_entry_that_is_also_a_worker() {
    for start in [
        "import W from './w.ts?worker';\nnew W();\n",
        "new Worker(new URL('./w.ts', import.meta.url), { type: 'module' });\n",
    ] {
        let (ctx, _dir) = project(&[
            (
                "content/main.typ",
                "#context [MAIN #asset.js(\"/main.js\", minify: false).url()]\n\n\
                 #context [W #asset.js(\"/w.ts\", minify: false).url()]",
            ),
            ("main.js", start),
            ("w.ts", WORKER_TS),
            ("shared.ts", WORKER_SHARED_TS),
        ]);
        let outputs = compile(&ctx).unwrap();
        let main = key(&outputs, "assets/main-");
        let Some(Output::Asset { asset, .. }) = outputs.get(&main) else {
            panic!()
        };
        let code = text(&outputs, &main);
        let workers: Vec<&String> = asset
            .built
            .companions
            .iter()
            .filter(|c| c.path.starts_with("assets/w-") && c.path.ends_with(".js"))
            .map(|c| &c.path)
            .collect();
        assert_eq!(workers.len(), 1, "{start}: {workers:?}");
        let name = workers[0].strip_prefix("assets/").unwrap();
        assert!(code.contains(name), "{start}: {name} not in\n{code}");
        assert!(keys(&outputs).contains(workers[0]));
        let html = html(&outputs);
        let w_url = html
            .split("W ")
            .nth(1)
            .unwrap()
            .split('<')
            .next()
            .unwrap()
            .trim();
        let w_key = w_url.trim_start_matches('/');
        assert!(keys(&outputs).iter().any(|k| k == w_key), "{w_key}");
    }
}

/// An inlined (`.read()`) script can start a `?worker` (fetched by its
/// absolute URL): the worker's files are emitted though the entry isn't.
/// Not a relative `new URL` worker, and not a real chunk import.
#[test]
fn read_with_workers() {
    let read = "#context html.elem(\"script\", attrs: (type: \"module\"), \
                asset.js(\"/main.js\", minify: false).read())";
    let (ctx, _dir) = project(&[
        ("content/main.typ", read),
        ("main.js", "import W from './w.ts?worker';\nnew W();\n"),
        (
            "w.ts",
            "import { lazy } from './lazy.ts';\nconsole.log(lazy);\nimport('./lazy.ts');\n",
        ),
        ("lazy.ts", "export const lazy = 'LAZY';\n"),
    ]);
    let outputs = compile(&ctx).unwrap();
    let keys = keys(&outputs);
    assert!(keys.iter().any(|k| k.starts_with("assets/w-")), "{keys:?}");
    assert!(
        keys.iter().all(|k| !k.starts_with("assets/main-")),
        "{keys:?}"
    );
    let worker = keys.iter().find(|k| k.starts_with("assets/w-")).unwrap();
    assert!(
        html(&outputs).contains(&format!("/{worker}")),
        "{}",
        html(&outputs)
    );

    let (ctx, _dir) = project(&[
        ("content/main.typ", read),
        (
            "main.js",
            "new Worker(new URL('./w.ts', import.meta.url), { type: 'module' });\n",
        ),
        ("w.ts", "console.log('W');\n"),
    ]);
    let err = compile_err(&ctx);
    assert!(
        err.contains("starts the worker `w-") && err.contains("relative"),
        "{err}"
    );

    let (ctx, _dir) = project(&[
        ("content/main.typ", read),
        (
            "main.js",
            "import W from './w.ts?worker';\nnew W();\n\
             import('./lazy.ts').then((m) => console.log(m.lazy));\n",
        ),
        ("w.ts", "console.log('W');\n"),
        ("lazy.ts", "export const lazy = 'LAZY';\n"),
    ]);
    let err = compile_err(&ctx);
    assert!(err.contains("imports a chunk (`lazy-"), "{err}");
}

/// Two modules in one chunk starting different workers: the companions are
/// in file-name order whichever finished loading first, so warm and cold
/// builds agree.
#[test]
fn several_workers_warm_equals_cold() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false)"),
        ),
        (
            "main.js",
            "import A from './a.js';\nimport B from './b.js';\nconsole.log(A(), B());\n",
        ),
        (
            "a.js",
            "import W from './wa.ts?worker';\nexport default () => new W();\n",
        ),
        (
            "b.js",
            "export default () => new Worker(new URL('./wb.ts', import.meta.url), \
             { type: 'module' });\n",
        ),
        ("wa.ts", "console.log('WA');\n"),
        ("wb.ts", "console.log('WB');\n"),
    ]);
    let mut session = DevSession::new(&ctx).unwrap();
    rebuild_entry(&mut session).unwrap();
    edit(&ctx.root, "wb.ts", "console.log('WB2');\n");
    assert_warm_is_cold(&mut session, &ctx, "after editing wb.ts");
    edit(&ctx.root, "wa.ts", "console.log('WA2');\n");
    assert_warm_is_cold(&mut session, &ctx, "after editing wa.ts");
}
