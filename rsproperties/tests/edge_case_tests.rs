// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! API compatibility and edge case tests for rsproperties
//!
//! These tests verify edge cases, error conditions, and API stability
//! to ensure robust behavior under unusual conditions.

use rsproperties::{self, PROP_DIRNAME, PROP_VALUE_MAX};

mod common;
use common::init_test;

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
#[test]
fn test_empty_property_names() {
    setup_edge_test_env();

    // Test empty property name
    let result: Result<String, _> = rsproperties::get("");
    assert!(result.is_err(), "Getting empty property name should fail");

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

/// Test edge cases with property values
#[test]
fn test_property_value_edge_cases() {
    setup_edge_test_env();

    let test_values = vec![
        ("", "empty value"),
        (" ", "single space"),
        ("  ", "multiple spaces"),
        ("\t", "tab character"),
        ("\n", "newline character"),
        ("\r", "carriage return"),
        ("\r\n", "CRLF"),
        ("value with spaces", "spaces in value"),
        ("value\twith\ttabs", "tabs in value"),
        ("\"quoted value\"", "quoted value"),
        ("'single quoted'", "single quoted"),
        ("value=with=equals", "equals in value"),
        ("value:with:colons", "colons in value"),
        ("value;with;semicolons", "semicolons in value"),
        ("value/with/slashes", "slashes in value"),
        ("value\\with\\backslashes", "backslashes in value"),
        ("value with unicode: 🚀🌟⭐", "unicode characters"),
        ("0", "zero"),
        ("false", "false string"),
        ("true", "true string"),
        ("-1", "negative number"),
        ("3.14159", "decimal number"),
        ("1e10", "scientific notation"),
    ];

    for (i, (value, description)) in test_values.iter().enumerate() {
        let prop_name = format!("edge.value.test.{i}");

        match rsproperties::set(&prop_name, value) {
            Ok(_) => {
                let retrieved: Result<String, _> = rsproperties::get(&prop_name);
                assert_eq!(
                    retrieved.unwrap(),
                    *value,
                    "Failed for {}: {}",
                    description,
                    value.escape_debug()
                );
                println!("✓ {}: '{}'", description, value.escape_debug());
            }
            Err(e) => {
                println!(
                    "✗ Failed to set {}: {} - Error: {}",
                    description,
                    value.escape_debug(),
                    e
                );
            }
        }
    }
}

/// Test maximum length property values
#[test]
#[cfg(feature = "builder")]
fn test_maximum_length_values() {
    setup_edge_test_env();

    // Test values at and around PROP_VALUE_MAX
    let test_lengths = vec![
        PROP_VALUE_MAX - 10,
        PROP_VALUE_MAX - 5,
        PROP_VALUE_MAX - 1,
        PROP_VALUE_MAX,
    ];

    for length in test_lengths {
        let prop_name = format!("edge.maxlen.{length}");
        let value = "x".repeat(length);

        match rsproperties::set(&prop_name, &value) {
            Ok(_) => {
                let retrieved: Result<String, _> = rsproperties::get(&prop_name);
                let retrieved_value = retrieved.unwrap();
                assert_eq!(retrieved_value.len(), length);
                assert_eq!(retrieved_value, value);
                println!("✓ Successfully set/get property with {length} byte value");
            }
            Err(e) => {
                println!("✗ Failed to set property with {length} byte value: {e}");
            }
        }
    }

    // Test value that exceeds PROP_VALUE_MAX
    let oversized_value = "x".repeat(PROP_VALUE_MAX + 10);
    let result = rsproperties::set("edge.oversized", &oversized_value);
    match result {
        Ok(_) => {
            // If it succeeds, check if value was truncated
            let retrieved: Result<String, _> = rsproperties::get("edge.oversized");
            let retrieved_value = retrieved.unwrap();
            println!(
                "Oversized value handling: set {} bytes, retrieved {} bytes",
                oversized_value.len(),
                retrieved_value.len()
            );
            assert!(
                retrieved_value.len() <= PROP_VALUE_MAX,
                "Retrieved value should not exceed PROP_VALUE_MAX"
            );
        }
        Err(e) => {
            println!("Oversized value correctly rejected: {e}");
        }
    }
}

/// Test error propagation and handling
#[test]
fn test_error_handling() {
    setup_edge_test_env();

    // Test get on non-existent property
    let result: Result<String, _> = rsproperties::get("definitely.does.not.exist.anywhere");
    assert!(
        result.is_err(),
        "Should return error for non-existent property"
    );

    // Test that error contains useful information
    if let Err(e) = result {
        let error_msg = format!("{e}");
        assert!(!error_msg.is_empty(), "Error message should not be empty");
        println!("Error message for non-existent property: {error_msg}");
    }

    #[cfg(feature = "builder")]
    {
        // Test set with invalid inputs
        let invalid_cases = vec![("", "some_value", "empty property name")];

        for (name, value, description) in invalid_cases {
            let result = rsproperties::set(name, value);
            println!("Testing {description}: {result:?}");
            // Should either succeed or fail gracefully with a proper error
            if let Err(e) = result {
                assert!(
                    !format!("{e}").is_empty(),
                    "Error message should not be empty for {description}"
                );
            }
        }
    }
}

/// Test behavior with null bytes and other special characters
#[test]
#[cfg(feature = "builder")]
fn test_null_bytes_and_special_chars() {
    setup_edge_test_env();

    let test_cases = vec![
        ("prop.with.null", "value\0with\0nulls"),
        (
            "prop.with.high.unicode",
            "value with high unicode: \u{1F600}\u{1F601}",
        ),
        (
            "prop.with.control.chars",
            "value\x01with\x02control\x03chars",
        ),
        ("prop.with.del", "value\x7Fwith\x7Fdel"),
        ("prop.with.escape", "value\x1Bwith\x1Bescape"),
    ];

    for (prop_name, prop_value) in test_cases {
        let result = rsproperties::set(prop_name, prop_value);
        match result {
            Ok(_) => {
                let retrieved: Result<String, _> = rsproperties::get(prop_name);
                let retrieved_value = retrieved.unwrap();
                println!(
                    "✓ Special chars in '{}': {} bytes -> {} bytes",
                    prop_name,
                    prop_value.len(),
                    retrieved_value.len()
                );
                // Value might be modified/filtered by the implementation
            }
            Err(e) => {
                println!("✗ Failed to set '{prop_name}': {e}");
            }
        }
    }
}
