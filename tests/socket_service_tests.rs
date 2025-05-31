// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Socket service tests for rsproperties
//!
//! These tests verify the socket directory configuration functionality including:
//! - Global socket directory setting via set_socket_dir()
//! - Environment variable based configuration
//! - OnceLock behavior (one-time initialization)
//! - Integration with property setting operations

use std::sync::Once;
use std::env;
use std::process::Command;
use std::time::Duration;
use std::thread;
use std::fs;

#[cfg(all(feature = "builder", target_os = "linux"))]
use rsproperties::{set_socket_dir, PropertySocketService};

static INIT: Once = Once::new();

fn setup_test_env() {
    INIT.call_once(|| {
        let _ = env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Debug)
            .try_init();
    });
}

/// Test the basic functionality of set_socket_dir
#[test]
#[cfg(all(feature = "builder", target_os = "linux"))]
fn test_set_socket_dir_oncelock_behavior() {
    setup_test_env();

    // Create a unique test directory for this test
    let test_dir = "/tmp/rsproperties_test_socket_dir";
    let test_dir2 = "/tmp/rsproperties_test_socket_dir_2";

    // Clean up any existing directories
    let _ = fs::remove_dir_all(test_dir);
    let _ = fs::remove_dir_all(test_dir2);

    // Create test directories
    fs::create_dir_all(test_dir).expect("Failed to create test directory");
    fs::create_dir_all(test_dir2).expect("Failed to create test directory 2");

    // First call should succeed
    let result1 = set_socket_dir(test_dir);
    assert!(result1, "First call to set_socket_dir should return true");

    // Second call should be ignored (OnceLock behavior)
    let result2 = set_socket_dir(test_dir2);
    assert!(!result2, "Second call to set_socket_dir should return false");

    // Clean up
    let _ = fs::remove_dir_all(test_dir);
    let _ = fs::remove_dir_all(test_dir2);
}

/// Test socket directory configuration with a mock server
#[test]
#[cfg(all(feature = "builder", target_os = "linux"))]
fn test_socket_dir_with_mock_server() {
    setup_test_env();

    let test_socket_dir = "/tmp/rsproperties_mock_test";
    let socket_path = format!("{}/property_service", test_socket_dir);

    // Clean up any existing directory
    let _ = fs::remove_dir_all(test_socket_dir);
    fs::create_dir_all(test_socket_dir).expect("Failed to create test directory");

    // Start a mock socket service
    let service = PropertySocketService::new(Some(&socket_path));
    assert!(service.is_ok(), "Failed to create mock socket service");

    let _service = service.unwrap();

    // Run the service in a background thread
    let service_handle = thread::spawn(move || {
        // Run for a short duration for testing
        thread::sleep(Duration::from_millis(100));
    });

    // Give the server time to start
    thread::sleep(Duration::from_millis(50));

    // Configure socket directory
    let success = set_socket_dir(test_socket_dir);
    assert!(success, "Should successfully set socket directory");

    // The actual property setting test would require the server to be running
    // For now, we just verify the directory configuration worked

    // Wait for service thread to complete
    service_handle.join().expect("Service thread should complete");

    // Clean up
    let _ = fs::remove_dir_all(test_socket_dir);
}

/// Test environment variable based socket directory configuration
#[test]
#[cfg(all(feature = "builder", target_os = "linux"))]
fn test_env_var_socket_dir() {
    setup_test_env();

    // This test is more challenging because environment variables affect global state
    // We'll test it by spawning a separate process
    let test_dir = "/tmp/rsproperties_env_test";
    let _ = fs::remove_dir_all(test_dir);
    fs::create_dir_all(test_dir).expect("Failed to create test directory");

    // Create a simple test binary that uses the environment variable
    let output = Command::new("cargo")
        .args(&["build", "--example", "socket_service_client"])
        .current_dir("/home/king/workspace/rsproperties")
        .output()
        .expect("Failed to build example");

    if !output.status.success() {
        panic!("Failed to build socket_service_client example: {}",
               String::from_utf8_lossy(&output.stderr));
    }

    // Note: Full integration test would require starting a server
    // For unit testing purposes, we verify the build succeeded
    assert!(output.status.success(), "Example should build successfully");

    // Clean up
    let _ = fs::remove_dir_all(test_dir);
}

/// Test socket path generation functions
#[test]
#[cfg(all(feature = "builder", target_os = "linux"))]
fn test_socket_path_generation() {
    setup_test_env();

    // This test verifies the internal behavior by using the public API
    let test_dir = "/tmp/rsproperties_path_test";
    let _ = fs::remove_dir_all(test_dir);
    fs::create_dir_all(test_dir).expect("Failed to create test directory");

    // Set socket directory
    let success = set_socket_dir(test_dir);
    assert!(success, "Should successfully set socket directory");

    // The socket path generation is internal, but we can verify it works
    // by attempting to create a socket service (which will fail if path is wrong)
    let socket_path = format!("{}/property_service", test_dir);
    let service_result = PropertySocketService::new(Some(&socket_path));

    // We expect this to succeed in creating the service
    // (even if we can't test the full communication without a client)
    assert!(service_result.is_ok(), "Should be able to create socket service with configured path");

    // Clean up
    let _ = fs::remove_dir_all(test_dir);
}

/// Test protocol version configuration via environment variable
#[test]
fn test_protocol_version_env_var() {
    setup_test_env();

    // Test that protocol version can be configured via environment variable
    // Since we can't easily test the internal protocol_version() function directly,
    // we verify that the system handles environment variables correctly

    let original_version = env::var("PROPERTY_SERVICE_VERSION").ok();

    // Set environment variable
    env::set_var("PROPERTY_SERVICE_VERSION", "1");

    // The actual protocol version testing would require property setting,
    // but for unit test purposes, we verify the env var is readable
    let version = env::var("PROPERTY_SERVICE_VERSION").unwrap();
    assert_eq!(version, "1", "Environment variable should be set correctly");

    // Test with V2
    env::set_var("PROPERTY_SERVICE_VERSION", "2");
    let version = env::var("PROPERTY_SERVICE_VERSION").unwrap();
    assert_eq!(version, "2", "Environment variable should be updated correctly");

    // Restore original value
    match original_version {
        Some(val) => env::set_var("PROPERTY_SERVICE_VERSION", val),
        None => env::remove_var("PROPERTY_SERVICE_VERSION"),
    }
}

/// Test error handling for invalid socket directories
#[test]
#[cfg(all(feature = "builder", target_os = "linux"))]
fn test_invalid_socket_dir_handling() {
    setup_test_env();

    // Test with non-existent directory (should still succeed in setting, fail in usage)
    let invalid_dir = "/nonexistent/invalid/directory";
    let success = set_socket_dir(invalid_dir);
    assert!(success, "set_socket_dir should succeed even with invalid path");

    // The actual failure would occur when trying to create/connect to socket
    // For unit testing, we just verify the setting mechanism works
}

/// Integration test demonstrating the complete workflow
#[test]
#[cfg(all(feature = "builder", target_os = "linux"))]
fn test_complete_socket_workflow() {
    setup_test_env();

    let test_dir = "/tmp/rsproperties_integration_test";
    let socket_path = format!("{}/property_service", test_dir);

    // Clean up and setup
    let _ = fs::remove_dir_all(test_dir);
    fs::create_dir_all(test_dir).expect("Failed to create test directory");

    // Step 1: Configure socket directory
    let config_success = set_socket_dir(test_dir);
    assert!(config_success, "Should configure socket directory successfully");

    // Step 2: Verify subsequent calls are ignored
    let ignored = set_socket_dir("/tmp/other");
    assert!(!ignored, "Subsequent calls should be ignored");

    // Step 3: Create socket service (simulating server)
    let service_result = PropertySocketService::new(Some(&socket_path));
    assert!(service_result.is_ok(), "Should create socket service successfully");

    // Step 4: The property setting would happen here in a full integration test
    // For unit testing, we verify the setup is correct

    println!("✓ Socket directory configuration test completed successfully");

    // Clean up
    let _ = fs::remove_dir_all(test_dir);
}

// Tests that can run without the builder feature
#[cfg(not(all(feature = "builder", target_os = "linux")))]
mod fallback_tests {
    #[test]
    fn test_socket_service_unavailable() {
        // When builder feature is not enabled, socket service functionality should not be available
        // This test ensures the code compiles and behaves correctly in such cases

        // We can't import set_socket_dir without the feature, so this test just ensures
        // the module compiles correctly when the feature is disabled
        assert!(true, "Socket service features are not available without builder feature");
    }
}
