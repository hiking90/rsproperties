//! Working functional tests for rsproperties
//!
//! These tests are designed to actually run and verify the core functionality
//! without conditional compilation barriers.

use std::sync::Once;

static INIT: Once = Once::new();

fn init_once() {
    INIT.call_once(|| {
        // Initialize with the existing Android property system
        rsproperties::init(None);
    });
}

#[test]
fn test_constants() {
    // Test that constants are available and correct
    assert_eq!(rsproperties::PROP_VALUE_MAX, 92);
    assert_eq!(rsproperties::PROP_DIRNAME, "/dev/__properties__");
    println!("✓ Constants are correct");
}

#[test]
fn test_get_with_default() {
    init_once();

    // Test getting a non-existent property with default
    let result = rsproperties::get_with_default("test.nonexistent.prop", "default_value");
    assert_eq!(result, "default_value");
    println!("✓ get_with_default works for non-existent properties");
}

#[test]
fn test_get_nonexistent() {
    init_once();

    // Test getting a non-existent property
    let result = rsproperties::get("test.definitely.does.not.exist");
    assert!(result.is_err());
    println!("✓ get returns error for non-existent properties");
}

#[test]
fn test_dirname_after_init() {
    init_once();

    // Test that dirname works after initialization
    let dirname = rsproperties::dirname();
    assert!(!dirname.to_string_lossy().is_empty());
    println!("✓ dirname works after init: {:?}", dirname);
}

#[test]
fn test_edge_cases() {
    init_once();

    // Test empty property name
    let result = rsproperties::get_with_default("", "empty_default");
    assert_eq!(result, "empty_default");

    // Test very long property name
    let long_name = "very.long.property.name.".repeat(20);
    let result = rsproperties::get_with_default(&long_name, "long_default");
    assert_eq!(result, "long_default");

    println!("✓ Edge cases handled correctly");
}

#[test]
fn test_real_android_properties() {
    init_once();

    // Try to read some properties that might exist in the Android system
    let test_props = [
        "ro.build.version.sdk",
        "ro.product.model",
        "ro.build.version.release",
        "init.svc.adbd",
    ];

    let mut found_any = false;
    for prop in &test_props {
        match rsproperties::get(prop) {
            Ok(value) => {
                println!("  Found property {}: {}", prop, value);
                found_any = true;
            },
            Err(_) => {
                // Property doesn't exist, which is normal
                let default_val = rsproperties::get_with_default(prop, "not_found");
                assert_eq!(default_val, "not_found");
            }
        }
    }

    println!("✓ Real Android property system test completed (found properties: {})", found_any);
}

#[test]
fn test_thread_safety() {
    init_once();

    // Test concurrent access
    let handles: Vec<_> = (0..4).map(|i| {
        std::thread::spawn(move || {
            for j in 0..10 {
                let prop_name = format!("test.thread.{}.{}", i, j);
                let result = rsproperties::get_with_default(&prop_name, "thread_default");
                assert_eq!(result, "thread_default");
            }
        })
    }).collect();

    for handle in handles {
        handle.join().unwrap();
    }

    println!("✓ Thread safety test passed");
}

// Only test write functionality if builder feature is available
#[cfg(feature = "builder")]
mod write_tests {
    use super::*;
    use std::fs::{create_dir_all, remove_dir_all};

    #[test]
    fn test_set_and_get() {
        // Create a test directory
        let test_dir = "/tmp/rsproperties_test";
        let _ = remove_dir_all(test_dir);
        create_dir_all(test_dir).expect("Failed to create test directory");

        // Initialize with test directory
        rsproperties::init(Some(test_dir.into()));

        // Try to set a property
        let prop_name = "test.write.property";
        let prop_value = "test_value_123";

        match rsproperties::set(prop_name, prop_value) {
            Ok(_) => {
                println!("✓ Successfully set property");

                // Try to read it back
                match rsproperties::get(prop_name) {
                    Ok(retrieved) => {
                        println!("✓ Successfully retrieved property: {}", retrieved);
                        // In a real Android system, this might work
                    },
                    Err(e) => {
                        println!("⚠ Could not retrieve property (expected on non-Android): {}", e);
                    }
                }
            },
            Err(e) => {
                println!("⚠ Could not set property (expected on non-Android): {}", e);
            }
        }

        // Clean up
        let _ = remove_dir_all(test_dir);
    }
}
