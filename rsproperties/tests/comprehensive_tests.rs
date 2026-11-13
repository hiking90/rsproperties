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

/// The directory `ensure_init` leaves configured: on a host this binary's
/// store, `temp_dir()/rsprops_it_comprehensive_tests_<pid>`
/// (`tests/common/mod.rs`); on Android the default directory, because
/// `ensure_init` does nothing there.
#[cfg(any(feature = "builder", target_os = "android"))]
fn expected_properties_dir() -> std::path::PathBuf {
    if cfg!(target_os = "android") {
        std::path::PathBuf::from(PROP_DIRNAME)
    } else {
        std::env::temp_dir().join(format!(
            "rsprops_it_comprehensive_tests_{}",
            std::process::id()
        ))
    }
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_dirname_function() {
    ensure_init();

    let dirname = rsproperties::properties_dir();
    let dirname_str = dirname.to_string_lossy();

    // Should not be empty
    assert!(!dirname_str.is_empty(), "dirname should not be empty");

    // Should be the directory `ensure_init` configured
    assert_eq!(
        dirname,
        expected_properties_dir(),
        "dirname should be the configured properties directory"
    );

    println!("✓ dirname function returns: {dirname_str}");
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_property_name_validation() {
    ensure_init();

    // Test various property name formats
    // The second field is the value in the host store: of these names,
    // `tests/android/*.prop` set only `ro.build.version.sdk`. `None` is a
    // name the store does not hold.
    let valid_names = [
        ("ro.build.version.sdk", Some("34")),
        ("sys.boot_completed", None),
        ("persist.sys.timezone", None),
        ("test.property", None),
        ("a.b.c.d.e.f.g", None),
        ("prop123", None),
    ];

    for (name, stored) in &valid_names {
        let result: Result<String, _> = rsproperties::get(name);
        if cfg!(target_os = "android") {
            // A device may or may not set these, but a well-formed name is
            // either read or reported as absent.
            assert!(
                matches!(result, Ok(_) | Err(Error::NotFound(_))),
                "valid name {name:?}: {result:?}"
            );
        } else {
            match stored {
                Some(value) => assert_eq!(result.as_deref().ok(), Some(*value), "{name}"),
                None => assert!(
                    matches!(result, Err(Error::NotFound(_))),
                    "valid name {name:?}: {result:?}"
                ),
            }
        }
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
            let expected_dir = expected_properties_dir();

            // Each thread performs multiple property operations
            for j in 0..5 {
                // No `test.thread.*` property is stored.
                let prop_name = format!("test.thread.{i}.{j}");

                // Test get_or
                let result: String = rsproperties::get_or(&prop_name, "default".to_string());
                assert_eq!(result, "default", "{prop_name}");
                counter_clone.fetch_add(1, Ordering::SeqCst);

                // Test get
                let result: Result<String, _> = rsproperties::get(&prop_name);
                assert!(
                    matches!(result, Err(Error::NotFound(_))),
                    "{prop_name}: {result:?}"
                );
                counter_clone.fetch_add(1, Ordering::SeqCst);

                // Test dirname
                let dirname = rsproperties::properties_dir();
                assert_eq!(dirname, expected_dir);
                counter_clone.fetch_add(1, Ordering::SeqCst);
            }
        });
        handles.push(handle);
    }

    // Wait for all threads to complete
    for handle in handles {
        handle.join().expect("Thread should complete successfully");
    }

    // Each increment follows a checked result, so the count is the number
    // of verified operations.
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
    let max_length_value = "x".repeat(PROP_VALUE_MAX - 1);
    let too_long_value = "x".repeat(PROP_VALUE_MAX);

    ensure_init();

    // A non-`ro.` value of PROP_VALUE_MAX bytes is rejected before `set`
    // connects. On a host, a `set` that went on to the socket would
    // return `Error::Io` instead (see below).
    let result = rsproperties::set("test.too.long", &too_long_value);
    assert!(
        matches!(result, Err(Error::InvalidArgument(_))),
        "{PROP_VALUE_MAX} byte value: {result:?}"
    );

    // A value of PROP_VALUE_MAX - 1 bytes passes the length check and `set`
    // goes on to the property service socket.
    let result = rsproperties::set("test.max.length", &max_length_value);

    // Host: no property service runs on this binary's store (its socket
    // dir is never created), so the request fails at the socket. The
    // rsproperties-service tests check the stored value against a service.
    #[cfg(not(target_os = "android"))]
    assert!(
        matches!(result, Err(Error::Io(_))),
        "{} byte value: {result:?}",
        PROP_VALUE_MAX - 1
    );

    // Android: init answers. It stores the value, or refuses the name for
    // the caller's SELinux domain.
    #[cfg(target_os = "android")]
    match result {
        Ok(()) => assert_eq!(
            rsproperties::get::<String>("test.max.length").unwrap(),
            max_length_value
        ),
        Err(e) => assert!(
            matches!(e, Error::SetRejected { .. }),
            "{} byte value: {e:?}",
            PROP_VALUE_MAX - 1
        ),
    }

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
