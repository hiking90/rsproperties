// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `SystemProperties::open`: a read-only instance on a directory the
//! caller names, independent of the process-wide reader and of `init`.

#![cfg(all(feature = "builder", not(target_os = "android")))]

mod common;

use std::path::Path;

use common::TempDir;
use rsproperties::{
    build_trie, AreaWriterLock, Error, MatchKeyword, PropertyInfoEntry, SystemProperties,
};

/// Writer on `dir` with one context, holding `test.x = value`.
fn writer(dir: &Path, value: &str) -> SystemProperties {
    std::fs::create_dir_all(dir).unwrap();
    let contexts = dir.join("property_contexts");
    std::fs::write(&contexts, "test. u:object_r:test_prop:s0 prefix string\n").unwrap();
    let (entries, errors) =
        PropertyInfoEntry::parse_from_file(&contexts, MatchKeyword::Unchecked).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let trie = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();

    let lock = AreaWriterLock::acquire(dir).unwrap();
    lock.publish_property_info(&trie).unwrap();
    let mut props = SystemProperties::new_area_locked(lock).unwrap();
    props.add("test.x", value).unwrap();
    props
}

/// Two directories are read in one process, and neither read goes through
/// `init` or the process-wide reader.
#[test]
fn reads_two_directories_in_one_process() {
    let dir_a = TempDir::new("rsprops_open_a");
    let dir_b = TempDir::new("rsprops_open_b");
    let mut writer_a = writer(dir_a.path(), "a");
    let _writer_b = writer(dir_b.path(), "b");

    let reader_a = SystemProperties::open(dir_a.path()).unwrap();
    let reader_b = SystemProperties::open(dir_b.path()).unwrap();
    assert_eq!(reader_a.get("test.x").unwrap(), "a");
    assert_eq!(reader_b.get("test.x").unwrap(), "b");

    // The reader maps the writer's files: a later update is visible
    // without opening again.
    writer_a.set("test.x", "a2").unwrap();
    assert_eq!(reader_a.get("test.x").unwrap(), "a2");
    assert_eq!(reader_b.get("test.x").unwrap(), "b");
}

/// A directory without a store fails with an OS `NotFound` at the root of
/// the error, so a caller can tell "no service yet" from a corrupt store.
#[test]
fn missing_store_is_io_not_found() {
    let dir = TempDir::new("rsprops_open_missing");
    let err = SystemProperties::open(dir.path()).unwrap_err();
    assert!(
        matches!(err.root_cause(), Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound),
        "{err:?}"
    );

    std::fs::create_dir_all(dir.path()).unwrap();
    let err = SystemProperties::open(dir.path()).unwrap_err();
    assert!(
        matches!(err.root_cause(), Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound),
        "{err:?}"
    );
}

/// An index from an opened reader is bound to that reader, like any other
/// instance's.
#[test]
fn index_is_bound_to_the_opened_instance() {
    let dir = TempDir::new("rsprops_open_index");
    let _writer = writer(dir.path(), "1");

    let first = SystemProperties::open(dir.path()).unwrap();
    let second = SystemProperties::open(dir.path()).unwrap();
    let index = first.find("test.x").unwrap().expect("property exists");
    assert!(first.serial(index).is_ok());
    let err = second.serial(index).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err:?}");
}
