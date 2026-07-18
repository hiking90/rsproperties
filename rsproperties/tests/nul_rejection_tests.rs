// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Regression tests for interior-NUL rejection.
//!
//! Rust `&str` can carry NUL bytes where bionic's C API cannot, and the
//! property area stores names/values as C strings: an accepted interior
//! NUL desyncs `namelen` from the NUL scan, producing unreachable "ghost"
//! entries and leaking area space on every retry. These pin the
//! validation added at the builder API boundary.

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
fn test_interior_nul_rejected_and_area_stays_healthy() {
    let dir = std::env::temp_dir().join(format!("rsprops_nul_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    build_property_info(&dir);

    let mut props = SystemProperties::new_area(&dir).expect("new_area");

    // A NUL in the name would target C-string storage at a different key
    // than the caller asked for.
    assert!(
        props.add("test.a\0b", "v").is_err(),
        "interior NUL in a property name must be rejected"
    );
    // A NUL in the value would silently truncate at the first NUL when
    // read back through the C-string layout.
    assert!(
        props.add("test.nulvalue", "va\0l").is_err(),
        "interior NUL in a property value must be rejected"
    );

    // The rejections must not have consumed or corrupted area state:
    // ordinary adds/reads keep working.
    props.add("test.ok", "1").unwrap();
    assert_eq!(props.get_with_result("test.ok").unwrap(), "1");
    props.set("test.ok", "2").unwrap();
    assert_eq!(props.get_with_result("test.ok").unwrap(), "2");

    // Updating an existing entry with a NUL-carrying value must fail too
    // (the update path validates independently of add).
    assert!(props.set("test.ok", "x\0y").is_err());
    assert_eq!(
        props.get_with_result("test.ok").unwrap(),
        "2",
        "failed update must not modify the stored value"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_build_trie_rejects_nul_in_context_and_type() {
    // `build_trie` writes contexts/types into the serialized C-string
    // pool; a NUL would desync the on-disk format for every consumer.
    let entries = vec![PropertyInfoEntry::new(
        "test.prop".to_string(),
        "u:object_r:bad\0ctx:s0".to_string(),
        "string",
        false,
    )
    .unwrap()];
    assert!(build_trie(&entries, "u:object_r:default:s0", "string").is_err());

    // The defaults bypass `add_to_trie` and are interned directly, so they
    // have their own gate at `build_trie`'s entry.
    let entries = vec![PropertyInfoEntry::new(
        "test.prop".to_string(),
        "u:object_r:ok:s0".to_string(),
        "string",
        false,
    )
    .unwrap()];
    assert!(build_trie(&entries, "u:object_r:def\0ault:s0", "string").is_err());
    assert!(build_trie(&entries, "u:object_r:default:s0", "str\0ing").is_err());
}
