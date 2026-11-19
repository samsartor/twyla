//! The dependency ledger: what a build read from disk, recorded at the moment
//! it read it.
//!
//! Every asset build does its file I/O through a [`DepRecorder`], and the
//! typst [`TwylaLoader`](crate::render::TwylaLoader) records each project-file
//! load the same way. Each access becomes one [`Dep`]: the absolute path, what
//! kind of access it was, the [`Stamp`] observed *at access time*, and for a
//! read the SHA-256 of the bytes actually consumed. Recording happens around
//! the I/O whether or not it succeeds, so a build that fails (a sass syntax
//! error, an unresolved `@use`, a missing image) still knows exactly which
//! files it looked at — fixing any of them is noticed.
//!
//! Why not route asset reads through the typst `World`/`FileStore` (which
//! already tracks dependencies)? `FileStore` keeps every file it loads
//! resident and re-reads + re-hashes it on every compile. Twyla's assets can
//! be large (photos, video posters, PDFs), and a hard requirement is that an
//! asset's sources are only in memory transiently, while it's being built. The
//! ledger instead keeps a stamp and a digest per file, and only re-reads a file
//! when its stamp says it may have changed ([`revalidate`]).
//!
//! [`Dep`]s are plain data — absolute paths, no `FileId`s (process-interned),
//! no `Arc`s or world references — so a future on-disk cache can persist them
//! and [`revalidate`] them on a cold start without a compile.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::watch::Stamp;

/// How a build touched a path. Decides what [`revalidate`] compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DepKind {
    /// The build consumed the file's contents (or tried to). Valid while the
    /// contents are the same — checked by stamp, then by hash.
    Read,
    /// The build asked whether something exists at the path (e.g. sass import
    /// resolution trying `_partial.scss`, `partial.scss`, …). Valid while the
    /// answer — missing, regular file, directory, or something else (a FIFO)
    /// — stays the same. Its stamp is
    /// always [`Stamp::Missing`] or [`Stamp::Exists`].
    Probe,
    /// The build listed a directory. Valid while the entry names are the same.
    Dir,
}

/// One recorded access. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dep {
    /// Absolute path, as accessed (not canonicalized: a symlink and its target
    /// are different dependencies, like they are to the build).
    pub path: PathBuf,
    pub kind: DepKind,
    /// The path as observed when it was accessed, taken *before* the access.
    /// For a successful read it's re-checked after the read: if the two
    /// differ the file changed under the read, and the stamp is
    /// [`Stamp::Unknown`], which never matches. For a failed access it's kept
    /// as is — the baseline the failure is compared against, so a fix that
    /// lands between the failure and the watcher subscribing still differs
    /// from it.
    pub stamp: Stamp,
    /// [`DepKind::Read`] only: SHA-256 of the bytes the build consumed. `None`
    /// when the read failed or was abandoned (or for other kinds).
    pub sha256: Option<[u8; 32]>,
    /// Why the access failed, if it did (`NotFound`, `PermissionDenied`,
    /// `IsADirectory`, …). Kept for diagnosis; validity is decided by the
    /// stamp (see [`revalidate`]), except that a [transient](is_transient)
    /// error is never valid.
    pub error: Option<io::ErrorKind>,
    /// Wall-clock time just before the stamp was taken. A file changed
    /// within the timestamp granularity of this may be rewritten without its
    /// stamp changing, so its stamp alone can't vouch for it — see
    /// [`Dep::is_racy`].
    pub observed_at: SystemTime,
}

/// How close a file's change time may be to the observation time before
/// its stamp is "racy" (git's racy-index problem), on a filesystem with
/// whole-second timestamps (FAT: 2s; ext3, HFS+: 1s): a later write within
/// the same tick can leave every timestamp unchanged. 2s covers all of
/// them. A racy stamp costs one re-hash per check; it never forces a
/// rebuild by itself.
pub const RACY_WINDOW: Duration = Duration::from_secs(2);

/// [`RACY_WINDOW`] for timestamps with sub-second precision (ext4, xfs,
/// btrfs, tmpfs, APFS, NTFS). Linux stamps files from a coarse clock that
/// lags real time by up to one scheduler tick (≤10ms), so a later write can
/// share a timestamp only within that lag; 50ms leaves margin.
pub const FINE_RACY_WINDOW: Duration = Duration::from_millis(50);

impl Dep {
    /// Whether this read's stamp can't vouch for its contents: the file
    /// changed within the timestamp granularity of being read, so a later
    /// same-length rewrite could have left the stamp unchanged. Such a dep is
    /// verified by hash instead.
    ///
    /// Keyed off the change time the kernel sets on every write (unix ctime;
    /// mtime elsewhere), compared within a window *either side* of the
    /// observation. The granularity is guessed from the timestamp itself: a
    /// whole-second change time means a whole-second filesystem
    /// ([`RACY_WINDOW`]), anything else a fine one ([`FINE_RACY_WINDOW`]). A
    /// change time far in the *future* (clock skew, or on non-unix an mtime
    /// set ahead by `touch -d`) isn't racy: a later write is stamped with the
    /// current clock, so it can't reproduce it. So a future-dated file is
    /// re-hashed at most until its stamp settles, not on every compile.
    pub fn is_racy(&self) -> bool {
        self.kind == DepKind::Read
            && self.sha256.is_some()
            && stamp_is_racy(&self.stamp, self.observed_at)
    }
}

/// Whether a file `stamp` taken at `observed_at` is racy: see
/// [`Dep::is_racy`]. Also used by the watcher's poll to decide when a racy
/// polled file has settled.
pub fn stamp_is_racy(stamp: &Stamp, observed_at: SystemTime) -> bool {
    let Stamp::File { mtime, ctime, .. } = stamp else {
        return false;
    };
    let changed = ctime.unwrap_or(*mtime);
    let whole_second = changed
        .duration_since(SystemTime::UNIX_EPOCH)
        .is_ok_and(|d| d.subsec_nanos() == 0);
    let window = if whole_second {
        RACY_WINDOW
    } else {
        FINE_RACY_WINDOW
    };
    let gap = match changed.duration_since(observed_at) {
        Ok(ahead) => ahead,
        Err(behind) => behind.duration(),
    };
    gap < window
}

// ---------------------------------------------------------------------------
// Observing one access
// ---------------------------------------------------------------------------

/// Read `path` fully, recording the access. Used by [`DepRecorder::read`] and
/// the typst loader, which need the bytes anyway.
pub fn observe_read(path: &Path) -> (Dep, io::Result<Vec<u8>>) {
    let path = absolute(path);
    let observed_at = SystemTime::now();
    let before = Stamp::now(&path);
    let result = fs::read(&path);
    let dep = match &result {
        Ok(bytes) => Dep {
            stamp: settle(&path, before),
            sha256: Some(sha256(bytes)),
            error: None,
            path,
            kind: DepKind::Read,
            observed_at,
        },
        Err(err) => failed_read(path, before, err.kind(), observed_at),
    };
    (dep, result)
}

/// Hash `path` by streaming it, recording the access the same way
/// [`observe_read`] would — without ever holding the contents.
pub fn observe_hash(path: &Path) -> (Dep, io::Result<[u8; 32]>) {
    let path = absolute(path);
    let observed_at = SystemTime::now();
    let before = Stamp::now(&path);
    let result = fs::File::open(&path).and_then(|mut file| {
        let mut hasher = Sha256::new();
        io::copy(&mut file, &mut hasher)?;
        Ok(<[u8; 32]>::from(hasher.finalize()))
    });
    let dep = match &result {
        Ok(hash) => Dep {
            stamp: settle(&path, before),
            sha256: Some(*hash),
            error: None,
            path,
            kind: DepKind::Read,
            observed_at,
        },
        Err(err) => failed_read(path, before, err.kind(), observed_at),
    };
    (dep, result)
}

/// Record an existence check, returning the `metadata` (following symlinks)
/// it was decided from: callers derive their answer ("is it a file?") from
/// this same lookup, so the answer and the record can't disagree. Never lists
/// a directory.
pub fn observe_probe(path: &Path) -> (Dep, Option<fs::Metadata>) {
    let path = absolute(path);
    let observed_at = SystemTime::now();
    let meta = fs::metadata(&path).ok();
    let dep = Dep {
        stamp: Stamp::existence(meta.as_ref()),
        path,
        kind: DepKind::Probe,
        sha256: None,
        error: None,
        observed_at,
    };
    (dep, meta)
}

/// List `path`'s entry names, recording the access (names and entry kinds,
/// see [`Stamp::Dir`]).
pub fn observe_dir(path: &Path) -> (Dep, io::Result<Vec<OsString>>) {
    let path = absolute(path);
    let observed_at = SystemTime::now();
    // For a failed listing, the baseline is what was there *before* (one
    // stat, no listing), as for a failed read.
    let before = fs::metadata(&path);
    let result = crate::watch::read_dir_entries(&path);
    let (stamp, error) = match &result {
        Ok(entries) => (Stamp::dir_listing(entries.clone()), None),
        Err(err) => {
            let stamp = match &before {
                Err(_) => Stamp::Missing,
                Ok(meta) if meta.is_dir() => Stamp::Unreadable,
                Ok(meta) => Stamp::file(meta),
            };
            (stamp, Some(err.kind()))
        }
    };
    let dep = Dep {
        path,
        kind: DepKind::Dir,
        stamp,
        sha256: None,
        error,
        observed_at,
    };
    (
        dep,
        result.map(|entries| entries.into_iter().map(|(name, _)| name).collect()),
    )
}

/// The racy-read guard: the stamp taken before a successful read, if the path
/// still looks the same after it; otherwise the file changed while it was
/// being read, and the bytes may be a mix of old and new — record a stamp
/// that never matches so the next check re-hashes.
fn settle(path: &Path, before: Stamp) -> Stamp {
    if Stamp::now(path) == before {
        before
    } else {
        Stamp::Unknown
    }
}

/// The dep of a read that failed with `error`, stamped with the state
/// observed *before* the attempt. A stable observation, deliberately: a
/// failure that persists (a missing file, a directory, no permission)
/// re-checks as unchanged and stays cached, instead of being retried — and
/// rebuilding — on every compile. A fix is a change to that baseline: the
/// file appearing, or (on unix) any write, replace or `chmod`, which moves
/// its ctime and permission bits (see [`Stamp::File`]). Taking the baseline
/// before the attempt, not after, means a fix that lands between the failure
/// and the stamp still registers as a change.
fn failed_read(path: PathBuf, before: Stamp, error: io::ErrorKind, observed_at: SystemTime) -> Dep {
    Dep {
        path,
        kind: DepKind::Read,
        stamp: before,
        sha256: None,
        error: Some(error),
        observed_at,
    }
}

/// Whether an I/O error says nothing about the file, only about the moment:
/// out of file descriptors (`EMFILE`/`ENFILE`), interrupted, would block,
/// timed out, out of memory. A build that failed this way is retried by the
/// next compile instead of being cached against the (unchanged) stamp —
/// otherwise nothing would ever retry it, since the file never changes.
/// Retried on the next compile only, never forcing one, so a persistent
/// `EMFILE` can't loop the dev server.
pub fn is_transient(kind: io::ErrorKind) -> bool {
    use io::ErrorKind::*;
    matches!(kind, Interrupted | WouldBlock | TimedOut | OutOfMemory)
        // `ErrorKind::TooManyOpenFiles` (EMFILE/ENFILE) is unstable
        // (`io_error_too_many_open_files`), so it can't be named here yet.
        || format!("{kind:?}") == "TooManyOpenFiles"
}

/// Make `path` absolute without touching the filesystem (it may not exist).
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

// ---------------------------------------------------------------------------
// The recorder
// ---------------------------------------------------------------------------

/// Collects the [`Dep`]s of one asset build. Owned by the resolver *call*,
/// not returned from the build, so an early `?` in a build can't drop what it
/// recorded. `Send + Sync` (a `Mutex`, not a `RefCell`) because tools that may
/// run multithreaded (a future rolldown integration) share one.
#[derive(Debug, Default)]
pub struct DepRecorder {
    deps: Mutex<Vec<Dep>>,
    /// Files whose contents were read (or streamed). Test-visible I/O budget.
    content_reads: AtomicUsize,
}

impl DepRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open `path` for streaming. The returned reader hashes what it reads;
    /// [`TrackedFile::finish`] records the dep (and the reader records it on
    /// drop if the read failed or was abandoned before EOF). A failed open is
    /// recorded immediately.
    pub fn open(&self, path: &Path) -> io::Result<TrackedFile<'_>> {
        let path = absolute(path);
        let observed_at = SystemTime::now();
        let before = Stamp::now(&path);
        self.content_reads.fetch_add(1, Ordering::Relaxed);
        match fs::File::open(&path) {
            Ok(file) => Ok(TrackedFile {
                recorder: self,
                file,
                hasher: Sha256::new(),
                path: Some(path),
                before,
                error: None,
                observed_at,
            }),
            Err(err) => {
                self.push(failed_read(path, before, err.kind(), observed_at));
                Err(err)
            }
        }
    }

    /// Read `path` fully. Errors carry the path (`/abs/path: No such file…`).
    pub fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.content_reads.fetch_add(1, Ordering::Relaxed);
        let (dep, result) = observe_read(path);
        let shown = dep.path.clone();
        self.push(dep);
        result.map_err(|err| annotate(&shown, err))
    }

    /// Whether `path` is a file (following symlinks), recorded as a probe.
    pub fn is_file(&self, path: &Path) -> bool {
        let (dep, meta) = observe_probe(path);
        self.push(dep);
        meta.is_some_and(|m| m.is_file())
    }

    /// Whether `path` is a directory (following symlinks), recorded as a probe.
    pub fn is_dir(&self, path: &Path) -> bool {
        let (dep, meta) = observe_probe(path);
        self.push(dep);
        meta.is_some_and(|m| m.is_dir())
    }

    /// The entry names of directory `path`, recorded by listing.
    pub fn list_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        let (dep, result) = observe_dir(path);
        self.push(dep);
        result
    }

    /// Record deps a tool observed itself and reports after the fact (e.g. a
    /// bundler's module graph). The tool is responsible for having stamped
    /// them at access time.
    pub fn ingest(&self, deps: impl IntoIterator<Item = Dep>) {
        self.lock().extend(deps);
    }

    /// How many files' contents were read since the last
    /// [`take`](Self::take) (or since creation).
    pub fn content_reads(&self) -> usize {
        self.content_reads.load(Ordering::Relaxed)
    }

    /// Everything recorded since the last call, in access order; resets the
    /// recorder (including [`content_reads`](Self::content_reads)).
    pub fn take(&self) -> Vec<Dep> {
        self.content_reads.store(0, Ordering::Relaxed);
        std::mem::take(&mut *self.lock())
    }

    /// Everything recorded so far, without resetting.
    pub fn deps(&self) -> Vec<Dep> {
        self.lock().clone()
    }

    /// The deps, even if a panicking build poisoned the lock: what was
    /// recorded before the panic is exactly what the dev server needs.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Dep>> {
        self.deps.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, dep: Dep) {
        self.lock().push(dep);
    }
}

/// A file opened through [`DepRecorder::open`]. See there.
pub struct TrackedFile<'a> {
    recorder: &'a DepRecorder,
    file: fs::File,
    hasher: Sha256,
    /// Taken by `finish`; still set on drop means the read failed or was
    /// abandoned.
    path: Option<PathBuf>,
    before: Stamp,
    /// The error a read returned, if one did. Tells a *failed* read (a
    /// stable failure, e.g. `EISDIR` reading a directory) apart from an
    /// *abandoned* one when the reader is dropped.
    error: Option<io::ErrorKind>,
    observed_at: SystemTime,
}

impl Read for TrackedFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.file.read(buf) {
            Ok(n) => {
                self.hasher.update(&buf[..n]);
                Ok(n)
            }
            Err(err) => {
                if err.kind() != io::ErrorKind::Interrupted {
                    self.error = Some(err.kind());
                }
                Err(err)
            }
        }
    }
}

impl TrackedFile<'_> {
    /// Read whatever is left (discarding it), record the dep, and return the
    /// SHA-256 of the whole file. On a read error, the drop records it.
    pub fn finish(mut self) -> io::Result<[u8; 32]> {
        io::copy(&mut self, &mut io::sink())?;
        let path = self.path.take().expect("finish runs once");
        let hash: [u8; 32] = std::mem::take(&mut self.hasher).finalize().into();
        let stamp = settle(&path, std::mem::replace(&mut self.before, Stamp::Unknown));
        self.recorder.push(Dep {
            path,
            kind: DepKind::Read,
            stamp,
            sha256: Some(hash),
            error: None,
            observed_at: self.observed_at,
        });
        Ok(hash)
    }
}

impl Drop for TrackedFile<'_> {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let before = std::mem::replace(&mut self.before, Stamp::Unknown);
        let dep = match self.error {
            // The read *failed*: as stable as a failed open (see
            // `failed_read`). Recording `Unknown` here would evict and
            // rebuild the asset on every compile, and keep a polled path
            // rebuilding forever.
            Some(error) => failed_read(path, before, error, self.observed_at),
            // Abandoned mid-read (the caller bailed): what the build consumed
            // is unknown, so record a stamp that never matches.
            None => Dep {
                path,
                kind: DepKind::Read,
                stamp: Stamp::Unknown,
                sha256: None,
                error: None,
                observed_at: self.observed_at,
            },
        };
        self.recorder.push(dep);
    }
}

/// Annotate an I/O error with the absolute path it concerns, so a failed asset
/// read reports *which* file (`/abs/path: No such file or directory`) instead of
/// a bare OS error.
pub fn annotate(path: &Path, err: io::Error) -> io::Error {
    let shown = absolute(path);
    io::Error::new(err.kind(), format!("{}: {err}", shown.display()))
}

// ---------------------------------------------------------------------------
// Revalidation
// ---------------------------------------------------------------------------

/// The verdict of [`revalidate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revalidation {
    /// Every dep still holds, with the stamps it has.
    Valid,
    /// Every dep still holds, but some stamps were stale (e.g. `touch`ed, or
    /// racy) and were re-taken after their contents hashed the same. Replace
    /// the entry's deps with these.
    Restamped(Vec<Dep>),
    /// Something the build depended on changed: rebuild.
    Changed,
}

/// Check recorded `deps` against the disk. A function of ledger data alone —
/// no world, no compile — so a cold start can check a persisted cache with it.
///
/// Per dep, cheapest test first:
/// - **Read**: stamp equal (and not [racy](Dep::is_racy)) → valid. Otherwise
///   re-hash by streaming: same digest → valid with a fresh stamp (so a
///   `touch` doesn't redo an image pipeline); different → changed. A read
///   that *failed* is valid while its pre-access stamp still matches (see
///   [`failed_read`]): the same state fails the same way — unless the error
///   was [transient](is_transient), which is always retried.
/// - **Probe**: valid while the path is still missing / a file / a directory.
/// - **Dir**: valid while the listing is the same.
///
/// `content_reads` counts the files re-hashed (the test-visible I/O budget).
pub fn revalidate(deps: &[Dep], content_reads: &mut usize) -> Revalidation {
    let mut restamped: Option<Vec<Dep>> = None;
    for (i, dep) in deps.iter().enumerate() {
        if dep.error.is_some_and(is_transient) {
            return Revalidation::Changed;
        }
        let fresh = match dep.kind {
            DepKind::Read => {
                let now = Stamp::now(&dep.path);
                if dep.sha256.is_none() {
                    // A failed (or abandoned: `Unknown`) read.
                    if now != dep.stamp || dep.stamp == Stamp::Unknown {
                        return Revalidation::Changed;
                    }
                    None
                } else if now == dep.stamp && !dep.is_racy() {
                    None
                } else {
                    *content_reads += 1;
                    let (fresh, hash) = observe_hash(&dep.path);
                    match hash {
                        Ok(hash) if Some(hash) == dep.sha256 => Some(fresh),
                        _ => return Revalidation::Changed,
                    }
                }
            }
            DepKind::Probe => {
                // One `metadata` call, as when it was recorded: never a listing.
                if Stamp::existence(fs::metadata(&dep.path).ok().as_ref()) != dep.stamp {
                    return Revalidation::Changed;
                }
                None
            }
            DepKind::Dir => {
                if Stamp::now(&dep.path) != dep.stamp {
                    return Revalidation::Changed;
                }
                None
            }
        };
        if let Some(fresh) = fresh {
            restamped.get_or_insert_with(|| deps.to_vec())[i] = fresh;
        }
    }
    match restamped {
        Some(deps) => Revalidation::Restamped(deps),
        None => Revalidation::Valid,
    }
}

#[cfg(test)]
mod tests;
