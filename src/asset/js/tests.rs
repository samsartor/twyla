//! `asset.js` end to end: fixtures written by hand into a temp dir (no npm,
//! no network — packages are fake `node_modules/<pkg>/package.json`s).

use std::path::Path;
use std::time::Duration;

use crate::project::TwylaContext;
use crate::render::{Output, Outputs, RenderWorld};
use crate::resolver::Resolver;
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

/// Two entries in separate builds that load the same module dynamically
/// both carry the same chunk (same name, same bytes): it's emitted once,
/// not a conflict.
#[test]
fn identical_chunks_from_two_entries_dedupe() {
    let (ctx, _dir) = project(&[
        (
            "content/main.typ",
            "#context [#asset.js(\"/a.js\", minify: false).url() \
             #asset.js(\"/b.js\", minify: false).url()]",
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
