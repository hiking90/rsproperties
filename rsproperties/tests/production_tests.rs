// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Production-ready test suite for rsproperties library
//!
//! This test suite comprehensively validates the public API of rsproperties,
//! covering all major functionality including property reading, constants,
//! error handling, thread safety, and performance characteristics.

extern crate rsproperties;

#[cfg(any(feature = "builder", target_os = "android"))]
use rsproperties::Error;
use rsproperties::{wire::PROP_VALUE_MAX, PROP_DIRNAME};

mod common;
#[cfg(any(feature = "builder", target_os = "android"))]
use common::init_test;

/// Initialize the property system once for all tests
#[cfg(any(feature = "builder", target_os = "android"))]
fn init_properties() {
    init_test();
}

#[test]
fn test_constants_validation() {
    // Validate that Android system property constants are correct
    assert_eq!(
        PROP_VALUE_MAX, 92,
        "PROP_VALUE_MAX must match Android specification"
    );
    assert_eq!(
        PROP_DIRNAME, "/dev/__properties__",
        "PROP_DIRNAME must match Android default"
    );

    println!("✓ Constants validation passed");
    println!("  PROP_VALUE_MAX = {PROP_VALUE_MAX}");
    println!("  PROP_DIRNAME = '{PROP_DIRNAME}'");
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_get_with_default_basic() {
    init_properties();

    // Test basic get_or functionality
    let result = rsproperties::get_or("test.nonexistent.basic", "default_value".to_string());
    assert_eq!(result, "default_value");

    println!("✓ Basic get_with_default test passed");
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_get_with_default_edge_cases() {
    init_properties();

    // Test edge cases for get_with_default
    let test_cases = [
        ("test.empty.default", "", "empty default value"),
        ("test.spaces", "value with spaces", "default with spaces"),
        ("test.special", "!@#$%^&*()", "special characters"),
        ("test.unicode", "üñíçødé", "unicode characters"),
        (
            "test.long.name.with.many.segments",
            "default",
            "long property name",
        ),
    ];

    for (prop, default, description) in &test_cases {
        let result = rsproperties::get_or(prop, default.to_string());
        assert_eq!(result, *default, "Failed for {description}");
    }

    println!(
        "✓ get_with_default edge cases passed ({} test cases)",
        test_cases.len()
    );
}

#[cfg(any(feature = "builder", target_os = "android"))]
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
        let result: Result<String, _> = rsproperties::get(prop);
        assert!(
            matches!(result, Err(Error::NotFound(_))),
            "Property '{prop}' should not exist and should return NotFound: {result:?}"
        );
    }

    println!("✓ get nonexistent properties test passed");
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_dirname_function() {
    init_properties();

    let dirname = rsproperties::properties_dir();
    let dirname_str = dirname.to_string_lossy();

    // Verify dirname returns a valid path
    assert!(!dirname_str.is_empty(), "dirname should not be empty");

    println!("✓ dirname function test passed");
    println!("  dirname = '{dirname_str}'");
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_value_length_limits() {
    // Test property value length limits
    let max_value = "x".repeat(PROP_VALUE_MAX);
    assert_eq!(max_value.len(), PROP_VALUE_MAX);

    let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
    assert_eq!(too_long_value.len(), PROP_VALUE_MAX + 1);

    init_properties();

    // Test with get_or (should work with any length default)
    let result1 = rsproperties::get_or("test.max.length", max_value.clone());
    assert_eq!(result1, max_value);

    let result2 = rsproperties::get_or("test.too.long", too_long_value.clone());
    assert_eq!(result2, too_long_value);

    println!("✓ Value length limits test passed");
}

/// Concurrent reads of a stored property all see its value.
#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_thread_safety_basic() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    init_properties();

    let expected: Arc<String> = Arc::new(
        rsproperties::get("ro.build.version.sdk").expect("ro.build.version.sdk is stored"),
    );
    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    // Spawn multiple threads
    for _ in 0..5 {
        let counter_clone = Arc::clone(&counter);
        let expected = Arc::clone(&expected);

        let handle = thread::spawn(move || {
            // Each thread performs property operations
            for _ in 0..10 {
                let prop_name = "ro.build.version.sdk";

                // Test get_or
                let result = rsproperties::get_or(prop_name, "default".to_string());
                assert_eq!(result, *expected);
                counter_clone.fetch_add(1, Ordering::SeqCst);

                // Test get
                let result: String = rsproperties::get(prop_name).unwrap();
                assert_eq!(result, *expected);
                counter_clone.fetch_add(1, Ordering::SeqCst);

                // Test dirname
                let _dirname = rsproperties::properties_dir();
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

    println!("✓ Thread safety test passed ({final_count} operations)");
}

/// Times reads of a stored property. Prints the rate only: wall-clock
/// bounds fail on loaded CI hosts without a regression
/// (`benches/props_bench.rs` measures performance).
#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_performance_baseline() {
    use std::time::Instant;

    init_properties();

    let start = Instant::now();
    let iterations = 1000;

    for _ in 0..iterations {
        let result: String = rsproperties::get("ro.build.version.sdk").unwrap();
        assert!(!result.is_empty());
    }

    let elapsed = start.elapsed();
    let ops_per_sec = iterations as f64 / elapsed.as_secs_f64();

    println!("✓ Performance baseline test completed");
    println!("  {iterations} iterations in {elapsed:?} ({ops_per_sec:.0} ops/sec)");
}

/// Names with an empty segment are absent, not parse errors.
#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_error_conditions() {
    init_properties();

    let edge_cases = [
        "",      // empty
        ".",     // just dot
        "..",    // double dot
        "...",   // triple dot
        ".test", // starts with dot
        "test.", // ends with dot
    ];

    for case in &edge_cases {
        let result: Result<String, _> = rsproperties::get(case);
        assert!(
            matches!(result, Err(Error::NotFound(_))),
            "{case:?}: {result:?}"
        );
        assert_eq!(rsproperties::get_or(case, "default".to_string()), "default");
    }
}

// Successful `set` frames are checked in `set_frame_tests.rs`; these
// inputs are rejected before `set` connects.
#[cfg(feature = "builder")]
mod builder_tests {
    use super::*;

    /// Non-`ro.` values of `PROP_VALUE_MAX` bytes or more are rejected.
    #[test]
    fn test_set_length_limits() {
        init_properties();

        for (prop, len) in [
            ("test.set.max", PROP_VALUE_MAX),
            ("test.set.long", PROP_VALUE_MAX + 1),
        ] {
            let result = rsproperties::set(prop, "x".repeat(len));
            assert!(
                matches!(result, Err(Error::InvalidArgument(_))),
                "{len} byte value: {result:?}"
            );
        }
    }
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_comprehensive_integration() {
    init_properties();

    // Integration test combining multiple features

    // 1. Test constants
    assert_eq!(PROP_VALUE_MAX, 92);
    assert_eq!(PROP_DIRNAME, "/dev/__properties__");

    // 2. Test dirname
    let dirname = rsproperties::properties_dir();
    assert!(!dirname.to_string_lossy().is_empty());

    // 3. Test get_or for multiple properties
    let props = ["int.test.1", "int.test.2", "int.test.3"];
    for (i, prop) in props.iter().enumerate() {
        let default = format!("default_{i}");
        let result = rsproperties::get_or(prop, default.clone());
        assert_eq!(result, default);
    }

    // 4. Test get for non-existent properties
    for prop in &props {
        let result: Result<String, _> = rsproperties::get(prop);
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }

    println!("✓ Comprehensive integration test passed");
    println!("  All API components working correctly together");
}
