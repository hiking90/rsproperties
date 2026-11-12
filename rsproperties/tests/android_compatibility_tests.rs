// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Android-specific integration tests for rsproperties
//!
//! These tests use the actual Android property files in tests/android/
//! to verify compatibility with real Android property data.

#![cfg(feature = "builder")]

mod common;

use rsproperties::{
    self, build_trie, load_properties_from_file, MatchKeyword, PropFileOutcome, PropertyInfoEntry,
};
use std::collections::HashMap;
use std::path::Path;

#[test]
fn test_android_property_contexts_parsing() {
    let _ = env_logger::builder().is_test(true).try_init();

    let context_files = vec![
        "tests/android/plat_property_contexts",
        "tests/android/system_ext_property_contexts",
        "tests/android/vendor_property_contexts",
    ];

    for file in context_files {
        let (property_infos, errors) =
            PropertyInfoEntry::parse_from_file(Path::new(file), MatchKeyword::Unchecked)
                .unwrap_or_else(|_| panic!("Failed to parse {file}"));

        assert!(
            !property_infos.is_empty(),
            "Should have parsed some property info from {file}"
        );

        assert!(errors.is_empty(), "{file}: {errors:?}");

        println!(
            "Parsed {} property entries from {}",
            property_infos.len(),
            file
        );

        // Verify some basic structure
        for (i, _info) in property_infos.iter().take(3).enumerate() {
            println!("Property info entry {i} parsed successfully");
        }
    }
}

#[test]
fn test_android_build_prop_parsing() {
    let _ = env_logger::builder().is_test(true).try_init();

    let build_files = vec![
        "tests/android/system_build.prop",
        "tests/android/vendor_build.prop",
        "tests/android/product_build.prop",
    ];

    // 0644 copies: a checkout under umask 002 leaves the originals
    // group-writable, and the loader skips those.
    let copies = common::TempDir::new("rsprops_compat_prop");
    for file in build_files {
        let copy = common::copy_fixture_0644(Path::new(file), copies.path());
        let mut properties = HashMap::new();
        let result = load_properties_from_file(&copy, None, &mut properties);

        match result {
            Ok(outcome) => {
                assert_eq!(outcome, PropFileOutcome::Loaded, "{file}");
                assert!(
                    !properties.is_empty(),
                    "Should have loaded properties from {file}"
                );
                println!("Loaded {} properties from {}", properties.len(), file);

                // Print a few properties as examples
                for (key, value) in properties.iter().take(3) {
                    println!("  {key}={value}");
                }
            }
            Err(e) => {
                panic!("Failed to load {file}: {e}");
            }
        }
    }
}

#[test]
fn test_property_trie_building() {
    let _ = env_logger::builder().is_test(true).try_init();

    // Parse property contexts to get property info entries
    let (property_infos, _) = PropertyInfoEntry::parse_from_file(
        Path::new("tests/android/plat_property_contexts"),
        MatchKeyword::Unchecked,
    )
    .expect("Failed to parse property contexts");

    assert!(
        !property_infos.is_empty(),
        "Should have property info entries"
    );

    // Build trie
    let trie_data = build_trie(&property_infos, "u:object_r:default_prop:s0", "string")
        .expect("Failed to build trie");

    assert!(!trie_data.is_empty(), "Trie data should not be empty");

    println!(
        "Built trie with {} bytes of data from {} property info entries",
        trie_data.len(),
        property_infos.len()
    );
}
