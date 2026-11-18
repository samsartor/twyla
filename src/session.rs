//! The dev server's persistent rebuild step.
//!
//! [`DevSession`] owns everything that survives between `twyla serve`
//! recompiles — the [`RenderWorld`] (FileStore + comemo-cached typst state)
//! and the asset/document [`Resolver`] — and exposes exactly one operation,
//! [`rebuild`](DevSession::rebuild): invalidate, compile, and report both the
//! result and the [`WatchSet`] of paths whose change should trigger the next
//! rebuild.
//!
//! `serve`'s watcher thread is just `rebuild → watcher.update(watch) → wait`
//! in a loop; tests drive the same `rebuild` by hand (edit a file, call it
//! again), so what serve watches and how it recompiles are both testable
//! without a file watcher or a socket.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use crate::project::TwylaContext;
use crate::render::{Outputs, RenderError, RenderErrorKind, RenderWorld, plain_err};
use crate::resolver::Resolver;

/// Persistent compile state for `twyla serve`. See the module docs.
pub struct DevSession {
    pub world: RenderWorld,
    pub resolver: Resolver,
}

/// The outcome of one [`DevSession::rebuild`].
pub struct Rebuild {
    pub result: Result<Outputs, RenderError>,
    /// What to watch until the next rebuild. Computed whether or not the
    /// compile succeeded, so fixing an error is itself noticed.
    pub watch: WatchSet,
}

/// Every path whose change should trigger a rebuild. Each is watched
/// non-recursively (files and directories alike); paths that don't exist are
/// polled by `typst_kit::watcher::Watcher` until they appear.
///
/// The union of three sources, each explicit in [`DevSession::rebuild`]:
/// - files typst read (`world.dependencies()`),
/// - files twyla's asset pass read (each resolved asset's upstream paths),
/// - every directory the page scan listed (`content/` and its subdirs), so
///   creating or deleting a page anywhere under `content/` is noticed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchSet {
    pub paths: BTreeSet<PathBuf>,
}

impl WatchSet {
    pub fn contains(&self, path: impl AsRef<Path>) -> bool {
        self.paths.contains(path.as_ref())
    }
}

impl DevSession {
    pub fn new(ctx: &TwylaContext) -> Result<Self, RenderError> {
        Ok(Self {
            world: RenderWorld::new(ctx)?,
            resolver: Resolver::new(ctx),
        })
    }

    /// Recompile the site against the current disk state. The first call is
    /// the initial (cold) compile; later calls reuse the warm caches.
    ///
    /// Mirrors one iteration of typst's own `typst watch` loop
    /// (`typst-cli/src/watch.rs`): age comemo, mark FileStore slots stale (both
    /// no-ops before the first compile), compile, then collect what to watch.
    /// The page set is rescanned inside `compile_bundle` every time, so added
    /// or removed pages need no separate refresh step.
    pub fn rebuild(&mut self) -> Rebuild {
        comemo::evict(10);
        self.world.files.reset();

        // A panic anywhere in the compile (typst, an asset pipeline, or an
        // invariant `panic!` in `compile_bundle`) would otherwise unwind
        // through serve's watcher thread and end hot reload for the session.
        // Catch it and report it like any other render error. The resolver
        // may have been left mid-update, so it's replaced with a fresh one —
        // losing the asset cache is cheap next to serving stale state. The
        // world needs no repair: the next rebuild resets its FileStore anyway.
        let world = &self.world;
        let resolver = &mut self.resolver;
        let result = match catch_unwind(AssertUnwindSafe(|| world.compile_bundle(resolver))) {
            Ok(result) => result,
            Err(payload) => {
                self.resolver = Resolver::new(&self.world.ctx);
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

        Rebuild {
            result,
            watch: self.watch_set(),
        }
    }

    /// What to watch after the compile that just ran. See [`WatchSet`].
    fn watch_set(&mut self) -> WatchSet {
        let mut paths: BTreeSet<PathBuf> = self.world.dependencies().collect();

        // Asset upstreams (e.g. sass `@import` partials) are read by twyla's
        // own pass, not typst, so they're not in `world.dependencies()`. Pulled
        // from the resolver, not the published outputs, so that an asset that
        // is only `.read()` (inlined, never emitted as a file) is still watched.
        for asset in self.resolver.resolved_assets() {
            paths.extend(asset.upstream_paths().map(Path::to_path_buf));
        }

        // `content/` itself always, even if it's missing or the scan failed
        // (the watcher polls missing paths), so creating the first page works.
        // Then every directory the page scan visits: watching each one
        // non-recursively sees pages added to any existing subdirectory, and
        // a new subdirectory shows up as a change to its parent.
        let ctx = &self.world.ctx;
        paths.insert(ctx.content_dir());
        if let Ok(scan) = ctx.scan_content() {
            paths.extend(scan.dirs);
        }

        WatchSet { paths }
    }
}

#[cfg(test)]
mod tests {
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
}
