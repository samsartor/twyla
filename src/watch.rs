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
//!   compile observed (a file's mtime, length and — on unix — ctime, inode
//!   and permissions; a directory's listing; existence alone for a probe;
//!   or "missing").
//!   When `update` subscribes a path it wasn't already watching, it re-stamps
//!   the path *after* subscribing and compares: a difference means the path
//!   changed in the window no subscription covered, and the next `wait`
//!   returns immediately. This closes the read→subscribe race and covers
//!   edits made while a broken watcher was being replaced.
//! - **Identity invalidation.** Any create/remove/rename event drops the
//!   bookkeeping for that path *and everything under it*, so the next
//!   `update` re-subscribes (and re-checks) whatever now lives there.
//! - **Per-path failures stay per-path.** A path the backend refuses to
//!   watch (permission denied, vanished mid-call, inotify's watch limit) is
//!   polled instead, by a [`Probe`] of its existence, permissions and
//!   identity, and reported once as a warning. Healthy subscriptions are
//!   kept, so one unreadable directory can't suspend reload for the site.
//! - **No spinning, no panics escaping.** A disconnected channel, an error
//!   event, or a backend panic is returned as `Err`; the caller recreates the
//!   watcher. notify's inotify backend `unwrap`s its event-loop channel in
//!   `watch`, `unwatch` and `Drop`, so all three are run under
//!   `catch_unwind`, and after one panic the backend is never called again.
//!
//! Known limitations (not handled; see the rev-1b review):
//! - On Linux, two watched paths that resolve to the same directory (e.g.
//!   `content/real` and a symlink `content/alias` to it) share one inotify
//!   watch descriptor in notify 8.2. Unwatching one (say the alias is
//!   removed) drops the watch for both, while the other stays marked watched
//!   here, so changes inside it can go unnoticed until something else
//!   invalidates it.
//! - On macOS, notify's FSEvents backend stops and restarts its whole stream
//!   on every `watch`/`unwatch` call, starting "since now". An edit to an
//!   already-subscribed path landing in that gap is lost: `update` only
//!   re-checks paths it newly subscribes.
//!
//! [`Stamp`] and [`WatchSet`] are plain data keyed by absolute paths (no typst
//! `FileId`s); the stamps come from [`crate::ledger`], taken at the moment the
//! compile read each path.

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
    /// A file (anything that isn't a directory). Not a content hash: cheap
    /// enough to take for every dependency, but (on unix) strong enough that
    /// any write, replace or `chmod` changes it — see [`Stamp::file`].
    File {
        mtime: SystemTime,
        len: u64,
        /// Unix: the inode change time (`st_ctime`). The kernel bumps it on
        /// every write, rename, link or permission change, and — unlike
        /// mtime — nothing can set it back, so an in-place rewrite that keeps
        /// the length and restores the mtime still changes the stamp. It is
        /// also what [`Dep::is_racy`](crate::ledger::Dep::is_racy) keys off.
        /// `None` elsewhere.
        ctime: Option<SystemTime>,
        /// Unix: `(st_dev, st_ino)`. A file replaced by a rename (an
        /// editor's atomic save) is a different file even if every time and
        /// the length agree. `None` elsewhere.
        inode: Option<(u64, u64)>,
        /// Permission bits (unix: `st_mode & 0o7777`; elsewhere 1 if
        /// read-only, else 0). A `chmod` can fix (or break) a failed read
        /// without touching anything else, so it must be a change — even on
        /// filesystems whose ctime is too coarse to show it.
        perm: u32,
    },
    /// A directory, by a hash of its sorted entry names (see
    /// [`Stamp::dir_listing`]). Only the *names* matter: a directory is
    /// watched for pages/files appearing or disappearing; edits to the files
    /// inside are observed through their own stamps or events.
    Dir { listing: u64 },
    /// A directory that exists but can't be listed (e.g. permission
    /// denied). A stable observation, unlike [`Stamp::Unknown`]: an
    /// unreadable directory that stays unreadable has not changed, and
    /// treating it as changed would rebuild forever.
    Unreadable,
    /// Something exists here — a directory if `dir`, otherwise a file — and
    /// nothing else about it was observed: the stamp of an existence probe
    /// (sass import resolution asking "is `_partial.scss` a file?", a package
    /// lookup asking "is this version installed?"). Taken from a single
    /// `metadata` call ([`Stamp::existence`]), so a probed directory is never
    /// listed. Matches any later observation of the same type, so `touch`ing
    /// a probed file doesn't invalidate the probe; in a [`WatchSet`] it
    /// merges with a full observation of the same path (see
    /// [`WatchSet::observe`]).
    Exists { dir: bool },
    /// The observation can't be trusted: the file changed while the compile
    /// was reading it (see [`crate::ledger`]'s racy-read guard). Never
    /// matches, so re-checking it always reports a change. Only produced by
    /// an actual change, never by a stable state, so it forces at most one
    /// extra rebuild per change rather than a loop.
    Unknown,
}

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
                Err(_) => Stamp::Unreadable,
            };
        }
        Stamp::file(&meta)
    }

    /// The stamp of a non-directory from its metadata. Every field comes from
    /// this one `stat`, so they describe one moment. A platform that can't
    /// report an mtime gets the epoch: a stable (if weak) stamp, never
    /// [`Stamp::Unknown`], which would rebuild forever.
    pub fn file(meta: &fs::Metadata) -> Stamp {
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Stamp::File {
                mtime,
                len: meta.len(),
                ctime: Some(unix_time(meta.ctime(), meta.ctime_nsec())),
                inode: Some((meta.dev(), meta.ino())),
                perm: meta.mode() & 0o7777,
            }
        }
        #[cfg(not(unix))]
        {
            Stamp::File {
                mtime,
                len: meta.len(),
                ctime: None,
                inode: None,
                perm: u32::from(meta.permissions().readonly()),
            }
        }
    }

    /// The stamp of an existence probe, from the probe's own `metadata`
    /// result (`None`: not found or not stat-able). See [`Stamp::Exists`].
    pub fn existence(meta: Option<&fs::Metadata>) -> Stamp {
        match meta {
            Some(meta) => Stamp::Exists { dir: meta.is_dir() },
            None => Stamp::Missing,
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
    /// An [`Exists`](Stamp::Exists) stamp only checks existence and type
    /// (one `metadata` call, no listing).
    pub fn matches_now(&self, path: &Path) -> bool {
        match self {
            Stamp::Unknown => false,
            Stamp::Exists { .. } => *self == Stamp::existence(fs::metadata(path).ok().as_ref()),
            stamp => *stamp == Stamp::now(path),
        }
    }
}

/// A unix `(seconds, nanoseconds)` timestamp as a [`SystemTime`].
#[cfg(unix)]
fn unix_time(secs: i64, nsec: i64) -> SystemTime {
    let nsec = Duration::from_nanos(nsec.clamp(0, 999_999_999) as u64);
    if secs >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64) + nsec
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()) + nsec
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
    /// Content hashes of files whose stamp is *racy* (changed within the
    /// timestamp granularity of being read, so a same-length rewrite could
    /// keep the stamp — see [`Dep::is_racy`](crate::ledger::Dep::is_racy)).
    /// When [`Watcher::update`] re-checks such a path and its stamp still
    /// matches, it re-hashes it. A matching hash is a stable answer, so a
    /// racy file never forces a rebuild by itself.
    pub racy: BTreeMap<PathBuf, [u8; 32]>,
    /// The compile that produced this set failed. A failure can be fixed by
    /// a metadata-only change — `chmod` on an unreadable dependency (in a
    /// file's stamp on unix, but not in a directory's listing, nor in any
    /// stamp elsewhere) — so while this is set the watcher also treats
    /// metadata-only events as changes. (When the build succeeds, metadata events stay ignored, as
    /// they're noise: editors and backup tools touch them constantly.)
    pub failed: bool,
}

impl WatchSet {
    pub fn contains(&self, path: impl AsRef<Path>) -> bool {
        self.paths.contains_key(path.as_ref())
    }

    /// Record an observation, replacing any earlier one of the same path.
    pub fn insert(&mut self, path: PathBuf, stamp: Stamp) {
        self.racy.remove(&path);
        self.paths.insert(path, stamp);
    }

    /// Merge one of several observations of `path` made during one compile
    /// (a file both typst and an asset read, or a sass probe followed by the
    /// read). Compatible observations merge (see [`merge`]); disagreeing ones
    /// mean the path changed mid-compile, so neither can be trusted and the
    /// result is [`Stamp::Unknown`] (one recheck). `racy_hash` is the content
    /// hash for a racy read (see [`WatchSet::racy`]).
    pub fn observe(&mut self, path: PathBuf, stamp: Stamp, racy_hash: Option<[u8; 32]>) {
        let merged = match self.paths.get(&path) {
            None => Some(stamp),
            Some(seen) => merge(seen, &stamp),
        };
        let Some(merged) = merged else {
            self.racy.remove(&path);
            self.paths.insert(path, Stamp::Unknown);
            return;
        };
        if let Some(hash) = racy_hash {
            match self.racy.get(&path) {
                Some(seen) if *seen != hash => {
                    self.racy.remove(&path);
                    self.paths.insert(path, Stamp::Unknown);
                    return;
                }
                _ => {
                    self.racy.insert(path.clone(), hash);
                }
            }
        }
        self.paths.insert(path, merged);
    }

    /// Add every path of `older` that this set doesn't have, stamped as it
    /// is *now*. Used after a failed rebuild so the watch set never shrinks
    /// because a compile stopped early.
    ///
    /// Retained paths get a fresh baseline rather than their old stamp: this
    /// attempt didn't observe them, so the old stamp says nothing about what
    /// it saw. Keeping it would also loop forever — e.g. an import deleted
    /// while `main.typ` is broken before reaching it: the old `File` stamp
    /// never matches the now-missing path, so the watcher's post-subscribe
    /// re-check queues a rebuild, which fails the same way and retains the
    /// same stamp, with no filesystem event involved. Baselined at the
    /// current state, a retained path only triggers on its next transition.
    /// Paths this attempt did observe keep their compile observations.
    pub fn keep_from(&mut self, older: &WatchSet) {
        for path in older.paths.keys() {
            if !self.paths.contains_key(path) {
                self.paths.insert(path.clone(), Stamp::now(path));
            }
        }
    }
}

/// Two observations of one path from one compile, combined — or `None` if
/// they disagree. Equal observations agree. An existence probe
/// ([`Stamp::Exists`]) agrees with any full observation of the same type,
/// which is kept as the more precise: grass probes `_partial.scss` and then
/// reads it, and after a `touch` the read's stamp is refreshed (by
/// revalidation) while the probe's needn't be — comparing them for equality
/// would merge a stable state into a never-matching `Unknown` on every
/// rebuild.
fn merge(a: &Stamp, b: &Stamp) -> Option<Stamp> {
    let same_type = |dir: bool, full: &Stamp| match full {
        Stamp::File { .. } => !dir,
        Stamp::Dir { .. } | Stamp::Unreadable => dir,
        _ => false,
    };
    match (a, b) {
        _ if a == b && *a != Stamp::Unknown => Some(a.clone()),
        (Stamp::Exists { dir }, full) | (full, Stamp::Exists { dir }) if same_type(*dir, full) => {
            Some(full.clone())
        }
        _ => None,
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

/// How a polled path looked: its [`Stamp`] plus, for paths the backend
/// refused to watch, what can make it watchable again without changing the
/// stamp — permissions (a `chmod` touches neither mtime nor listing) and
/// identity (the inode, on unix). Taken when the path starts being polled and
/// compared on every poll; any difference is reported as a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub stamp: Stamp,
    pub permissions: Option<fs::Permissions>,
    pub inode: Option<(u64, u64)>,
}

impl Probe {
    pub fn now(path: &Path) -> Probe {
        let meta = fs::metadata(path).ok();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            meta.as_ref().map(|m| (m.dev(), m.ino()))
        };
        #[cfg(not(unix))]
        let inode = None;
        Probe {
            stamp: Stamp::now(path),
            permissions: meta.map(|m| m.permissions()),
            inode,
        }
    }
}

/// Watches a [`WatchSet`]. See the module docs.
pub struct Watcher<B: Backend = RecommendedWatcher> {
    /// `Option` only so `Drop` can move it into a `catch_unwind`.
    backend: Option<B>,
    /// Set when a backend call panicked: the backend's internals are in an
    /// unknown state, so it's never called again (except to drop it, which
    /// is itself panic-guarded) and every later call fails with this message.
    dead: Option<String>,
    rx: Receiver<notify::Result<Event>>,
    /// Paths subscribed through `backend`. The bool is the mark for
    /// `update`'s mark-and-sweep.
    watched: HashMap<PathBuf, bool>,
    /// Paths in the watch set without a subscription — missing ones, and ones
    /// the backend refused — polled by `wait` against how they looked when
    /// `update` gave up on subscribing them.
    polled: BTreeMap<PathBuf, Probe>,
    /// Paths already warned about as unwatchable, so a path that stays
    /// unwatchable is reported once, not on every `update`. A path leaves
    /// this set when it's subscribed or leaves the watch set.
    warned: BTreeSet<PathBuf>,
    /// Changes found by `update`'s re-check; the next `wait` returns them
    /// without blocking.
    pending: ChangedPaths,
    /// [`WatchSet::failed`] of the last `update`: metadata events count.
    metadata_relevant: bool,
}

/// How long to wait for a shortly following event before ending a batch.
/// Lets editors' write-temp-then-rename saves land as one rebuild.
const BATCH_TIMEOUT: Duration = Duration::from_millis(100);
/// The longest a batch may run, so a barrage of events can't starve rebuilds.
const STARVE_TIMEOUT: Duration = Duration::from_millis(500);
/// How often polled paths are checked (and the poll backend's interval).
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Why a [`Backend`] call failed.
enum CallError {
    /// The backend refused this one path (permission denied, not found, out
    /// of inotify watches, …). The backend itself is fine.
    Refused(notify::Error),
    /// The backend panicked, now or earlier: the watcher is unusable.
    Dead(String),
}

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
            dead: None,
            rx,
            watched: HashMap::new(),
            polled: BTreeMap::new(),
            warned: BTreeSet::new(),
            pending: ChangedPaths::default(),
            metadata_relevant: false,
        }
    }

    /// Watch exactly the paths of `set`: subscribe new ones, unsubscribe ones
    /// no longer listed, poll missing ones and ones the backend refuses. Every
    /// path not already subscribed is re-checked against its observed
    /// [`Stamp`] *after* subscribing, so a change in between is reported by
    /// the next [`wait`](Self::wait).
    ///
    /// Returns one warning per path that newly became unwatchable (it's
    /// polled instead). `Err` only when the backend itself is unusable (it
    /// panicked); the watcher should then be replaced.
    pub fn update(&mut self, set: &WatchSet) -> Result<Vec<String>, String> {
        let mut warnings = Vec::new();
        self.metadata_relevant = set.failed;
        for seen in self.watched.values_mut() {
            *seen = false;
        }
        self.polled.clear();
        self.warned.retain(|p| set.paths.contains_key(p));

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
                        self.warned.remove(path);
                    }
                    Err(CallError::Dead(e)) => return Err(e),
                    // Refused (or vanished between `exists` and `watch`):
                    // poll it from its current state. Healthy subscriptions
                    // are unaffected.
                    Err(CallError::Refused(e)) => {
                        let probe = Probe::now(path);
                        if probe.stamp != Stamp::Missing && self.warned.insert(path.clone()) {
                            warnings.push(format!(
                                "cannot watch {} ({e}); polling it instead",
                                path.display()
                            ));
                        }
                        self.polled.insert(path.clone(), probe);
                    }
                }
            } else {
                self.polled.insert(path.clone(), Probe::now(path));
            }
            let changed = !observed.matches_now(path)
                || set
                    .racy
                    .get(path)
                    .is_some_and(|hash| crate::ledger::observe_hash(path).1.ok() != Some(*hash));
            if changed {
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
            self.unwatch(&path)?;
        }
        Ok(warnings)
    }

    /// Block until a watched path changes; return what changed. Returns `Err`
    /// if the backend reports an error, panics, or its channel disconnects —
    /// the watcher is then unusable and should be replaced.
    pub fn wait(&mut self) -> Result<ChangedPaths, String> {
        if let Some(e) = &self.dead {
            return Err(e.clone());
        }
        if !self.pending.is_empty() {
            return Ok(std::mem::take(&mut self.pending));
        }
        loop {
            // With paths to poll, wake up periodically even without events.
            let first = if self.polled.is_empty() {
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
            self.poll(&mut changed);
            if !changed.is_empty() {
                return Ok(changed);
            }
        }
    }

    /// Add every polled path that no longer looks the way it did when
    /// `update` started polling it.
    fn poll(&self, changed: &mut ChangedPaths) {
        for (path, probe) in &self.polled {
            if Probe::now(path) != *probe {
                changed.paths.insert(path.clone());
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
                self.unwatch(&path)?;
            }
            changed.unknown = true;
            return Ok(());
        }
        let metadata = matches!(event.kind, EventKind::Modify(ModifyKind::Metadata(_)));
        if !is_relevant(&event.kind) && !(metadata && self.metadata_relevant) {
            return Ok(());
        }
        if changes_identity(&event.kind) {
            for path in &event.paths {
                self.invalidate(path)?;
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
    fn invalidate(&mut self, path: &Path) -> Result<(), String> {
        let gone: Vec<PathBuf> = self
            .watched
            .keys()
            .filter(|p| p.starts_with(path))
            .cloned()
            .collect();
        for p in gone {
            self.watched.remove(&p);
            self.unwatch(&p)?;
        }
        Ok(())
    }

    /// Unsubscribe `path`, best effort: the backend may already have dropped
    /// the watch (its inode was deleted), so a refusal is ignored. A panic is
    /// not: the backend is dead and the watcher must be replaced.
    fn unwatch(&mut self, path: &Path) -> Result<(), String> {
        match self.backend_call(|b| b.unwatch(path)) {
            Ok(()) | Err(CallError::Refused(_)) => Ok(()),
            Err(CallError::Dead(e)) => Err(e),
        }
    }

    /// Run a backend call, turning a panic into [`CallError::Dead`] (see
    /// module docs). After the first panic the backend is never called again.
    fn backend_call(
        &mut self,
        f: impl FnOnce(&mut B) -> notify::Result<()>,
    ) -> Result<(), CallError> {
        if let Some(e) = &self.dead {
            return Err(CallError::Dead(e.clone()));
        }
        let Some(backend) = self.backend.as_mut() else {
            return Err(CallError::Dead("file watcher backend is gone".into()));
        };
        match catch_unwind(AssertUnwindSafe(|| f(backend))) {
            Ok(result) => result.map_err(CallError::Refused),
            Err(_) => {
                let e = "file watcher backend panicked".to_string();
                self.dead = Some(e.clone());
                Err(CallError::Dead(e))
            }
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
/// content and namespace changes count; access and metadata-only changes don't
/// (except after a failed build — see [`WatchSet::failed`]).
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
