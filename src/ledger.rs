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
    /// answer — missing, file, or directory — stays the same.
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
    /// The path as observed when it was accessed. For a read, taken before the
    /// read and re-checked after it: if the two differ the file changed under
    /// the read, and the stamp is [`Stamp::Unknown`], which never matches.
    pub stamp: Stamp,
    /// [`DepKind::Read`] only: SHA-256 of the bytes the build consumed. `None`
    /// when the read failed (or for other kinds).
    pub sha256: Option<[u8; 32]>,
    /// Wall-clock time the stamp was taken. A file whose mtime is within
    /// [`RACY_WINDOW`] of this may be rewritten without its stamp changing
    /// (same length, mtime granularity), so its stamp alone can't vouch for
    /// it — see [`Dep::is_racy`].
    pub observed_at: SystemTime,
}

/// How close an mtime may be to the observation time before the stamp is
/// "racy" (git's racy-index problem): filesystems with coarse timestamps
/// (FAT: 2s; ext3, HFS+: 1s) can record a later same-length write with the
/// same mtime. 2s covers all of them. A racy stamp costs one re-hash on the
/// next check; it never forces a rebuild by itself.
pub const RACY_WINDOW: Duration = Duration::from_secs(2);

impl Dep {
    /// Whether this read's stamp can't vouch for its contents: the file was
    /// modified within [`RACY_WINDOW`] of being read (or has an mtime in the
    /// future, e.g. clock skew). Such a dep is verified by hash instead.
    pub fn is_racy(&self) -> bool {
        match (&self.kind, &self.stamp, self.sha256) {
            (DepKind::Read, Stamp::File { mtime, .. }, Some(_)) => {
                *mtime + RACY_WINDOW > self.observed_at
            }
            _ => false,
        }
    }

    /// A read that failed on a path that *exists* (permission denied, a
    /// directory, an I/O error). Its stamp can't tell a fix apart — `chmod`
    /// touches neither mtime nor length — so [`revalidate`] always retries it.
    pub fn is_failed_read(&self) -> bool {
        self.kind == DepKind::Read && self.sha256.is_none() && self.stamp != Stamp::Missing
    }
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
    let (stamp, sha256) = match &result {
        Ok(bytes) => (settle(&path, before), Some(sha256(bytes))),
        Err(_) => (failed_stamp(&path), None),
    };
    let dep = Dep {
        path,
        kind: DepKind::Read,
        stamp,
        sha256,
        observed_at,
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
    let (stamp, sha256) = match &result {
        Ok(hash) => (settle(&path, before), Some(*hash)),
        Err(_) => (failed_stamp(&path), None),
    };
    let dep = Dep {
        path,
        kind: DepKind::Read,
        stamp,
        sha256,
        observed_at,
    };
    (dep, result)
}

/// Record an existence check.
pub fn observe_probe(path: &Path) -> Dep {
    let path = absolute(path);
    Dep {
        observed_at: SystemTime::now(),
        stamp: Stamp::now(&path),
        path,
        kind: DepKind::Probe,
        sha256: None,
    }
}

/// List `path`'s entry names, recording the access.
pub fn observe_dir(path: &Path) -> (Dep, io::Result<Vec<OsString>>) {
    let path = absolute(path);
    let observed_at = SystemTime::now();
    let result = fs::read_dir(&path).and_then(|entries| {
        entries
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<io::Result<Vec<_>>>()
    });
    let stamp = match &result {
        Ok(names) => Stamp::dir_listing(names.clone()),
        Err(_) => Stamp::now(&path),
    };
    let dep = Dep {
        path,
        kind: DepKind::Dir,
        stamp,
        sha256: None,
        observed_at,
    };
    (dep, result)
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

/// The stamp for a failed read: how the path looks now — `Missing` for
/// not-found, otherwise the existing file or directory. Deliberately *not* a
/// never-matching stamp: the failure would recur on every rebuild, and so
/// would the forced recheck. A fix that doesn't change the stamp (`chmod`) is
/// caught by [`Dep::is_failed_read`] and the watcher's metadata events
/// instead.
fn failed_stamp(path: &Path) -> Stamp {
    Stamp::now(path)
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
    /// [`TrackedFile::finish`] records the dep (and the reader records a
    /// failed read if dropped before reaching EOF). A failed open is recorded
    /// immediately.
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
                observed_at,
            }),
            Err(err) => {
                self.push(Dep {
                    stamp: failed_stamp(&path),
                    path,
                    kind: DepKind::Read,
                    sha256: None,
                    observed_at,
                });
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
        let dep = observe_probe(path);
        let found = path.is_file();
        self.push(dep);
        found
    }

    /// Whether `path` is a directory (following symlinks), recorded as a probe.
    pub fn is_dir(&self, path: &Path) -> bool {
        let dep = observe_probe(path);
        let found = path.is_dir();
        self.push(dep);
        found
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
    /// Taken by `finish`; still set on drop means the read was abandoned.
    path: Option<PathBuf>,
    before: Stamp,
    observed_at: SystemTime,
}

impl Read for TrackedFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

impl TrackedFile<'_> {
    /// Read whatever is left (discarding it), record the dep, and return the
    /// SHA-256 of the whole file.
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
            observed_at: self.observed_at,
        });
        Ok(hash)
    }
}

impl Drop for TrackedFile<'_> {
    fn drop(&mut self) {
        // Abandoned mid-read (an I/O error, or the caller bailed): what the
        // build consumed is unknown, so record a stamp that never matches.
        if let Some(path) = self.path.take() {
            self.recorder.push(Dep {
                path,
                kind: DepKind::Read,
                stamp: Stamp::Unknown,
                sha256: None,
                observed_at: self.observed_at,
            });
        }
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
///   that failed on an existing path is always retried
///   ([`Dep::is_failed_read`]); one that failed as missing is valid while the
///   path stays missing.
/// - **Probe**: valid while the path is still missing / a file / a directory.
/// - **Dir**: valid while the listing is the same.
///
/// `content_reads` counts the files re-hashed (the test-visible I/O budget).
pub fn revalidate(deps: &[Dep], content_reads: &mut usize) -> Revalidation {
    let mut restamped: Option<Vec<Dep>> = None;
    for (i, dep) in deps.iter().enumerate() {
        let fresh = match dep.kind {
            DepKind::Read => {
                if dep.is_failed_read() {
                    return Revalidation::Changed;
                }
                let now = Stamp::now(&dep.path);
                if now == dep.stamp && !dep.is_racy() {
                    None
                } else if dep.sha256.is_none() {
                    // Failed as missing (the only failed read that gets
                    // here), and no longer missing.
                    return Revalidation::Changed;
                } else {
                    *content_reads += 1;
                    let (fresh, hash) = observe_hash(&dep.path);
                    match hash {
                        Ok(hash) if Some(hash) == dep.sha256 => (fresh.stamp != dep.stamp
                            || fresh.observed_at != dep.observed_at)
                            .then_some(fresh),
                        _ => return Revalidation::Changed,
                    }
                }
            }
            DepKind::Probe => match (existence(&dep.stamp), existence(&Stamp::now(&dep.path))) {
                (Some(then), Some(now)) if then == now => None,
                _ => return Revalidation::Changed,
            },
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

/// What a probe cares about: is anything there, and is it a directory.
#[derive(PartialEq, Eq)]
enum Existence {
    Missing,
    File,
    Dir,
}

/// `None` for an [`Stamp::Unknown`] observation, which can't vouch for
/// anything (so it never compares equal).
fn existence(stamp: &Stamp) -> Option<Existence> {
    match stamp {
        Stamp::Missing => Some(Existence::Missing),
        Stamp::File { .. } => Some(Existence::File),
        Stamp::Dir { .. } | Stamp::Unreadable => Some(Existence::Dir),
        Stamp::Unknown => None,
    }
}

#[cfg(test)]
mod tests;
