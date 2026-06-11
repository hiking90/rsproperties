// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Regression test for writer re-initialisation over an existing directory.
//!
//! `PropertyAreaMap::new_rw` creates area files with `O_EXCL` and mode
//! `0444` (matching bionic). AOSP gets away with that because /dev is a
//! fresh tmpfs on every boot; for an arbitrary properties dir a leftover
//! file from a previous service instance used to make every second
//! `SystemProperties::new_area` fail with EEXIST — and the 0444 mode meant
//! the file couldn't be reopened read-write either. `new_rw` now removes
//! stale files before the exclusive create.

#![cfg(all(feature = "builder", not(target_os = "android")))]

use std::fs::File;
use std::io::Write;
use std::path::Path;

use rsproperties::{build_trie, PropertyInfoEntry, SystemProperties};

fn build_property_info(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();

    let contexts_path = dir.join("property_contexts");
    File::create(&contexts_path)
        .unwrap()
        .write_all(b"test. u:object_r:test_prop:s0 prefix string\n")
        .unwrap();

    let (entries, errors) = PropertyInfoEntry::parse_from_file(&contexts_path, false).unwrap();
    assert!(errors.is_empty(), "parse errors: {errors:?}");

    let data = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();
    File::create(dir.join("property_info"))
        .unwrap()
        .write_all(&data)
        .unwrap();
}

#[test]
fn test_new_area_restart_over_existing_dir() {
    let dir = std::env::temp_dir().join(format!("rsprops_restart_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    build_property_info(&dir);

    {
        let mut props = SystemProperties::new_area(&dir).expect("first new_area");
        props.add("test.restart", "1").unwrap();
        assert_eq!(props.get_with_result("test.restart").unwrap(), "1");
    }

    // Simulates a service restart: the dir still holds the context area
    // files and properties_serial from the first instance. This used to
    // fail with EEXIST.
    let mut props = SystemProperties::new_area(&dir).expect("second new_area over existing dir");

    // The rebuilt area starts fresh — the old entry must be gone, and a
    // new add must land in the new mapping.
    assert!(props.find("test.restart").unwrap().is_none());
    props.add("test.restart", "2").unwrap();
    assert_eq!(props.get_with_result("test.restart").unwrap(), "2");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_concurrent_writer_rejected_by_lock() {
    let dir = std::env::temp_dir().join(format!("rsprops_wlock_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    build_property_info(&dir);

    let first = SystemProperties::new_area(&dir).expect("first writer");

    // While the first writer is alive, a second writer must fail fast on
    // the `.writer_lock` flock — *before* unlinking any of the area files
    // the first writer owns.
    let second = SystemProperties::new_area(&dir);
    assert!(
        second.is_err(),
        "second concurrent writer must be rejected by the writer lock"
    );

    // The loser must not have destroyed the winner's files: the first
    // instance keeps working.
    drop(second);
    let mut first = first;
    first.add("test.lock", "alive").unwrap();
    assert_eq!(first.get_with_result("test.lock").unwrap(), "alive");

    // Releasing the first writer releases the flock; a new writer succeeds.
    drop(first);
    SystemProperties::new_area(&dir).expect("writer after lock release");

    let _ = std::fs::remove_dir_all(&dir);
}
