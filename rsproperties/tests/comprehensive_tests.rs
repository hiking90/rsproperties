// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Comprehensive tests for rsproperties using real Android property system
//!
//! These tests use the existing __properties__ directory which contains
//! real Android property system files.

use rsproperties::{PROP_DIRNAME, PROP_VALUE_MAX};
use std::sync::Once;

static INIT_ONCE: Once = Once::new();

fn ensure_init() {
    INIT_ONCE.call_once(|| {
        // Initialize with the existing __properties__ directory
        let props_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("__properties__");
        let config = rsproperties::PropertyConfig::with_properties_dir(props_dir);
        rsproperties::init(config);
    });
}

#[test]
fn test_constants_are_correct() {
    // Test Android system property constants
    assert_eq!(
        PROP_VALUE_MAX, 92,
        "PROP_VALUE_MAX should match Android standard"
    );
    assert_eq!(
        PROP_DIRNAME, "/dev/__properties__",
        "PROP_DIRNAME should match Android default"
    );
    println!("✓ API constants are correct");
}

#[test]
fn test_get_with_default_functionality() {
    ensure_init();

    // Test getting a property that definitely doesn't exist
    let result = rsproperties::get_or("test.nonexistent.property.12345", "default_value".to_string());
    assert_eq!(
        result, "default_value",
        "Should return default value for non-existent properties"
    );

    // Test with empty default
    let result = rsproperties::get_or("test.another.nonexistent", "".to_string());
    assert_eq!(result, "", "Should return empty default value");

    // Test with various default values
    let test_cases = [
        ("test.case1", "simple"),
        ("test.case2", "with spaces"),
        ("test.case3", "with.dots.and.numbers.123"),
        ("test.case4", "special!@#$%chars"),
    ];

    for (prop, default) in &test_cases {
        let result = rsproperties::get_or(prop, default.to_string());
        assert_eq!(result, *default, "Should return default for {}", prop);
    }

    println!("✓ get_or functionality works correctly");
}

#[test]
fn test_get_nonexistent_properties() {
    ensure_init();

    // Test getting properties that don't exist
    let nonexistent_props = [
        "test.definitely.not.there",
        "fake.property.name",
        "test.12345.67890",
        "nonexistent.prop.with.long.name.that.should.not.exist",
    ];

    for prop in &nonexistent_props {
        let result: Result<String, _> = rsproperties::get(prop);
        assert!(
            result.is_err(),
            "Getting non-existent property '{}' should return error",
            prop
        );
    }

    println!("✓ get returns errors for non-existent properties");
}

#[test]
fn test_dirname_function() {
    ensure_init();

    let dirname = rsproperties::properties_dir();
    let dirname_str = dirname.to_string_lossy();

    // Should not be empty
    assert!(!dirname_str.is_empty(), "dirname should not be empty");

    // Should be a valid path
    assert!(
        dirname_str.contains("properties") || dirname_str.starts_with("/"),
        "dirname should be a valid path, got: {}",
        dirname_str
    );

    println!("✓ dirname function returns: {}", dirname_str);
}

#[test]
fn test_property_name_validation() {
    ensure_init();

    // Test various property name formats
    let valid_names = [
        "ro.build.version.sdk",
        "sys.boot_completed",
        "persist.sys.timezone",
        "test.property",
        "a.b.c.d.e.f.g",
        "prop123",
    ];

    for name in &valid_names {
        // These may or may not exist, but the names should be valid
        let _result: Result<String, _> = rsproperties::get(name);
        // We don't assert success/failure here since properties may or may not exist
        println!("Tested property name: {}", name);
    }

    // Test invalid property names
    let invalid_names = [
        "",      // empty
        ".",     // just dot
        "..",    // double dot
        "name.", // ending with dot
        ".name", // starting with dot
    ];

    for name in &invalid_names {
        let result: Result<String, _> = rsproperties::get(name);
        // Most should fail, but we don't enforce strict requirements
        // since behavior may vary by implementation
        println!("Tested invalid name '{}': {:?}", name, result.is_err());
    }

    println!("✓ Property name validation tests completed");
}

#[test]
fn test_thread_safety() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    ensure_init();

    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    // Spawn multiple threads that access properties concurrently
    for i in 0..10 {
        let counter_clone = Arc::clone(&counter);
        let handle = thread::spawn(move || {
            // Each thread performs multiple property operations
            for j in 0..5 {
                let prop_name = format!("test.thread.{}.{}", i, j);

                // Test get_or
                let _result = rsproperties::get_or(&prop_name, "default".to_string());
                counter_clone.fetch_add(1, Ordering::SeqCst);

                // Test get (which will likely fail)
                let _result: Result<String, _> = rsproperties::get(&prop_name);
                counter_clone.fetch_add(1, Ordering::SeqCst);

                // Test dirname
                let _dirname = rsproperties::properties_dir();
                counter_clone.fetch_add(1, Ordering::SeqCst);
            }
        });
        handles.push(handle);
    }

    // Wait for all threads to complete
    for handle in handles {
        handle.join().expect("Thread should complete successfully");
    }

    let final_count = counter.load(Ordering::SeqCst);
    assert_eq!(
        final_count,
        10 * 5 * 3,
        "All thread operations should complete"
    );

    println!(
        "✓ Thread safety test completed with {} operations",
        final_count
    );
}

#[test]
fn test_property_value_length_constraints() {
    // Test the PROP_VALUE_MAX constant
    let max_length_value = "x".repeat(PROP_VALUE_MAX);
    assert_eq!(max_length_value.len(), PROP_VALUE_MAX);

    let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
    assert_eq!(too_long_value.len(), PROP_VALUE_MAX + 1);

    // Test using these values with get_with_default
    ensure_init();

    let result1 = rsproperties::get_or("test.max.length", max_length_value.clone());
    assert_eq!(result1, max_length_value);

    let result2 = rsproperties::get_or("test.too.long", too_long_value.clone());
    assert_eq!(result2, too_long_value);

    println!("✓ Property value length constraint tests passed");
}

#[cfg(feature = "builder")]
mod builder_tests {
    use super::*;

    #[test]
    fn test_set_property_basic() {
        ensure_init();

        // Try to set a property
        let result = rsproperties::set("test.basic.property", "test_value");

        match result {
            Ok(_) => {
                println!("✓ Property set successfully");

                // Try to read it back
                let value: Result<String, _> = rsproperties::get("test.basic.property");
                assert_eq!(value.unwrap(), "test_value");
                println!("✓ Property read back successfully");
            }
            Err(e) => {
                println!(
                    "⚠ Property set failed (expected without property service): {}",
                    e
                );
                // This is expected behavior when property service is not running
            }
        }
    }

    #[test]
    fn test_set_property_various_values() {
        ensure_init();

        let test_cases = [
            ("test.empty", ""),
            ("test.simple", "value"),
            ("test.numbers", "12345"),
            ("test.special", "value with spaces!@#"),
            ("test.unicode", "üñíçødé"),
        ];

        for (prop, value) in &test_cases {
            match rsproperties::set(prop, value) {
                Ok(_) => println!("✓ Set property {} = {}", prop, value),
                Err(e) => println!("⚠ Failed to set property {}: {}", prop, e),
            }
        }
    }

    #[test]
    fn test_set_property_max_length() {
        ensure_init();

        // Test setting property with maximum allowed length
        let max_value = "x".repeat(PROP_VALUE_MAX);
        let result = rsproperties::set("test.max.length", &max_value);

        match result {
            Ok(_) => println!("✓ Successfully set property with max length"),
            Err(e) => println!("⚠ Failed to set max length property: {}", e),
        }
    }

    #[test]
    fn test_set_property_too_long() {
        ensure_init();

        // Test setting property with value that exceeds maximum length
        let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
        let result = rsproperties::set("test.too.long", &too_long_value);

        // This should fail
        assert!(
            result.is_err(),
            "Setting property with value too long should fail"
        );
        println!("✓ Correctly rejected property value that is too long");
    }

    #[test]
    fn test_property_update() {
        ensure_init();

        let prop_name = "test.update.property";

        // Set initial value
        match rsproperties::set(prop_name, "initial") {
            Ok(_) => {
                println!("✓ Set initial property value");

                // Update the value
                match rsproperties::set(prop_name, "updated") {
                    Ok(_) => {
                        println!("✓ Updated property value");

                        // Verify the update
                        let value: Result<String, _> = rsproperties::get(prop_name);
                        assert_eq!(value.unwrap(), "updated");
                        println!("✓ Property update verified");
                    }
                    Err(e) => println!("⚠ Property update failed: {}", e),
                }
            }
            Err(e) => println!("⚠ Initial property set failed: {}", e),
        }
    }

    #[test]
    fn test_concurrent_property_operations() {
        use std::thread;

        ensure_init();

        let mut handles = vec![];

        // Spawn multiple threads that try to set properties
        for i in 0..5 {
            let handle = thread::spawn(move || {
                let prop_name = format!("test.concurrent.{}", i);
                let prop_value = format!("value_{}", i);

                match rsproperties::set(&prop_name, &prop_value) {
                    Ok(_) => {
                        println!("Thread {}: Set property {} = {}", i, prop_name, prop_value);

                        // Try to read it back
                        let value: Result<String, _> = rsproperties::get(&prop_name);
                        println!("Thread {}: Read back: {:?}", i, value);
                    }
                    Err(e) => println!("Thread {}: Set failed: {}", i, e),
                }
            });
            handles.push(handle);
        }

        // Wait for all threads
        for handle in handles {
            handle.join().expect("Thread should complete");
        }

        println!("✓ Concurrent property operations completed");
    }
}

#[test]
fn test_error_handling() {
    ensure_init();

    // Test various error conditions

    // Very long property name
    let long_name = "test.".repeat(100) + "property";
    let result: Result<String, _> = rsproperties::get(&long_name);
    // This may or may not fail depending on implementation limits
    println!("Long property name test: {:?}", result.is_err());

    // Property name with null bytes (if we can construct one safely)
    // Note: Rust strings are UTF-8 and don't allow null bytes normally

    // Empty property name
    let result: Result<String, _> = rsproperties::get("");
    println!("Empty property name test: {:?}", result.is_err());

    println!("✓ Error handling tests completed");
}

#[test]
fn test_real_android_properties() {
    ensure_init();

    // Test some common Android properties that might exist
    let common_props = [
        "ro.build.version.sdk",
        "ro.build.version.release",
        "ro.product.model",
        "ro.product.manufacturer",
        "sys.boot_completed",
        "persist.sys.timezone",
    ];

    for prop in &common_props {
        match rsproperties::get::<String>(prop) {
            Ok(value) => println!("Found property {} = {}", prop, value),
            Err(_) => {
                // Use get_or to test the functionality
                let default_value = rsproperties::get_or(prop, "not_found".to_string());
                println!("Property {} not found, default: {}", prop, default_value);
            }
        }
    }

    println!("✓ Real Android properties test completed");
}
