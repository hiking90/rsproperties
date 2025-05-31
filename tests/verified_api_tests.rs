//! Verified API test suite for rsproperties
//!
//! This test suite provides working tests for the rsproperties library's public API.
//! Tests are designed to work without conditional compilation restrictions.

use std::sync::Once;

static INIT: Once = Once::new();

/// Ensure the library is initialized only once across all tests
fn ensure_init() {
    INIT.call_once(|| {
        // Use the existing Android property system in __properties__
        rsproperties::init(None);
    });
}

#[test]
fn test_library_constants() {
    // Test that library constants are properly exposed and have correct values
    assert_eq!(rsproperties::PROP_VALUE_MAX, 92, "PROP_VALUE_MAX should be 92 bytes");
    assert_eq!(rsproperties::PROP_DIRNAME, "/dev/__properties__", "PROP_DIRNAME should match Android default");

    println!("✓ Library constants are correct");
    println!("  PROP_VALUE_MAX = {}", rsproperties::PROP_VALUE_MAX);
    println!("  PROP_DIRNAME = {}", rsproperties::PROP_DIRNAME);
}

#[test]
fn test_init_and_dirname() {
    ensure_init();

    // Test that dirname function works after initialization
    let dirname = rsproperties::dirname();
    assert!(!dirname.to_string_lossy().is_empty(), "dirname should not be empty after init");

    println!("✓ init() and dirname() work correctly");
    println!("  Property directory: {:?}", dirname);
}

#[test]
fn test_get_with_default_functionality() {
    ensure_init();

    // Test with non-existent property
    let result = rsproperties::get_with_default("test.nonexistent.property.12345", "default_value");
    assert_eq!(result, "default_value", "Should return default for non-existent property");

    // Test with empty property name
    let result = rsproperties::get_with_default("", "empty_default");
    assert_eq!(result, "empty_default", "Should return default for empty property name");

    println!("✓ get_with_default() works correctly");
}

#[test]
fn test_get_functionality() {
    ensure_init();

    // Test with non-existent property (should return error)
    let result = rsproperties::get("test.nonexistent.property.54321");
    assert!(result.is_err(), "Should return error for non-existent property");

    // Test with empty property name (should return error)
    let result = rsproperties::get("");
    assert!(result.is_err(), "Should return error for empty property name");

    println!("✓ get() error handling works correctly");
}

#[test]
fn test_edge_cases() {
    ensure_init();

    // Test with null byte in property name
    let result = rsproperties::get_with_default("test\0property", "null_default");
    assert_eq!(result, "null_default", "Should handle null bytes gracefully");

    // Test with very long property name
    let long_name = "a".repeat(300);
    let result = rsproperties::get_with_default(&long_name, "long_default");
    assert_eq!(result, "long_default", "Should handle long names gracefully");

    // Test with special characters
    let result = rsproperties::get_with_default("test.special.chars.!@#$%", "special_default");
    assert_eq!(result, "special_default", "Should handle special characters gracefully");

    println!("✓ Edge case handling works correctly");
}

#[test]
fn test_thread_safety() {
    ensure_init();

    // Test that multiple threads can safely call the library functions
    let handles: Vec<_> = (0..4).map(|i| {
        std::thread::spawn(move || {
            for j in 0..50 {
                let prop_name = format!("test.thread.{}.{}", i, j);
                let default_value = format!("default_{}", j);

                // Test get_with_default
                let result = rsproperties::get_with_default(&prop_name, &default_value);
                assert_eq!(result, default_value);

                // Test get (should return error for non-existent property)
                let result = rsproperties::get(&prop_name);
                assert!(result.is_err());
            }
            println!("✓ Thread {} completed successfully", i);
        })
    }).collect();

    // Wait for all threads to complete
    for handle in handles {
        handle.join().expect("Thread should complete successfully");
    }

    println!("✓ Thread safety test completed successfully");
}

#[test]
fn test_real_android_properties() {
    ensure_init();

    // Test if we can access the real Android property system        let _system_props = rsproperties::system_properties();

    // Check if the property system is initialized
    let dirname = rsproperties::dirname();
    println!("✓ Property system directory: {:?}", dirname);

    // Try to check for some common Android properties that might exist
    let common_props = [
        "ro.build.version.sdk",
        "ro.product.model",
        "ro.build.version.release",
        "ro.serialno",
    ];

    let mut found_count = 0;
    for prop in &common_props {
        match rsproperties::get(prop) {
            Ok(value) => {
                println!("  Found property {}: {}", prop, value);
                found_count += 1;
            },
            Err(_) => {
                println!("  Property {} not found (normal on non-Android systems)", prop);
            }
        }
    }

    println!("✓ Real Android property system test completed (found {} properties)", found_count);
}

#[test]
fn test_performance_basic() {
    ensure_init();

    let start = std::time::Instant::now();

    // Perform many get operations to test performance
    for i in 0..1000 {
        let prop_name = format!("test.perf.{}", i % 10);
        let _result = rsproperties::get_with_default(&prop_name, "default");
    }

    let duration = start.elapsed();
    println!("✓ Performance test: 1000 get_with_default operations took {:?}", duration);

    // Ensure it's reasonably fast (less than 1 second for 1000 operations)
    assert!(duration.as_secs() < 1, "Operations should be fast");
}

// Module for testing functions that don't interfere with global state
mod isolated_tests {
    #[test]
    fn test_constants_independent() {
        // These don't require initialization
        assert_eq!(rsproperties::PROP_VALUE_MAX, 92);
        assert_eq!(rsproperties::PROP_DIRNAME, "/dev/__properties__");
        println!("✓ Constants test (no init required)");
    }

    #[test]
    fn test_error_handling() {
        // Test library behavior before initialization
        // Note: calling dirname() before init() should panic, so we won't test that

        // Test error types - use the new_context method instead of IO variant
        let err = rsproperties::Error::new_context("test error".to_string());
        assert!(format!("{}", err).contains("test"));

        println!("✓ Error handling test completed");
    }
}
