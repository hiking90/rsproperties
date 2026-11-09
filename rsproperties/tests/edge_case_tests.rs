// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! API compatibility and edge case tests for rsproperties
//!
//! These tests verify edge cases, error conditions, and API stability
//! to ensure robust behavior under unusual conditions.

#[cfg(any(feature = "builder", target_os = "android"))]
use rsproperties::Error;
use rsproperties::{self, PROP_DIRNAME, PROP_VALUE_MAX};

mod common;
#[cfg(any(feature = "builder", target_os = "android"))]
use common::init_test;

#[cfg(any(feature = "builder", target_os = "android"))]
fn setup_edge_test_env() {
    let _ = env_logger::builder().is_test(true).try_init();
    init_test();
}

/// Test API constants are properly exposed and have expected values
#[test]
fn test_api_constants() {
    // Verify constants match Android system property limits
    assert_eq!(
        PROP_VALUE_MAX, 92,
        "PROP_VALUE_MAX should be 92 bytes as per Android"
    );
    assert_eq!(
        PROP_DIRNAME, "/dev/__properties__",
        "PROP_DIRNAME should match Android default"
    );
}

/// Test empty and whitespace property names
#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_empty_property_names() {
    setup_edge_test_env();

    // Test empty property name
    let result: Result<String, _> = rsproperties::get("");
    assert!(
        matches!(result, Err(Error::NotFound(_))),
        "Getting empty property name should be NotFound: {result:?}"
    );

    let default_result = rsproperties::get_or("", "default".to_string());
    assert_eq!(
        default_result, "default",
        "Empty property should return default"
    );

    // Test whitespace-only property names
    let whitespace_names = vec![" ", "\t", "\n", "  ", "\t\n  "];
    for name in whitespace_names {
        let result = rsproperties::get_or(name, "default".to_string());
        assert_eq!(
            result,
            "default",
            "Whitespace property '{}' should return default",
            name.escape_debug()
        );
    }
}

/// Test property names with special characters
#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_special_character_property_names() {
    setup_edge_test_env();

    let special_names = vec![
        "prop.with.dots",
        "prop_with_underscores",
        "prop-with-dashes",
        "prop123with456numbers",
        "PROP.WITH.UPPERCASE",
        "prop.with.MixedCase",
    ];

    for name in special_names {
        let result = rsproperties::get_or(name, "not_found".to_string());
        // These should not crash and should return the default
        assert_eq!(
            result, "not_found",
            "Special property name '{name}' should return default"
        );
    }
}

/// Test property names with invalid characters (if any restrictions exist)
#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_invalid_property_names() {
    setup_edge_test_env();

    let potentially_invalid_names = vec![
        "prop with spaces",
        "prop\twith\ttabs",
        "prop\nwith\nnewlines",
        "prop=with=equals",
        "prop:with:colons",
        "prop;with;semicolons",
        "prop/with/slashes",
        "prop\\with\\backslashes",
        "prop\"with\"quotes",
        "prop'with'singles",
        "prop<with>brackets",
        "prop{with}braces",
        "prop(with)parens",
        "prop[with]squares",
        "prop|with|pipes",
        "prop&with&ampersands",
        "prop*with*asterisks",
        "prop?with?questions",
        "prop!with!exclamations",
        "prop@with@ats",
        "prop#with#hashes",
        "prop$with$dollars",
        "prop%with%percents",
        "prop^with^carets",
        "prop+with+pluses",
    ];

    for name in potentially_invalid_names {
        // Test that these don't crash the system
        let result = rsproperties::get_or(name, "default".to_string());
        println!("Property name '{}' -> '{}'", name.escape_debug(), result);
        // Should at least return the default without crashing
        assert_eq!(result, "default");
    }
}

/// Test very long property names
#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_long_property_names() {
    setup_edge_test_env();

    let lengths = vec![100, 500, 1000, 2000];

    for length in lengths {
        let long_name = "a".repeat(length);
        let result = rsproperties::get_or(&long_name, "default".to_string());
        assert_eq!(
            result, "default",
            "Long property name ({length} chars) should return default"
        );

        // Test that we don't crash on extremely long names
        println!("Tested property name of length {length}");
    }
}

// `set` cases that should succeed (empty, whitespace, control and UTF-8
// values, values up to `PROP_VALUE_MAX - 1` bytes) are checked frame by
// frame against a fake service in `set_frame_tests.rs`. The cases below
// are rejected before `set` connects, so they need no service.

/// Values of `PROP_VALUE_MAX` bytes or more are rejected for non-`ro.`
/// names.
#[test]
#[cfg(feature = "builder")]
fn test_maximum_length_values() {
    setup_edge_test_env();

    for length in [PROP_VALUE_MAX, PROP_VALUE_MAX + 10] {
        let result = rsproperties::set("edge.oversized", &"x".repeat(length));
        assert!(
            matches!(result, Err(Error::InvalidArgument(_))),
            "{length} byte value: {result:?}"
        );
    }
}

/// Test error propagation and handling
#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_error_handling() {
    setup_edge_test_env();

    // Test get on non-existent property
    let result: Result<String, _> = rsproperties::get("definitely.does.not.exist.anywhere");
    assert!(
        matches!(result, Err(Error::NotFound(_))),
        "Should return NotFound for non-existent property: {result:?}"
    );

    // Test that error contains useful information
    if let Err(e) = result {
        let error_msg = format!("{e}");
        assert!(!error_msg.is_empty(), "Error message should not be empty");
        println!("Error message for non-existent property: {error_msg}");
    }

    let result = rsproperties::set("", "some_value");
    assert!(
        matches!(result, Err(Error::InvalidArgument(_))),
        "empty property name: {result:?}"
    );
}

/// A NUL byte in a name or value is rejected: the service reads both as C
/// strings, so it would set a truncated name or value.
#[test]
#[cfg(feature = "builder")]
fn test_null_bytes_and_special_chars() {
    setup_edge_test_env();

    for (prop_name, prop_value) in [
        ("prop.with.null", "value\0with\0nulls"),
        ("prop.with\0null", "value"),
    ] {
        let result = rsproperties::set(prop_name, prop_value);
        assert!(
            matches!(result, Err(Error::InvalidArgument(_))),
            "{prop_name:?} = {prop_value:?}: {result:?}"
        );
    }
}
