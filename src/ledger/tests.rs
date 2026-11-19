use std::fs::File;

use super::*;

/// An mtime safely outside the racy window.
fn long_ago() -> SystemTime {
    SystemTime::now() - Duration::from_secs(3600)
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
    assert!(matches!(deps[3].stamp, Stamp::Dir { .. }));
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
    let rec = DepRecorder::new();
    rec.read(&a).unwrap();
    let deps = rec.take();
    let mut reads = 0;

    // Untouched: a stat, no read.
    assert_eq!(revalidate(&deps, &mut reads), Revalidation::Valid);
    assert_eq!(reads, 0);

    // Touched (same bytes, new mtime): re-hashed once, restamped.
    set_mtime(&a, long_ago() + Duration::from_secs(60));
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

/// git's racy-index case: a same-length rewrite that keeps the mtime the
/// read observed. The stamp matches; the hash doesn't.
#[test]
fn racy_same_length_rewrite_is_caught_by_hash() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "red").unwrap();
    let (dep, _) = observe_read(&a);
    assert!(dep.is_racy(), "a just-written file is racy: {dep:?}");
    let Stamp::File { mtime, .. } = dep.stamp else {
        panic!()
    };

    std::fs::write(&a, "tan").unwrap();
    set_mtime(&a, mtime);
    assert!(Stamp::now(&a) == dep.stamp, "stamp can't see the rewrite");
    let mut reads = 0;
    assert_eq!(revalidate(&[dep], &mut reads), Revalidation::Changed);
}

#[test]
fn probes_compare_existence_only() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("_partial.scss");
    let probe = observe_probe(&p);
    let mut reads = 0;
    assert_eq!(
        revalidate(std::slice::from_ref(&probe), &mut reads),
        Revalidation::Valid
    );
    std::fs::write(&p, "x").unwrap();
    assert_eq!(revalidate(&[probe], &mut reads), Revalidation::Changed);

    // A probe of an existing file doesn't care about its contents.
    let probe = observe_probe(&p);
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

/// A read that failed on a file that exists (here: permission denied) is
/// retried on every revalidation: a `chmod` fix changes no stamp.
#[cfg(unix)]
#[test]
fn failed_read_of_existing_file_is_always_retried() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "x").unwrap();
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (dep, result) = observe_read(&a);
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).unwrap();
    if result.is_ok() {
        return; // running as root: permissions aren't enforced
    }
    assert!(dep.is_failed_read());
    assert!(
        matches!(dep.stamp, Stamp::File { .. }),
        "stable stamp: {dep:?}"
    );
    let mut reads = 0;
    assert_eq!(revalidate(&[dep], &mut reads), Revalidation::Changed);
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

/// A future mtime (clock skew) is racy forever, which costs a re-hash per
/// check but never reports a change while the contents are the same.
#[test]
fn future_mtime_is_racy_but_stable() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    std::fs::write(&a, "x").unwrap();
    set_mtime(&a, SystemTime::now() + Duration::from_secs(3600));
    let (dep, _) = observe_read(&a);
    assert!(dep.is_racy());
    let mut deps = vec![dep];
    let mut reads = 0;
    for _ in 0..3 {
        match revalidate(&deps, &mut reads) {
            Revalidation::Valid => {}
            Revalidation::Restamped(fresh) => deps = fresh,
            Revalidation::Changed => panic!("unchanged file reported as changed"),
        }
    }
}
