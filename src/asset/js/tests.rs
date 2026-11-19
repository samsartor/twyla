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
    }
}

/// `new URL(.., import.meta.url)` isn't an import: it's left as written
/// (rolldown's asset resolution for it is opt-in, and twyla leaves it off,
/// since it reads the file behind the sandbox).
#[test]
fn new_url_is_left_alone() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            &page("asset.js(\"/main.js\", minify: false)"),
        ),
        (
            "main.js",
            "console.log(new URL('./pic.png', import.meta.url));\n",
        ),
        ("pic.png", "not really"),
    ]);
    let outputs = compile(&ctx).unwrap();
    assert_eq!(keys(&outputs).len(), 1, "{:?}", keys(&outputs));
    assert!(entry(&outputs).contains("new URL(\"./pic.png\", import.meta.url)"));
}

/// Rolldown errors blame the `asset.js` call and carry rolldown's own
/// location (file and line).
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
