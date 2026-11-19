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
use std::time::SystemTime;

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
    /// - files typst read (`world.dependencies()`);
    /// - files twyla's asset pass read (each resolved asset's upstream paths);
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

        // Taken before anything is read: a file modified after this instant
        // may have changed after the compile read it (see `Stamp::after_read`).
        let started = SystemTime::now();
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
        let mut watch = self.observed(&scan, started);

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
        }
        self.last_watch = watch.clone();
        Rebuild { result, watch }
    }

    /// What the compile that just ran observed. See [`Rebuild::watch`].
    fn observed(&mut self, scan: &ContentScan, started: SystemTime) -> WatchSet {
        let mut watch = WatchSet::default();
        for (path, stamp) in &scan.observed {
            watch.insert(path.clone(), stamp.clone());
        }

        for path in self.world.dependencies().collect::<Vec<_>>() {
            let stamp = Stamp::after_read(&path, started);
            watch.insert(path, stamp);
        }

        // Asset upstreams (e.g. sass `@import` partials) are read by twyla's
        // own pass, not typst, so they're not in `world.dependencies()`. Pulled
        // from the resolver, not the published outputs, so that an asset that
        // is only `.read()` (inlined, never emitted as a file) is still watched.
        for asset in self.resolver.resolved_assets() {
            for path in asset.upstream_paths() {
                watch.insert(path.to_path_buf(), Stamp::after_read(path, started));
            }
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

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

    /// KNOWN BUG (fixed by the dep-ledger revision): an asset whose build
    /// fails never lands in the resolver, so its `upstream` — the only record
    /// of the files twyla's own asset pass read — is lost, and those files drop
    /// out of the watch set. Editing the broken stylesheet (or the broken
    /// partial it imports) to fix it then doesn't trigger a rebuild; serve is
    /// stuck on the error page until some *typst* dependency changes.
    #[test]
    #[ignore = "fixed by dep-ledger revision"]
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

    /// Same known bug, via a broken `@use`d partial: the partial is only ever
    /// recorded in the failed asset's (discarded) upstream.
    #[test]
    #[ignore = "fixed by dep-ledger revision"]
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
}
