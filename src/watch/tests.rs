use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use notify::event::{DataChange, RemoveKind};

use super::*;

// ---- fake backend: deterministic bookkeeping tests --------------------------

/// Records every successful call, and counts every call at all.
#[derive(Default)]
struct Log {
    watched: Vec<PathBuf>,
    unwatched: Vec<PathBuf>,
    calls: usize,
}

/// The `panic_on_*` flags make calls panic like notify's inotify backend does
/// when its event loop is dead; `refuse` makes `watch` fail for those paths
/// like it does for, e.g., a directory without read permission.
#[derive(Default)]
struct Fake {
    log: Arc<Mutex<Log>>,
    refuse: Vec<PathBuf>,
    panic_on_watch: bool,
    panic_on_unwatch: bool,
    panic_on_drop: bool,
}

impl Backend for Fake {
    fn watch(&mut self, path: &Path) -> notify::Result<()> {
        self.log.lock().unwrap().calls += 1;
        if self.panic_on_watch {
            panic!("fake backend: event loop is dead");
        }
        if self.refuse.iter().any(|p| p == path) {
            return Err(notify::Error::io(io::Error::from(
                io::ErrorKind::PermissionDenied,
            )));
        }
        self.log.lock().unwrap().watched.push(path.to_path_buf());
        Ok(())
    }
    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.log.lock().unwrap().calls += 1;
        if self.panic_on_unwatch {
            panic!("fake backend: event loop is dead");
        }
        self.log.lock().unwrap().unwatched.push(path.to_path_buf());
        Ok(())
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        if self.panic_on_drop {
            panic!("fake backend: drop with dead event loop");
        }
    }
}

type Tx = Sender<notify::Result<Event>>;

fn fake() -> (Watcher<Fake>, Tx, Arc<Mutex<Log>>) {
    fake_with(Fake::default())
}

/// A watcher on `backend` (whose log is replaced by the returned one).
fn fake_with(mut backend: Fake) -> (Watcher<Fake>, Tx, Arc<Mutex<Log>>) {
    let (tx, rx) = mpsc::channel();
    let log = Arc::new(Mutex::new(Log::default()));
    backend.log = Arc::clone(&log);
    (Watcher::with_backend(backend, rx), tx, log)
}

/// A watch set of existing paths, each stamped as it is now.
fn observed(paths: &[&Path]) -> WatchSet {
    let mut set = WatchSet::default();
    for p in paths {
        set.insert(p.to_path_buf(), Stamp::now(p));
    }
    set
}

fn event(kind: EventKind, path: &Path) -> notify::Result<Event> {
    Ok(Event::new(kind).add_path(path.to_path_buf()))
}

#[test]
fn update_marks_and_sweeps() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        dir.path().join("a"),
        dir.path().join("b"),
        dir.path().join("c"),
    );
    for p in [&a, &b, &c] {
        fs::write(p, "x").unwrap();
    }
    let (mut w, _tx, log) = fake();

    w.update(&observed(&[&a, &b])).unwrap();
    w.update(&observed(&[&b, &c])).unwrap();

    let log = log.lock().unwrap();
    // `b` stayed watched: subscribed once, never unsubscribed.
    assert_eq!(log.watched, [a.clone(), b.clone(), c.clone()]);
    assert_eq!(log.unwatched, [a]);
    assert!(w.pending.is_empty(), "{:?}", w.pending);
}

#[test]
fn missing_paths_are_polled_not_watched() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone");
    let mut set = WatchSet::default();
    set.insert(gone.clone(), Stamp::Missing);
    let (mut w, _tx, log) = fake();

    w.update(&set).unwrap();
    assert!(log.lock().unwrap().watched.is_empty());
    assert!(w.polled.contains_key(&gone));
    assert!(w.pending.is_empty(), "still missing, as observed");

    // Appearing is reported by the poll in `wait`, without any event.
    fs::write(&gone, "here").unwrap();
    let changed = w.wait().unwrap();
    assert!(changed.paths.contains(&gone), "{changed:?}");
}

/// Removing a watched directory forgets it *and* everything under it, so the
/// next `update` re-subscribes the recreated paths (typst-kit's watcher kept
/// them marked watched forever).
#[test]
fn folder_removal_invalidates_subtree() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    let file = sub.join("page.typ");
    fs::create_dir(&sub).unwrap();
    fs::write(&file, "x").unwrap();
    let set = observed(&[dir.path(), &sub, &file]);
    let (mut w, tx, log) = fake();
    w.update(&set).unwrap();

    tx.send(event(EventKind::Remove(RemoveKind::Folder), &sub))
        .unwrap();
    let changed = w.wait().unwrap();
    assert!(changed.paths.contains(&sub));
    assert!(w.watched.contains_key(dir.path()), "parent untouched");
    assert!(!w.watched.contains_key(&sub));
    assert!(!w.watched.contains_key(&file));

    log.lock().unwrap().watched.clear();
    w.update(&set).unwrap();
    let mut rewatched = log.lock().unwrap().watched.clone();
    rewatched.sort();
    assert_eq!(rewatched, [sub, file]);
}

/// A plain data modification doesn't touch the subscription.
#[test]
fn data_modification_keeps_subscription() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a");
    fs::write(&file, "x").unwrap();
    let (mut w, tx, log) = fake();
    w.update(&observed(&[&file])).unwrap();

    tx.send(event(
        EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        &file,
    ))
    .unwrap();
    assert!(w.wait().unwrap().paths.contains(&file));
    assert!(w.watched.contains_key(&file));
    assert!(log.lock().unwrap().unwatched.is_empty());
}

/// The read→subscribe race: the compile observed one state, the file changed
/// before `update` subscribed it. `wait` must report it at once — no event
/// will ever arrive for it.
#[test]
fn update_rechecks_new_paths_against_observation() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a");
    let sub = dir.path().join("sub");
    fs::write(&file, "v1").unwrap();
    fs::create_dir(&sub).unwrap();
    let set = observed(&[&file, &sub]);

    fs::write(&file, "v2, longer").unwrap();
    fs::write(sub.join("new.typ"), "").unwrap();

    let (mut w, _tx, _log) = fake();
    w.update(&set).unwrap();
    // `_tx` is alive and silent: if `wait` blocked on the channel this test
    // would hang.
    let changed = w.wait().unwrap();
    assert_eq!(changed.paths, BTreeSet::from([file, sub]));
}

/// Already-subscribed paths aren't re-checked (their changes arrive as events),
/// so a stale stamp on them doesn't cause a rebuild loop.
#[test]
fn update_does_not_recheck_already_watched_paths() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a");
    fs::write(&file, "v1").unwrap();
    let mut set = WatchSet::default();
    set.insert(file.clone(), Stamp::now(&file));
    let (mut w, _tx, _log) = fake();
    w.update(&set).unwrap();

    set.insert(file.clone(), Stamp::Unknown);
    w.update(&set).unwrap();
    assert!(w.pending.is_empty());
}

#[test]
fn disconnected_channel_is_an_error_not_a_spin() {
    let (mut w, tx, _log) = fake();
    drop(tx);
    assert!(w.wait().is_err());

    // Also while polling missing paths (the `recv_timeout` branch).
    let (mut w, tx, _log) = fake();
    let mut set = WatchSet::default();
    set.insert(
        PathBuf::from("/nonexistent/twyla-watch-test"),
        Stamp::Missing,
    );
    w.update(&set).unwrap();
    drop(tx);
    assert!(w.wait().is_err());
}

#[test]
fn backend_error_event_is_an_error() {
    let (mut w, tx, _log) = fake();
    tx.send(Err(notify::Error::generic("backend died")))
        .unwrap();
    assert!(w.wait().is_err());
}

#[test]
fn overflow_forgets_everything_and_reports_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, tx, _log) = fake();
    w.update(&observed(&[dir.path()])).unwrap();
    tx.send(Ok(
        Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan)
    ))
    .unwrap();
    let changed = w.wait().unwrap();
    assert!(changed.unknown);
    assert!(w.watched.is_empty());
}

#[test]
fn backend_panics_become_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let backend = {
        let mut f = Fake::default();
        f.panic_on_watch = true;
        f.panic_on_drop = true;
        f
    };
    let mut w = Watcher::with_backend(backend, rx);
    assert!(w.update(&observed(&[dir.path()])).is_err());
    // Dropping the dead backend panics inside, but not out of, `drop`.
    drop(w);
    drop(tx);
}

#[test]
fn stamps() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a");
    assert_eq!(Stamp::now(&file), Stamp::Missing);

    fs::write(&file, "v1").unwrap();
    let s1 = Stamp::now(&file);
    assert!(matches!(s1, Stamp::File { len: 2, .. }));
    assert!(s1.matches_now(&file));
    fs::write(&file, "v22").unwrap();
    assert!(!s1.matches_now(&file));

    assert!(!Stamp::Unknown.matches_now(&file));

    // Directory stamps track names only, in any order.
    let d1 = Stamp::now(dir.path());
    fs::write(&file, "edited contents").unwrap();
    assert_eq!(
        Stamp::now(dir.path()),
        d1,
        "content edits don't change a listing"
    );
    fs::write(dir.path().join("b"), "").unwrap();
    assert_ne!(Stamp::now(dir.path()), d1);
    assert_eq!(
        Stamp::dir_listing(vec!["x".into(), "y".into()]),
        Stamp::dir_listing(vec!["y".into(), "x".into()]),
    );
}

#[test]
fn watch_set_keep_from_never_shrinks() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    let mut old = WatchSet::default();
    // `a` was a file when the old set observed it; it's gone now.
    old.insert(
        a.clone(),
        Stamp::File {
            mtime: SystemTime::UNIX_EPOCH,
            len: 1,
        },
    );
    old.insert(b.clone(), Stamp::Missing);
    let mut new = WatchSet::default();
    new.insert(b.clone(), Stamp::Unknown);
    new.keep_from(&old);
    assert_eq!(new.paths.len(), 2);
    assert_eq!(new.paths[&b], Stamp::Unknown, "this attempt's stamp wins");
    assert_eq!(
        new.paths[&a],
        Stamp::Missing,
        "retained-only paths are re-baselined at their current state"
    );
}

/// An unreadable directory is a stable observation (re-checking it while
/// it stays unreadable must not report a change), unlike `Unknown`.
#[cfg(unix)]
#[test]
fn unreadable_dir_stamp_is_stable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("locked");
    fs::create_dir(&sub).unwrap();
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&sub).is_ok() {
        return; // running as root: permissions aren't enforced
    }
    let stamp = Stamp::now(&sub);
    assert_eq!(stamp, Stamp::Unreadable);
    assert!(stamp.matches_now(&sub));
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!stamp.matches_now(&sub));
}

/// Regression (rev 1b): a failed rebuild retained an import's old `File`
/// stamp after the import was deleted, so every `update` re-check saw a
/// change, queued a rebuild, which failed the same way — forever, with no
/// filesystem events. The retained path must go idle, then wake when it
/// reappears.
#[test]
fn failed_rebuild_with_deleted_retained_import_goes_idle() {
    use crate::project::TwylaContext;
    use crate::session::DevSession;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir(root.join("content")).unwrap();
    let main = root.join("content/main.typ");
    let lib = root.join("content/_lib.typ");
    fs::write(&main, "#import \"_lib.typ\": x\n#x").unwrap();
    fs::write(&lib, "#let x = [lib]").unwrap();
    let ctx = TwylaContext::new(&root, None).unwrap();
    let mut session = DevSession::new(&ctx).unwrap();
    let (mut w, tx, _log) = fake();

    let r1 = session.rebuild();
    assert!(r1.result.is_ok());
    assert!(matches!(r1.watch.paths.get(&lib), Some(Stamp::File { .. })));
    w.update(&r1.watch).unwrap();

    // Break main.typ before it reaches the import, and delete the import.
    std::thread::sleep(Duration::from_millis(10));
    fs::write(&main, "#oops-undefined\n#import \"_lib.typ\": x").unwrap();
    fs::remove_file(&lib).unwrap();
    tx.send(event(
        EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        &main,
    ))
    .unwrap();
    tx.send(event(EventKind::Remove(RemoveKind::File), &lib))
        .unwrap();
    let changed = w.wait().unwrap();
    assert!(changed.paths.contains(&lib));

    // Two failed rounds: neither may leave anything to rebuild for.
    for _ in 0..2 {
        let r = session.rebuild();
        assert!(r.result.is_err());
        assert_eq!(r.watch.paths.get(&lib), Some(&Stamp::Missing));
        w.update(&r.watch).unwrap();
        assert!(
            w.pending.is_empty(),
            "re-check queued a rebuild: {:?}",
            w.pending
        );
        let mut polled = ChangedPaths::default();
        w.poll(&mut polled);
        assert!(polled.is_empty(), "poll reported a change: {polled:?}");
    }

    // The import reappearing wakes the loop.
    fs::write(&lib, "#let x = [back]").unwrap();
    let mut polled = ChangedPaths::default();
    w.poll(&mut polled);
    assert!(polled.paths.contains(&lib), "{polled:?}");
}

/// A path the backend refuses doesn't take the watcher down: the other
/// subscriptions stay, the refused path is polled (and warned about once,
/// not on every `update`), and a change to it — here a `chmod`, which
/// leaves its stamp unchanged — is reported.
#[cfg(unix)]
#[test]
fn refused_path_is_polled_and_others_stay_watched() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    fs::write(&a, "x").unwrap();
    fs::write(&b, "x").unwrap();
    let (mut w, _tx, log) = fake_with({
        let mut f = Fake::default();
        f.refuse = vec![b.clone()];
        f
    });
    let set = observed(&[&a, &b]);

    let warnings = w.update(&set).unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("polling"), "{warnings:?}");
    assert!(w.watched.contains_key(&a));
    assert!(!w.watched.contains_key(&b));
    assert!(w.polled.contains_key(&b));
    assert!(w.pending.is_empty());

    assert_eq!(w.update(&set).unwrap(), Vec::<String>::new(), "warned once");
    assert_eq!(log.lock().unwrap().watched, [a]);

    fs::set_permissions(&b, fs::Permissions::from_mode(0o000)).unwrap();
    let changed = w.wait().unwrap();
    assert_eq!(changed.paths, BTreeSet::from([b]));
}

fn set_mtime(path: &Path, mtime: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

/// A temp-dir site for session-driven watcher tests.
fn session_site(
    files: &[(&str, &str)],
) -> (PathBuf, tempfile::TempDir, crate::session::DevSession) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir(root.join("content")).unwrap();
    for (rel, contents) in files {
        fs::write(root.join(rel), contents).unwrap();
    }
    let ctx = crate::project::TwylaContext::new(&root, None).unwrap();
    let session = crate::session::DevSession::new(&ctx).unwrap();
    (root, dir, session)
}

/// Regression (rev 1c): a dependency with an mtime in the future (clock skew)
/// used to be stamped `Unknown` on every rebuild, which never matches, so
/// every `update` queued another rebuild — a tight loop with no filesystem
/// change. With read-time stamps it's merely *racy*: the watcher verifies it
/// by hash, which matches, so the loop goes idle (here: immediately; the
/// contract is at most one extra rebuild).
#[test]
fn future_mtime_dependency_goes_idle() {
    let (root, _dir, mut session) = session_site(&[
        (
            "content/main.typ",
            "#import \"_lib.typ\": x\n#x\n#context asset.sass(\"main.scss\").url()",
        ),
        ("content/_lib.typ", "#let x = [lib]"),
        ("content/main.scss", ".a { color: red; }"),
    ]);
    let future = SystemTime::now() + Duration::from_secs(3600);
    for f in ["content/_lib.typ", "content/main.scss"] {
        set_mtime(&root.join(f), future);
    }
    // Refused paths are polled, the case the review found looping.
    let (mut w, _tx, _log) = fake_with({
        let mut f = Fake::default();
        f.refuse = vec![
            root.join("content/_lib.typ"),
            root.join("content/main.scss"),
        ];
        f
    });

    let mut extra = 0;
    for round in 0..3 {
        let r = session.rebuild();
        assert!(r.result.is_ok(), "round {round}");
        w.update(&r.watch).unwrap();
        let mut polled = ChangedPaths::default();
        w.poll(&mut polled);
        assert!(
            polled.is_empty(),
            "round {round}: poll saw a change: {polled:?}"
        );
        if !w.pending.is_empty() {
            extra += 1;
            w.pending = ChangedPaths::default();
        }
    }
    assert!(extra <= 1, "{extra} extra rebuilds: not idle");
    assert_eq!(session.resolver.stats.builds, 1, "asset rebuilt");
}

/// A racy file (modified within the filesystem-granularity window of its
/// read) rewritten with the same length *and* mtime before the watcher
/// subscribes: the stamp can't see it, the re-hash does.
#[test]
fn racy_rewrite_before_subscribe_is_reported() {
    let (root, _dir, mut session) = session_site(&[
        ("content/main.typ", "#import \"_lib.typ\": x\n#x"),
        ("content/_lib.typ", "#let x = [aaa]"),
    ]);
    let lib = root.join("content/_lib.typ");
    // Pinned ahead so it's racy however slowly the compile runs.
    let mtime = SystemTime::now() + Duration::from_secs(30);
    set_mtime(&lib, mtime);
    let r = session.rebuild();
    assert!(r.watch.racy.contains_key(&lib), "{r:?}", r = r.watch);

    fs::write(&lib, "#let x = [bbb]").unwrap();
    set_mtime(&lib, mtime);
    let (mut w, _tx, _log) = fake();
    w.update(&r.watch).unwrap();
    assert!(w.pending.paths.contains(&lib), "{:?}", w.pending);
}

/// Regression (rev 1c): a subscribed dependency becomes unreadable, the
/// build fails on it, then `chmod` restores it. That's a metadata-only event,
/// which the watcher filters as noise — so nothing ever rebuilt. While the
/// last build failed, metadata events count; and the asset ledger retries a
/// read that failed on an existing file even though its stamp is unchanged.
#[cfg(unix)]
#[test]
fn permission_restore_after_failed_build_rebuilds() {
    use notify::event::MetadataKind;
    use std::os::unix::fs::PermissionsExt;

    let (root, _dir, mut session) = session_site(&[
        (
            "content/main.typ",
            "#context asset.sass(\"main.scss\").url()",
        ),
        ("content/main.scss", ".a { color: red; }"),
        ("content/other.typ", "#import \"_lib.typ\": x\n#x"),
        ("content/_lib.typ", "#let x = [lib]"),
    ]);
    let r1 = session.rebuild();
    assert!(r1.result.is_ok());
    let (mut w, _tx, _log) = fake();
    w.update(&r1.watch).unwrap();
    let metadata = |p: &Path| {
        event(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions)),
            p,
        )
    };

    // A successful build: metadata events are noise.
    let mut changed = ChangedPaths::default();
    w.handle_event(metadata(&root.join("content/main.scss")), &mut changed)
        .unwrap();
    assert!(changed.is_empty(), "{changed:?}");

    for broken in ["content/main.scss", "content/_lib.typ"] {
        let path = root.join(broken);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&path).is_ok() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            return; // running as root: permissions aren't enforced
        }
        // `chmod` alone changes no stamp, so a cached asset stays valid
        // (rightly: its bytes didn't change). Also bump the mtime so the next
        // build re-reads the file and fails on it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        set_mtime(&path, SystemTime::now() - Duration::from_secs(60));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let r = session.rebuild();
        assert!(r.result.is_err(), "{broken}: expected a read failure");
        assert!(r.watch.failed);
        assert!(r.watch.contains(&path), "{broken} not watched");
        w.update(&r.watch).unwrap();
        assert!(w.pending.is_empty(), "{broken}: {:?}", w.pending);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let mut changed = ChangedPaths::default();
        w.handle_event(metadata(&path), &mut changed).unwrap();
        assert!(changed.paths.contains(&path), "{broken}: chmod ignored");

        let r = session.rebuild();
        assert!(r.result.is_ok(), "{broken}: still failing after chmod");
        w.update(&r.watch).unwrap();
    }
}

/// A panic from `unwatch` (best-effort elsewhere) kills the watcher: `update`
/// returns `Err`, and the dead backend is never called again.
#[test]
fn unwatch_panic_is_fatal_and_backend_is_not_called_again() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, "x").unwrap();
    let (mut w, _tx, log) = fake_with({
        let mut f = Fake::default();
        f.panic_on_unwatch = true;
        f
    });
    w.update(&observed(&[&a])).unwrap();

    // Dropping `a` from the set unwatches it: panics.
    assert!(w.update(&WatchSet::default()).is_err());
    let calls = log.lock().unwrap().calls;
    assert!(w.update(&observed(&[&a])).is_err());
    assert!(w.wait().is_err());
    assert_eq!(
        log.lock().unwrap().calls,
        calls,
        "dead backend called again"
    );
}

/// Same via the event path: an identity-changing event unwatches, and a
/// panic there makes `wait` fail.
#[test]
fn unwatch_panic_during_invalidation_fails_wait() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, "x").unwrap();
    let (mut w, tx, _log) = fake_with({
        let mut f = Fake::default();
        f.panic_on_unwatch = true;
        f
    });
    w.update(&observed(&[&a])).unwrap();
    tx.send(event(EventKind::Remove(RemoveKind::File), &a))
        .unwrap();
    assert!(w.wait().is_err());
}

// ---- real notify backend -----------------------------------------------------

/// Run `wait` on another thread so a missed event fails the test instead of
/// hanging it. Returns the watcher for further use.
fn wait_with_timeout(w: Watcher) -> (Watcher, ChangedPaths) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut w = w;
        let changed = w.wait();
        let _ = tx.send((w, changed));
    });
    let (w, changed) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("watcher missed the change");
    (w, changed.unwrap())
}

/// Wait until `path` shows up among the changes (events may arrive in more
/// than one batch).
fn wait_for(mut w: Watcher, path: &Path) -> Watcher {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (next, changed) = wait_with_timeout(w);
        w = next;
        if changed.paths.contains(path) {
            return w;
        }
        assert!(
            Instant::now() < deadline,
            "{} never reported",
            path.display()
        );
    }
}

/// Drain whatever events are already queued (e.g. from setup writes).
fn settle(w: &mut Watcher) {
    std::thread::sleep(Duration::from_millis(150));
    while w.rx.try_recv().is_ok() {}
}

#[test]
fn notify_edit_of_watched_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let file = root.join("a.typ");
    fs::write(&file, "v1").unwrap();

    let mut w = Watcher::new().unwrap();
    w.update(&observed(&[&file])).unwrap();
    settle(&mut w);
    fs::write(&file, "v2").unwrap();
    wait_for(w, &file);
}

/// Delete and recreate a watched nested directory within one batch, rebuild
/// (re-observe + update), then add a file inside it: must be noticed.
#[test]
fn notify_recreated_nested_dir_is_rewatched() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sub = root.join("posts");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("first.typ"), "").unwrap();

    let mut w = Watcher::new().unwrap();
    w.update(&observed(&[&root, &sub])).unwrap();
    settle(&mut w);

    fs::remove_dir_all(&sub).unwrap();
    fs::create_dir(&sub).unwrap();
    let mut w = wait_for(w, &sub);

    // What the next rebuild's scan would observe.
    w.update(&observed(&[&root, &sub])).unwrap();
    settle(&mut w);
    let new = sub.join("new.typ");
    fs::write(&new, "").unwrap();
    wait_for(w, &new);
}

/// The compile observed a file, then it changed before `update` subscribed
/// it: reported without any further filesystem event.
#[test]
fn notify_change_before_subscribe_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let file = root.join("a.typ");
    fs::write(&file, "v1").unwrap();
    let set = observed(&[&file]);

    fs::write(&file, "v2 (edited during the compile)").unwrap();

    let mut w = Watcher::new().unwrap();
    w.update(&set).unwrap();
    let (_, changed) = wait_with_timeout(w);
    assert!(changed.paths.contains(&file), "{changed:?}");
}

#[test]
fn notify_create_in_watched_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mut w = Watcher::new().unwrap();
    w.update(&observed(&[&root])).unwrap();
    settle(&mut w);
    let file = root.join("b.css");
    fs::write(&file, "").unwrap();
    wait_for(w, &file);
}
