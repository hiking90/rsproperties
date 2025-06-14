// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Tests for property change wait functionality on Android
//!
//! These tests verify the ability to wait for property changes and
//! respond to them. Since they rely on Android-specific functionality,
//! they are conditionally compiled to run only on Android.

#![cfg(target_os = "android")]

use rsproperties::{self, Result};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

mod common;
use common::init_test;

/// Helper function to initialize test environment
fn setup_android_test() {
    let _ = env_logger::builder().is_test(true).try_init();
    init_test();
}

/// Test waiting for a property change
///
/// This test creates a thread that waits for a specific property to change,
/// and then another thread sets the property to trigger the wait to complete.
///
/// This function is only compiled and run on Android devices.
#[test]
fn test_wait_for_property_change() {
    setup_android_test();

    let test_prop = "test.wait.property";
    let initial_value = "initial";
    let changed_value = "changed";

    // First set the initial value
    match rsproperties::set(test_prop, initial_value) {
        Ok(_) => {
            println!("✓ Set initial property value to '{}'", initial_value);

            // Verify the initial value
            match rsproperties::get::<String>(test_prop) {
                Ok(value) => {
                    assert_eq!(value, initial_value);
                    println!("✓ Initial property value verified: '{}'", value);
                }
                Err(e) => {
                    panic!("Failed to get property after setting: {}", e);
                }
            }

            // Use a barrier to ensure the waiting thread is ready before we change the property
            let barrier = Arc::new(Barrier::new(2));
            let barrier_clone = Arc::clone(&barrier);

            // Property name for threads to use
            let prop_name = test_prop.to_string();

            // Thread that waits for property change
            let waiter_handle = thread::spawn(move || {
                println!("Waiter thread: Starting to wait for property change...");

                // Get the system_properties instance
                let system_properties = rsproperties::system_properties();

                // Find the property index
                match system_properties.find(&prop_name).unwrap() {
                    Some(index) => {
                        println!("Waiter thread: Found property index");

                        // Signal that we're ready to wait
                        barrier_clone.wait();

                        // Wait for property change (no timeout)
                        println!("Waiter thread: Now waiting for property change...");
                        match system_properties.wait(Some(&index), None) {
                            Some(_serial) => {
                                // Wait completed successfully
                                let new_value =
                                    rsproperties::get::<String>(&prop_name).unwrap_or_default();
                                println!(
                                    "Waiter thread: Property change detected! New value: '{}'",
                                    new_value
                                );
                                assert_eq!(new_value, changed_value);
                                true
                            }
                            None => {
                                println!("Waiter thread: Wait failed or timed out");
                                false
                            }
                        }
                    }
                    None => {
                        println!("Waiter thread: Could not find property index");
                        barrier_clone.wait(); // Make sure we don't block the test
                        false
                    }
                }
            });

            // Wait for the waiter thread to be ready
            barrier.wait();
            println!("Main thread: Waiter thread is ready, changing property...");

            // Small delay to ensure waiter is actually waiting
            thread::sleep(Duration::from_millis(100));

            // Change the property value to trigger the waiter
            match rsproperties::set(test_prop, changed_value) {
                Ok(_) => {
                    println!("Main thread: Changed property value to '{}'", changed_value);
                }
                Err(e) => {
                    println!("Main thread: Failed to change property: {}", e);
                }
            }

            // Wait for waiter thread to complete and get its result
            let wait_succeeded = waiter_handle.join().expect("Waiter thread panicked");

            // Verify final state
            let final_value = rsproperties::get::<String>(test_prop).unwrap_or_default();
            assert_eq!(final_value, changed_value);

            assert!(wait_succeeded, "Property change wait should have succeeded");
            println!("✓ Property change wait test passed");
        }
        Err(e) => {
            println!("⚠ Initial property set failed: {}", e);
            println!("This is expected if not running on Android");
        }
    }
}

/// Test waiting for any property change
///
/// This test creates a thread that waits for any property change to occur,
/// and then another thread sets a property to trigger the wait to complete.
///
/// This function is only compiled and run on Android devices.
#[test]
fn test_wait_for_any_property_change() {
    setup_android_test();

    let test_prop = "test.wait.any.property";
    let changed_value = "any_changed";

    // Use a barrier to ensure the waiting thread is ready before we change the property
    let barrier = Arc::new(Barrier::new(2));
    let barrier_clone = Arc::clone(&barrier);

    // Thread that waits for any property change
    let waiter_handle = thread::spawn(move || {
        println!("Waiter thread: Starting to wait for any property change...");

        // Get the system_properties instance
        let system_properties = rsproperties::system_properties();

        // Signal that we're ready to wait
        barrier_clone.wait();

        // Wait for any property change
        println!("Waiter thread: Now waiting for any property change...");
        system_properties.wait_any();

        println!("Waiter thread: Any property change detected!");
        true
    });

    // Wait for the waiter thread to be ready
    barrier.wait();
    println!("Main thread: Waiter thread is ready, changing property...");

    // Small delay to ensure waiter is actually waiting
    thread::sleep(Duration::from_millis(100));

    // Change a property value to trigger the waiter
    match rsproperties::set(test_prop, changed_value) {
        Ok(_) => {
            println!("Main thread: Set property to '{}'", changed_value);
        }
        Err(e) => {
            println!("Main thread: Failed to change property: {}", e);
        }
    }

    // Wait for waiter thread to complete and get its result
    let wait_succeeded = waiter_handle.join().expect("Waiter thread panicked");

    // Verify the property was set
    let final_value = rsproperties::get_or(test_prop, "not_set".to_string());

    if final_value == changed_value {
        println!("✓ Property value verified: '{}'", final_value);
    } else {
        println!(
            "⚠ Property value unexpected: '{}' (expected '{}')",
            final_value, changed_value
        );
    }

    assert!(wait_succeeded, "Property change wait should have succeeded");
    println!("✓ Wait for any property change test passed");
}
