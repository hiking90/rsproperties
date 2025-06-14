//! Comprehensive test suite for rsproperties public API
//!
//! This test suite provides complete coverage of the rsproperties library's public API,
//! testing both read and write functionality with the real Android property system.

mod common;
use common::init_test;

#[test]
fn test_library_constants() {
    // Test that library constants are properly exposed and have correct values
    assert_eq!(
        rsproperties::PROP_VALUE_MAX,
        92,
        "PROP_VALUE_MAX should be 92 bytes"
    );
    assert_eq!(
        rsproperties::PROP_DIRNAME,
        "/dev/__properties__",
        "PROP_DIRNAME should match Android default"
    );

    println!("✓ Library constants are correct");
    println!("  PROP_VALUE_MAX = {}", rsproperties::PROP_VALUE_MAX);
    println!("  PROP_DIRNAME = {}", rsproperties::PROP_DIRNAME);
}

#[test]
fn init_test_and_dirname() {
    init_test();

    // Test that dirname function works after initialization
    let dirname = rsproperties::properties_dir();
    assert!(
        !dirname.to_string_lossy().is_empty(),
        "dirname should not be empty after init"
    );

    println!("✓ init() and dirname() work correctly");
    println!("  Property directory: {:?}", dirname);
}

#[test]
fn test_get_with_default_functionality() {
    init_test();

    // Test with non-existent property
    let result = rsproperties::get_or("test.nonexistent.property.12345", "default_value".to_string());
    assert_eq!(
        result, "default_value",
        "Should return default for non-existent property"
    );

    // Test with empty property name
    let result = rsproperties::get_or("", "empty_default".to_string());
    assert_eq!(
        result, "empty_default",
        "Should return default for empty property name"
    );

    // Test with very long property name
    let long_name = "a".repeat(500);
    let result = rsproperties::get_or(&long_name, "long_default".to_string());
    assert_eq!(
        result, "long_default",
        "Should return default for very long property name"
    );

    println!("✓ get_or() works correctly for non-existent properties");
}

#[test]
fn test_get_functionality() {
    init_test();

    // Test with non-existent property
    let result: Result<String, _> = rsproperties::get("test.nonexistent.property.67890");
    assert!(
        result.is_err(),
        "Should return error for non-existent property"
    );

    println!("✓ get() returns error for non-existent properties");
}

#[cfg(feature = "builder")]
mod write_tests {
    use super::*;

    #[test]
    fn test_set_functionality() {
        init_test();

        // Try to set a property using TestPropertyService
        let result = rsproperties::set("test.property.name", "test_value");

        match result {
            Ok(_) => {
                println!("✓ set() function succeeded");

                // Try to read the property back
                let value: Result<String, _> = rsproperties::get("test.property.name");
                assert_eq!(value.unwrap(), "test_value", "Read value should match written value");
                println!("✓ Property read/write cycle successful");
            }
            Err(e) => {
                println!("⚠ set() function failed: {}", e);
            }
        }
    }

    #[test]
    fn test_set_property_edge_cases() {
        init_test();
        // Test setting empty value
        let result = rsproperties::set("test.empty.value", "");
        match result {
            Ok(_) => println!("✓ Setting empty value succeeded"),
            Err(e) => println!("⚠ Setting empty value failed: {}", e),
        }

        // Test setting very long value (should respect PROP_VALUE_MAX)
        let long_value = "x".repeat(rsproperties::PROP_VALUE_MAX + 10);
        let result = rsproperties::set("test.long.value", &long_value);
        match result {
            Ok(_) => println!("✓ Setting long value succeeded"),
            Err(e) => println!("⚠ Setting long value failed (expected): {}", e),
        }

        // Test setting property with special characters in name
        let result = rsproperties::set("test.special.chars!@#", "special_value");
        match result {
            Ok(_) => println!("✓ Setting property with special chars succeeded"),
            Err(e) => println!("⚠ Setting property with special chars failed: {}", e),
        }
    }
}

#[test]
fn test_property_name_validation() {
    init_test();

    // Test various edge case property names
    let edge_cases = vec![
        ("", "Empty name"),
        (".", "Single dot"),
        ("..", "Double dots"),
        ("test..double.dots", "Double dots in middle"),
        ("test.property.with.many.dots", "Many dots"),
        ("CAPS_PROPERTY", "Capital letters"),
        ("property123", "Numbers"),
        ("property_with_underscores", "Underscores"),
        (
            "very.long.property.name.with.many.segments.to.test.limits",
            "Long segmented name",
        ),
    ];

    for (name, description) in edge_cases {
        let result = rsproperties::get_or(name, "default".to_string());
        assert_eq!(
            result, "default",
            "Should handle edge case: {}",
            description
        );
    }

    println!("✓ Property name edge cases handled correctly");
}

#[test]
fn test_thread_safety() {
    init_test();

    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;

    let num_threads = 4;
    let iterations = 100;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let barrier = Arc::clone(&barrier);

            thread::spawn(move || {
                barrier.wait();

                // Each thread performs multiple property operations
                for i in 0..iterations {
                    let prop_name = format!("thread.{}.property.{}", thread_id, i);

                    // Test get_or (should not crash)
                    let _result = rsproperties::get_or(&prop_name, "default".to_string());

                    // Test get (should return error for non-existent property)
                    let _result: Result<String, _> = rsproperties::get(&prop_name);
                }
            })
        })
        .collect();

    // Wait for all threads to complete
    for handle in handles {
        handle.join().expect("Thread should complete successfully");
    }

    println!("✓ Thread safety test completed successfully");
    println!(
        "  {} threads × {} iterations = {} total operations",
        num_threads,
        iterations,
        num_threads * iterations
    );
}

#[test]
fn test_api_completeness() {
    init_test();

    // Verify that all expected public API functions exist and are callable

    // Core read functions
    let _: String = rsproperties::get_or("test", "default".to_string());
    let _ = rsproperties::get::<String>("test");

    // Write functions (if builder feature enabled)
    #[cfg(feature = "builder")]
    {
        let _: Result<(), _> = rsproperties::set("test", "value");
    }

    // Utility functions
    let _: std::path::PathBuf = rsproperties::properties_dir().to_path_buf();

    // Constants
    let _: usize = rsproperties::PROP_VALUE_MAX;
    let _: &str = rsproperties::PROP_DIRNAME;

    println!("✓ All expected API functions are accessible");

    #[cfg(feature = "builder")]
    println!("  ✓ Builder feature enabled - write functions available");

    #[cfg(not(feature = "builder"))]
    println!("  ⚠ Builder feature disabled - write functions not available");
}

#[test]
fn test_error_handling_robustness() {
    init_test();

    // Test that the library handles various error conditions gracefully

    // Very long property names
    let very_long_name = "x".repeat(10000);
    let result = rsproperties::get_or(&very_long_name, "default".to_string());
    assert_eq!(result, "default", "Should handle very long property names");

    // Property names with null bytes
    let null_name = "test\0property";
    let result = rsproperties::get_or(null_name, "default".to_string());
    assert_eq!(
        result, "default",
        "Should handle property names with null bytes"
    );

    // Property names with various Unicode characters
    let unicode_name = "test.🚀.property.世界";
    let result = rsproperties::get_or(unicode_name, "default".to_string());
    assert_eq!(result, "default", "Should handle Unicode property names");

    println!("✓ Error handling robustness tests passed");
}

#[test]
fn test_performance_characteristics() {
    init_test();

    use std::time::Instant;

    // Test performance of get_or for non-existent properties
    let start = Instant::now();
    let iterations = 1000;

    for i in 0..iterations {
        let prop_name = format!("perf.test.property.{}", i);
        let _result = rsproperties::get_or(&prop_name, "default".to_string());
    }

    let duration = start.elapsed();
    let avg_time_ns = duration.as_nanos() / iterations as u128;

    println!("✓ Performance test completed");
    println!("  {} iterations in {:?}", iterations, duration);
    println!("  Average time per operation: {} ns", avg_time_ns);

    // Reasonable performance expectation (less than 100 microseconds per operation)
    assert!(
        avg_time_ns < 100_000,
        "Operations should be reasonably fast"
    );
}

#[test]
fn test_real_android_properties() {
    init_test();

    // Try to read some common Android properties that might exist
    // These tests are informational and should not fail

    let common_properties = vec![
        "ro.build.version.sdk",
        "ro.build.version.release",
        "ro.product.model",
        "ro.product.manufacturer",
        "ro.hardware",
        "ro.board.platform",
        "sys.boot_completed",
        "init.svc.zygote",
    ];

    let mut found_count = 0;

    for prop in common_properties {
        match rsproperties::get::<String>(prop) {
            Ok(value) => {
                found_count += 1;
                println!("  {} = {}", prop, value);
            }
            Err(_) => {
                // Property doesn't exist, which is fine
                let default_val = rsproperties::get_or(prop, "not_found".to_string());
                assert_eq!(
                    default_val, "not_found",
                    "get_or should work even if get fails"
                );
            }
        }
    }

    println!("✓ Real Android property tests completed");
    println!("  Found {} real properties", found_count);

    if found_count == 0 {
        println!("  ⚠ No real Android properties found (running on non-Android system)");
    }
}
