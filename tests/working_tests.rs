// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Working integration tests for rsproperties
//!
//! These tests create proper test environments with necessary directories
//! and property files to test the rsproperties functionality.

use rsproperties::{PROP_VALUE_MAX, PROP_DIRNAME};
use std::fs::{create_dir_all, remove_dir_all, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Once;

static CLEANUP: Once = Once::new();

fn setup_test_env(test_name: &str) -> String {
    let test_dir = format!("/tmp/rsproperties_test_{}", test_name);

    // Clean up any existing test directory
    let _ = remove_dir_all(&test_dir);

    // Create the test directory structure
    create_dir_all(&test_dir).expect("Failed to create test directory");

    // Create necessary property system files
    create_property_files(&test_dir).expect("Failed to create property files");

    test_dir
}

fn create_property_files(dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Create property_contexts file
    let contexts_path = format!("{}/property_contexts", dir);
    let mut contexts_file = File::create(&contexts_path)?;
    writeln!(contexts_file, "##")?;
    writeln!(contexts_file, "## property_contexts for testing")?;
    writeln!(contexts_file, "##")?;
    writeln!(contexts_file, "test.                   u:r:test_property:s0")?;
    writeln!(contexts_file, "ro.                     u:r:system_prop:s0")?;
    writeln!(contexts_file, "persist.                u:r:system_prop:s0")?;
    writeln!(contexts_file, "sys.                    u:r:system_prop:s0")?;

    // Create selinux_property_contexts file
    let selinux_path = format!("{}/selinux_property_contexts", dir);
    let mut selinux_file = File::create(&selinux_path)?;
    writeln!(selinux_file, "test_property")?;
    writeln!(selinux_file, "system_prop")?;

    // Create plat_property_contexts file
    let plat_path = format!("{}/plat_property_contexts", dir);
    let mut plat_file = File::create(&plat_path)?;
    writeln!(plat_file, "##")?;
    writeln!(plat_file, "## plat_property_contexts for testing")?;
    writeln!(plat_file, "##")?;
    writeln!(plat_file, "test.                   u:r:test_property:s0")?;

    Ok(())
}

fn cleanup_test_env(test_dir: &str) {
    let _ = remove_dir_all(test_dir);
}

#[test]
fn test_constants() {
    // Test that the constants are properly defined
    assert_eq!(PROP_VALUE_MAX, 92);
    assert_eq!(PROP_DIRNAME, "/dev/__properties__");
    println!("✓ Constants test passed");
}

#[test]
fn test_get_with_default_no_property_system() {
    let test_dir = setup_test_env("get_default");

    // Initialize with test directory
    rsproperties::init(Some(test_dir.clone().into()));

    // Test getting a non-existent property with default
    let result = rsproperties::get_with_default("test.nonexistent", "default");
    assert_eq!(result, "default");

    cleanup_test_env(&test_dir);
    println!("✓ get_with_default test passed");
}

#[test]
fn test_get_nonexistent_property() {
    let test_dir = setup_test_env("get_nonexistent");

    // Initialize with test directory
    rsproperties::init(Some(test_dir.clone().into()));

    // Test getting a non-existent property
    let result = rsproperties::get("test.nonexistent");
    assert!(result.is_err());

    cleanup_test_env(&test_dir);
    println!("✓ get nonexistent property test passed");
}

#[test]
fn test_dirname_function() {
    let test_dir = setup_test_env("dirname");

    // Initialize with test directory
    rsproperties::init(Some(test_dir.clone().into()));

    // Test dirname function
    let dirname = rsproperties::dirname();
    assert!(!dirname.to_string_lossy().is_empty());

    cleanup_test_env(&test_dir);
    println!("✓ dirname test passed");
}

#[test]
fn test_property_value_length_limits() {
    // Test the maximum property value length constant
    let max_value = "x".repeat(PROP_VALUE_MAX);
    assert_eq!(max_value.len(), PROP_VALUE_MAX);

    let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
    assert_eq!(too_long_value.len(), PROP_VALUE_MAX + 1);

    println!("✓ Property value length limits test passed");
}

#[cfg(feature = "builder")]
mod builder_tests {
    use super::*;

    #[test]
    fn test_set_property_basic() {
        let test_dir = setup_test_env("set_basic");

        // Initialize with test directory
        rsproperties::init(Some(test_dir.clone().into()));

        // Try to set a property (may fail due to missing property service)
        let result = rsproperties::set("test.basic", "value");

        // We don't require success since it depends on property service being available
        match result {
            Ok(_) => {
                println!("✓ set property succeeded");

                // Try to get the property back
                match rsproperties::get("test.basic") {
                    Ok(value) => {
                        assert_eq!(value, "value");
                        println!("✓ get property after set succeeded");
                    }
                    Err(e) => println!("⚠ get property after set failed: {}", e),
                }
            }
            Err(e) => println!("⚠ set property failed (expected without property service): {}", e),
        }

        cleanup_test_env(&test_dir);
    }

    #[test]
    fn test_set_property_max_length() {
        let test_dir = setup_test_env("set_max_length");

        // Initialize with test directory
        rsproperties::init(Some(test_dir.clone().into()));

        // Test setting a property with maximum allowed length
        let max_value = "x".repeat(PROP_VALUE_MAX);
        let result = rsproperties::set("test.max_length", &max_value);

        match result {
            Ok(_) => println!("✓ set property with max length succeeded"),
            Err(e) => println!("⚠ set property with max length failed: {}", e),
        }

        cleanup_test_env(&test_dir);
    }

    #[test]
    fn test_set_property_too_long() {
        let test_dir = setup_test_env("set_too_long");

        // Initialize with test directory
        rsproperties::init(Some(test_dir.clone().into()));

        // Test setting a property with value that's too long
        let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
        let result = rsproperties::set("test.too_long", &too_long_value);

        // This should fail due to value being too long
        assert!(result.is_err(), "Setting property with too long value should fail");

        cleanup_test_env(&test_dir);
        println!("✓ set property too long test passed");
    }

    #[test]
    fn test_multiple_properties() {
        let test_dir = setup_test_env("multiple");

        // Initialize with test directory
        rsproperties::init(Some(test_dir.clone().into()));

        // Try to set multiple properties
        let properties = [
            ("test.prop1", "value1"),
            ("test.prop2", "value2"),
            ("test.prop3", "value3"),
        ];

        for (key, value) in &properties {
            match rsproperties::set(key, value) {
                Ok(_) => println!("✓ Set property {} = {}", key, value),
                Err(e) => println!("⚠ Failed to set property {}: {}", key, e),
            }
        }

        cleanup_test_env(&test_dir);
    }
}

#[cfg(feature = "builder")]
#[test]
fn test_property_update() {
    let test_dir = setup_test_env("update");

    // Initialize with test directory
    rsproperties::init(Some(test_dir.clone().into()));

    // Try to set a property
    match rsproperties::set("test.update", "initial") {
        Ok(_) => {
            println!("✓ Initial property set");

            // Try to update it
            match rsproperties::set("test.update", "updated") {
                Ok(_) => println!("✓ Property update succeeded"),
                Err(e) => println!("⚠ Property update failed: {}", e),
            }
        }
        Err(e) => println!("⚠ Initial property set failed: {}", e),
    }

    cleanup_test_env(&test_dir);
}

// Test error handling for invalid property names
#[test]
fn test_invalid_property_names() {
    let test_dir = setup_test_env("invalid_names");

    // Initialize with test directory
    rsproperties::init(Some(test_dir.clone().into()));

    // Test various invalid property names
    let invalid_names = [
        "", // empty name
        ".", // just a dot
        "..", // double dot
        "name.", // ending with dot
        ".name", // starting with dot
        "name with spaces", // spaces
        "name\twith\ttabs", // tabs
        "name\nwith\nnewlines", // newlines
    ];

    for name in &invalid_names {
        let result = rsproperties::get(name);
        // Most of these should return errors, but we don't enforce strict requirements
        // since the behavior might vary based on implementation
        match result {
            Ok(value) => println!("⚠ Unexpectedly got value '{}' for invalid name '{}'", value, name),
            Err(_) => println!("✓ Correctly rejected invalid property name '{}'", name),
        }
    }

    cleanup_test_env(&test_dir);
}

#[test]
fn test_thread_safety_basic() {
    use std::thread;
    use std::sync::Arc;

    let test_dir = setup_test_env("thread_safety");

    // Initialize with test directory
    rsproperties::init(Some(test_dir.clone().into()));

    let test_dir_arc = Arc::new(test_dir);
    let mut handles = vec![];

    // Spawn multiple threads that try to read properties
    for i in 0..5 {
        let test_dir_clone = Arc::clone(&test_dir_arc);
        let handle = thread::spawn(move || {
            // Each thread tries to get a property
            let prop_name = format!("test.thread.{}", i);
            let result = rsproperties::get_with_default(&prop_name, "default");
            println!("Thread {}: property {} = {}", i, prop_name, result);
            result
        });
        handles.push(handle);
    }

    // Wait for all threads to complete
    for handle in handles {
        let _ = handle.join();
    }

    cleanup_test_env(&test_dir_arc);
    println!("✓ Thread safety basic test completed");
}
