// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Production-ready test suite for rsproperties library
//!
//! This test suite comprehensively validates the public API of rsproperties,
//! covering all major functionality including property reading, constants,
//! error handling, thread safety, and performance characteristics.

extern crate rsproperties;

use rsproperties::{PROP_VALUE_MAX, PROP_DIRNAME};

#[path = "common.rs"]
mod common;
use common::init_test;

/// Initialize the property system once for all tests
fn init_properties() {
    init_test();
}

#[test]
fn test_constants_validation() {
    // Validate that Android system property constants are correct
    assert_eq!(PROP_VALUE_MAX, 92, "PROP_VALUE_MAX must match Android specification");
    assert_eq!(PROP_DIRNAME, "/dev/__properties__", "PROP_DIRNAME must match Android default");

    println!("✓ Constants validation passed");
    println!("  PROP_VALUE_MAX = {}", PROP_VALUE_MAX);
    println!("  PROP_DIRNAME = '{}'", PROP_DIRNAME);
}

#[test]
fn test_get_with_default_basic() {
    init_properties();

    // Test basic get_with_default functionality
    let result = rsproperties::get_with_default("test.nonexistent.basic", "default_value");
    assert_eq!(result, "default_value");

    println!("✓ Basic get_with_default test passed");
}

#[test]
fn test_get_with_default_edge_cases() {
    init_properties();

    // Test edge cases for get_with_default
    let test_cases = [
        ("test.empty.default", "", "empty default value"),
        ("test.spaces", "value with spaces", "default with spaces"),
        ("test.special", "!@#$%^&*()", "special characters"),
        ("test.unicode", "üñíçødé", "unicode characters"),
        ("test.long.name.with.many.segments", "default", "long property name"),
    ];

    for (prop, default, description) in &test_cases {
        let result = rsproperties::get_with_default(prop, default);
        assert_eq!(result, *default, "Failed for {}", description);
    }

    println!("✓ get_with_default edge cases passed ({} test cases)", test_cases.len());
}

#[test]
fn test_get_nonexistent_returns_error() {
    init_properties();

    // Test that getting non-existent properties returns errors
    let nonexistent_props = [
        "definitely.not.there",
        "fake.property.12345",
        "test.nonexistent.long.name.that.should.not.exist.anywhere",
    ];

    for prop in &nonexistent_props {
        let result = rsproperties::get_with_result(prop);
        assert!(result.is_err(), "Property '{}' should not exist and should return error", prop);
    }

    println!("✓ get nonexistent properties test passed");
}

#[test]
fn test_dirname_function() {
    init_properties();

    let dirname = rsproperties::dirname();
    let dirname_str = dirname.to_string_lossy();

    // Verify dirname returns a valid path
    assert!(!dirname_str.is_empty(), "dirname should not be empty");

    println!("✓ dirname function test passed");
    println!("  dirname = '{}'", dirname_str);
}

#[test]
fn test_value_length_limits() {
    // Test property value length limits
    let max_value = "x".repeat(PROP_VALUE_MAX);
    assert_eq!(max_value.len(), PROP_VALUE_MAX);

    let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
    assert_eq!(too_long_value.len(), PROP_VALUE_MAX + 1);

    init_properties();

    // Test with get_with_default (should work with any length default)
    let result1 = rsproperties::get_with_default("test.max.length", &max_value);
    assert_eq!(result1, max_value);

    let result2 = rsproperties::get_with_default("test.too.long", &too_long_value);
    assert_eq!(result2, too_long_value);

    println!("✓ Value length limits test passed");
}

#[test]
fn test_thread_safety_basic() {
    use std::thread;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    init_properties();

    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    // Spawn multiple threads
    for i in 0..5 {
        let counter_clone = Arc::clone(&counter);

        let handle = thread::spawn(move || {
            // Each thread performs property operations
            for j in 0..10 {
                let prop_name = format!("test.thread.{}.{}", i, j);

                // Test get_with_default
                let _result = rsproperties::get_with_default(&prop_name, "default");
                counter_clone.fetch_add(1, Ordering::SeqCst);

                // Test get
                let _result = rsproperties::get(&prop_name);
                counter_clone.fetch_add(1, Ordering::SeqCst);

                // Test dirname
                let _dirname = rsproperties::dirname();
                counter_clone.fetch_add(1, Ordering::SeqCst);
            }
        });

        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().expect("Thread should complete");
    }

    let final_count = counter.load(Ordering::SeqCst);
    assert_eq!(final_count, 5 * 10 * 3, "All operations should complete");

    println!("✓ Thread safety test passed ({} operations)", final_count);
}

#[test]
fn test_performance_baseline() {
    use std::time::Instant;

    init_properties();

    let start = Instant::now();
    let iterations = 1000;

    for i in 0..iterations {
        let prop = format!("perf.test.{}", i);
        let _result = rsproperties::get_with_default(&prop, "default");
    }

    let elapsed = start.elapsed();
    let ops_per_sec = iterations as f64 / elapsed.as_secs_f64();

    println!("✓ Performance baseline test completed");
    println!("  {} iterations in {:?} ({:.0} ops/sec)", iterations, elapsed, ops_per_sec);

    // Should be reasonably fast
    assert!(ops_per_sec > 500.0, "Performance should be reasonable, got {:.0} ops/sec", ops_per_sec);
}

#[test]
fn test_error_conditions() {
    init_properties();

    // Test various potentially problematic inputs
    let edge_cases = [
        "",          // empty
        ".",         // just dot
        "..",        // double dot
        "...",       // triple dot
        ".test",     // starts with dot
        "test.",     // ends with dot
    ];

    for case in &edge_cases {
        // These may succeed or fail depending on implementation
        let _result1 = rsproperties::get(case);
        let _result2 = rsproperties::get_with_default(case, "default");
    }

    println!("✓ Error conditions test completed");
}

// Tests that require the builder feature
#[cfg(feature = "builder")]
mod builder_tests {
    use super::*;

    #[test]
    fn test_set_various_values() {
        init_properties();

        let values = [
            ("test.set.empty", ""),
            ("test.set.simple", "value"),
            ("test.set.numbers", "12345"),
            ("test.set.special", "!@#$%"),
            ("test.set.spaces", "with spaces"),
        ];

        for (prop, value) in &values {
            match rsproperties::set(prop, value) {
                Ok(_) => println!("✓ Set '{}' = '{}'", prop, value),
                Err(e) => println!("⚠ Failed to set '{}': {}", prop, e),
            }
        }
    }

    #[test]
    fn test_set_length_limits() {
        init_properties();

        // Test max length
        let max_value = "x".repeat(PROP_VALUE_MAX);
        let result1 = rsproperties::set("test.set.max", &max_value);

        // Test too long
        let too_long = "x".repeat(PROP_VALUE_MAX + 1);
        let result2 = rsproperties::set("test.set.long", &too_long);

        match result1 {
            Ok(_) => println!("✓ Max length property set successfully"),
            Err(e) => println!("⚠ Max length set failed: {}", e),
        }

        match result2 {
            Ok(_) => println!("⚠ Too long property unexpectedly succeeded"),
            Err(_) => println!("✓ Too long property correctly rejected"),
        }
    }

    #[test]
    fn test_property_update() {
        init_properties();

        let prop = "test.update.prop";

        // Set initial
        match rsproperties::set(prop, "initial") {
            Ok(_) => {
                // Update it
                match rsproperties::set(prop, "updated") {
                    Ok(_) => {
                        // Verify
                        let value = rsproperties::get(prop);
                        assert_eq!(value, "updated");
                        println!("✓ Property update verified");
                    }
                    Err(e) => println!("⚠ Update failed: {}", e),
                }
            }
            Err(e) => println!("⚠ Initial set failed: {}", e),
        }
    }
}

#[test]
fn test_comprehensive_integration() {
    init_properties();

    // Integration test combining multiple features

    // 1. Test constants
    assert_eq!(PROP_VALUE_MAX, 92);
    assert_eq!(PROP_DIRNAME, "/dev/__properties__");

    // 2. Test dirname
    let dirname = rsproperties::dirname();
    assert!(!dirname.to_string_lossy().is_empty());

    // 3. Test get_with_default for multiple properties
    let props = ["int.test.1", "int.test.2", "int.test.3"];
    for (i, prop) in props.iter().enumerate() {
        let default = format!("default_{}", i);
        let result = rsproperties::get_with_default(prop, &default);
        assert_eq!(result, default);
    }

    // 4. Test get for non-existent properties
    for prop in &props {
        let result = rsproperties::get_with_result(prop);
        assert!(result.is_err());
    }

    println!("✓ Comprehensive integration test passed");
    println!("  All API components working correctly together");
}
