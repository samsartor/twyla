//! The dev server's persistent rebuild step.
//!
//! [`DevSession`] owns everything that survives between `twyla serve`
//! recompiles — the [`RenderWorld`] (FileStore + comemo-cached typst state)
//! and the asset/document [`Resolver`] — and exposes exactly one operation,
//! [`rebuild`](DevSession::rebuild): invalidate, compile, and report both the
//! result and the [`WatchSet`] of paths whose change should trigger the next
//! rebuild, each with what this compile observed there.
//!
//! `serve`'s watcher thread is just `watcher.update(watch) → wait → rebuild`
//! in a loop; tests drive the same `rebuild` by hand (edit a file, call it
//! again), so what serve watches and how it recompiles are both testable
//! without a file watcher or a socket.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::ledger::Dep;
use crate::project::{ContentScan, TwylaContext};
use crate::render::{Outputs, RenderError, RenderErrorKind, RenderWorld, plain_err};
use crate::resolver::Resolver;
pub use crate::watch::{Stamp, WatchSet};

/// Persistent compile state for `twyla serve`. See the module docs.
pub struct DevSession {
    pub world: RenderWorld,
    pub resolver: Resolver,
    /// The previous rebuild's watch set, unioned into the next one when that
    /// rebuild fails (see [`rebuild`](Self::rebuild)).
    last_watch: WatchSet,
    /// Test hook: panic inside the next compile, after it has run, to
    /// exercise panic recovery.
    #[cfg(test)]
    inject_panic: bool,
}

/// The outcome of one [`DevSession::rebuild`].
pub struct Rebuild {
    pub result: Result<Outputs, RenderError>,
    /// What to watch until the next rebuild. Computed whether or not the
    /// compile succeeded, so fixing an error is itself noticed.
    ///
    /// The union of four sources, each explicit in [`DevSession::rebuild`]:
    /// - every path the [`ContentScan`] observed (each directory under
    ///   `content/` and `static/` by its listing, the roots even when missing),
    ///   so creating or deleting a page or static file anywhere is noticed;
    /// - every file typst loaded ([`RenderWorld::take_loads`]), stamped at
    ///   load time — failed loads too, so a missing import is polled;
    /// - every ledger dep of the asset pass ([`Resolver::asset_deps`]):
    ///   reads, existence probes (a missing sass import candidate is polled
    ///   as missing) and directory listings, of built *and failed* assets,
    ///   stamped at access time;
    /// - after a *failed* rebuild, everything the previous rebuild watched, so
    ///   a compile that stopped early (scan error, typst error, panic) never
    ///   shrinks the watch set. Those retained paths are stamped as they are
    ///   now, not as the earlier compile saw them (see [`WatchSet::keep_from`]
    ///   for the rebuild loop that would cause).
    pub watch: WatchSet,
}

impl DevSession {
    pub fn new(ctx: &TwylaContext) -> Result<Self, RenderError> {
        Ok(Self {
            world: RenderWorld::new(ctx)?,
            resolver: Resolver::new(ctx),
            last_watch: WatchSet::default(),
            #[cfg(test)]
            inject_panic: false,
        })
    }

    /// Recompile the site against the current disk state. The first call is
    /// the initial (cold) compile; later calls reuse the warm caches.
    ///
    /// Mirrors one iteration of typst's own `typst watch` loop
    /// (`typst-cli/src/watch.rs`): age comemo, mark FileStore slots stale (both
    /// no-ops before the first compile), compile, then collect what to watch.
    /// `content/` and `static/` are rescanned every time, once, and that one
    /// scan feeds both the compile and the watch set.
    pub fn rebuild(&mut self) -> Rebuild {
        comemo::evict(10);
        self.world.files.reset();
        // Start this compile's load log empty (a one-shot `compile_bundle`
        // outside `rebuild` may have left loads behind).
        self.world.take_loads();
        let scan = self.world.ctx.scan_content();

        // A panic anywhere in the compile (typst, an asset pipeline, or an
        // invariant `panic!` in `compile_bundle`) would otherwise unwind
        // through serve's watcher thread and end hot reload for the session.
        // Catch it and report it like any other render error.
        #[cfg(test)]
        let inject_panic = std::mem::take(&mut self.inject_panic);
        let world = &self.world;
        let resolver = &mut self.resolver;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let result = world.compile_bundle_with(&scan, resolver);
            #[cfg(test)]
            if inject_panic {
                panic!("injected test panic");
            }
            result
        }));

        // Collect what this attempt observed *before* any recovery below
        // discards the world and resolver that hold those observations.
        let mut watch = self.observed(&scan);

        let result = match outcome {
            Ok(result) => result,
            Err(payload) => {
                self.reset_after_panic();
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "(non-string panic payload)".to_string());
                Err(plain_err(
                    format!("twyla panicked during compile: {msg}"),
                    RenderErrorKind::Panic,
                ))
            }
        };

        if result.is_err() {
            watch.keep_from(&self.last_watch);
            watch.failed = true;
        }
        self.last_watch = watch.clone();
        Rebuild { result, watch }
    }

    /// What the compile that just ran observed. See [`Rebuild::watch`].
    /// Every stamp here was taken when the compile touched the path, never
    /// after the fact.
    fn observed(&mut self, scan: &ContentScan) -> WatchSet {
        let mut watch = WatchSet::default();
        for (path, stamp) in &scan.observed {
            watch.insert(path.clone(), stamp.clone());
        }
        for dep in self.world.take_loads() {
            observe(&mut watch, &dep);
        }
        // Asset deps are read by twyla's own asset pass, not typst. Pulled
        // from the resolver's cache, not the published outputs, so that an
        // asset that's only `.read()` (inlined, never emitted as a file) and
        // one whose build failed are still watched — plus whatever a build
        // that panicked touched before it did.
        for dep in self.resolver.asset_deps() {
            observe(&mut watch, dep);
        }
        for dep in &self.resolver.interrupted_build_deps() {
            observe(&mut watch, dep);
        }
        watch
    }

    /// After a panic, nothing the compile touched can be trusted: the
    /// FileStore may hold half-updated slots (and `reset` would keep its
    /// parsed sources for reuse), the resolver may be mid-update, and comemo
    /// may have cached results computed from either. Start the next attempt
    /// from a clean slate — a fresh world (re-enumerating fonts is slow, but
    /// this is the rare path), a fresh resolver, and an empty comemo cache.
    fn reset_after_panic(&mut self) {
        comemo::evict(0);
        let ctx = self.world.ctx.clone();
        match RenderWorld::new(&ctx) {
            Ok(world) => self.world = world,
            // Can't build a new one: keep the old world (its FileStore is
            // reset at the start of every rebuild anyway).
            Err(e) => eprintln!("cannot rebuild compile state after panic: {e}"),
        }
        self.resolver = Resolver::new(&ctx);
    }
}

/// Add one ledger observation to the watch set: its access-time stamp, plus
/// its content hash when the stamp alone can't vouch for it (racy).
fn observe(watch: &mut WatchSet, dep: &Dep) {
    let racy_hash = if dep.is_racy() { dep.sha256 } else { None };
    watch.observe(dep.path.clone(), dep.stamp.clone(), racy_hash);
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use super::*;

    /// A temp-dir site with `content/` and the given files (parent dirs are
    /// created as needed, so nested paths work).
    fn site(files: &[(&str, &str)]) -> (TwylaContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("content")).unwrap();
        for (rel, contents) in files {
            write(dir.path(), rel, contents);
        }
        let ctx = TwylaContext::new(dir.path(), Some("https://example.com".into())).unwrap();
        (ctx, dir)
    }

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn doc_paths(outputs: &Outputs) -> Vec<String> {
        let mut v: Vec<String> = outputs.docs().map(|d| d.output_path.clone()).collect();
        v.sort();
        v
    }

    /// Pages are discovered by recursing `content/`, so every directory in
    /// that walk must be watched — not just `content/` — or a page added to an
    /// existing subdirectory never triggers a rebuild.
    #[test]
    fn watch_set_includes_nested_content_dirs_and_picks_up_new_pages() {
        let (ctx, dir) = site(&[
            ("content/main.typ", "home"),
            ("content/posts/first.typ", "first"),
            ("content/posts/2024/old.typ", "old"),
        ]);
        let content = ctx.content_dir();
        let mut session = DevSession::new(&ctx).unwrap();

        let r1 = session.rebuild();
        let out1 = r1.result.unwrap();
        assert_eq!(
            doc_paths(&out1),
            [
                "index.html",
                "posts/2024/old/index.html",
                "posts/first/index.html"
            ],
        );
        for d in [
            content.clone(),
            content.join("posts"),
            content.join("posts/2024"),
        ] {
            assert!(
                r1.watch.contains(&d),
                "{} not watched: {:?}",
                d.display(),
                r1.watch
            );
        }
        assert!(r1.watch.contains(content.join("posts/first.typ")));

        write(dir.path(), "content/posts/second.typ", "second");
        let r2 = session.rebuild();
        assert!(
            doc_paths(&r2.result.unwrap()).contains(&"posts/second/index.html".to_string()),
            "new nested page not picked up",
        );
        assert!(r2.watch.contains(content.join("posts/second.typ")));
    }

    /// A missing `content/` is still watched (polled), so creating it and its
    /// first page is noticed.
    #[test]
    fn missing_content_dir_is_still_watched() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        let mut session = DevSession::new(&ctx).unwrap();
        let r = session.rebuild();
        assert!(r.result.is_err());
        assert!(r.watch.contains(ctx.content_dir()));
    }

    /// After a typst error, the erroring file stays watched (so saving a fix is
    /// noticed), and the next rebuild of the same session succeeds.
    #[test]
    fn typ_compile_error_keeps_file_watched_and_recovers() {
        let (ctx, dir) = site(&[
            ("content/main.typ", "#import \"_lib.typ\": greet\n#greet()"),
            ("content/_lib.typ", "#let greet() = { oops-undefined }"),
        ]);
        let content = ctx.content_dir();
        let mut session = DevSession::new(&ctx).unwrap();

        let r1 = session.rebuild();
        assert!(r1.result.is_err(), "expected a compile error");
        assert!(
            r1.watch.contains(content.join("main.typ")),
            "{:?}",
            r1.watch
        );
        assert!(
            r1.watch.contains(content.join("_lib.typ")),
            "{:?}",
            r1.watch
        );

        std::thread::sleep(Duration::from_millis(10));
        write(
            dir.path(),
            "content/_lib.typ",
            "#let greet() = [hello-fixed]",
        );
        let r2 = session.rebuild();
        let html = r2.result.unwrap().docs().next().unwrap().html.clone();
        assert!(html.contains("hello-fixed"), "{html}");
    }

    /// Regression (fixed by the dependency ledger): an asset whose build
    /// failed used to drop the files it read (they were only recorded in the
    /// successful build's output), so they fell out of the watch set, and
    /// fixing the broken stylesheet didn't trigger a rebuild. Now the
    /// resolver owns the recorder and caches the failure with its deps.
    #[test]
    fn sass_error_keeps_source_watched() {
        let (ctx, _dir) = site(&[
            (
                "content/main.typ",
                "#context asset.sass(\"main.scss\").url()",
            ),
            ("content/main.scss", ".a { color: "),
        ]);
        let mut session = DevSession::new(&ctx).unwrap();
        let r = session.rebuild();
        assert!(r.result.is_err(), "expected a sass error");
        assert!(
            r.watch.contains(ctx.content_dir().join("main.scss")),
            "broken main.scss not watched: {:?}",
            r.watch,
        );
    }

    /// Same regression, via a broken `@use`d partial: grass reads it through
    /// the recording fs, so the failed entry's deps include it.
    #[test]
    fn sass_partial_error_keeps_partial_watched() {
        let (ctx, _dir) = site(&[
            (
                "content/main.typ",
                "#context asset.sass(\"main.scss\").url()",
            ),
            ("content/main.scss", "@use \"partial\";\n.a { color: red; }"),
            ("content/_partial.scss", ".b { color: "),
        ]);
        let mut session = DevSession::new(&ctx).unwrap();
        let r = session.rebuild();
        assert!(r.result.is_err(), "expected a sass error");
        assert!(
            r.watch.contains(ctx.content_dir().join("_partial.scss")),
            "broken partial not watched: {:?}",
            r.watch,
        );
    }

    /// Static files take part in the bundle's conflict check, so a static
    /// change must rebuild: removing a static file that collides with a page
    /// clears the error. `static/` itself is watched by its listing.
    #[test]
    fn removing_conflicting_static_file_clears_bundle_error() {
        let (ctx, dir) = site(&[
            ("content/main.typ", "home"),
            ("static/index.html", "<p>static</p>"),
            ("static/site.css", "body {}"),
        ]);
        let mut session = DevSession::new(&ctx).unwrap();

        let r1 = session.rebuild();
        let err = r1.result.err().expect("expected an output conflict");
        assert!(err.to_string().contains("conflicts"), "{err}");
        assert!(matches!(
            r1.watch.paths.get(&ctx.static_dir()),
            Some(Stamp::Dir { .. })
        ));

        std::fs::remove_file(dir.path().join("static/index.html")).unwrap();
        let r2 = session.rebuild();
        let out = r2.result.unwrap();
        assert!(out.get("site.css").is_some());
        assert!(out.get("index.html").is_some());
    }

    /// A missing `static/` is watched (polled) as missing, so creating it is
    /// noticed.
    #[test]
    fn missing_static_dir_is_watched_as_missing() {
        let (ctx, _dir) = site(&[("content/main.typ", "home")]);
        let mut session = DevSession::new(&ctx).unwrap();
        let r = session.rebuild();
        assert!(r.result.is_ok());
        assert_eq!(r.watch.paths.get(&ctx.static_dir()), Some(&Stamp::Missing));
    }

    /// A scan error (here a dangling symlink in a nested dir) still reports
    /// every directory the scan visited, plus the broken entry itself, so
    /// removing the symlink or creating its target is noticed.
    #[cfg(unix)]
    #[test]
    fn scan_error_keeps_partial_dirs_watched() {
        let (ctx, dir) = site(&[
            ("content/main.typ", "home"),
            ("content/posts/2024/old.typ", "old"),
            ("content/zzz/late.typ", "late"),
        ]);
        let content = ctx.content_dir();
        let link = content.join("posts/2024/broken.typ");
        std::os::unix::fs::symlink(dir.path().join("nowhere.typ"), &link).unwrap();
        let mut session = DevSession::new(&ctx).unwrap();

        let r = session.rebuild();
        assert!(r.result.is_err(), "expected a scan error");
        for d in [
            content.clone(),
            content.join("posts"),
            content.join("posts/2024"),
            content.join("zzz"),
        ] {
            assert!(r.watch.contains(&d), "{} not watched", d.display());
        }
        assert_eq!(r.watch.paths.get(&link), Some(&Stamp::Missing));

        std::fs::remove_file(&link).unwrap();
        assert!(session.rebuild().result.is_ok());
    }

    /// A failed rebuild never shrinks the watch set: whatever the previous
    /// rebuild watched (here, an imported file the failing compile never got
    /// to) stays watched.
    #[test]
    fn failed_rebuild_keeps_previous_watch_set() {
        let (ctx, dir) = site(&[
            ("content/main.typ", "#import \"_lib.typ\": x\n#x"),
            ("content/_lib.typ", "#let x = [lib]"),
        ]);
        let lib = ctx.content_dir().join("_lib.typ");
        let mut session = DevSession::new(&ctx).unwrap();
        assert!(session.rebuild().watch.contains(&lib));

        // Break main.typ before the import is reached.
        std::thread::sleep(Duration::from_millis(10));
        write(
            dir.path(),
            "content/main.typ",
            "#oops-undefined\n#import \"_lib.typ\": x",
        );
        let r = session.rebuild();
        assert!(r.result.is_err());
        assert!(r.watch.contains(&lib), "{:?}", r.watch);
    }

    /// A panic mid-compile: the error is reported as a panic, what the failed
    /// attempt observed (and what the previous rebuild watched) stays watched,
    /// and the next rebuild starts from fresh state and succeeds.
    #[test]
    fn panic_keeps_observations_and_recovers() {
        let (ctx, dir) = site(&[
            ("content/main.typ", "#import \"_lib.typ\": x\n#x"),
            ("content/_lib.typ", "#let x = [v1]"),
            ("content/style.scss", ".a { color: red; }"),
            (
                "content/other.typ",
                "#context asset.sass(\"style.scss\").url()",
            ),
        ]);
        let content = ctx.content_dir();
        let mut session = DevSession::new(&ctx).unwrap();
        let r1 = session.rebuild();
        r1.result.unwrap();
        assert!(r1.watch.contains(content.join("style.scss")));

        session.inject_panic = true;
        let r2 = session.rebuild();
        let err = r2.result.err().expect("expected the injected panic");
        assert!(matches!(err.kind, RenderErrorKind::Panic), "{err}");
        for p in ["main.typ", "_lib.typ", "style.scss"] {
            assert!(
                r2.watch.contains(content.join(p)),
                "{p} dropped: {:?}",
                r2.watch
            );
        }

        std::thread::sleep(Duration::from_millis(10));
        write(dir.path(), "content/_lib.typ", "#let x = [v2]");
        let r3 = session.rebuild();
        let out = r3.result.unwrap();
        let html = &out.get("index.html").and_then(|o| match o {
            crate::render::Output::Doc(d) => Some(d.html.clone()),
            _ => None,
        });
        assert!(html.as_deref().unwrap_or("").contains("v2"), "{html:?}");
    }

    // ---- dependency ledger ------------------------------------------------

    fn write_bytes(root: &Path, rel: &str, contents: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn set_mtime(path: &Path, mtime: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// An mtime well outside the ledger's racy window, so a stamp alone
    /// vouches for the file (tests asserting "no content reads" need this:
    /// a file written moments ago is re-hashed on every check).
    fn age(path: &Path) {
        set_mtime(path, SystemTime::now() - Duration::from_secs(3600));
    }

    /// Wait until files changed just now are clear of the (fine) racy
    /// window: setting an mtime moves the ctime, which can't be set back, so
    /// tests asserting "no content reads" wait it out instead.
    fn settle_ctime() {
        std::thread::sleep(crate::ledger::FINE_RACY_WINDOW + Duration::from_millis(10));
    }

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

    /// One asset failure mode: the site starts broken at `offending`, which
    /// must be watched as `expect` says; applying `fix` must make the next
    /// rebuild succeed.
    struct Failure {
        name: &'static str,
        files: Vec<(&'static str, Vec<u8>)>,
        offending: &'static str,
        expect: fn(&Stamp) -> bool,
        fix: Vec<(&'static str, Vec<u8>)>,
    }

    fn is_file(stamp: &Stamp) -> bool {
        matches!(stamp, Stamp::File { .. })
    }

    fn is_missing(stamp: &Stamp) -> bool {
        *stamp == Stamp::Missing
    }

    /// The core bug the ledger fixes: for every asset kind and failure mode,
    /// the file that broke the build is watched (with the stamp the build
    /// saw), and fixing *only that file* makes the next rebuild succeed.
    #[test]
    fn asset_failures_keep_offending_path_watched_and_recover() {
        let page = |src: &str| src.as_bytes().to_vec();
        let cases = vec![
            Failure {
                name: "sass syntax error in entry",
                files: vec![
                    (
                        "content/main.typ",
                        page("#context asset.sass(\"main.scss\").url()"),
                    ),
                    ("content/main.scss", page(".a { color: ")),
                ],
                offending: "content/main.scss",
                expect: is_file,
                fix: vec![("content/main.scss", page(".a { color: red; }"))],
            },
            Failure {
                name: "sass error in a partial",
                files: vec![
                    (
                        "content/main.typ",
                        page("#context asset.sass(\"main.scss\").url()"),
                    ),
                    (
                        "content/main.scss",
                        page("@use \"partial\";\n.a { color: red; }"),
                    ),
                    ("content/_partial.scss", page(".b { color: ")),
                ],
                offending: "content/_partial.scss",
                expect: is_file,
                fix: vec![("content/_partial.scss", page(".b { color: blue; }"))],
            },
            Failure {
                name: "unresolved sass @use",
                files: vec![
                    (
                        "content/main.typ",
                        page("#context asset.sass(\"main.scss\").url()"),
                    ),
                    (
                        "content/main.scss",
                        page("@use \"missing\";\n.a { color: red; }"),
                    ),
                ],
                offending: "content/_missing.scss",
                expect: is_missing,
                fix: vec![("content/_missing.scss", page(".m { color: green; }"))],
            },
            Failure {
                name: "unresolved sass @import in a subdir",
                files: vec![
                    (
                        "content/main.typ",
                        page("#context asset.sass(\"css/main.scss\").url()"),
                    ),
                    (
                        "content/css/main.scss",
                        page("@import \"lib/vars\";\n.a { color: $c; }"),
                    ),
                ],
                offending: "content/css/lib/_vars.scss",
                expect: is_missing,
                fix: vec![("content/css/lib/_vars.scss", page("$c: red;"))],
            },
            Failure {
                name: "missing asset.file source",
                files: vec![(
                    "content/main.typ",
                    page("#context asset.file(\"data.txt\").url()"),
                )],
                offending: "content/data.txt",
                expect: is_missing,
                fix: vec![("content/data.txt", page("data"))],
            },
            Failure {
                name: "image decode failure",
                files: vec![
                    (
                        "content/main.typ",
                        page("#context asset.image(\"photo.png\", width: 4).url()"),
                    ),
                    ("content/photo.png", page("not an image")),
                ],
                offending: "content/photo.png",
                expect: is_file,
                fix: vec![("content/photo.png", png(8, 8))],
            },
            Failure {
                name: "missing svg",
                files: vec![(
                    "content/main.typ",
                    page("#context asset.svg(\"icon.svg\").url()"),
                )],
                offending: "content/icon.svg",
                expect: is_missing,
                fix: vec![(
                    "content/icon.svg",
                    page(
                        "<svg xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"1\" height=\"1\"/></svg>",
                    ),
                )],
            },
            Failure {
                name: "typst asset with an error in an imported file",
                files: vec![
                    (
                        "content/main.typ",
                        page("#context asset.typst(\"/assets/doc.typ\", format: \"svg\").url()"),
                    ),
                    ("assets/doc.typ", page("#import \"_dep.typ\": x\n#x")),
                    ("assets/_dep.typ", page("#let x = { oops-undefined }")),
                ],
                offending: "assets/_dep.typ",
                expect: is_file,
                fix: vec![("assets/_dep.typ", page("#let x = [fixed]"))],
            },
        ];

        for case in cases {
            let (ctx, dir) = site(&[]);
            for (rel, contents) in &case.files {
                write_bytes(dir.path(), rel, contents);
            }
            let offending = ctx.root.join(case.offending);
            let mut session = DevSession::new(&ctx).unwrap();

            let r1 = session.rebuild();
            assert!(r1.result.is_err(), "{}: expected a build error", case.name);
            assert!(r1.watch.failed, "{}", case.name);
            let stamp = r1.watch.paths.get(&offending).unwrap_or_else(|| {
                panic!(
                    "{}: {} not watched: {:#?}",
                    case.name,
                    offending.display(),
                    r1.watch.paths.keys()
                )
            });
            assert!(
                (case.expect)(stamp),
                "{}: unexpected stamp {stamp:?}",
                case.name
            );

            // A second failing rebuild re-reports the cached failure without
            // rebuilding, and still watches the path.
            let builds = session.resolver.stats.builds;
            let r2 = session.rebuild();
            assert!(r2.result.is_err(), "{}", case.name);
            assert!(r2.watch.contains(&offending), "{}", case.name);
            assert_eq!(
                session.resolver.stats.builds, builds,
                "{}: rebuilt a cached failure",
                case.name
            );

            std::thread::sleep(Duration::from_millis(10));
            for (rel, contents) in &case.fix {
                write_bytes(dir.path(), rel, contents);
            }
            // The watcher's recheck would see the fix...
            assert!(
                !r2.watch.paths[&offending].matches_now(&offending),
                "{}: fix not visible to the watcher",
                case.name
            );
            // ...and the rebuild it triggers succeeds.
            let r3 = session.rebuild();
            if let Err(err) = &r3.result {
                panic!("{}: still failing after the fix:\n{err}", case.name);
            }
            assert!(!r3.watch.failed);
        }
    }

    /// The I/O budget of a warm rebuild: editing a page that uses no assets
    /// reads no asset source (stats only), and `touch`ing an image re-hashes
    /// it once without re-running its pipeline.
    #[test]
    fn warm_rebuild_reads_no_asset_sources() {
        let (ctx, dir) = site(&[
            (
                "content/main.typ",
                "#context asset.image(\"photo.png\", width: 4).url()\n\
                 #context asset.sass(\"main.scss\").url()\n\
                 #context asset.file(\"data.txt\").url()",
            ),
            ("content/main.scss", "@use \"partial\";\n.a { color: red; }"),
            ("content/_partial.scss", ".b { color: blue; }"),
            ("content/data.txt", "data"),
            ("content/other.typ", "other v1"),
        ]);
        write_bytes(dir.path(), "content/photo.png", &png(8, 8));
        for f in ["photo.png", "main.scss", "_partial.scss", "data.txt"] {
            age(&ctx.content_dir().join(f));
        }
        settle_ctime();
        let mut session = DevSession::new(&ctx).unwrap();
        let out1 = session.rebuild().result.unwrap();
        let cold = session.resolver.stats;
        assert_eq!(cold.builds, 3);

        write(dir.path(), "content/other.typ", "other v2");
        session.rebuild().result.unwrap();
        assert_eq!(
            session.resolver.stats, cold,
            "warm rebuild touched asset sources"
        );

        // `touch`: same bytes, new mtime. One re-hash, no rebuild, same output.
        let photo = ctx.content_dir().join("photo.png");
        set_mtime(&photo, SystemTime::now() - Duration::from_secs(1800));
        settle_ctime();
        let out2 = session.rebuild().result.unwrap();
        let stats = session.resolver.stats;
        assert_eq!(stats.builds, cold.builds, "touch rebuilt the image");
        assert_eq!(stats.content_reads, cold.content_reads + 1);
        let images = |out: &Outputs| -> Vec<String> {
            let mut v: Vec<_> = out
                .assets()
                .filter(|a| a.output_path.ends_with(".png"))
                .map(|a| a.output_path.clone())
                .collect();
            v.sort();
            v
        };
        assert_eq!(images(&out1), images(&out2));

        // Restamped: the next rebuild doesn't re-hash it again.
        session.rebuild().result.unwrap();
        assert_eq!(session.resolver.stats, stats);
    }

    /// A same-length in-place rewrite that restores the mtime the build saw
    /// is still a change: on unix the stamp includes the ctime, which nothing
    /// can set back. (Elsewhere this is only caught while the stamp is racy.)
    #[cfg(unix)]
    #[test]
    fn same_length_rewrite_with_preserved_mtime_rebuilds() {
        let (ctx, _dir) = site(&[
            (
                "content/main.typ",
                "#context asset.sass(\"main.scss\").read()",
            ),
            ("content/main.scss", ".a { color: red; }"),
        ]);
        let scss = ctx.content_dir().join("main.scss");
        let mut session = DevSession::new(&ctx).unwrap();
        let r1 = session.rebuild();
        let html = |out: &Outputs| out.docs().next().unwrap().html.clone();
        assert!(html(&r1.result.unwrap()).contains("red"));
        let Some(Stamp::File { mtime, .. }) = r1.watch.paths.get(&scss).cloned() else {
            panic!("{:?}", r1.watch)
        };

        std::fs::write(&scss, ".a { color: tan; }").unwrap();
        set_mtime(&scss, mtime);
        assert!(
            !r1.watch.paths[&scss].matches_now(&scss),
            "stamp missed the rewrite"
        );
        let r2 = session.rebuild();
        assert!(html(&r2.result.unwrap()).contains("tan"));
    }

    /// Asset warnings belong to the cached entry: a compile that reuses the
    /// asset reports them again (they used to vanish after the first).
    #[test]
    fn cached_asset_warnings_are_re_emitted() {
        use typst::syntax::{FileId, RootedPath, VirtualPath, VirtualRoot};
        let (ctx, _dir) = site(&[
            ("content/main.typ", "#context asset.svg(\"bad.svg\").url()"),
            ("content/bad.svg", "<svg><open></svg>"),
        ]);
        let world = RenderWorld::new(&ctx).unwrap();
        let main = FileId::new(RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new("content/main.typ").unwrap(),
        ));
        let mut resolver = Resolver::new(&ctx);
        for round in 0..2 {
            let warned = crate::compile::compile_bundle(&ctx, &world, &[main], &mut resolver);
            assert!(warned.output.is_ok(), "round {round}");
            assert!(
                warned.warnings.iter().any(|w| w.message.contains("svg")),
                "round {round}: warning missing: {:?}",
                warned.warnings
            );
        }
        assert_eq!(resolver.stats.builds, 1);
    }

    /// Rev 2b finding 4: an `asset.typst` document importing a `@local`
    /// package that isn't installed fails — and the cached failure records
    /// the package lookup, so installing the package (without touching the
    /// asset's own file) rebuilds it. Its files are then deps too.
    #[test]
    fn installing_missing_local_package_rebuilds_typst_asset() {
        let (mut ctx, _dir) = site(&[
            (
                "content/main.typ",
                "#context asset.typst(\"/assets/doc.typ\", format: \"svg\").url()",
            ),
            ("assets/doc.typ", "#import \"@local/greet:0.1.0\": hi\n#hi"),
        ]);
        let packages = tempfile::tempdir().unwrap();
        ctx.package_dir = Some(packages.path().to_path_buf());
        let pkg = packages.path().join("local/greet/0.1.0");
        let mut session = DevSession::new(&ctx).unwrap();

        let r1 = session.rebuild();
        assert!(r1.result.is_err(), "expected a missing-package error");
        assert_eq!(r1.watch.paths.get(&pkg), Some(&Stamp::Missing));
        assert_eq!(
            session.rebuild().watch.paths.get(&pkg),
            Some(&Stamp::Missing)
        );
        assert_eq!(session.resolver.stats.builds, 1);

        write(
            &pkg,
            "typst.toml",
            "[package]\nname = \"greet\"\nversion = \"0.1.0\"\nentrypoint = \"lib.typ\"\n",
        );
        write(&pkg, "lib.typ", "#let hi = [hello]");
        let r2 = session.rebuild();
        if let Err(err) = &r2.result {
            panic!("still failing after installing the package:\n{err}");
        }
        assert!(r2.watch.contains(pkg.join("lib.typ")), "{:?}", r2.watch);

        // Editing the installed package rebuilds the asset too.
        std::thread::sleep(Duration::from_millis(10));
        write(&pkg, "lib.typ", "#let hi = [hello again]");
        let builds = session.resolver.stats.builds;
        assert!(session.rebuild().result.is_ok());
        assert_eq!(session.resolver.stats.builds, builds + 1);
    }

    fn page_id(rel: &str) -> typst::syntax::FileId {
        use typst::syntax::{FileId, RootedPath, VirtualPath, VirtualRoot};
        FileId::new(RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new(rel).unwrap(),
        ))
    }

    /// Rev 2b finding 8: warnings queued by an asset earlier in a discovery
    /// pass are reported by the compile whose pass then fails on another
    /// asset — not lost, and not left queued to leak into the next compile.
    #[test]
    fn warnings_before_a_failing_asset_are_reported() {
        let (ctx, _dir) = site(&[
            (
                "content/main.typ",
                "#context asset.svg(\"bad.svg\").url()\n\
                 #context asset.file(\"missing.txt\").url()",
            ),
            ("content/bad.svg", "<svg><open></svg>"),
        ]);
        let world = RenderWorld::new(&ctx).unwrap();
        let mut resolver = Resolver::new(&ctx);
        let warned = crate::compile::compile_bundle(
            &ctx,
            &world,
            &[page_id("content/main.typ")],
            &mut resolver,
        );
        assert!(warned.output.is_err());
        assert!(
            warned.warnings.iter().any(|w| w.message.contains("svg")),
            "warning lost: {:?}",
            warned.warnings
        );
        assert!(resolver.take_warnings().is_empty(), "warnings left queued");
    }

    /// Rev 2b finding 8: an output-policy error on a cached asset blames the
    /// request as it is *now*, not where it was when the asset was built.
    #[test]
    fn moved_request_with_invalid_output_blames_new_location() {
        let (ctx, dir) = site(&[
            (
                "content/main.typ",
                "#context asset.file(\"data.txt\", output: \"../x.txt\").url()",
            ),
            ("content/data.txt", "data"),
        ]);
        let main = page_id("content/main.typ");
        let mut world = RenderWorld::new(&ctx).unwrap();
        let mut resolver = Resolver::new(&ctx);
        let error_line = |world: &RenderWorld, resolver: &mut Resolver| {
            let warned = crate::compile::compile_bundle(&ctx, world, &[main], resolver);
            let errors = warned.output.err().expect("expected an output error");
            let error = errors
                .iter()
                .find(|e| e.message.contains("escapes"))
                .unwrap_or_else(|| panic!("{errors:?}"));
            let id = error.span.id().expect("error has a location");
            let source = typst::World::source(world, id).unwrap();
            let range = typst::WorldExt::range(world, error.span).unwrap();
            source.lines().byte_to_line(range.start).unwrap()
        };
        assert_eq!(error_line(&world, &mut resolver), 0);

        write(
            dir.path(),
            "content/main.typ",
            "first line\n\nsecond\n#context asset.file(\"data.txt\", output: \"../x.txt\").url()",
        );
        world.files.reset();
        assert_eq!(error_line(&world, &mut resolver), 3);
        assert_eq!(resolver.stats.builds, 1, "the asset itself is cached");
    }

    /// Rev 2b findings 8 and 9: an `asset.typst` sub-compile's warnings are
    /// kept with the cached entry, and its errors render with a snippet of
    /// the sub-document taken from the build — the live world never loads
    /// the sub-document into its persistent `FileStore`.
    #[test]
    fn typst_asset_diagnostics_come_from_the_build() {
        let (ctx, dir) = site(&[
            (
                "content/main.typ",
                "#context asset.typst(\"/assets/doc.typ\", format: \"svg\").url()",
            ),
            (
                "assets/doc.typ",
                "#set text(font: \"twyla-no-such-font\")\nhello",
            ),
        ]);
        let world = RenderWorld::new(&ctx).unwrap();
        let mut resolver = Resolver::new(&ctx);
        for round in 0..2 {
            let warned = crate::compile::compile_bundle(
                &ctx,
                &world,
                &[page_id("content/main.typ")],
                &mut resolver,
            );
            assert!(warned.output.is_ok(), "round {round}");
            assert!(
                warned.warnings.iter().any(|w| w.message.contains("font")),
                "round {round}: sub-compile warning missing: {:?}",
                warned.warnings
            );
        }
        assert_eq!(resolver.stats.builds, 1);

        write(
            dir.path(),
            "assets/doc.typ",
            "#let x = 1\n#oops-undefined-name",
        );
        let world = RenderWorld::new(&ctx).unwrap();
        let mut resolver = Resolver::new(&ctx);
        let err = world.compile_bundle(&mut resolver).err().expect("an error");
        let doc = ctx.root.join("assets/doc.typ");
        assert!(
            !world.take_loads().iter().any(|dep| dep.path == doc),
            "the live world loaded the sub-document"
        );
        let text = err.to_string();
        assert!(text.contains("oops-undefined-name"), "no snippet:\n{text}");
        assert!(text.contains("assets/doc.typ"), "no file name:\n{text}");
    }

    /// A panic in the middle of an asset build: what the build touched so far
    /// is still watched.
    #[test]
    fn asset_build_panic_keeps_its_deps_watched() {
        let (ctx, _dir) = site(&[
            (
                "content/main.typ",
                "#context asset.sass(\"main.scss\").url()",
            ),
            ("content/main.scss", "@use \"partial\";\n.a { color: red; }"),
            ("content/_partial.scss", ".b { color: blue; }"),
        ]);
        let mut session = DevSession::new(&ctx).unwrap();
        session.resolver.inject_build_panic = true;
        let r1 = session.rebuild();
        assert!(matches!(
            r1.result.err().unwrap().kind,
            RenderErrorKind::Panic
        ));
        for f in ["main.scss", "_partial.scss"] {
            assert!(
                r1.watch.contains(ctx.content_dir().join(f)),
                "{f}: {:?}",
                r1.watch
            );
        }
        assert!(session.rebuild().result.is_ok());
    }
}
