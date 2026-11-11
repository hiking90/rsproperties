// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Regression tests for writer re-initialisation over an existing directory
//! and for the writer lock.
//!
//! `PropertyAreaMap::new_rw` creates area files with `O_EXCL` and mode
//! `0444` (matching bionic). AOSP gets away with that because /dev is a
//! fresh tmpfs on every boot; for an arbitrary properties dir a leftover
//! file from a previous service instance used to make every second
//! `SystemProperties::new_area` fail with EEXIST — and the 0444 mode meant
//! the file couldn't be reopened read-write either. `new_rw` builds each
//! area under a temp name and renames it into place.

#![cfg(all(feature = "builder", not(target_os = "android")))]

mod common;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use rsproperties::{build_trie, AreaWriterLock, Error, PropertyInfoEntry, SystemProperties};

fn trie_for(dir: &Path, contexts: &[u8]) -> Vec<u8> {
    std::fs::create_dir_all(dir).unwrap();

    let contexts_path = dir.join("property_contexts");
    File::create(&contexts_path)
        .unwrap()
        .write_all(contexts)
        .unwrap();

    let (entries, errors) = PropertyInfoEntry::parse_from_file(&contexts_path, false).unwrap();
    assert!(errors.is_empty(), "parse errors: {errors:?}");

    build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap()
}

fn build_property_info(dir: &Path) {
    let data = trie_for(dir, b"test. u:object_r:test_prop:s0 prefix string\n");
    // Mode 0444 whatever the umask; `File::create` under umask 002 makes
    // a group-writable file that readers refuse.
    AreaWriterLock::acquire(dir)
        .unwrap()
        .publish_property_info(&data)
        .unwrap();
}

fn leftover_property_info_temps(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(".property_info.tmp-"))
        .collect()
}

/// Inode of every entry in `dir` except dot-files (`.writer_lock` and
/// writer temp files), by name.
fn inodes(dir: &Path) -> BTreeMap<String, u64> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            (!name.starts_with('.')).then(|| (name, e.metadata().unwrap().ino()))
        })
        .collect()
}

#[test]
fn test_new_area_restart_over_existing_dir() {
    let guard = common::TempDir::new("rsprops_restart");
    let dir = guard.path();
    build_property_info(dir);

    {
        let mut props = SystemProperties::new_area(dir).expect("first new_area");
        props.add("test.restart", "1").unwrap();
        assert_eq!(props.get("test.restart").unwrap(), "1");
    }

    // Simulates a service restart: the dir still holds the context area
    // files and properties_serial from the first instance. This used to
    // fail with EEXIST.
    let mut props = SystemProperties::new_area(dir).expect("second new_area over existing dir");

    // The rebuilt area starts fresh — the old entry must be gone, and a
    // new add must land in the new mapping.
    assert!(props.find("test.restart").unwrap().is_none());
    props.add("test.restart", "2").unwrap();
    assert_eq!(props.get("test.restart").unwrap(), "2");
}

/// The second writer fails on the lock before it replaces any file.
///
/// The winner's own reads cannot show a replaced file: it keeps using the
/// inodes it mapped. So the test compares the inode of every non-dot entry
/// before and after the loser's attempt. That includes `property_info`; a
/// `new_area` that published it would fail here too, which is intended.
#[test]
fn test_concurrent_writer_rejected_by_lock() {
    let guard = common::TempDir::new("rsprops_wlock");
    let dir = guard.path();
    build_property_info(dir);

    let first = SystemProperties::new_area(dir).expect("first writer");
    let before = inodes(dir);

    // While the first writer is alive, a second writer must fail fast on
    // the `.writer_lock` flock — *before* replacing any of the area files
    // the first writer owns.
    let second = SystemProperties::new_area(dir);
    assert!(
        matches!(second, Err(Error::Lock(_))),
        "second concurrent writer must fail with Error::Lock: {:?}",
        second.as_ref().err()
    );
    assert_eq!(
        inodes(dir),
        before,
        "the loser replaced a file the winner owns"
    );

    // The first instance keeps working.
    drop(second);
    let mut first = first;
    first.add("test.lock", "alive").unwrap();
    assert_eq!(first.get("test.lock").unwrap(), "alive");

    // Releasing the first writer releases the flock; a new writer succeeds.
    drop(first);
    SystemProperties::new_area(dir).expect("writer after lock release");
}

/// A context named like a writer temp file or dot-file must not become an
/// area file: readers would open it, and the writer's temp files and
/// `.writer_lock` live under such names.
#[test]
fn leading_dot_context_is_skipped() {
    let guard = common::TempDir::new("rsprops_dotctx");
    let dir = guard.path();
    let data = trie_for(
        dir,
        b"test. u:object_r:test_prop:s0 prefix string\nhidden. .hidden prefix string\n",
    );
    let lock = AreaWriterLock::acquire(dir).unwrap();
    lock.publish_property_info(&data).unwrap();
    let mut props = SystemProperties::new_area_locked(lock).expect("new_area_locked");

    assert!(
        !dir.join(".hidden").exists(),
        "a context starting with '.' must be skipped"
    );
    // Other contexts still work.
    props.add("test.dot", "1").unwrap();
    assert_eq!(props.get("test.dot").unwrap(), "1");

    // `add` looks up the area before it validates the name; the error for
    // the skipped context must print the name escaped, so a name with a
    // newline cannot forge a log line where the error is logged.
    let err = props
        .add("hidden.x\nFAKE: forged", "1")
        .expect_err("a skipped context has no area");
    let shown = err.to_string();
    assert!(
        shown.contains(r#""hidden.x\nFAKE: forged""#) && !shown.contains('\n'),
        "name must be Debug-escaped: {shown:?}"
    );
    // `set` (through `find`) and the read path reach the skipped context
    // through `prop_area_for_name`, not the `add` lookup above.
    let set_err = props
        .set("hidden.x\nFAKE: forged", "1")
        .expect_err("a skipped context has no area");
    let get_err = props
        .get("hidden.x\nFAKE: forged")
        .expect_err("a skipped context has no area");
    for err in [set_err, get_err] {
        let shown = err.to_string();
        assert!(
            shown.contains(r#""hidden.x\nFAKE: forged""#) && !shown.contains('\n'),
            "name must be Debug-escaped: {shown:?}"
        );
    }

    drop(props);
}

/// The second writer fails on the lock before it can publish, so the first
/// writer's `property_info` stays.
#[test]
fn locked_loser_leaves_property_info() {
    let guard = common::TempDir::new("rsprops_loser");
    let dir = guard.path();
    let trie_a = trie_for(dir, b"test. u:object_r:test_prop:s0 prefix string\n");
    let trie_b = trie_for(dir, b"other. u:object_r:other_prop:s0 prefix string\n");
    assert_ne!(trie_a, trie_b);

    let first = AreaWriterLock::acquire(dir).unwrap();
    first.publish_property_info(&trie_a).unwrap();

    let second = AreaWriterLock::acquire(dir);
    if let Ok(second) = &second {
        let _ = second.publish_property_info(&trie_b);
    }
    assert!(
        matches!(second, Err(Error::Lock(_))),
        "second writer must fail with Error::Lock: {second:?}"
    );
    assert_eq!(std::fs::read(dir.join("property_info")).unwrap(), trie_a);

    drop(first);
}

/// `new_area` takes the lock before it reads `property_info`: with the lock
/// held elsewhere it fails with `Error::Lock` even when `property_info`
/// would not load (0.6.0 read it first and returned the load error).
#[test]
fn new_area_locks_before_reading_property_info() {
    let guard = common::TempDir::new("rsprops_lockfirst");
    let dir = guard.path();
    std::fs::create_dir_all(dir).unwrap();
    let held = AreaWriterLock::acquire(dir).unwrap();
    // Present but not a trie (0444, so no owner-write warning).
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o444)
        .open(dir.join("property_info"))
        .unwrap()
        .write_all(b"not a trie")
        .unwrap();
    // `SystemProperties` has no `Debug`, hence `.err()`.
    let err = SystemProperties::new_area(dir)
        .err()
        .expect("the lock is held by another instance");
    assert!(matches!(err, Error::Lock(_)), "{err:?}");
    drop(held);
}

fn is_not_found(err: &Error) -> bool {
    matches!(err, Error::Context { source, .. }
        if matches!(&**source, Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound))
}

/// Without `property_info`, `new_area` returns the loader's error and
/// creates nothing: neither a missing dir nor `.writer_lock` in an existing
/// one.
#[test]
fn new_area_without_property_info_creates_nothing() {
    let guard = common::TempDir::new("rsprops_noinfo"); // not created

    let missing = guard.path();
    let err = SystemProperties::new_area(missing)
        .err()
        .expect("no property_info");
    assert!(is_not_found(&err), "{err:?}");
    assert!(!missing.exists(), "{missing:?} must not be created");

    std::fs::create_dir(missing).unwrap();
    let err = SystemProperties::new_area(missing)
        .err()
        .expect("no property_info");
    assert!(is_not_found(&err), "{err:?}");
    assert!(
        !missing.join(".writer_lock").exists(),
        ".writer_lock must not be created"
    );
}

/// A dangling symlink at `property_info` counts as present (`lstat`
/// finds it): `new_area` takes the lock, leaves `.writer_lock`, and then
/// fails the `O_NOFOLLOW` load (the CHANGELOG Migration notes say so).
#[test]
fn new_area_dangling_property_info_takes_lock() {
    let guard = common::TempDir::new("rsprops_dangling");
    let dir = guard.path();
    std::fs::create_dir_all(dir).unwrap();
    std::os::unix::fs::symlink(dir.join("missing"), dir.join("property_info")).unwrap();

    let err = SystemProperties::new_area(dir)
        .err()
        .expect("a dangling property_info does not load");
    assert!(
        !matches!(err, Error::Lock(_)) && !is_not_found(&err),
        "must be the O_NOFOLLOW load error: {err:?}"
    );
    assert!(dir.join(".writer_lock").exists());
}

/// Bytes the loader rejects are not renamed into place.
#[test]
fn publish_rejects_invalid_trie() {
    let guard = common::TempDir::new("rsprops_badtrie");
    let dir = guard.path();
    let trie = trie_for(dir, b"test. u:object_r:test_prop:s0 prefix string\n");

    let lock = AreaWriterLock::acquire(dir).unwrap();
    lock.publish_property_info(&trie).unwrap();
    let info = dir.join("property_info");
    let ino = std::fs::metadata(&info).unwrap().ino();
    assert_eq!(std::fs::metadata(&info).unwrap().mode() & 0o777, 0o444);

    // Header claims a size that does not match the file.
    let mut bad = trie.clone();
    bad.truncate(bad.len() - 4);
    let err = lock.publish_property_info(&bad).unwrap_err();
    assert!(matches!(err, Error::Context { .. }), "{err:?}");

    assert_eq!(std::fs::metadata(&info).unwrap().ino(), ino);
    assert_eq!(std::fs::read(&info).unwrap(), trie);
    assert!(leftover_property_info_temps(dir).is_empty());

    // Publishing again works and replaces the inode.
    lock.publish_property_info(&trie).unwrap();
    assert_ne!(std::fs::metadata(&info).unwrap().ino(), ino);

    drop(lock);
}
