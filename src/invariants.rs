//! End-to-end tests of the invariant `twyla serve`'s self-healing rests on:
//!
//! > The watch set after any rebuild (successful or failed) contains every
//! > path whose contents, existence, or listing could change that rebuild's
//! > output.
//!
//! Three angles on it, all over one rich fixture site ([`site_files`]) that
//! exercises every dependency kind twyla has:
//!
//! - **Completeness** ([`watch_set_is_complete`]): build the site cold, then
//!   for every file and directory under the project root and the package dir,
//!   apply each mutation (rewrite, same-length rewrite keeping the mtime,
//!   delete, file replaced by a directory, add a file to a directory) to a
//!   fresh copy. A mutation the watch set can't see must leave a cold build's
//!   output unchanged.
//! - **Incremental = cold** ([`incremental_matches_cold`]): drive one
//!   persistent [`DevSession`] through a seeded pseudo-random edit sequence,
//!   as serve would, and after every step compare its output with a fresh
//!   cold build of the same tree. [`polling_watcher_schedules_targeted_rebuilds`]
//!   drives a few targeted edits through the real (polling) watcher instead.
//! - **Determinism** ([`cold_builds_are_deterministic`]): two cold builds of
//!   one tree agree. See [`inline_document_names_are_stable_across_processes`]
//!   for the cross-process case.
//!
//! Outputs are compared as a [`Snapshot`]: every output path with a digest of
//! what's written there, or the normalized error text of a failed build.
//!
//! What "cold" means here: a fresh [`DevSession`] — new world, `FileStore`
//! and resolver — in the same process, so it still shares comemo's global
//! memo cache (and, in test builds, the font scan; see `render::load_fonts`).
//! comemo validates every memo hit against the tracked world, so sharing it
//! can only hide a bug where twyla feeds typst data comemo doesn't track.
//! [`incremental_matches_cold_evicted`] is the one lane that clears it
//! (`comemo::evict(0)`) before every cold build — short and serial, since
//! that's ~3× slower.
//!
//! The racy-stamp path (a same-length rewrite within one timestamp tick of
//! the read) is exercised by the differential edits but rarely *needed*:
//! Linux ≥ 6.13's multigrain timestamps give a write after a `stat` a fresh
//! fine-grained ctime, so on such a system the stamp alone already differs.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::project::TwylaContext;
use crate::render::{Emit, Output, Outputs, RenderError};
use crate::session::{DevSession, Stamp, WatchSet};

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// Fixture paths are relative to a base dir holding the project (`site/`) and
/// the injected package dir (`packages/`), so both get mutated alike.
const SITE: &str = "site";
const PACKAGES: &str = "packages";

/// A file of the fixture, or (contents `None`) an empty directory.
type Files = BTreeMap<String, Option<Vec<u8>>>;

fn png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let img = ::image::RgbImage::from_pixel(width, height, ::image::Rgb(rgb));
    let mut buf = Vec::new();
    ::image::DynamicImage::ImageRgb8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut buf),
            ::image::ImageFormat::Png,
        )
        .unwrap();
    buf
}

const SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 2 2\">\
                   <rect width=\"1\" height=\"1\" fill=\"red\"/></svg>";

const GREET_TOML: &str =
    "[package]\nname = \"greet\"\nversion = \"0.1.0\"\nentrypoint = \"lib.typ\"\n";

/// The passing fixture: every dependency kind, plus decoys nothing reads.
fn site_files() -> Files {
    let text = |s: &str| Some(s.as_bytes().to_vec());
    let mut files = Files::new();
    let mut add = |path: &str, contents: Option<Vec<u8>>| {
        files.insert(path.to_string(), contents);
    };

    // Pages. `main.typ` uses every kind of dependency at least once.
    add(
        "site/content/main.typ",
        text(
            "#import \"/templates/base.typ\": layout, shout\n\
             #import \"@local/greet:0.1.0\": hi\n\
             #show: layout\n\
             = Home\n\
             #shout[welcome] #hi\n\n\
             Notes: #read(\"/data/notes.txt\")\n\n\
             #image(\"/images/pic.png\", alt: \"pic\")\n\n\
             #context link(asset.file(\"/files/doc.txt\").url())[doc]\n\
             #context link(asset.image(\"/images/photo.png\", width: 4).url())[photo]\n\
             #context link(asset.svg(\"/images/icon.svg\").url())[icon]\n\
             #context link(asset.sass(\"/styles/main.scss\").url())[css]\n\
             #context raw-html(str(asset.raw(read(\"/data/raw.css\"), extension: \"css\").read(encoding: none)))\n\
             #context link(asset.raw(\"b{}\", extension: \"css\").url())[inline-raw]\n\
             #context link(asset.typst(\"/assets/fig.typ\", format: \"svg\").url())[fig]\n\
             #context link(asset.js(\"/scripts/app.ts\", sourcemap: true).url())[js]\n\
             #context link(document(output: auto)[Inline child #shout[x]].url())[child]\n\
             #document(output: \"extra.html\")[Extra page]\n",
        ),
    );
    add(
        "site/content/_private.typ",
        text("#let private-note = [private note]"),
    );
    add(
        "site/content/blog/main.typ",
        text("Blog: #context documents().map(d => d.output).sorted().join(\", \")"),
    );
    add(
        "site/content/blog/post.typ",
        text(
            "#import \"/content/_private.typ\": private-note\n\
             Post #private-note\n\
             #context link(asset.sass(\"/styles/main.scss\").url())[css]\n\
             #context link(asset.js(\"/scripts/post.ts\", sourcemap: true).url())[js]\n\
             #context link(asset.js(\"/scripts/solo.ts\", split: false).url())[solo]",
        ),
    );
    add("site/content/blog/2024/old.typ", text("Old post"));
    add("site/content/blog/notes.txt", text("not a page"));

    // Templates.
    add(
        "site/templates/base.typ",
        text(
            "#import \"_parts.typ\": footer\n\
             #let layout(body) = { body; footer }\n\
             #let shout(x) = strong(x)",
        ),
    );
    add(
        "site/templates/_parts.typ",
        text("#let footer = [Footer text]"),
    );

    // Data read by typst, and by `asset.raw` through typst.
    add("site/data/notes.txt", text("some notes"));
    add("site/data/raw.css", text(".raw { color: green; }"));

    // Asset sources.
    add("site/files/doc.txt", text("a document"));
    add("site/images/pic.png", Some(png(3, 3, [200, 10, 10])));
    add("site/images/photo.png", Some(png(8, 8, [10, 120, 200])));
    add("site/images/icon.svg", text(SVG));
    add(
        "site/styles/main.scss",
        text("@use \"partials/colors\";\n.a { color: colors.$c; }"),
    );
    add("site/styles/partials/_colors.scss", text("$c: red;"));
    add(
        "site/assets/fig.typ",
        text(
            "#import \"_fig-dep.typ\": caption\n\
             #import \"@local/greet:0.1.0\": hi\n\
             #set page(width: 3cm, height: 1cm)\n\
             #caption #hi",
        ),
    );
    add("site/assets/_fig-dep.typ", text("#let caption = [Figure]"));
    // `asset.js`: a tsconfig `paths` alias, a package resolved through its
    // `package.json` `exports`, and a dynamic-import chunk.
    add(
        "site/tsconfig.json",
        text(r#"{ "compilerOptions": { "paths": { "@util/*": ["./scripts/util/*"] } } }"#),
    );
    add(
        "site/scripts/app.ts",
        text(
            "import { v } from \"@util/v\";\n\
             import { p } from \"pkg\";\n\
             console.log(v, p);\n\
             export const later = () => import(\"./lazy\");\n",
        ),
    );
    // A second entry with the same options, on another page: one group
    // with `app.ts`, sharing `util/v.ts` as a chunk. It also imports a
    // `?raw` and a `?url` file (one root-absolute), and starts a worker
    // (itself importing `util/v.ts`). And a `split: false` entry, a group of
    // its own.
    add(
        "site/scripts/post.ts",
        text(
            "import { v } from \"@util/v\";\n\
             import notes from \"/data/notes.txt?raw\";\n\
             import pic from \"../images/pic.png?url\";\n\
             import W from \"./worker.ts?worker\";\n\
             console.log(\"post\", v, notes, pic, new W());\n",
        ),
    );
    add(
        "site/scripts/worker.ts",
        text(
            "import { v } from \"/scripts/util/v.ts\";\n\
             self.onmessage = () => postMessage(v);\n",
        ),
    );
    add(
        "site/scripts/solo.ts",
        text(
            "import { v } from \"@util/v\";\n\
             console.log(\"solo\", v);\n",
        ),
    );
    add(
        "site/scripts/util/v.ts",
        text("export const v: string = \"v\";"),
    );
    add(
        "site/scripts/lazy.ts",
        text("export const lazy = \"lazy\";"),
    );
    add(
        "site/node_modules/pkg/package.json",
        text(
            r#"{ "name": "pkg", "exports": { ".": { "browser": "./browser.js", "default": "./index.js" } } }"#,
        ),
    );
    add(
        "site/node_modules/pkg/browser.js",
        text("export const p = \"browser\";"),
    );

    // Static files.
    add("site/static/robots.txt", text("User-agent: *"));
    add("site/static/css/extra.css", text("body { margin: 0; }"));

    // The `@local` package, used by a page and by `asset.typst`.
    add("packages/local/greet/0.1.0/typst.toml", text(GREET_TOML));
    add(
        "packages/local/greet/0.1.0/lib.typ",
        text("#import \"util.typ\": who\n#let hi = [hi from #who]"),
    );
    add(
        "packages/local/greet/0.1.0/util.typ",
        text("#let who = \"greet\""),
    );

    // Decoys: nothing reads these.
    add("site/README.md", text("# readme"));
    add("site/data/unused.txt", text("unused"));
    add("site/images/unused.png", Some(png(2, 2, [0, 0, 0])));
    add("site/styles/partials/_unused.scss", text("$u: blue;"));
    add("site/templates/unused.typ", text("#let unused = 1"));
    add("site/assets/unused.typ", text("unused"));
    add("site/scripts/unused.ts", text("export const unused = 1;"));
    add(
        "site/node_modules/pkg/index.js",
        text("export const p = \"node\";"),
    );
    add("site/drafts", None);
    add("packages/local/greet/0.1.0/README.md", text("greet"));
    add(
        "packages/local/other/0.2.0/typst.toml",
        text("[package]\nname = \"other\"\nversion = \"0.2.0\"\nentrypoint = \"lib.typ\"\n"),
    );
    add("packages/local/other/0.2.0/lib.typ", text("#let other = 1"));
    files
}

/// One way of breaking the fixture: files to overwrite, or (`None`) delete.
struct Variant {
    name: &'static str,
    changes: Vec<(&'static str, Option<&'static str>)>,
}

/// The passing fixture, then every failing variant: each asset kind broken in
/// turn, a broken and a missing `.typ` import, a broken package, and an
/// output conflict. The invariant must hold after failed builds too.
fn variants() -> Vec<Variant> {
    let v = |name, changes| Variant { name, changes };
    vec![
        v("passing", vec![]),
        v(
            "error in an imported template",
            vec![(
                "site/templates/_parts.typ",
                Some("#let footer = { oops-undefined }"),
            )],
        ),
        v(
            "missing imported template",
            vec![("site/templates/_parts.typ", None)],
        ),
        v(
            "unresolved sass @use",
            vec![(
                "site/styles/main.scss",
                Some("@use \"partials/missing\";\n.a { color: red; }"),
            )],
        ),
        v(
            "sass error in a partial",
            vec![("site/styles/partials/_colors.scss", Some("$c: ;"))],
        ),
        v(
            "missing asset.file source",
            vec![("site/files/doc.txt", None)],
        ),
        v(
            "undecodable asset.image",
            vec![("site/images/photo.png", Some("not a png"))],
        ),
        v(
            "missing asset.svg source",
            vec![("site/images/icon.svg", None)],
        ),
        v("missing asset.raw input", vec![("site/data/raw.css", None)]),
        v(
            "error in an asset.typst import",
            vec![(
                "site/assets/_fig-dep.typ",
                Some("#let caption = { oops-undefined }"),
            )],
        ),
        v(
            "error in a package file",
            vec![("packages/local/greet/0.1.0/util.typ", Some("#let who = {"))],
        ),
        v("missing native image", vec![("site/images/pic.png", None)]),
        v(
            "syntax error in an asset.js module",
            vec![("site/scripts/util/v.ts", Some("export const v = ;"))],
        ),
        v(
            "syntax error in one asset.js entry of a group",
            vec![("site/scripts/post.ts", Some("console.log(;"))],
        ),
        v(
            "asset.js package export to a missing file",
            vec![(
                "site/node_modules/pkg/package.json",
                Some(r#"{ "name": "pkg", "exports": "./missing.js" }"#),
            )],
        ),
        v(
            "static file conflicts with a page",
            vec![("site/static/index.html", Some("<p>static</p>"))],
        ),
    ]
}

fn apply_variant(files: &mut Files, variant: &Variant) {
    for (path, contents) in &variant.changes {
        match contents {
            Some(c) => files.insert(path.to_string(), Some(c.as_bytes().to_vec())),
            None => files.remove(*path),
        };
    }
}

/// Write `files` under a fresh temp dir.
fn materialize(files: &Files) -> tempfile::TempDir {
    let base = tempfile::tempdir().unwrap();
    for (rel, contents) in files {
        let path = base.path().join(rel);
        match contents {
            Some(bytes) => {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, bytes).unwrap();
            }
            None => fs::create_dir_all(&path).unwrap(),
        }
    }
    base
}

fn context(base: &Path) -> TwylaContext {
    let mut ctx = TwylaContext::new(base.join(SITE), Some("https://example.com".into())).unwrap();
    ctx.package_dir = Some(base.join(PACKAGES));
    ctx
}

// ---------------------------------------------------------------------------
// Comparing builds
// ---------------------------------------------------------------------------

/// What a build produced, comparable across trees: output path → digest of
/// what's written there, or the error text with the base dir replaced by
/// `<base>` and colors stripped.
///
/// Path-backed outputs are compared by their source path, not bytes read
/// now: serve reads the bytes live. Static files by path alone; `asset.file`
/// copies also by the digest the build captured, since a fixed `output:`
/// path keeps the key and source across an edit while a stale build's digest
/// (and so its `.sha256`/`.read()` results) would differ.
type Snapshot = Result<BTreeMap<String, String>, String>;

fn snapshot(result: &Result<Outputs, RenderError>, base: &Path) -> Snapshot {
    let rel = |p: &Path| {
        p.strip_prefix(base)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| p.display().to_string())
    };
    match result {
        Ok(outputs) => Ok(outputs
            .iter()
            .map(|output| {
                let digest = match output {
                    Output::Doc(doc) => format!("doc {}", doc.html),
                    Output::Asset { asset, .. } => match &asset.built.emit {
                        Emit::Bytes(bytes) => {
                            format!("bytes {}", hex::encode(crate::ledger::sha256(bytes)))
                        }
                        // The digest captured at build time, not the bytes
                        // there now: a stale build of an edited source keeps
                        // its output path and source, but not its digest.
                        Emit::Copy(src) => {
                            format!("copy {} {}", rel(src), hex::encode(asset.built.sha256))
                        }
                    },
                    Output::Companion { sha256, .. } => format!("bytes {}", hex::encode(sha256)),
                    Output::Static(_, src) => format!("static {}", rel(src)),
                };
                (output.key().to_string(), digest)
            })
            .collect()),
        Err(err) => {
            Err(strip_ansi(&err.to_string()).replace(&base.display().to_string(), "<base>"))
        }
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: ESC [ params final-byte
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Where two snapshots differ, briefly (first few differing keys).
fn describe_diff(a: &Snapshot, b: &Snapshot) -> String {
    match (a, b) {
        (Ok(a), Ok(b)) => {
            let mut out = String::new();
            for key in a
                .keys()
                .chain(b.keys())
                .collect::<std::collections::BTreeSet<_>>()
            {
                let (x, y) = (a.get(key), b.get(key));
                if x != y {
                    let short = |v: Option<&String>| match v {
                        None => "(absent)".to_string(),
                        Some(v) => v.chars().take(300).collect(),
                    };
                    let _ = writeln!(out, "  {key}:\n    - {}\n    + {}", short(x), short(y));
                }
            }
            out
        }
        (Ok(_), Err(e)) => format!("  succeeded, then failed:\n{e}"),
        (Err(e), Ok(_)) => format!("  failed, then succeeded (was:\n{e})"),
        (Err(x), Err(y)) => format!("  different errors:\n--- was\n{x}\n--- now\n{y}"),
    }
}

/// A fresh session's first build of `base`: the cold reference.
fn cold(base: &Path) -> (Snapshot, WatchSet) {
    let mut session = DevSession::new(&context(base)).unwrap();
    let rebuild = session.rebuild();
    (snapshot(&rebuild.result, base), rebuild.watch)
}

/// The watched paths whose observation no longer holds — what serve's
/// watcher would report. Same check as [`crate::watch::Watcher::update`].
fn stale_paths(watch: &WatchSet) -> Vec<PathBuf> {
    watch
        .paths
        .iter()
        .filter(|(path, stamp)| {
            !stamp.matches_now(path)
                || watch
                    .racy
                    .get(*path)
                    .is_some_and(|hash| crate::ledger::observe_hash(path).1.ok() != Some(*hash))
        })
        .map(|(path, _)| path.clone())
        .collect()
}

/// Run `jobs` on every core. Each job is independent (its own temp dir and
/// session), so the order doesn't matter; results come back in job order.
fn parallel<J: Sync, R: Send>(jobs: &[J], run: impl Fn(&J) -> R + Sync) -> Vec<R> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let result = run(job);
                    results.lock().unwrap().push((i, result));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, r)| r).collect()
}

// ---------------------------------------------------------------------------
// 1. Watch-set completeness
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mutation {
    /// Prefix the file's contents (changes every file kind's meaning: a
    /// `.typ` page's text, a stylesheet's syntax, an image's decodability).
    Rewrite,
    /// Change the first byte in place, keeping the length, then set the
    /// mtime back: only the ctime (unix) or a racy hash can see it. Exposes
    /// any stamp weaker than that (mtime + length alone; or no length,
    /// which every other rewrite here would mask by changing it).
    RewriteSameLength,
    /// Replace the file with a same-named directory holding `new.typ`. The
    /// parent's entry names don't change, so only the entry kind in its
    /// listing stamp can see it (e.g. an ignored `notes.txt` under
    /// `content/` becoming a section with a page).
    ReplaceWithDir,
    /// Remove the file, or the directory and everything under it.
    Delete,
    /// Create `new.typ` in the directory (a routed page under `content/`).
    AddFile,
    /// Create a file next to a stylesheet that sass import resolution could
    /// pick instead of (or in conflict with) it: the other `_` form, another
    /// extension, or a directory index. `.0` is the name to create, with
    /// `{bare}` standing for the stylesheet's stem without its `_`.
    AddShadow(&'static str),
    /// Create the (missing) file at this path: its fixture contents if it has
    /// any, else something small and valid. The way a failed build gets
    /// fixed, so every failing variant must notice it. See [`CREATIONS`].
    Create,
}

/// Paths a build might look for without finding: the files the failing
/// variants delete, the candidates of an unresolved sass `@use`, pages and
/// static files that don't exist yet, another package version.
const CREATIONS: &[&str] = &[
    "site/templates/_parts.typ",
    "site/files/doc.txt",
    "site/images/icon.svg",
    "site/images/pic.png",
    "site/data/raw.css",
    "site/styles/partials/_missing.scss",
    "site/styles/partials/missing.scss",
    "site/styles/partials/missing/_index.scss",
    "site/static/index.html",
    "site/content/blog/2024/new.typ",
    "site/content/new-dir/deep/page.typ",
    "packages/local/greet/0.1.0/util.typ",
    "packages/local/greet/0.2.0/lib.typ",
    "packages/local/greet/0.2.0/typst.toml",
];

/// Every file and directory under `base` — the project and the package dir,
/// both roots included — relative to it.
fn all_paths(base: &Path) -> Vec<(PathBuf, bool)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, bool)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let is_dir = path.is_dir();
            out.push((path.strip_prefix(base).unwrap().to_path_buf(), is_dir));
            if is_dir {
                walk(base, &path, out);
            }
        }
    }
    let mut out = vec![(PathBuf::from(SITE), true)];
    walk(base, &base.join(SITE), &mut out);
    out.push((PathBuf::from(PACKAGES), true));
    walk(base, &base.join(PACKAGES), &mut out);
    out
}

fn mutations_of(rel: &Path, is_dir: bool) -> Vec<Mutation> {
    let mut mutations = match (is_dir, rel == Path::new(SITE)) {
        // Deleting the project root makes the context itself fail.
        (true, true) => vec![Mutation::AddFile],
        (true, false) => vec![Mutation::Delete, Mutation::AddFile],
        (false, _) => vec![
            Mutation::Rewrite,
            Mutation::RewriteSameLength,
            Mutation::Delete,
        ],
    };
    // Only the scanned trees discover files by listing directories; anywhere
    // else a file turned directory is just the file gone (`Delete`), and the
    // extra cold builds would double the test's runtime.
    if !is_dir && (rel.starts_with("site/content") || rel.starts_with("site/static")) {
        mutations.push(Mutation::ReplaceWithDir);
    }
    if rel.extension().is_some_and(|e| e == "scss") {
        let shadows: &[&'static str] = &[
            "{bare}.scss",
            "_{bare}.scss",
            "{bare}.sass",
            "{bare}.css",
            "{bare}/_index.scss",
            "{bare}/index.scss",
        ];
        mutations.extend(shadows.iter().map(|s| Mutation::AddShadow(s)));
    }
    mutations
}

fn mutate(base: &Path, rel: &Path, mutation: Mutation) {
    let path = base.join(rel);
    match mutation {
        Mutation::Rewrite => {
            let mut bytes = b"twyla-mutated\n".to_vec();
            bytes.extend(fs::read(&path).unwrap());
            fs::write(&path, bytes).unwrap();
        }
        Mutation::RewriteSameLength => {
            let mtime = fs::metadata(&path).unwrap().modified().unwrap();
            let mut bytes = fs::read(&path).unwrap();
            bytes[0] = if bytes[0] == b'#' { b'%' } else { b'#' };
            fs::write(&path, bytes).unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        }
        Mutation::ReplaceWithDir => {
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            fs::write(path.join("new.typ"), "new page").unwrap();
        }
        Mutation::Delete if path.is_dir() => fs::remove_dir_all(&path).unwrap(),
        Mutation::Delete => fs::remove_file(&path).unwrap(),
        Mutation::AddFile => fs::write(path.join("new.typ"), "new page").unwrap(),
        Mutation::Create => {
            let contents = site_files()
                .remove(rel.to_str().unwrap())
                .flatten()
                .unwrap_or_else(|| match rel.extension().and_then(|e| e.to_str()) {
                    Some("scss") => b"$c: navy;".to_vec(),
                    _ => b"created".to_vec(),
                });
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
        }
        Mutation::AddShadow(pattern) => {
            let stem = path.file_stem().unwrap().to_str().unwrap();
            let name = pattern.replace("{bare}", stem.trim_start_matches('_'));
            let shadow = path.with_file_name(name);
            if !shadow.exists() {
                fs::create_dir_all(shadow.parent().unwrap()).unwrap();
                fs::write(shadow, ".shadow { color: purple; }").unwrap();
            }
        }
    }
}

/// For a mutation of a path the watch set doesn't contain: whether `watch`,
/// taken from a build of the pristine tree at `pristine`, would notice the
/// difference between it and the (mutated) copy at `copy`. Saves a build
/// per job; mutations of *watched* paths are checked with the real stamps
/// on their own tree instead (see [`watch_set_is_complete`]).
///
/// Stamps are per-tree (inodes, times), so they can't be compared across
/// copies directly. Every stamp kind except [`Stamp::File`] is a function of
/// names and entry kinds only, so it's checked with the real
/// [`Stamp::matches_now`] against the copy. A mutation of an unwatched path
/// leaves every watched file's bytes alone (it can only delete one, by
/// deleting a directory), so for a `File` stamp "same bytes and permissions"
/// is the copy-independent equivalent — and can't credit a weak stamp.
fn copy_differs(watch: &WatchSet, pristine: &Path, copy: &Path) -> bool {
    watch.paths.iter().any(|(path, stamp)| {
        let Ok(rel) = path.strip_prefix(pristine) else {
            panic!("watched path outside the fixture: {}", path.display());
        };
        let there = copy.join(rel);
        match stamp {
            Stamp::Unknown => true,
            Stamp::File { .. } => {
                let perm = |p: &Path| fs::metadata(p).map(|m| m.permissions()).ok();
                !there.is_file()
                    || fs::read(path).ok() != fs::read(&there).ok()
                    || perm(path) != perm(&there)
            }
            stamp => !stamp.matches_now(&there),
        }
    })
}

/// The key test. For the passing fixture and every failing variant: build it
/// cold, then for every file and directory (watched or not), apply every
/// mutation to a fresh copy. If the watch set can't see the mutation, a cold
/// build of the copy must produce exactly the same output (or error) as the
/// pristine tree — otherwise serve would keep serving a stale build.
///
/// Watched paths are mutated too: the "can it see it" check is the real
/// question (a watched path with a too-weak stamp is as bad as an unwatched
/// one). Such a job builds its copy cold first, mutates that same copy, and
/// checks the copy's own watch set with the real stamp check
/// ([`stale_paths`]: stamps, then racy hashes) — stamps are per-tree, so
/// only that tests them. Mutations of unwatched paths use the pristine
/// watch set ([`copy_differs`]). A detected mutation needs no further
/// build. Over-watching is fine and not checked.
#[test]
fn watch_set_is_complete() {
    let start = Instant::now();
    let mut failures = Vec::new();
    let mut checked = 0;
    for variant in variants() {
        let mut files = site_files();
        apply_variant(&mut files, &variant);
        let pristine = materialize(&files);
        let (baseline, watch) = cold(pristine.path());
        if (variant.name == "passing") != baseline.is_ok() {
            panic!(
                "{}: unexpected build status: {:?}",
                variant.name,
                baseline.err()
            );
        }

        let jobs: Vec<(PathBuf, Mutation)> = all_paths(pristine.path())
            .into_iter()
            .flat_map(|(rel, is_dir)| {
                mutations_of(&rel, is_dir)
                    .into_iter()
                    .map(move |m| (rel.clone(), m))
            })
            // A same-length rewrite tests a stamp's strength; an unwatched
            // file has none, and `Rewrite` already covers its invisibility.
            .filter(|(rel, m)| {
                *m != Mutation::RewriteSameLength || watch.contains(pristine.path().join(rel))
            })
            .chain(
                CREATIONS
                    .iter()
                    .filter(|rel| !pristine.path().join(rel).exists())
                    .map(|rel| (PathBuf::from(rel), Mutation::Create)),
            )
            .collect();
        checked += jobs.len();
        let results = parallel(&jobs, |(rel, mutation)| {
            let copy = materialize(&files);
            let detected = if watch.contains(pristine.path().join(rel)) {
                let (_, own_watch) = cold(copy.path());
                mutate(copy.path(), rel, *mutation);
                !stale_paths(&own_watch).is_empty()
            } else {
                mutate(copy.path(), rel, *mutation);
                copy_differs(&watch, pristine.path(), copy.path())
            };
            if detected {
                return None;
            }
            let (after, _) = cold(copy.path());
            (after != baseline).then(|| {
                format!(
                    "[{}] {mutation:?} {} is invisible to the watch set but changes the build:\n{}",
                    variant.name,
                    rel.display(),
                    describe_diff(&baseline, &after),
                )
            })
        });
        failures.extend(results.into_iter().flatten());
    }
    eprintln!(
        "watch_set_is_complete: {checked} mutations in {:?}",
        start.elapsed()
    );
    assert!(
        failures.is_empty(),
        "{} gaps:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// 2. Incremental vs cold
// ---------------------------------------------------------------------------

/// xorshift64*: deterministic, dependency-free, good enough to pick edits.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Apply one random edit to the tree at `base`; returns its description.
/// `pristine` is the fixture, for restoring deleted or broken files. One edit
/// kind rebuilds in the middle (`serve`), so `Err` is that rebuild's failure.
fn random_edit(
    rng: &mut Rng,
    base: &Path,
    pristine: &Files,
    step: usize,
    serve: &mut Serve,
) -> Result<String, String> {
    let path = |rel: &str| base.join(rel);
    let write = |rel: &str, contents: &[u8]| {
        let p = path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    };
    let restore = |rel: &str| write(rel, pristine[rel].as_deref().unwrap());
    // Files with a "broken" form; same-length variants of the good form are
    // included on purpose (a same-length rewrite is what stamps miss).
    let variants: &[(&str, &[&str])] = &[
        (
            "site/content/blog/post.typ",
            &[
                "Post v1",
                "Post v2",
                "#import \"/content/_private.typ\": private-note\n#private-note again",
                "#context link(asset.sass(\"/styles/main.scss\").url())[css only]",
                "#oops-undefined",
                // An asset only this page uses, at an explicit output path: it
                // must disappear when the page stops requesting it.
                "#context link(asset.file(\"/data/notes.txt\", output: \"/notes.txt\").url())[notes]",
                // The same spec main.typ requests, so a cached build (or cached
                // failure) is shared between two call sites.
                "Post\n#context link(asset.image(\"/images/photo.png\", width: 4).url())[photo]",
                "#context link(asset.svg(\"/images/icon.svg\").url())[icon] #context link(asset.file(\"/files/doc.txt\").url())[doc]",
            ],
        ),
        (
            "site/templates/_parts.typ",
            &[
                "#let footer = [Footer AAA]",
                "#let footer = [Footer BBB]",
                "#let footer = { oops-undefined }",
            ],
        ),
        (
            "site/styles/partials/_colors.scss",
            &["$c: red;", "$c: tan;", "$c: ;", "$c: #00f;"],
        ),
        (
            "site/styles/main.scss",
            &[
                "@use \"partials/colors\";\n.a { color: colors.$c; }",
                "@use \"partials/colors\";\n.b { color: colors.$c; }",
                "@use \"partials/missing\";\n.a { color: red; }",
                ".a { color: blue; }",
            ],
        ),
        (
            "site/data/notes.txt",
            &["notes one", "notes two", "notes three!"],
        ),
        (
            "site/data/raw.css",
            &[".raw { color: green; }", ".raw { color: olive; }"],
        ),
        ("site/files/doc.txt", &["doc AAA", "doc BBB"]),
        (
            "site/images/icon.svg",
            &[SVG, "<svg><open></svg>", "not svg at all"],
        ),
        (
            "site/assets/_fig-dep.typ",
            &[
                "#let caption = [Fig A]",
                "#let caption = [Fig B]",
                "#let caption = { oops-undefined }",
            ],
        ),
        (
            "packages/local/greet/0.1.0/util.typ",
            &[
                "#let who = \"greet\"",
                "#let who = \"GREET\"",
                "#let who = {",
            ],
        ),
        (
            "site/content/blog/2024/old.typ",
            &[
                "Old post",
                "Old POST",
                "#document(output: \"/extra.html\")[clash]",
            ],
        ),
    ];
    // Paths to delete and recreate (from the fixture).
    let deletable = [
        "site/templates/_parts.typ",
        "site/styles/partials/_colors.scss",
        "site/images/photo.png",
        "site/images/pic.png",
        "site/images/icon.svg",
        "site/files/doc.txt",
        "site/data/raw.css",
        "site/assets/_fig-dep.typ",
        "site/content/_private.typ",
        "site/static/robots.txt",
        "packages/local/greet/0.1.0/lib.typ",
        "site/data/unused.txt",
    ];
    // Any path an mtime-only or same-content edit may hit.
    let existing: Vec<&String> = pristine
        .iter()
        .filter(|(rel, contents)| contents.is_some() && path(rel).is_file())
        .map(|(rel, _)| rel)
        .collect();

    Ok(match rng.below(16) {
        12..=15 => {
            // Heal: restore one fixture file that's broken or missing, so
            // the run keeps returning to a passing site instead of
            // accumulating breakage.
            let damaged: Vec<&String> = pristine
                .iter()
                .filter(|(rel, contents)| match contents {
                    Some(bytes) => fs::read(path(rel)).ok().as_ref() != Some(bytes),
                    None => false,
                })
                .map(|(rel, _)| rel)
                .collect();
            if damaged.is_empty() {
                "heal: nothing to heal".to_string()
            } else {
                let rel = *rng.pick(&damaged);
                restore(rel);
                format!("heal {rel}")
            }
        }
        0..=2 => {
            let (rel, options) = rng.pick(variants);
            let contents = rng.pick(options);
            if rng.below(2) == 0 {
                write(rel, contents.as_bytes());
                format!("write {rel} = {contents:?}")
            } else {
                // An editor's atomic save: write a temp file, rename it over.
                let tmp = path(&format!("{rel}.twyla-tmp"));
                fs::create_dir_all(tmp.parent().unwrap()).unwrap();
                fs::write(&tmp, contents).unwrap();
                fs::rename(&tmp, path(rel)).unwrap();
                format!("atomic save {rel} = {contents:?}")
            }
        }
        3 => {
            let rel = *rng.pick(&deletable);
            if path(rel).exists() {
                fs::remove_file(path(rel)).unwrap();
                format!("delete {rel}")
            } else {
                restore(rel);
                format!("recreate {rel}")
            }
        }
        4 => {
            let pages = [
                "site/content/blog/2024/new.typ",
                "site/content/fresh/deep/page.typ",
                "site/content/blog/_hidden.typ",
                "site/styles/partials/_missing.scss",
            ];
            let rel = *rng.pick(&pages);
            if path(rel).exists() {
                fs::remove_file(path(rel)).unwrap();
                format!("remove page {rel}")
            } else {
                write(rel, format!("New page {step}").as_bytes());
                format!("add page {rel}")
            }
        }
        5 => {
            let rel = *rng.pick(&existing);
            let mtime = std::time::SystemTime::now()
                - std::time::Duration::from_secs(rng.below(5000) as u64);
            fs::File::options()
                .write(true)
                .open(path(rel))
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            format!("touch {rel}")
        }
        6 => {
            let rel = *rng.pick(&existing);
            let bytes = fs::read(path(rel)).unwrap();
            fs::write(path(rel), bytes).unwrap();
            format!("same-content rewrite {rel}")
        }
        7 => {
            let statics = [
                "site/static/extra.txt",
                "site/static/css/deep/more.css",
                "site/static/index.html",
            ];
            let rel = *rng.pick(&statics);
            if path(rel).exists() {
                fs::remove_file(path(rel)).unwrap();
                format!("remove static {rel}")
            } else {
                write(rel, format!("static {step}").as_bytes());
                format!("add static {rel}")
            }
        }
        8 => {
            let rgb = [rng.below(256) as u8, rng.below(256) as u8, 7];
            write("site/images/photo.png", &png(8, 8, rgb));
            format!("repaint site/images/photo.png {rgb:?}")
        }
        9 => {
            // Restore a random fixture file (fixes whatever broke it).
            let rel = rng.pick(variants).0;
            restore(rel);
            format!("restore {rel}")
        }
        10 => {
            if rng.below(2) == 0 {
                let rel = "site/images/photo.png";
                write(rel, b"not a png");
                format!("break {rel}")
            } else {
                // Delete a whole directory of dependencies, or put it back.
                let dirs = [
                    "site/styles/partials",
                    "site/assets",
                    "packages/local/greet/0.1.0",
                    "site/content/blog/2024",
                ];
                let rel = *rng.pick(&dirs);
                if path(rel).exists() {
                    fs::remove_dir_all(path(rel)).unwrap();
                    format!("delete dir {rel}")
                } else {
                    for (file, contents) in pristine.range(format!("{rel}/")..) {
                        if !file.starts_with(&format!("{rel}/")) {
                            break;
                        }
                        if let Some(contents) = contents {
                            write(file, contents);
                        }
                    }
                    format!("restore dir {rel}")
                }
            }
        }
        11 => {
            // The racy case: a same-length rewrite right after a rebuild read
            // the file, possibly within one timestamp tick, so its stamp may
            // not change. (On Linux ≥ 6.13 multigrain timestamps make the
            // stamp change anyway, so this only bites on coarser systems.)
            let pairs = [
                ("site/styles/partials/_colors.scss", "$c: red;", "$c: tan;"),
                ("site/data/notes.txt", "notes one", "notes two"),
                ("site/files/doc.txt", "doc AAA", "doc BBB"),
                (
                    "packages/local/greet/0.1.0/util.typ",
                    "#let who = \"greet\"",
                    "#let who = \"GREET\"",
                ),
                (
                    "site/templates/_parts.typ",
                    "#let footer = [Footer AAA]",
                    "#let footer = [Footer BBB]",
                ),
            ];
            let (rel, a, b) = *rng.pick(&pairs);
            write(rel, a.as_bytes());
            serve.rebuild_and_check()?;
            write(rel, b.as_bytes());
            format!("racy: write {rel} = {a:?}, rebuild, write {b:?}")
        }
        _ => unreachable!(),
    })
}

/// A persistent [`DevSession`] driven the way serve drives it, with every
/// rebuild checked against a cold build of the same tree.
struct Serve {
    base: PathBuf,
    session: DevSession,
    /// The last rebuild's output and watch set.
    output: Snapshot,
    watch: WatchSet,
    /// Clear comemo's process-wide memo cache before each cold build, so the
    /// reference can't reuse anything the session memoized (see
    /// [`incremental_matches_cold_evicted`]).
    evict: bool,
}

impl Serve {
    fn new(base: &Path, evict: bool) -> Serve {
        let mut session = DevSession::new(&context(base)).unwrap();
        let first = session.rebuild();
        Serve {
            base: base.to_path_buf(),
            output: snapshot(&first.result, base),
            watch: first.watch,
            session,
            evict,
        }
    }

    /// Edits happened since the last rebuild: (a) if they changed what a
    /// cold build produces, the last watch set must see a change (or serve
    /// would never rebuild); (b) the session's rebuild must equal the cold
    /// build; (c) the new watch set must be quiescent (or serve would
    /// rebuild forever).
    fn rebuild_and_check(&mut self) -> Result<(), String> {
        if self.evict {
            comemo::evict(0);
        }
        let (cold_output, _) = cold(&self.base);
        if stale_paths(&self.watch).is_empty() && cold_output != self.output {
            return Err(format!(
                "the edit changed the build but no watched path looks different:\n{}",
                describe_diff(&self.output, &cold_output)
            ));
        }
        let rebuild = self.session.rebuild();
        self.output = snapshot(&rebuild.result, &self.base);
        self.watch = rebuild.watch;
        if self.output != cold_output {
            return Err(format!(
                "incremental rebuild differs from a cold build (- incremental, + cold):\n{}",
                describe_diff(&self.output, &cold_output)
            ));
        }
        let stale = stale_paths(&self.watch);
        if !stale.is_empty() {
            return Err(format!(
                "watch set is stale right after the rebuild: {stale:?}"
            ));
        }
        Ok(())
    }
}

/// One seeded run of `steps` edits, each followed by a checked rebuild.
/// Returns the failure report, if any.
fn differential_run(seed: u64, steps: usize, evict: bool) -> Option<String> {
    let pristine = site_files();
    let base = materialize(&pristine);
    let mut serve = Serve::new(base.path(), evict);
    let mut rng = Rng(seed);
    let mut history = Vec::new();
    for step in 0..steps {
        let result =
            random_edit(&mut rng, base.path(), &pristine, step, &mut serve).and_then(|edit| {
                history.push(edit);
                serve.rebuild_and_check()
            });
        if let Err(what) = result {
            if history.len() == step {
                history.push("(the next edit, which failed in its mid-edit rebuild)".into());
            }
            return Some(format!(
                "seed {seed:#x}, step {step}: {what}\nedit history:\n  {}",
                history.join("\n  ")
            ));
        }
    }
    None
}

/// Serve's rebuild loop, checked against cold builds: 3 seeds × 40 random
/// edits (rewrites, broken and fixed files, deletes and recreates, new and
/// removed nested pages, `touch`, same-content and same-length rewrites,
/// static files, package edits). After each edit, (a) if the edit changed the
/// cold build's output, the previous watch set must see a change; (b) the
/// persistent session's rebuild must equal a cold build; (c) the new watch
/// set must be quiescent. On failure, prints the seed and edit history.
#[test]
fn incremental_matches_cold() {
    let start = Instant::now();
    let seeds = [0x5eed_0001_u64, 0x0bad_cafe_f00d, 0x1234_5678_9abc_def1];
    let failures: Vec<String> = parallel(&seeds, |&seed| differential_run(seed, 40, false))
        .into_iter()
        .flatten()
        .collect();
    eprintln!("incremental_matches_cold: {:?}", start.elapsed());
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// [`incremental_matches_cold`] with an independent oracle: one seed, run
/// serially, with comemo's memo cache cleared before every cold build. The
/// cold reference otherwise shares the process-wide cache with the session
/// it's checking, so a stale result the session fed comemo from an input
/// comemo doesn't track could be reused by the reference too and compare
/// equal. (Other tests running concurrently can repopulate the cache, but
/// only from their own trees.)
#[test]
fn incremental_matches_cold_evicted() {
    let start = Instant::now();
    let failure = differential_run(0x00c0_1d00_5eed, 12, true);
    eprintln!("incremental_matches_cold_evicted: {:?}", start.elapsed());
    assert!(failure.is_none(), "{}", failure.unwrap_or_default());
}

/// A [`Backend`](crate::watch::Backend) that refuses every path, so the
/// watcher polls everything — the path the targeted cases below exercise.
struct RefuseAll;

impl crate::watch::Backend for RefuseAll {
    fn watch(&mut self, _: &Path) -> notify::Result<()> {
        Err(notify::Error::generic("refused (test)"))
    }
    fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
        Ok(())
    }
}

/// Targeted edits driven through the real [`Watcher`](crate::watch::Watcher)
/// rather than [`stale_paths`]: every subscription refused, so `update`'s
/// re-check and the poll decide alone. For each edit that changes the cold
/// build, the watcher must actually report a change (schedule the rebuild);
/// the rebuild must then match cold, and the watcher go quiet again.
#[test]
fn polling_watcher_schedules_targeted_rebuilds() {
    type Edit = fn(&Path);
    let cases: &[(&str, Edit)] = &[
        ("same-length rewrite right after the read", |base| {
            fs::write(base.join("site/styles/partials/_colors.scss"), "$c: tan;").unwrap();
        }),
        ("same-length rewrite with the mtime preserved", |base| {
            let path = base.join("site/data/notes.txt");
            let mtime = fs::metadata(&path).unwrap().modified().unwrap();
            fs::write(&path, "SOME NOTES").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        }),
        ("ignored file replaced by a same-named section", |base| {
            let path = base.join("site/content/blog/notes.txt");
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            fs::write(path.join("new.typ"), "new page").unwrap();
        }),
        ("asset.file source rewritten", |base| {
            fs::write(base.join("site/files/doc.txt"), "a DOCUMENT").unwrap();
        }),
    ];
    for (name, edit) in cases {
        let base = materialize(&site_files());
        let mut serve = Serve::new(base.path(), false);
        // No events ever arrive; the sender only keeps the channel open.
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut watcher = crate::watch::Watcher::with_backend(RefuseAll, rx);
        watcher.update(&serve.watch).unwrap();
        let quiet = watcher.poll_now();
        assert!(
            quiet.is_empty(),
            "{name}: not quiet before the edit: {quiet:?}"
        );

        edit(base.path());
        let (cold_output, _) = cold(base.path());
        assert_ne!(
            cold_output, serve.output,
            "{name}: edit doesn't change the build"
        );
        let changed = watcher.poll_now();
        assert!(
            !changed.is_empty(),
            "{name}: the watcher scheduled no rebuild"
        );

        serve
            .rebuild_and_check()
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        watcher.update(&serve.watch).unwrap();
        let quiet = watcher.poll_now();
        assert!(
            quiet.is_empty(),
            "{name}: not quiet after the rebuild: {quiet:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Determinism
// ---------------------------------------------------------------------------

/// Two cold builds of the same tree (in two different directories) agree,
/// for the passing fixture and every failing variant. Within one process;
/// see [`inline_document_names_are_stable_across_processes`] for across.
#[test]
fn cold_builds_are_deterministic() {
    let variants = variants();
    let failures: Vec<String> = parallel(&variants, |variant| {
        let mut files = site_files();
        apply_variant(&mut files, variant);
        let (a, b) = (materialize(&files), materialize(&files));
        let (first, _) = cold(a.path());
        let (second, _) = cold(b.path());
        (first != second).then(|| format!("[{}]\n{}", variant.name, describe_diff(&first, &second)))
    })
    .into_iter()
    .flatten()
    .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

const CHILD_ENV: &str = "TWYLA_INVARIANTS_CHILD_SITE";

/// Child half of [`inline_document_names_are_stable_across_processes`]: build
/// the site at `$TWYLA_INVARIANTS_CHILD_SITE` and print its output paths. A
/// no-op when run directly.
#[test]
#[ignore = "helper, run by inline_document_names_are_stable_across_processes"]
fn print_output_paths_for_parent() {
    let Some(base) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let (snapshot, _) = cold(Path::new(&base));
    for key in snapshot.unwrap().keys() {
        println!("twyla-output-path {key}");
    }
}

/// The same site built by two processes gets the same output paths.
///
/// Fails today: an inline `document(output: auto)` is named by
/// [`ResolvedDocument::sha256_hex`](crate::document::ResolvedDocument::sha256_hex),
/// which feeds the body `Content` through `std::hash::Hash`. typst's `Content`
/// hash is only meant to be stable within one process: it hashes each
/// element's vtable *address* (`Element` → `typst_utils::Static`, which
/// hashes the pointer — different on every run under ASLR), plus spans
/// (process-interned `FileId` numbers) and labels (interned `PicoStr`s).
#[test]
#[ignore = "known bug: inline document names hash process-local data (element vtable addresses, spans); see the doc comment"]
fn inline_document_names_are_stable_across_processes() {
    let base = materialize(&site_files());
    let run = || {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "invariants::print_output_paths_for_parent",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD_ENV, base.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix("twyla-output-path ").map(str::to_string))
            .collect();
        assert!(!paths.is_empty(), "child printed nothing");
        paths
    };
    assert_eq!(run(), run());
}

/// [`incremental_matches_cold`] at length: 64 seeds × 80 steps. Opt-in
/// (`cargo test -- --ignored`); run it after touching the ledger, the
/// resolver's cache, or the watch set.
#[test]
#[ignore = "slow (about 30s); the soak version of incremental_matches_cold"]
fn incremental_matches_cold_soak() {
    let seeds: Vec<u64> = (1..=64)
        .map(|i| 0x9e37_79b9_7f4a_7c15_u64.wrapping_mul(i))
        .collect();
    let failures: Vec<String> = parallel(&seeds, |&seed| differential_run(seed, 80, false))
        .into_iter()
        .flatten()
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} seeds failed:\n\n{}",
        failures.len(),
        seeds.len(),
        failures.join("\n\n")
    );
}
