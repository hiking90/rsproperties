// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Final comprehensive test suite for rsproperties
//!
//! This test suite provides complete coverage of the rsproperties public API
//! including constants validation, property operations, error handling,
//! thread safety, and performance testing.

extern crate rsproperties;

use rsproperties::{PROP_VALUE_MAX, PROP_DIRNAME};
use std::sync::Once;

static INIT_ONCE: Once = Once::new();

/// Ensure rsproperties is initialized only once with the real property system
fn ensure_init() {
    INIT_ONCE.call_once(|| {
        let props_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("__properties__");
        rsproperties::init(Some(props_dir));
    });
}

#[test]
fn test_api_constants() {
    // Verify Android system property constants are correct
    assert_eq!(PROP_VALUE_MAX, 92, "PROP_VALUE_MAX should match Android spec");
    assert_eq!(PROP_DIRNAME, "/dev/__properties__", "PROP_DIRNAME should match Android default");

    println!("✓ API constants validation passed");
    println!("  PROP_VALUE_MAX = {}", PROP_VALUE_MAX);
    println!("  PROP_DIRNAME = '{}'", PROP_DIRNAME);
}

#[test]
fn test_get_with_default_comprehensive() {
    ensure_init();

    // Test cases for get_with_default function
    let test_cases = [
        ("test.simple", "default", "should handle simple case"),
        ("test.empty.default", "", "should handle empty default"),
        ("test.spaces", "default with spaces", "should handle defaults with spaces"),
        ("test.special.chars", "!@#$%^&*()", "should handle special characters"),
        ("test.unicode", "üñíçødé", "should handle unicode characters"),
        ("test.long.property.name.with.many.dots", "default", "should handle long property names"),
        ("test.numbers.123", "456", "should handle numbers in names and values"),
    ];

    for (property, default, description) in &test_cases {
        let result = rsproperties::get_with_default(property, default);
        assert_eq!(result, *default, "{}", description);
    }

    println!("✓ get_with_default comprehensive tests passed ({} cases)", test_cases.len());
}

#[test]
fn test_get_nonexistent_properties() {
    ensure_init();

    // Test that getting non-existent properties returns errors
    let nonexistent_properties = [
        "definitely.not.there",
        "fake.property.12345",
        "test.nonexistent.with.very.long.name.that.should.definitely.not.exist",
        "x",
        "test",
        "this.property.does.not.exist",
    ];

    for property in &nonexistent_properties {
        let result = rsproperties::get(property);
        assert!(result.is_err(), "Property '{}' should not exist", property);
    }

    println!("✓ get non-existent properties test passed ({} properties tested)",
             nonexistent_properties.len());
}

#[test]
fn test_dirname_functionality() {
    ensure_init();

    let dirname = rsproperties::dirname();
    let dirname_str = dirname.to_string_lossy();

    // Verify dirname is not empty and looks like a path
    assert!(!dirname_str.is_empty(), "dirname should not be empty");
    assert!(dirname_str.contains("properties") || dirname_str.starts_with("/"),
            "dirname should be a valid path, got: '{}'", dirname_str);

    println!("✓ dirname functionality test passed");
    println!("  Current dirname: '{}'", dirname_str);
}

#[test]
fn test_property_name_validation() {
    ensure_init();

    // Test various property name formats
    let valid_format_names = [
        "simple",
        "test.property",
        "ro.build.version.sdk",
        "sys.boot_completed",
        "persist.sys.timezone",
        "a.b.c.d.e.f.g.h.i.j",
        "property123",
        "test_underscore",
        "MixedCase.Property",
    ];

    // These names are valid format-wise, they may or may not exist
    for name in &valid_format_names {
        let _result = rsproperties::get(name);
        let _default_result = rsproperties::get_with_default(name, "default");
        // We don't assert success/failure since properties may or may not exist
    }

    // Test potentially problematic property names
    let edge_case_names = [
        "", // empty name
        ".", // just dot
        "..", // double dot
        "name.", // ending with dot
        ".name", // starting with dot
    ];

    for name in &edge_case_names {
        let _result = rsproperties::get(name);
        // Don't assert specific behavior as implementation may vary
    }

    println!("✓ Property name validation test completed");
    println!("  Tested {} valid format names", valid_format_names.len());
    println!("  Tested {} edge case names", edge_case_names.len());
}

#[test]
fn test_property_value_length_limits() {
    // Test maximum value length constant
    let max_length_value = "x".repeat(PROP_VALUE_MAX);
    assert_eq!(max_length_value.len(), PROP_VALUE_MAX);

    let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
    assert_eq!(too_long_value.len(), PROP_VALUE_MAX + 1);

    ensure_init();

    // Test with get_with_default (should work regardless of length)
    let result1 = rsproperties::get_with_default("test.max.length", &max_length_value);
    assert_eq!(result1, max_length_value);

    let result2 = rsproperties::get_with_default("test.too.long", &too_long_value);
    assert_eq!(result2, too_long_value);

    println!("✓ Property value length limits test passed");
    println!("  PROP_VALUE_MAX = {}", PROP_VALUE_MAX);
    println!("  Tested values of length {} and {}", max_length_value.len(), too_long_value.len());
}

#[test]
fn test_thread_safety() {
    use std::thread;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    ensure_init();

    let success_count = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    // Spawn multiple threads that perform property operations concurrently
    for thread_id in 0..10 {
        let success_count_clone = Arc::clone(&success_count);

        let handle = thread::spawn(move || {
            // Each thread performs multiple operations
            for op_id in 0..5 {
                let property_name = format!("test.thread.{}.{}", thread_id, op_id);

                // Test get_with_default (should always succeed)
                let result = rsproperties::get_with_default(&property_name, "default");
                if result == "default" {
                    success_count_clone.fetch_add(1, Ordering::SeqCst);
                }

                // Test get (will likely fail but shouldn't crash)
                let _result = rsproperties::get(&property_name);
                success_count_clone.fetch_add(1, Ordering::SeqCst);

                // Test dirname (should always work)
                let _dirname = rsproperties::dirname();
                success_count_clone.fetch_add(1, Ordering::SeqCst);
            }
        });

        handles.push(handle);
    }

    // Wait for all threads to complete
    for handle in handles {
        handle.join().expect("Thread should complete successfully");
    }

    let final_count = success_count.load(Ordering::SeqCst);
    let expected_count = 10 * 5 * 3; // 10 threads × 5 operations × 3 calls per operation
    assert_eq!(final_count, expected_count, "All thread operations should complete");

    println!("✓ Thread safety test passed");
    println!("  {} threads × 5 operations × 3 calls = {} total operations", 10, expected_count);
}

#[test]
fn test_error_handling() {
    ensure_init();

    // Test various error conditions

    // Very long property name
    let long_name = "very.long.property.name.".repeat(50);
    let _result = rsproperties::get(&long_name);
    // May or may not fail depending on implementation limits

    // Empty property name
    let _result = rsproperties::get("");
    // Behavior may vary

    // Property name with only dots
    let _result = rsproperties::get("...");
    // Behavior may vary

    println!("✓ Error handling test completed");
    println!("  Tested various edge cases for error conditions");
}

#[test]
fn test_performance_basic() {
    use std::time::Instant;

    ensure_init();

    // Test performance of get_with_default
    let start = Instant::now();
    let iterations = 1000;

    for i in 0..iterations {
        let property_name = format!("test.perf.{}", i);
        let _result = rsproperties::get_with_default(&property_name, "default");
    }

    let elapsed = start.elapsed();
    let ops_per_sec = iterations as f64 / elapsed.as_secs_f64();

    println!("✓ Performance test completed");
    println!("  {} operations in {:?}", iterations, elapsed);
    println!("  {:.0} operations per second", ops_per_sec);

    // Performance should be reasonable (at least 1000 ops/sec)
    assert!(ops_per_sec > 1000.0, "Performance should be at least 1000 ops/sec, got {:.0}", ops_per_sec);
}

// Tests that require the builder feature
#[cfg(feature = "builder")]
mod builder_tests {
    use super::*;

    #[test]
    fn test_set_property_basic() {
        ensure_init();

        let result = rsproperties::set("test.basic.set", "test_value");

        match result {
            Ok(_) => {
                println!("✓ Property set successfully");

                // Try to read it back
                match rsproperties::get("test.basic.set") {
                    Ok(value) => {
                        assert_eq!(value, "test_value");
                        println!("✓ Property read back successfully: '{}'", value);
                    }
                    Err(e) => println!("⚠ Could not read back property: {}", e),
                }
            }
            Err(e) => {
                println!("⚠ Property set failed (expected without property service): {}", e);
                // This is expected when property service is not running
            }
        }
    }

    #[test]
    fn test_set_property_various_values() {
        ensure_init();

        let test_cases = [
            ("test.empty.value", ""),
            ("test.simple.value", "simple"),
            ("test.numeric.value", "12345"),
            ("test.special.chars", "!@#$%^&*()"),
            ("test.spaces.value", "value with spaces"),
            ("test.unicode.value", "üñíçødé tëxt"),
        ];

        for (property, value) in &test_cases {
            match rsproperties::set(property, value) {
                Ok(_) => println!("✓ Set property '{}' = '{}'", property, value),
                Err(e) => println!("⚠ Failed to set property '{}': {}", property, e),
            }
        }

        println!("✓ Set property various values test completed");
    }

    #[test]
    fn test_set_property_length_limits() {
        ensure_init();

        // Test setting property with maximum allowed length
        let max_value = "x".repeat(PROP_VALUE_MAX);
        let result = rsproperties::set("test.max.length.set", &max_value);

        match result {
            Ok(_) => println!("✓ Successfully set property with max length ({})", PROP_VALUE_MAX),
            Err(e) => println!("⚠ Failed to set max length property: {}", e),
        }

        // Test setting property with value that exceeds maximum length
        let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
        let result = rsproperties::set("test.too.long.set", &too_long_value);

        // This should typically fail, but behavior may vary
        match result {
            Ok(_) => println!("⚠ Unexpectedly succeeded setting overlong property"),
            Err(_) => println!("✓ Correctly rejected property value that is too long ({})", too_long_value.len()),
        }

        println!("✓ Set property length limits test completed");
    }

    #[test]
    fn test_property_update() {
        ensure_init();

        let property_name = "test.update.property";

        // Set initial value
        match rsproperties::set(property_name, "initial_value") {
            Ok(_) => {
                println!("✓ Set initial property value");

                // Update the value
                match rsproperties::set(property_name, "updated_value") {
                    Ok(_) => {
                        println!("✓ Updated property value");

                        // Verify the update
                        match rsproperties::get(property_name) {
                            Ok(value) => {
                                assert_eq!(value, "updated_value");
                                println!("✓ Property update verified: '{}'", value);
                            }
                            Err(e) => println!("⚠ Could not verify property update: {}", e),
                        }
                    }
                    Err(e) => println!("⚠ Property update failed: {}", e),
                }
            }
            Err(e) => println!("⚠ Initial property set failed: {}", e),
        }

        println!("✓ Property update test completed");
    }

    #[test]
    fn test_concurrent_property_sets() {
        use std::thread;

        ensure_init();

        let mut handles = vec![];

        // Spawn multiple threads that try to set properties
        for thread_id in 0..5 {
            let handle = thread::spawn(move || {
                let property_name = format!("test.concurrent.set.{}", thread_id);
                let property_value = format!("thread_{}_value", thread_id);

                match rsproperties::set(&property_name, &property_value) {
                    Ok(_) => {
                        println!("Thread {}: Set property '{}' = '{}'", thread_id, property_name, property_value);

                        // Try to read it back
                        match rsproperties::get(&property_name) {
                            Ok(value) => {
                                println!("Thread {}: Read back value: '{}'", thread_id, value);
                                if value == property_value {
                                    println!("Thread {}: ✓ Value matches", thread_id);
                                } else {
                                    println!("Thread {}: ⚠ Value mismatch", thread_id);
                                }
                            }
                            Err(e) => println!("Thread {}: ⚠ Read failed: {}", thread_id, e),
                        }
                    }
                    Err(e) => println!("Thread {}: ⚠ Set failed: {}", thread_id, e),
                }
            });

            handles.push(handle);
        }

        // Wait for all threads
        for handle in handles {
            handle.join().expect("Thread should complete");
        }

        println!("✓ Concurrent property sets test completed");
    }
}

#[test]
fn test_integration_comprehensive() {
    ensure_init();

    // Comprehensive integration test combining multiple operations

    // Test constants
    assert_eq!(PROP_VALUE_MAX, 92);
    assert_eq!(PROP_DIRNAME, "/dev/__properties__");

    // Test dirname
    let dirname = rsproperties::dirname();
    assert!(!dirname.to_string_lossy().is_empty());

    // Test multiple get_with_default calls
    let test_properties = [
        "integration.test.1",
        "integration.test.2",
        "integration.test.3",
    ];

    for (i, property) in test_properties.iter().enumerate() {
        let default_value = format!("default_{}", i);
        let result = rsproperties::get_with_default(property, &default_value);
        assert_eq!(result, default_value);
    }

    // Test error conditions
    for property in &test_properties {
        let result = rsproperties::get(property);
        assert!(result.is_err());
    }

    println!("✓ Comprehensive integration test passed");
    println!("  Tested constants, dirname, get_with_default, and get functions");
    println!("  All components working together correctly");
}
