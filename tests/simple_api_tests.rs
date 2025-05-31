// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Simple integration tests for rsproperties public API
//!
//! These tests verify basic functionality without requiring
//! a fully initialized property system.

use rsproperties::{PROP_VALUE_MAX, PROP_DIRNAME};

/// Test that API constants are properly exposed
#[test]
fn test_api_constants() {
    // Verify constants match Android system property limits
    assert_eq!(PROP_VALUE_MAX, 92, "PROP_VALUE_MAX should be 92 bytes as per Android");
    assert_eq!(PROP_DIRNAME, "/dev/__properties__", "PROP_DIRNAME should match Android default");

    println!("✓ API constants are correctly exposed");
    println!("  PROP_VALUE_MAX = {}", PROP_VALUE_MAX);
    println!("  PROP_DIRNAME = {}", PROP_DIRNAME);
}

/// Test get_with_default function with non-existent properties
#[test]
fn test_get_with_default_basic() {
    // Initialize with an unlikely-to-exist path
    rsproperties::init(Some("/tmp/nonexistent_test_props".into()));

    // This should return the default value since the property system isn't set up
    let result = rsproperties::get_with_default("test.nonexistent.property", "default_value");
    assert_eq!(result, "default_value");

    println!("✓ get_with_default returns default for non-existent properties");
}

/// Test get function with non-existent properties
#[test]
fn test_get_basic() {
    // Initialize with an unlikely-to-exist path
    rsproperties::init(Some("/tmp/nonexistent_test_props2".into()));

    // This should return an error since the property system isn't set up
    let result = rsproperties::get("test.nonexistent.property");
    assert!(result.is_err(), "get should return error for non-existent properties");

    println!("✓ get returns error for non-existent properties");
}

/// Test dirname function
#[test]
fn test_dirname_function() {
    // Note: Due to OnceLock, this might return a previously set value
    let dirname = rsproperties::dirname();
    assert!(!dirname.to_string_lossy().is_empty(), "dirname should not be empty");

    println!("✓ dirname function works, returns: {:?}", dirname);
}

#[cfg(feature = "builder")]
mod builder_tests {
    use super::*;
    use std::fs::{create_dir_all, remove_dir_all};
    use std::path::Path;

    /// Test basic setting functionality (will likely fail without proper setup)
    #[test]
    fn test_set_basic() {
        let test_dir = "/tmp/test_set_basic";
        let _ = remove_dir_all(test_dir);
        let _ = create_dir_all(test_dir);

        // Try to set a property (this may fail without proper property system setup)
        let result = rsproperties::set("test.basic.property", "test_value");

        // We don't assert success here since it requires a full property system
        match result {
            Ok(_) => println!("✓ set function succeeded"),
            Err(e) => println!("⚠ set function failed (expected): {}", e),
        }

        let _ = remove_dir_all(test_dir);
    }
}

/// Test API stability - verify that the expected public API is available
#[test]
fn test_api_availability() {
    // This test ensures that all expected functions exist and are callable

    // Test that all expected functions exist
    let _result1: String = rsproperties::get_with_default("test", "default");
    let _result2: Result<String, _> = rsproperties::get("test");

    #[cfg(feature = "builder")]
    {
        let _result3: Result<(), _> = rsproperties::set("test", "value");
    }

    // Test that constants are accessible
    let _max: usize = rsproperties::PROP_VALUE_MAX;
    let _dirname: &str = rsproperties::PROP_DIRNAME;

    println!("✓ All expected API functions are available");
}

/// Test error message quality
#[test]
fn test_error_messages() {
    // Initialize with non-existent path
    rsproperties::init(Some("/tmp/definitely_nonexistent".into()));

    // Test that error contains useful information
    if let Err(e) = rsproperties::get("nonexistent.property") {
        let error_msg = format!("{}", e);
        assert!(!error_msg.is_empty(), "Error message should not be empty");
        println!("✓ Error message: {}", error_msg);
    }
}

/// Test edge cases with property names
#[test]
fn test_property_name_edge_cases() {
    // Initialize with non-existent path
    rsproperties::init(Some("/tmp/edge_case_test".into()));

    let long_name = "a".repeat(1000);
    let edge_case_names = vec![
        "",                                    // Empty name
        &long_name,                           // Very long name
        "name.with.dots",                     // Dots
        "name_with_underscores",              // Underscores
        "name123with456numbers",              // Numbers
    ];

    for name in edge_case_names {
        let result = rsproperties::get_with_default(name, "default");
        // Should not crash and should return default
        assert_eq!(result, "default", "Should return default for edge case name");
    }

    println!("✓ Edge case property names handled gracefully");
}

/// Test that the library handles concurrent access without crashing
#[test]
fn test_basic_thread_safety() {
    use std::thread;
    use std::sync::Arc;
    use std::sync::Barrier;

    // Initialize once
    rsproperties::init(Some("/tmp/thread_safety_test".into()));

    let num_threads = 4;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads).map(|thread_id| {
        let barrier = Arc::clone(&barrier);

        thread::spawn(move || {
            barrier.wait();

            // Each thread tries to read properties
            for i in 0..10 {
                let prop_name = format!("thread.{}.prop.{}", thread_id, i);
                let _result = rsproperties::get_with_default(&prop_name, "default");
            }
        })
    }).collect();

    for handle in handles {
        handle.join().unwrap();
    }

    println!("✓ Basic thread safety test passed");
}
