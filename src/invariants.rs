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
//!   apply each mutation (rewrite, delete, add a file to a directory) to a
//!   fresh copy. A mutation the watch set can't see must leave a cold build's
//!   output unchanged.
//! - **Incremental = cold** ([`incremental_matches_cold`]): drive one
//!   persistent [`DevSession`] through a seeded pseudo-random edit sequence,
//!   as serve would, and after every step compare its output with a fresh
//!   cold build of the same tree.
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
//! can only hide a bug where twyla feeds typst data comemo doesn't track; a
//! run with `comemo::evict(0)` before each cold build and no parallelism
//! passed too (it's ~3× slower, so it isn't the default).
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
             #context link(asset.sass(\"/styles/main.scss\").url())[css]",
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
/// Path-backed outputs (static files, `asset.file` copies) are compared by
/// their source path, not bytes: the rebuild's output is the reference, and
/// serve reads the bytes live (an `asset.file`'s fingerprint is in its output
/// path, so its contents are still covered).
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
                        Emit::Copy(src) => format!("copy {}", rel(src)),
                    },
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
        (false, _) => vec![Mutation::Rewrite, Mutation::Delete],
    };
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

/// Whether `watch`, taken from a build of the pristine tree at `pristine`,
/// would notice the difference between it and the (mutated) copy at `copy`.
///
/// Stamps are per-tree (inodes, times), so they can't be compared across
/// copies directly. Every stamp kind except [`Stamp::File`] is a function of
/// names and file types only, so it's checked with the real
/// [`Stamp::matches_now`] against the copy. A `File` stamp changes on any
/// write; every [`Mutation`] changes the bytes (or removes the file), so the
/// copy-independent equivalent is "same bytes and permissions".
fn copy_differs(watch: &WatchSet, pristine: &Path, copy: &Path) -> Vec<PathBuf> {
    let mut stale = Vec::new();
    for (path, stamp) in &watch.paths {
        let Ok(rel) = path.strip_prefix(pristine) else {
            panic!("watched path outside the fixture: {}", path.display());
        };
        let there = copy.join(rel);
        let differs = match stamp {
            Stamp::Unknown => true,
            Stamp::File { .. } => {
                let perm = |p: &Path| fs::metadata(p).map(|m| m.permissions()).ok();
                !there.is_file()
                    || fs::read(path).ok() != fs::read(&there).ok()
                    || perm(path) != perm(&there)
            }
            stamp => !stamp.matches_now(&there),
        };
        if differs {
            stale.push(rel.to_path_buf());
        }
    }
    stale
}

/// The key test. For the passing fixture and every failing variant: build it
/// cold, then for every file and directory (watched or not), apply every
/// mutation to a fresh copy. If the watch set can't see the mutation, a cold
/// build of the copy must produce exactly the same output (or error) as the
/// pristine tree — otherwise serve would keep serving a stale build.
///
/// Watched paths are mutated too: the "can it see it" check is the real
/// question (a watched path with a too-weak stamp is as bad as an unwatched
/// one), and it's cheap — a detected mutation needs no second build.
/// Over-watching is fine and not checked.
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
            mutate(copy.path(), rel, *mutation);
            if !copy_differs(&watch, pristine.path(), copy.path()).is_empty() {
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
}

impl Serve {
    fn new(base: &Path) -> Serve {
        let mut session = DevSession::new(&context(base)).unwrap();
        let first = session.rebuild();
        Serve {
            base: base.to_path_buf(),
            output: snapshot(&first.result, base),
            watch: first.watch,
            session,
        }
    }

    /// Edits happened since the last rebuild: (a) if they changed what a
    /// cold build produces, the last watch set must see a change (or serve
    /// would never rebuild); (b) the session's rebuild must equal the cold
    /// build; (c) the new watch set must be quiescent (or serve would
    /// rebuild forever).
    fn rebuild_and_check(&mut self) -> Result<(), String> {
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
fn differential_run(seed: u64, steps: usize) -> Option<String> {
    let pristine = site_files();
    let base = materialize(&pristine);
    let mut serve = Serve::new(base.path());
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
    let failures: Vec<String> = parallel(&seeds, |&seed| differential_run(seed, 40))
        .into_iter()
        .flatten()
        .collect();
    eprintln!("incremental_matches_cold: {:?}", start.elapsed());
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
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
    let failures: Vec<String> = parallel(&seeds, |&seed| differential_run(seed, 80))
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
