// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A `PropertyIndex` is valid only on the `SystemProperties` instance whose
//! `find` returned it. An index holds area offsets; on another instance
//! (another directory, the same directory after a writer restart, or the
//! global reader of the same directory) those offsets address an
//! unrelated layout, and `update` could overwrite trie bytes.

#![cfg(all(feature = "builder", not(target_os = "android")))]

use std::path::{Path, PathBuf};

use std::time::Duration;

use rsproperties::{
    build_trie, AreaWriterLock, Error, PropertyConfig, PropertyIndex, PropertyInfoEntry,
    SystemProperties,
};

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rsprops_index_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writer on `dir` with one context, holding `test.x = 1`. Same layout for
/// every directory, so indexes from different instances are
/// interchangeable byte for byte.
fn writer(dir: &Path) -> SystemProperties {
    std::fs::create_dir_all(dir).unwrap();
    let contexts = dir.join("property_contexts");
    std::fs::write(&contexts, "test. u:object_r:test_prop:s0 prefix string\n").unwrap();
    let (entries, errors) = PropertyInfoEntry::parse_from_file(&contexts, false).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let trie = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();

    let lock = AreaWriterLock::acquire(dir).unwrap();
    lock.publish_property_info(&trie).unwrap();
    let mut props = SystemProperties::new_area_locked(lock).unwrap();
    props.add("test.x", "1").unwrap();
    props
}

/// `serial`/`wait` with an index from another instance fail with
/// `InvalidArgument`. `wait` gets an `old_serial` that differs from the
/// current one, so a missing instance check would return `Ok(Some)` at
/// once instead.
fn assert_serial_and_wait_reject(props: &SystemProperties, foreign: &PropertyIndex) {
    let err = props.serial(foreign).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err:?}");
    let got = props.wait(
        Some(foreign),
        Some(u32::MAX),
        Some(Duration::from_millis(1)),
    );
    assert!(matches!(got, Err(Error::InvalidArgument(_))), "{got:?}");
}

#[test]
fn foreign_index_from_other_directory_is_rejected() {
    let dir_a = Dir::new("a");
    let dir_b = Dir::new("b");
    let a = writer(&dir_a.0);
    let mut b = writer(&dir_b.0);

    let index_a = a.find("test.x").unwrap().expect("test.x in a");
    let err = b.update(&index_a, "2").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err:?}");
    assert_eq!(b.get("test.x").unwrap(), "1");
    assert_serial_and_wait_reject(&b, &index_a);

    // The instance that returned the index still accepts it.
    let index_b = b.find("test.x").unwrap().unwrap();
    b.update(&index_b, "3").unwrap();
    assert_eq!(b.get("test.x").unwrap(), "3");
    assert!(b.serial(&index_b).is_ok());
}

#[test]
fn index_from_before_writer_restart_is_rejected() {
    let dir = Dir::new("restart");
    let old = writer(&dir.0);
    let old_index = old.find("test.x").unwrap().unwrap();
    drop(old);

    let mut new = writer(&dir.0);
    let err = new.update(&old_index, "2").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err:?}");
    assert_eq!(new.get("test.x").unwrap(), "1");
    assert_serial_and_wait_reject(&new, &old_index);
}

/// The only test in this binary that initializes the global reader.
#[test]
fn global_reader_index_is_rejected_by_writer() {
    let dir = Dir::new("global");
    let mut w = writer(&dir.0);
    rsproperties::init(PropertyConfig::new().properties_dir(&dir.0)).unwrap();
    let reader = rsproperties::system_properties();

    let reader_index = reader.find("test.x").unwrap().unwrap();
    let err = w.update(&reader_index, "2").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err:?}");
    assert_eq!(reader.get("test.x").unwrap(), "1");
    assert_serial_and_wait_reject(&w, &reader_index);

    // And the other way round.
    let writer_index = w.find("test.x").unwrap().unwrap();
    assert_serial_and_wait_reject(reader, &writer_index);
}
