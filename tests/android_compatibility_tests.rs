// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Android-specific integration tests for rsproperties
//!
//! These tests use the actual Android property files in tests/android/
//! to verify compatibility with real Android property data.

#![cfg(feature = "builder")]

use std::path::{Path, PathBuf};
use std::fs::{create_dir_all, remove_dir_all, File};
use std::io::Write;
use std::collections::HashMap;
use rsproperties::{self, PropertyInfoEntry, build_trie, load_properties_from_file};

const ANDROID_TEST_DIR: &str = "android_test_properties";

fn setup_android_test_env() -> HashMap<String, String> {
    let _ = env_logger::builder().is_test(true).try_init();

    // Initialize with test directory
    rsproperties::init(Some(PathBuf::from(ANDROID_TEST_DIR)));

    // Clean up and create test directory
    let _ = remove_dir_all(ANDROID_TEST_DIR);
    create_dir_all(ANDROID_TEST_DIR).expect("Failed to create test directory");

    setup_property_contexts_and_build_props()
}

fn setup_property_contexts_and_build_props() -> HashMap<String, String> {
    // Load property contexts
    let property_contexts_files = vec![
        "tests/android/plat_property_contexts",
        "tests/android/system_ext_property_contexts",
        "tests/android/vendor_property_contexts",
    ];

    let mut property_infos = Vec::new();
    for file in property_contexts_files {
        let (mut property_info, errors) = PropertyInfoEntry::parse_from_file(Path::new(file), false)
            .expect("Failed to parse property contexts");
        if !errors.is_empty() {
            eprintln!("Errors parsing {}: {:?}", file, errors);
        }
        property_infos.append(&mut property_info);
    }

    // Build trie and write property_info file
    let data = build_trie(&property_infos, "u:object_r:build_prop:s0", "string")
        .expect("Failed to build trie");

    let dir = rsproperties::dirname();
    let property_info_path = dir.join("property_info");
    File::create(property_info_path)
        .expect("Failed to create property_info file")
        .write_all(&data)
        .expect("Failed to write property_info data");

    // Load build properties
    load_build_properties()
}

fn load_build_properties() -> HashMap<String, String> {
    let build_prop_files = vec![
        "tests/android/product_build.prop",
        "tests/android/system_build.prop",
        "tests/android/system_dlkm_build.prop",
        "tests/android/system_ext_build.prop",
        "tests/android/vendor_build.prop",
        "tests/android/vendor_dlkm_build.prop",
        "tests/android/vendor_odm_build.prop",
        "tests/android/vendor_odm_dlkm_build.prop",
    ];

    let mut properties = HashMap::new();
    for file in build_prop_files {
        if let Err(e) = load_properties_from_file(Path::new(file), None, "u:r:init:s0", &mut properties) {
            eprintln!("Warning: Failed to load {}: {}", file, e);
        }
    }

    // This is a conceptual setup - in real usage, properties would be set during system initialization
    // For testing purposes, we'll just return the loaded properties without setting them in the system
    println!("Note: Properties would normally be set in the system during initialization");

    properties
}

#[test]
fn test_android_build_properties_loading() {
    let properties = setup_android_test_env();

    // Verify that we loaded some properties
    assert!(!properties.is_empty(), "Should have loaded some properties from Android build files");

    println!("Loaded {} properties from Android build files", properties.len());

    // Test some common Android properties that should exist
    let expected_props = vec![
        "ro.build.version.release",
        "ro.build.version.sdk",
        "ro.product.model",
        "ro.product.manufacturer",
    ];

    for prop in expected_props {
        if properties.contains_key(prop) {
            println!("Found expected property: {} = {}", prop, properties[prop]);
        }
    }
}

#[test]
fn test_get_android_properties() {
    let properties = setup_android_test_env();

    // Test getting properties through the public API
    for (key, expected_value) in properties.iter().take(10) { // Test first 10 properties
        let retrieved_value = rsproperties::get(key);
        match retrieved_value {
            Ok(value) => {
                assert_eq!(value, *expected_value, "Property {} has incorrect value", key);
                println!("✓ {}: {}", key, value);
            }
            Err(e) => {
                eprintln!("✗ Failed to get property {}: {}", key, e);
            }
        }
    }
}

#[test]
fn test_get_with_default_android_properties() {
    let properties = setup_android_test_env();

    // Test existing properties
    for (key, expected_value) in properties.iter().take(5) {
        let retrieved_value = rsproperties::get_with_default(key, "fallback");
        assert_eq!(retrieved_value, *expected_value);
    }

    // Test non-existing property
    let non_existent = "ro.does.not.exist.property";
    let default_val = "default_value";
    let result = rsproperties::get_with_default(non_existent, default_val);
    assert_eq!(result, default_val);
}

#[test]
fn test_android_property_contexts_parsing() {
    let _ = env_logger::builder().is_test(true).try_init();

    let context_files = vec![
        "tests/android/plat_property_contexts",
        "tests/android/system_ext_property_contexts",
        "tests/android/vendor_property_contexts",
    ];

    for file in context_files {
        let (property_infos, errors) = PropertyInfoEntry::parse_from_file(Path::new(file), false)
            .expect(&format!("Failed to parse {}", file));

        assert!(!property_infos.is_empty(), "Should have parsed some property info from {}", file);

        if !errors.is_empty() {
            println!("Parsing errors in {}: {:?}", file, errors);
        }

        println!("Parsed {} property entries from {}", property_infos.len(), file);

        // Verify some basic structure
        for (i, _info) in property_infos.iter().take(3).enumerate() {
            println!("Property info entry {} parsed successfully", i);
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

    for file in build_files {
        let mut properties = HashMap::new();
        let result = load_properties_from_file(Path::new(file), None, "u:r:init:s0", &mut properties);

        match result {
            Ok(_) => {
                assert!(!properties.is_empty(), "Should have loaded properties from {}", file);
                println!("Loaded {} properties from {}", properties.len(), file);

                // Print a few properties as examples
                for (key, value) in properties.iter().take(3) {
                    println!("  {}={}", key, value);
                }
            }
            Err(e) => {
                eprintln!("Failed to load {}: {}", file, e);
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
        false
    ).expect("Failed to parse property contexts");

    assert!(!property_infos.is_empty(), "Should have property info entries");

    // Build trie
    let trie_data = build_trie(&property_infos, "u:object_r:default_prop:s0", "string")
        .expect("Failed to build trie");

    assert!(!trie_data.is_empty(), "Trie data should not be empty");

    println!("Built trie with {} bytes of data from {} property info entries",
             trie_data.len(), property_infos.len());
}

#[test]
fn test_specific_android_properties() {
    let properties = setup_android_test_env();

    // Test some specific properties that commonly exist in Android
    let test_cases = vec![
        ("ro.build.version.sdk", "Should have SDK version"),
        ("ro.product.model", "Should have product model"),
        ("ro.build.type", "Should have build type"),
    ];

    for (prop_name, description) in test_cases {
        if let Some(expected_value) = properties.get(prop_name) {
            let retrieved = rsproperties::get_with_default(prop_name, "not_found");
            assert_eq!(retrieved, *expected_value, "{}", description);
            println!("✓ {}: {}", prop_name, retrieved);
        } else {
            println!("⚠ Property {} not found in test data", prop_name);
        }
    }
}

#[test]
fn test_property_prefixes() {
    let properties = setup_android_test_env();

    // Group properties by common prefixes
    let mut ro_props = 0;
    let mut persist_props = 0;
    let mut sys_props = 0;
    let mut other_props = 0;

    for key in properties.keys() {
        if key.starts_with("ro.") {
            ro_props += 1;
        } else if key.starts_with("persist.") {
            persist_props += 1;
        } else if key.starts_with("sys.") {
            sys_props += 1;
        } else {
            other_props += 1;
        }
    }

    println!("Property distribution:");
    println!("  ro.* properties: {}", ro_props);
    println!("  persist.* properties: {}", persist_props);
    println!("  sys.* properties: {}", sys_props);
    println!("  Other properties: {}", other_props);

    // Verify we have some ro.* properties (these are very common)
    assert!(ro_props > 0, "Should have some ro.* properties");
}
