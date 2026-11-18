//! `twyla serve`'s file watcher, and the observations it checks against.
//!
//! Replaces `typst_kit::watcher::Watcher`, which serve outgrew:
//! - it only says "something changed", but serve needs the changed *paths*
//!   (to swap a static file in place, and, later, for path-based
//!   invalidation);
//! - it forgets nothing on `Remove(Folder)`, so a directory deleted and
//!   recreated between two `update`s stays "watched" with a dead subscription;
//! - its `wait` discards channel disconnection and spins forever once the
//!   backend dies;
//! - it can't tell whether a path changed *between* the compile reading it and
//!   the subscription being installed.
//!
//! The loop is the same shape as typst's: [`Watcher::update`] with the latest
//! [`WatchSet`] (mark-and-sweep, non-recursive watches, missing paths polled),
//! then [`Watcher::wait`] for a batch of relevant events. What's new:
//!
//! - **Observations.** Every [`WatchSet`] entry carries the [`Stamp`] the
//!   compile observed (file mtime+len, a directory's listing, or "missing").
//!   When `update` subscribes a path it wasn't already watching, it re-stamps
//!   the path *after* subscribing and compares: a difference means the path
//!   changed in the window no subscription covered, and the next `wait`
//!   returns immediately. This closes the read→subscribe race and covers
//!   edits made while a broken watcher was being replaced.
//! - **Identity invalidation.** Any create/remove/rename event drops the
//!   bookkeeping for that path *and everything under it*, so the next
//!   `update` re-subscribes (and re-checks) whatever now lives there.
//! - **No spinning, no panics escaping.** A disconnected channel or a backend
//!   error is returned as `Err`; the caller recreates the watcher. notify's
//!   inotify backend `unwrap`s its event-loop channel in `watch`, `unwatch`
//!   and `Drop`, so all three are run under `catch_unwind`.
//!
//! [`Stamp`] and [`WatchSet`] are plain data keyed by absolute paths (no typst
//! `FileId`s) so the planned dependency ledger can reuse or extend them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode};

/// What was observed at one path. Compared by [`Stamp::matches_now`] against a
/// fresh observation to decide whether the path changed since.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Stamp {
    /// The path didn't exist (or couldn't be stat'ed — the watcher treats
    /// both the same way, by polling `exists()`).
    Missing,
    /// A file (anything that isn't a directory), by modification time and
    /// length. Not a content hash: cheap enough to take for every dependency.
    File { mtime: SystemTime, len: u64 },
    /// A directory, by a hash of its sorted entry names (see
    /// [`Stamp::dir_listing`]). Only the *names* matter: a directory is
    /// watched for pages/files appearing or disappearing; edits to the files
    /// inside are observed through their own stamps or events.
    Dir { listing: u64 },
    /// The observation can't be trusted (the file may have changed while the
    /// compile was reading it, or a directory listing was cut short). Never
    /// matches, so re-checking it always reports a change.
    Unknown,
}

/// How far an mtime may lag the wall clock. Linux stamps files from a coarse
/// clock (one jiffy, 1–10ms), so an edit made just after a compile started
/// can carry an mtime slightly *before* the start. Generous on purpose; the
/// cost of a false positive is one extra rebuild.
const MTIME_SLACK: Duration = Duration::from_millis(50);

impl Stamp {
    /// Observe `path` as it is right now. Follows symlinks, like the compile.
    pub fn now(path: &Path) -> Stamp {
        let Ok(meta) = fs::metadata(path) else {
            return Stamp::Missing;
        };
        if meta.is_dir() {
            return match read_dir_names(path) {
                Ok(names) => Stamp::dir_listing(names),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Stamp::Missing,
                Err(_) => Stamp::Unknown,
            };
        }
        match meta.modified() {
            Ok(mtime) => Stamp::File {
                mtime,
                len: meta.len(),
            },
            Err(_) => Stamp::Unknown,
        }
    }

    /// Observe a file the compile read at some point after `started`.
    ///
    /// The stamp is taken *after* the compile, so an edit that landed between
    /// the read and now would be baked into the stamp and never noticed. Such
    /// an edit necessarily has an mtime at or after `started`, so any file
    /// modified since the compile began is recorded as [`Stamp::Unknown`]
    /// (forcing one re-check rebuild) instead.
    pub fn after_read(path: &Path, started: SystemTime) -> Stamp {
        match Stamp::now(path) {
            Stamp::File { mtime, .. } if mtime + MTIME_SLACK >= started => Stamp::Unknown,
            stamp => stamp,
        }
    }

    /// A directory stamp from its entry names, in any order. FNV-1a over the
    /// sorted names (each followed by a 0 byte): stable across runs and
    /// platforms' hash seeds, so it can be persisted.
    pub fn dir_listing(mut names: Vec<OsString>) -> Stamp {
        names.sort();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for name in &names {
            for &b in name.as_encoded_bytes().iter().chain(&[0]) {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        }
        Stamp::Dir { listing: hash }
    }

    /// Whether `path` still looks the way it did when this stamp was taken.
    pub fn matches_now(&self, path: &Path) -> bool {
        match self {
            Stamp::Unknown => false,
            stamp => *stamp == Stamp::now(path),
        }
    }
}

fn read_dir_names(dir: &Path) -> io::Result<Vec<OsString>> {
    fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect()
}

/// Every path whose change should trigger a rebuild, with what the compile
/// observed there. Each path is watched non-recursively, files and directories
/// alike; paths that don't exist are polled until they appear.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchSet {
    pub paths: BTreeMap<PathBuf, Stamp>,
}

impl WatchSet {
    pub fn contains(&self, path: impl AsRef<Path>) -> bool {
        self.paths.contains_key(path.as_ref())
    }

    /// Record an observation. A later observation of the same path replaces
    /// an earlier one.
    pub fn insert(&mut self, path: PathBuf, stamp: Stamp) {
        self.paths.insert(path, stamp);
    }

    /// Add every path of `older` that this set doesn't have, keeping its old
    /// stamp. Used after a failed rebuild so the watch set never shrinks
    /// because a compile stopped early.
    pub fn keep_from(&mut self, older: &WatchSet) {
        for (path, stamp) in &older.paths {
            self.paths
                .entry(path.clone())
                .or_insert_with(|| stamp.clone());
        }
    }
}

/// What [`Watcher::wait`] saw change.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChangedPaths {
    /// Paths named by relevant events, missing paths that appeared, and paths
    /// whose re-check in [`Watcher::update`] failed. May include paths outside
    /// the watch set (children of a watched directory).
    pub paths: BTreeSet<PathBuf>,
    /// Events were lost (the backend's queue overflowed): treat everything as
    /// changed.
    pub unknown: bool,
}

impl ChangedPaths {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.unknown
    }
}

/// The subscription calls [`Watcher`] needs from a notify backend. A trait
/// only so tests can drive the bookkeeping with a fake backend and synthetic
/// events.
pub trait Backend {
    fn watch(&mut self, path: &Path) -> notify::Result<()>;
    fn unwatch(&mut self, path: &Path) -> notify::Result<()>;
}

impl Backend for RecommendedWatcher {
    fn watch(&mut self, path: &Path) -> notify::Result<()> {
        notify::Watcher::watch(self, path, RecursiveMode::NonRecursive)
    }
    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        notify::Watcher::unwatch(self, path)
    }
}

/// Watches a [`WatchSet`]. See the module docs.
pub struct Watcher<B: Backend = RecommendedWatcher> {
    /// `Option` only so `Drop` can move it into a `catch_unwind`.
    backend: Option<B>,
    rx: Receiver<notify::Result<Event>>,
    /// Paths subscribed through `backend`. The bool is the mark for
    /// `update`'s mark-and-sweep.
    watched: HashMap<PathBuf, bool>,
    /// Paths in the watch set that don't exist, polled by `wait`.
    missing: BTreeSet<PathBuf>,
    /// Changes found by `update`'s re-check; the next `wait` returns them
    /// without blocking.
    pending: ChangedPaths,
}

/// How long to wait for a shortly following event before ending a batch.
/// Lets editors' write-temp-then-rename saves land as one rebuild.
const BATCH_TIMEOUT: Duration = Duration::from_millis(100);
/// The longest a batch may run, so a barrage of events can't starve rebuilds.
const STARVE_TIMEOUT: Duration = Duration::from_millis(500);
/// How often missing paths are polled (and the poll backend's interval).
const POLL_INTERVAL: Duration = Duration::from_millis(300);

impl Watcher {
    /// A watcher on the platform's recommended notify backend, watching
    /// nothing yet.
    pub fn new() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        // Only affects the poll backend (systems without native events).
        let config = notify::Config::default().with_poll_interval(POLL_INTERVAL);
        let backend = <RecommendedWatcher as notify::Watcher>::new(tx, config)
            .map_err(|e| format!("failed to set up file watching ({e})"))?;
        Ok(Watcher::with_backend(backend, rx))
    }
}

impl<B: Backend> Watcher<B> {
    /// A watcher on `backend`, which must deliver its events to `rx`.
    pub fn with_backend(backend: B, rx: Receiver<notify::Result<Event>>) -> Self {
        Watcher {
            backend: Some(backend),
            rx,
            watched: HashMap::new(),
            missing: BTreeSet::new(),
            pending: ChangedPaths::default(),
        }
    }

    /// Watch exactly the paths of `set`: subscribe new ones, unsubscribe ones
    /// no longer listed, poll missing ones. Every path not already subscribed
    /// is re-checked against its observed [`Stamp`] *after* subscribing, so a
    /// change in between is reported by the next [`wait`](Self::wait).
    pub fn update(&mut self, set: &WatchSet) -> Result<(), String> {
        for seen in self.watched.values_mut() {
            *seen = false;
        }
        self.missing.clear();

        for (path, observed) in &set.paths {
            if let Some(seen) = self.watched.get_mut(path) {
                // Subscribed since before the compile read it: any change
                // since then is already queued as an event.
                *seen = true;
                continue;
            }
            if path.exists() {
                match self.backend_call(|b| b.watch(path)) {
                    Ok(()) => {
                        self.watched.insert(path.clone(), true);
                    }
                    // Vanished between `exists` and `watch`: poll it instead.
                    Err(_) if !path.exists() => {
                        self.missing.insert(path.clone());
                    }
                    Err(e) => return Err(format!("failed to watch {} ({e})", path.display())),
                }
            } else {
                self.missing.insert(path.clone());
            }
            if !observed.matches_now(path) {
                self.pending.paths.insert(path.clone());
            }
        }

        let stale: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|(_, seen)| !**seen)
            .map(|(path, _)| path.clone())
            .collect();
        for path in stale {
            self.watched.remove(&path);
            // Best effort: the backend may already have dropped it.
            let _ = self.backend_call(|b| b.unwatch(&path));
        }
        Ok(())
    }

    /// Block until a watched path changes; return what changed. Returns `Err`
    /// if the backend reports an error or its channel disconnects — the
    /// watcher is then unusable and should be replaced.
    pub fn wait(&mut self) -> Result<ChangedPaths, String> {
        if !self.pending.is_empty() {
            return Ok(std::mem::take(&mut self.pending));
        }
        loop {
            // With missing paths to poll, wake up periodically even without
            // events.
            let first = if self.missing.is_empty() {
                Some(self.rx.recv().map_err(|_| disconnected())?)
            } else {
                match self.rx.recv_timeout(POLL_INTERVAL) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Err(disconnected()),
                }
            };

            let mut changed = ChangedPaths::default();
            if let Some(first) = first {
                let batch_start = Instant::now();
                self.handle_event(first, &mut changed)?;
                while batch_start.elapsed() <= STARVE_TIMEOUT {
                    match self.rx.recv_timeout(BATCH_TIMEOUT) {
                        Ok(event) => self.handle_event(event, &mut changed)?,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return Err(disconnected()),
                    }
                }
            }
            for path in &self.missing {
                if path.exists() {
                    changed.paths.insert(path.clone());
                }
            }
            if !changed.is_empty() {
                return Ok(changed);
            }
        }
    }

    fn handle_event(
        &mut self,
        event: notify::Result<Event>,
        changed: &mut ChangedPaths,
    ) -> Result<(), String> {
        let event = event.map_err(|e| format!("file watcher error ({e})"))?;

        // The backend dropped events: nothing we know can be trusted, so
        // forget every subscription (the next `update` re-subscribes and
        // re-checks all of them) and report "everything changed".
        if event.need_rescan() {
            for path in std::mem::take(&mut self.watched).into_keys() {
                let _ = self.backend_call(|b| b.unwatch(&path));
            }
            changed.unknown = true;
            return Ok(());
        }
        if !is_relevant(&event.kind) {
            return Ok(());
        }
        if changes_identity(&event.kind) {
            for path in &event.paths {
                self.invalidate(path);
            }
        }
        changed.paths.extend(event.paths);
        Ok(())
    }

    /// Forget the subscriptions for `path` and everything under it. After a
    /// create/remove/rename, whatever lives at those paths now may be a
    /// different inode than the one subscribed (inotify watches inodes, and
    /// drops the watch when its inode is deleted), so the next `update` must
    /// subscribe afresh.
    fn invalidate(&mut self, path: &Path) {
        let gone: Vec<PathBuf> = self
            .watched
            .keys()
            .filter(|p| p.starts_with(path))
            .cloned()
            .collect();
        for p in gone {
            self.watched.remove(&p);
            let _ = self.backend_call(|b| b.unwatch(&p));
        }
    }

    /// Run a backend call, turning a panic into an error (see module docs).
    fn backend_call(&mut self, f: impl FnOnce(&mut B) -> notify::Result<()>) -> Result<(), String> {
        let Some(backend) = self.backend.as_mut() else {
            return Err("file watcher backend is gone".into());
        };
        match catch_unwind(AssertUnwindSafe(|| f(backend))) {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => Err("file watcher backend panicked".into()),
        }
    }
}

impl<B: Backend> Drop for Watcher<B> {
    fn drop(&mut self) {
        // notify's inotify backend panics on drop if its event loop already
        // died; that must not take down serve's rebuild thread.
        if let Some(backend) = self.backend.take() {
            let _ = catch_unwind(AssertUnwindSafe(move || drop(backend)));
        }
    }
}

fn disconnected() -> String {
    "file watcher stopped (event channel disconnected)".into()
}

/// Whether an event kind can affect a compile. Same filter as typst's watcher:
/// content and namespace changes count; access and metadata-only changes don't.
fn is_relevant(kind: &EventKind) -> bool {
    match kind {
        EventKind::Any | EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(kind) => match kind {
            ModifyKind::Any | ModifyKind::Data(_) | ModifyKind::Name(_) => true,
            ModifyKind::Metadata(_) | ModifyKind::Other => false,
        },
        EventKind::Access(_) | EventKind::Other => false,
    }
}

/// Whether an event may mean the path now names a different file or
/// directory than the one subscribed (or none at all).
fn changes_identity(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Any
            | EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Name(_))
    )
}

#[cfg(test)]
mod tests;
