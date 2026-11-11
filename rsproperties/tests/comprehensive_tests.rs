// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Comprehensive tests for rsproperties using real Android property system
//!
//! On a host these tests read a store this binary builds from the
//! `tests/android` property contexts and build.prop files (see
//! `tests/common/mod.rs`); on Android they read the device store.

#[cfg(any(feature = "builder", target_os = "android"))]
use rsproperties::Error;
use rsproperties::{wire::PROP_VALUE_MAX, PROP_DIRNAME};

mod common;
#[cfg(any(feature = "builder", target_os = "android"))]
use common::init_test as ensure_init;

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

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_get_with_default_functionality() {
    ensure_init();

    // Test getting a property that definitely doesn't exist
    let result = rsproperties::get_or(
        "test.nonexistent.property.12345",
        "default_value".to_string(),
    );
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
        assert_eq!(result, *default, "Should return default for {prop}");
    }

    println!("✓ get_or functionality works correctly");
}

#[cfg(any(feature = "builder", target_os = "android"))]
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
            matches!(result, Err(Error::NotFound(_))),
            "Getting non-existent property '{prop}' should return NotFound: {result:?}"
        );
    }

    println!("✓ get returns errors for non-existent properties");
}

#[cfg(any(feature = "builder", target_os = "android"))]
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
        "dirname should be a valid path, got: {dirname_str}"
    );

    println!("✓ dirname function returns: {dirname_str}");
}

#[cfg(any(feature = "builder", target_os = "android"))]
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
        println!("Tested property name: {name}");
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
        // An empty segment never names a stored property (bionic parity).
        assert!(
            matches!(result, Err(Error::NotFound(_))),
            "invalid name {name:?}: {result:?}"
        );
    }

    println!("✓ Property name validation tests completed");
}

#[cfg(any(feature = "builder", target_os = "android"))]
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
                let prop_name = format!("test.thread.{i}.{j}");

                // Test get_or
                let _result: String = rsproperties::get_or(&prop_name, "default".to_string());
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

    println!("✓ Thread safety test completed with {final_count} operations");
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_property_value_length_constraints() {
    let max_length_value = "x".repeat(PROP_VALUE_MAX);
    let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);

    // Test using these values with get_with_default
    ensure_init();

    let result1 = rsproperties::get_or("test.max.length", max_length_value.clone());
    assert_eq!(result1, max_length_value);

    let result2 = rsproperties::get_or("test.too.long", too_long_value.clone());
    assert_eq!(result2, too_long_value);

    println!("✓ Property value length constraint tests passed");
}

// Successful `set` frames are checked in `set_frame_tests.rs`; these
// inputs are rejected before `set` connects.
#[cfg(feature = "builder")]
mod builder_tests {
    use super::*;

    /// A non-`ro.` value must be shorter than `PROP_VALUE_MAX`.
    #[test]
    fn test_set_property_max_length() {
        ensure_init();

        let max_value = "x".repeat(PROP_VALUE_MAX);
        let result = rsproperties::set("test.max.length", &max_value);
        assert!(
            matches!(result, Err(Error::InvalidArgument(_))),
            "{PROP_VALUE_MAX} byte value: {result:?}"
        );
    }

    #[test]
    fn test_set_property_too_long() {
        ensure_init();

        // Test setting property with value that exceeds maximum length
        let too_long_value = "x".repeat(PROP_VALUE_MAX + 1);
        let result = rsproperties::set("test.too.long", &too_long_value);

        assert!(
            matches!(result, Err(Error::InvalidArgument(_))),
            "Setting property with value too long should fail: {result:?}"
        );
    }
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_error_handling() {
    ensure_init();

    // A 508-byte name: no length limit on the read path, just absent.
    let long_name = "test.".repeat(100) + "property";
    let result: Result<String, _> = rsproperties::get(&long_name);
    assert!(
        matches!(result, Err(Error::NotFound(_))),
        "long name: {result:?}"
    );

    let result: Result<String, _> = rsproperties::get("");
    assert!(
        matches!(result, Err(Error::NotFound(_))),
        "empty name: {result:?}"
    );
}

/// Host: values from `tests/android/*.prop`, loaded by `ensure_init`.
#[cfg(all(feature = "builder", not(target_os = "android")))]
#[test]
fn test_real_android_properties() {
    ensure_init();

    assert_eq!(
        rsproperties::get::<String>("ro.build.version.sdk").unwrap(),
        "34"
    );
    assert_eq!(
        rsproperties::get::<String>("ro.build.version.release").unwrap(),
        "14"
    );
    for prop in [
        "ro.product.model",
        "sys.boot_completed",
        "persist.sys.timezone",
    ] {
        let result = rsproperties::get::<String>(prop);
        assert!(
            matches!(result, Err(Error::NotFound(_))),
            "{prop}: {result:?}"
        );
        assert_eq!(
            rsproperties::get_or(prop, "not_found".to_string()),
            "not_found"
        );
    }
}

/// Android: every device sets these.
#[cfg(target_os = "android")]
#[test]
fn test_real_android_properties() {
    ensure_init();

    for prop in ["ro.build.version.sdk", "ro.build.version.release"] {
        let value: String = rsproperties::get(prop).unwrap_or_else(|e| panic!("{prop}: {e}"));
        assert!(!value.is_empty(), "{prop} is empty");
    }
}
