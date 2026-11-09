// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Final comprehensive test suite for rsproperties
//!
//! This test suite provides complete coverage of the rsproperties public API
//! including constants validation, property operations, error handling,
//! thread safety, and performance testing.

extern crate rsproperties;

use rsproperties::{PROP_DIRNAME, PROP_VALUE_MAX};

#[path = "common.rs"]
mod common;
use common::init_test;

#[test]
fn test_api_constants() {
    // Verify Android system property constants are correct
    assert_eq!(
        PROP_VALUE_MAX, 92,
        "PROP_VALUE_MAX should match Android spec"
    );
    assert_eq!(
        PROP_DIRNAME, "/dev/__properties__",
        "PROP_DIRNAME should match Android default"
    );

    println!("✓ API constants validation passed");
    println!("  PROP_VALUE_MAX = {PROP_VALUE_MAX}");
    println!("  PROP_DIRNAME = '{PROP_DIRNAME}'");
}

#[tokio::test]
async fn test_get_with_default_comprehensive() {
    init_test().await;

    // Test cases for get_with_default function
    let test_cases = [
        ("test.simple", "default", "should handle simple case"),
        ("test.empty.default", "", "should handle empty default"),
        (
            "test.spaces",
            "default with spaces",
            "should handle defaults with spaces",
        ),
        (
            "test.special.chars",
            "!@#$%^&*()",
            "should handle special characters",
        ),
        (
            "test.unicode",
            "üñíçødé",
            "should handle unicode characters",
        ),
        (
            "test.long.property.name.with.many.dots",
            "default",
            "should handle long property names",
        ),
        (
            "test.numbers.123",
            "456",
            "should handle numbers in names and values",
        ),
    ];

    for (property, default, description) in &test_cases {
        let result = rsproperties::get_or(property, default.to_string());
        assert_eq!(result, *default, "{description}");
    }

    println!(
        "✓ get_with_default comprehensive tests passed ({} cases)",
        test_cases.len()
    );
}

#[tokio::test]
async fn test_get_nonexistent_properties() {
    init_test().await;

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
        let result = rsproperties::get::<String>(property);
        assert!(
            matches!(result, Err(rsproperties::Error::NotFound(_))),
            "Property '{property}' should not exist, got {result:?}"
        );
    }

    println!(
        "✓ get non-existent properties test passed ({} properties tested)",
        nonexistent_properties.len()
    );
}

#[tokio::test]
async fn test_dirname_functionality() {
    init_test().await;

    let dirname = rsproperties::properties_dir();
    let dirname_str = dirname.to_string_lossy();

    // Verify dirname is not empty and looks like a path
    assert!(!dirname_str.is_empty(), "dirname should not be empty");
    assert!(
        dirname_str.contains("properties") || dirname_str.starts_with("/"),
        "dirname should be a valid path, got: '{dirname_str}'"
    );

    println!("✓ dirname functionality test passed");
    println!("  Current dirname: '{dirname_str}'");
}

#[tokio::test]
async fn test_property_name_validation() {
    init_test().await;

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
        let _result: String = rsproperties::get(name).unwrap_or_default();
        let _default_result = rsproperties::get_or(name, "default".to_string());
        // We don't assert success/failure since properties may or may not exist
    }

    // Test potentially problematic property names
    let edge_case_names = [
        "",      // empty name
        ".",     // just dot
        "..",    // double dot
        "name.", // ending with dot
        ".name", // starting with dot
    ];

    // A name with an empty segment cannot exist in the trie, so `get`
    // reports it as missing (`NotFound`), not as a parse error.
    for name in &edge_case_names {
        let result = rsproperties::get::<String>(name);
        assert!(
            matches!(result, Err(rsproperties::Error::NotFound(_))),
            "get({name:?}) must be NotFound, got {result:?}"
        );
    }

    println!("✓ Property name validation test completed");
    println!("  Tested {} valid format names", valid_format_names.len());
    println!("  Tested {} edge case names", edge_case_names.len());
}

#[tokio::test]
async fn test_property_value_length_limits() {
    // Test maximum value length constant
    let max_length_value = "x".repeat(PROP_VALUE_MAX);
    assert_eq!(max_length_value.len(), PROP_VALUE_MAX);

    let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
    assert_eq!(too_long_value.len(), PROP_VALUE_MAX + 1);

    init_test().await;

    // Test with get_with_default (should work regardless of length)
    let result1 = rsproperties::get_or("test.max.length", max_length_value.to_string());
    assert_eq!(result1, max_length_value);

    let result2 = rsproperties::get_or("test.too.long", too_long_value.to_string());
    assert_eq!(result2, too_long_value);

    println!("✓ Property value length limits test passed");
    println!("  PROP_VALUE_MAX = {PROP_VALUE_MAX}");
    println!(
        "  Tested values of length {} and {}",
        max_length_value.len(),
        too_long_value.len()
    );
}

#[tokio::test]
async fn test_thread_safety() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    init_test().await;

    let success_count = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    // Spawn multiple threads that perform property operations concurrently
    for thread_id in 0..10 {
        let success_count_clone = Arc::clone(&success_count);

        let handle = thread::spawn(move || {
            // Each thread performs multiple operations
            for op_id in 0..5 {
                let property_name = format!("test.thread.{thread_id}.{op_id}");

                // Test get_with_default (should always succeed)
                let result = rsproperties::get_or(&property_name, "default".to_string());
                if result == "default" {
                    success_count_clone.fetch_add(1, Ordering::SeqCst);
                }

                // Test get (will likely fail but shouldn't crash)
                let _result: String = rsproperties::get(&property_name).unwrap_or_default();
                success_count_clone.fetch_add(1, Ordering::SeqCst);

                // Test dirname (should always work)
                let _dirname = rsproperties::properties_dir();
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
    assert_eq!(
        final_count, expected_count,
        "All thread operations should complete"
    );

    println!("✓ Thread safety test passed");
    println!(
        "  {} threads × 5 operations × 3 calls = {} total operations",
        10, expected_count
    );
}

#[tokio::test]
async fn test_error_handling() {
    init_test().await;

    // `get` has no name-length limit and does not validate names: a name
    // that was never set, or that has an empty segment, is `NotFound`.
    let long_name = "very.long.property.name.".repeat(50);
    for name in [long_name.as_str(), "", "..."] {
        let result = rsproperties::get::<String>(name);
        assert!(
            matches!(result, Err(rsproperties::Error::NotFound(_))),
            "get of a {}-byte name must be NotFound, got {result:?}",
            name.len()
        );
    }

    println!("✓ Error handling test completed");
    println!("  Tested various edge cases for error conditions");
}

#[tokio::test]
async fn test_performance_basic() {
    use std::time::Instant;

    init_test().await;

    // Test performance of get_with_default
    let start = Instant::now();
    let iterations = 1000;

    for i in 0..iterations {
        let property_name = format!("test.perf.{i}");
        let _result = rsproperties::get_or(&property_name, "default".to_string());
    }

    let elapsed = start.elapsed();
    let ops_per_sec = iterations as f64 / elapsed.as_secs_f64();

    println!("✓ Performance test completed");
    println!("  {iterations} operations in {elapsed:?}");
    println!("  {ops_per_sec:.0} operations per second");
}

// Tests that require the builder feature
mod builder_tests {
    use super::*;

    #[tokio::test]
    async fn test_set_property_basic() {
        init_test().await;

        rsproperties::set("test.basic.set", "test_value").expect("set with the service running");

        let value: String = rsproperties::get("test.basic.set").expect("get after set");
        assert_eq!(value, "test_value");
    }

    #[tokio::test]
    async fn test_set_property_various_values() {
        init_test().await;

        let test_cases = [
            ("test.empty.value", ""),
            ("test.simple.value", "simple"),
            ("test.numeric.value", "12345"),
            ("test.special.chars", "!@#$%^&*()"),
            ("test.spaces.value", "value with spaces"),
            ("test.unicode.value", "üñíçødé tëxt"),
        ];

        // Every value is shorter than PROP_VALUE_MAX and has no NUL byte.
        // `test.special.chars` is also read by
        // `test_get_with_default_comprehensive` with a default equal to this
        // value, so the two tests agree in either order.
        for (property, value) in &test_cases {
            rsproperties::set(property, value)
                .unwrap_or_else(|e| panic!("set({property:?}) failed: {e}"));
            let read: String = rsproperties::get(property)
                .unwrap_or_else(|e| panic!("get({property:?}) failed: {e}"));
            assert_eq!(read, *value, "value read back for {property}");
        }
    }

    #[tokio::test]
    async fn test_set_property_length_limits() {
        init_test().await;

        // A non-`ro.` value holds at most PROP_VALUE_MAX - 1 bytes (the slot
        // keeps a NUL terminator); the client rejects longer values before
        // connecting (`wire::validate_value_len`).
        let longest_value = "x".repeat(PROP_VALUE_MAX - 1);
        rsproperties::set("test.longest.length.set", &longest_value)
            .expect("a value of PROP_VALUE_MAX - 1 bytes is accepted");
        let read: String = rsproperties::get("test.longest.length.set").expect("get after set");
        assert_eq!(read, longest_value);

        for len in [PROP_VALUE_MAX, PROP_VALUE_MAX + 1] {
            let result = rsproperties::set("test.too.long.set", &"x".repeat(len));
            assert!(
                matches!(result, Err(rsproperties::Error::InvalidArgument(_))),
                "a {len}-byte value must be InvalidArgument, got {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_property_update() {
        init_test().await;

        let property_name = "test.update.property";

        rsproperties::set(property_name, "initial_value").expect("initial set");
        let value: String = rsproperties::get(property_name).expect("get after initial set");
        assert_eq!(value, "initial_value");

        rsproperties::set(property_name, "updated_value").expect("update");
        let value: String = rsproperties::get(property_name).expect("get after update");
        assert_eq!(value, "updated_value");
    }

    #[tokio::test]
    async fn test_concurrent_property_sets() {
        use std::thread;

        init_test().await;

        let mut handles = vec![];

        // Spawn multiple threads that try to set properties
        for thread_id in 0..5 {
            let handle = thread::spawn(move || {
                let property_name = format!("test.concurrent.set.{thread_id}");
                let property_value = format!("thread_{thread_id}_value");

                rsproperties::set(&property_name, &property_value)
                    .unwrap_or_else(|e| panic!("thread {thread_id}: set failed: {e}"));
                // Each thread owns its property, so its own write is the
                // only one the read can observe.
                let value: String = rsproperties::get(&property_name)
                    .unwrap_or_else(|e| panic!("thread {thread_id}: get failed: {e}"));
                assert_eq!(value, property_value, "thread {thread_id}");
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

#[tokio::test]
async fn test_integration_comprehensive() {
    init_test().await;

    // Comprehensive integration test combining multiple operations

    // Test constants
    assert_eq!(PROP_VALUE_MAX, 92);
    assert_eq!(PROP_DIRNAME, "/dev/__properties__");

    // Test dirname
    let dirname = rsproperties::properties_dir();
    assert!(!dirname.to_string_lossy().is_empty());

    // Test multiple get_with_default calls
    let test_properties = [
        "integration.test.1",
        "integration.test.2",
        "integration.test.3",
    ];

    for (i, property) in test_properties.iter().enumerate() {
        let default_value = format!("default_{i}");
        let result = rsproperties::get_or(property, default_value.to_string());
        assert_eq!(result, default_value);
    }

    // Test error conditions
    for property in &test_properties {
        let result = rsproperties::get::<String>(property);
        assert!(
            matches!(result, Err(rsproperties::Error::NotFound(_))),
            "{property}: {result:?}"
        );
    }

    println!("✓ Comprehensive integration test passed");
    println!("  Tested constants, dirname, get_with_default, and get functions");
    println!("  All components working together correctly");
}
