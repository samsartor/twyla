use std::fs::File;

use super::*;

/// An mtime safely outside the racy window.
fn long_ago() -> SystemTime {
    SystemTime::now() - Duration::from_secs(3600)
}

/// Wait until a file changed just now is clear of the (fine) racy window,
/// so its stamp alone vouches for it. Its ctime can't be set back, so tests
/// asserting "no content reads" wait instead.
fn settle_ctime() {
    std::thread::sleep(FINE_RACY_WINDOW + Duration::from_millis(10));
}

fn set_mtime(path: &Path, mtime: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

#[test]
fn read_records_stamp_and_hash_and_failures() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "hello").unwrap();
    let rec = DepRecorder::new();

    assert_eq!(rec.read(&a).unwrap(), b"hello");
    let missing = dir.path().join("nope.txt");
    let err = rec.read(&missing).unwrap_err();
    assert!(err.to_string().contains("nope.txt"), "{err}");
    assert!(!rec.is_file(&missing));
    assert!(rec.is_dir(dir.path()));
    assert_eq!(rec.content_reads(), 2);

    let deps = rec.take();
    assert_eq!(rec.content_reads(), 0);
    assert_eq!(deps.len(), 4);
    assert_eq!(deps[0].kind, DepKind::Read);
    assert_eq!(deps[0].sha256, Some(sha256(b"hello")));
    assert!(matches!(deps[0].stamp, Stamp::File { len: 5, .. }));
    assert_eq!(deps[1].stamp, Stamp::Missing);
    assert_eq!(deps[1].sha256, None);
    assert_eq!(
        (deps[2].kind, &deps[2].stamp),
        (DepKind::Probe, &Stamp::Missing)
    );
    assert_eq!(deps[3].stamp, Stamp::Exists { dir: true });
}

#[test]
fn open_streams_and_records_on_finish_or_drop() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.bin");
    std::fs::write(&a, vec![7u8; 100_000]).unwrap();
    let rec = DepRecorder::new();

    let hash = rec.open(&a).unwrap().finish().unwrap();
    assert_eq!(hash, sha256(&vec![7u8; 100_000]));
    // Abandoned before EOF: recorded, but as a never-matching stamp.
    drop(rec.open(&a).unwrap());
    let deps = rec.take();
    assert_eq!(deps[0].sha256, Some(hash));
    assert_eq!(deps[1].stamp, Stamp::Unknown);
}

#[test]
fn revalidate_by_stamp_then_hash() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    set_mtime(&a, long_ago());
    settle_ctime();
    let rec = DepRecorder::new();
    rec.read(&a).unwrap();
    let deps = rec.take();
    let mut reads = 0;

    // Untouched: a stat, no read.
    assert_eq!(revalidate(&deps, &mut reads), Revalidation::Valid);
    assert_eq!(reads, 0);

    // Touched (same bytes, new mtime): re-hashed once, restamped.
    set_mtime(&a, long_ago() + Duration::from_secs(60));
    settle_ctime();
    let Revalidation::Restamped(fresh) = revalidate(&deps, &mut reads) else {
        panic!("expected a restamp");
    };
    assert_eq!(reads, 1);
    assert_eq!(revalidate(&fresh, &mut reads), Revalidation::Valid);
    assert_eq!(reads, 1);

    // Edited: changed.
    std::fs::write(&a, "v2").unwrap();
    assert_eq!(revalidate(&fresh, &mut reads), Revalidation::Changed);

    // Deleted: changed.
    std::fs::remove_file(&a).unwrap();
    assert_eq!(revalidate(&fresh, &mut reads), Revalidation::Changed);
}

/// A same-length in-place rewrite that restores the mtime is still a stamp
/// change on unix: the ctime (which nothing can set back) moved.
#[cfg(unix)]
#[test]
fn rewrite_with_preserved_mtime_changes_the_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "red").unwrap();
    let before = Stamp::now(&a);
    let Stamp::File { mtime, .. } = before else {
        panic!()
    };
    std::thread::sleep(FINE_RACY_WINDOW);
    std::fs::write(&a, "tan").unwrap();
    set_mtime(&a, mtime);
    assert_ne!(Stamp::now(&a), before);
}

/// git's racy-index case: a dep observed within the timestamp granularity
/// of the file's last change can't be vouched for by its stamp, so it's
/// verified by hash (here the recorded hash is of different bytes, as if a
/// same-tick rewrite had left the stamp alone). Once the observation is
/// clear of that window, the stamp alone is trusted.
#[test]
fn racy_dep_is_verified_by_hash() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "tan").unwrap();
    let (mut dep, _) = observe_read(&a);
    let Stamp::File { mtime, ctime, .. } = dep.stamp else {
        panic!()
    };
    dep.sha256 = Some(sha256(b"red"));
    dep.observed_at = ctime.unwrap_or(mtime);
    assert!(dep.is_racy());
    let mut reads = 0;
    assert_eq!(
        revalidate(std::slice::from_ref(&dep), &mut reads),
        Revalidation::Changed
    );
    assert_eq!(reads, 1);

    dep.observed_at = ctime.unwrap_or(mtime) + RACY_WINDOW;
    assert!(!dep.is_racy());
    assert_eq!(revalidate(&[dep], &mut reads), Revalidation::Valid);
    assert_eq!(reads, 1);
}

#[test]
fn probes_compare_existence_only() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("_partial.scss");
    let probe = observe_probe(&p).0;
    let mut reads = 0;
    assert_eq!(
        revalidate(std::slice::from_ref(&probe), &mut reads),
        Revalidation::Valid
    );
    std::fs::write(&p, "x").unwrap();
    assert_eq!(revalidate(&[probe], &mut reads), Revalidation::Changed);

    // A probe of an existing file doesn't care about its contents.
    let probe = observe_probe(&p).0;
    std::fs::write(&p, "longer").unwrap();
    assert_eq!(revalidate(&[probe], &mut reads), Revalidation::Valid);
    assert_eq!(reads, 0);
}

#[test]
fn dir_listing_changes_invalidate() {
    let dir = tempfile::tempdir().unwrap();
    let rec = DepRecorder::new();
    assert!(rec.list_dir(dir.path()).unwrap().is_empty());
    let deps = rec.take();
    let mut reads = 0;
    assert_eq!(revalidate(&deps, &mut reads), Revalidation::Valid);
    std::fs::write(dir.path().join("new"), "").unwrap();
    assert_eq!(revalidate(&deps, &mut reads), Revalidation::Changed);
}

/// A read that failed on a file that exists (here: permission denied) is a
/// stable observation — valid, without a retry, while nothing changes — and
/// the `chmod` that fixes it is a change (permission bits are in the stamp).
#[cfg(unix)]
#[test]
fn failed_read_is_stable_until_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "x").unwrap();
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (dep, result) = observe_read(&a);
    if result.is_ok() {
        return; // running as root: permissions aren't enforced
    }
    assert_eq!(dep.error, Some(io::ErrorKind::PermissionDenied));
    assert!(matches!(dep.stamp, Stamp::File { .. }), "{dep:?}");
    let mut reads = 0;
    for _ in 0..2 {
        assert_eq!(
            revalidate(std::slice::from_ref(&dep), &mut reads),
            Revalidation::Valid
        );
    }
    assert_eq!(reads, 0);
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(revalidate(&[dep], &mut reads), Revalidation::Changed);
}

/// Finding: streaming a directory opens fine but fails on the first read
/// (`EISDIR`). That's a *failed* read, recorded as a stable observation — not
/// an abandoned one (`Unknown`), which would evict and rebuild forever.
#[cfg(unix)]
#[test]
fn stream_read_error_is_a_stable_failure() {
    let dir = tempfile::tempdir().unwrap();
    let rec = DepRecorder::new();
    assert!(rec.open(dir.path()).unwrap().finish().is_err());
    let deps = rec.take();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].error, Some(io::ErrorKind::IsADirectory));
    assert!(matches!(deps[0].stamp, Stamp::Dir { .. }), "{deps:?}");
    let mut reads = 0;
    assert_eq!(revalidate(&deps, &mut reads), Revalidation::Valid);
    assert_eq!(revalidate(&deps, &mut reads), Revalidation::Valid);
}

/// A failed read of a *missing* file is valid while it stays missing.
#[test]
fn failed_read_of_missing_file_waits_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let (dep, _) = observe_read(&a);
    let mut reads = 0;
    assert_eq!(
        revalidate(std::slice::from_ref(&dep), &mut reads),
        Revalidation::Valid
    );
    std::fs::write(&a, "x").unwrap();
    assert_eq!(revalidate(&[dep], &mut reads), Revalidation::Changed);
}

/// The racy-read guard: a stamp that changed across the read never matches.
#[test]
fn settle_rejects_a_stamp_that_moved() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let before = Stamp::now(&a);
    assert_eq!(settle(&a, before.clone()), before);
    std::fs::write(&a, "v22").unwrap();
    assert_eq!(settle(&a, before), Stamp::Unknown);
}

/// A future mtime (clock skew, `touch -d`) doesn't make a file racy on
/// unix: raciness keys off the ctime, which the kernel sets. So once the
/// ctime settles the file is trusted by stamp — no re-hash per check.
#[cfg(unix)]
#[test]
fn future_mtime_is_not_racy() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "x").unwrap();
    set_mtime(&a, SystemTime::now() + Duration::from_secs(3600));
    std::thread::sleep(FINE_RACY_WINDOW * 2);
    let (dep, _) = observe_read(&a);
    assert!(!dep.is_racy(), "{dep:?}");
    let mut reads = 0;
    for _ in 0..3 {
        assert_eq!(
            revalidate(std::slice::from_ref(&dep), &mut reads),
            Revalidation::Valid
        );
    }
    assert_eq!(reads, 0);
}

/// Probes record existence only, from one `metadata` call: a probed
/// directory is never listed, so adding to it doesn't invalidate the probe.
#[test]
fn dir_probe_does_not_list() {
    let dir = tempfile::tempdir().unwrap();
    let rec = DepRecorder::new();
    assert!(rec.is_dir(dir.path()));
    assert!(!rec.is_file(dir.path()));
    let deps = rec.take();
    assert_eq!(deps[0].stamp, Stamp::Exists { dir: true });
    std::fs::write(dir.path().join("new"), "").unwrap();
    let mut reads = 0;
    assert_eq!(revalidate(&deps, &mut reads), Revalidation::Valid);
}
