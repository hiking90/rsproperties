// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Error handling and edge case tests for new property APIs
//!
//! These tests focus on:
//! - Error conditions and error message validation
//! - Edge cases and boundary conditions
//! - Robustness and reliability
//! - API contract validation

#[path = "common.rs"]
mod common;
use common::init_test;

async fn setup_test_env() {
    let _ = env_logger::builder().is_test(true).try_init();
    init_test().await;
}

#[tokio::test]
async fn test_get_parsed_error_types() -> anyhow::Result<()> {
    setup_test_env().await;

    // Test NotFound error
    let non_existent_prop = "absolutely.does.not.exist";
    let result: Result<i32, _> = rsproperties::get(non_existent_prop);
    assert!(
        matches!(result, Err(rsproperties::Error::NotFound(_))),
        "Expected NotFound error, got: {result:?}"
    );

    // Test Parse error
    let parse_error_prop = "test.error.parse";
    rsproperties::set(parse_error_prop, "definitely_not_a_number")?;
    let result: Result<i32, _> = rsproperties::get(parse_error_prop);
    assert!(
        matches!(result, Err(rsproperties::Error::Parse(_))),
        "Expected Parse error, got: {result:?}"
    );

    // Test empty string parsing
    let empty_prop = "test.error.empty";
    rsproperties::set(empty_prop, "")?;
    let result: Result<i32, _> = rsproperties::get(empty_prop);
    assert!(
        matches!(result, Err(rsproperties::Error::Parse(_))),
        "Expected Parse error, got: {result:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_numeric_overflow_edge_cases() -> anyhow::Result<()> {
    setup_test_env().await;

    // Test values that overflow smaller integer types
    let overflow_cases = [
        ("i8_overflow", (i8::MAX as i32 + 1).to_string()),
        ("i16_overflow", (i16::MAX as i32 + 1).to_string()),
        ("u8_overflow", (u8::MAX as u32 + 1).to_string()),
        ("u16_overflow", (u16::MAX as u32 + 1).to_string()),
    ];

    for (name, overflow_value) in overflow_cases.iter() {
        let prop_name = format!("test.overflow.{name}");
        rsproperties::set(&prop_name, overflow_value)?;

        // Should parse successfully as larger types
        let as_i32: i32 = rsproperties::get(&prop_name)?;
        assert_eq!(as_i32.to_string(), *overflow_value);

        // But should fail for smaller types (depending on the specific case)
        if name.contains("i8") {
            let result: Result<i8, _> = rsproperties::get(&prop_name);
            assert!(
                matches!(result, Err(rsproperties::Error::Parse(_))),
                "i8 should overflow for {overflow_value}, got {result:?}"
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_floating_point_special_values() -> anyhow::Result<()> {
    setup_test_env().await;

    let special_values = [
        ("infinity", "inf"),
        ("negative_infinity", "-inf"),
        ("nan", "NaN"),
        ("scientific_notation", "1.23e10"),
        ("negative_scientific", "-4.56e-7"),
    ];

    for (name, special_value) in special_values.iter() {
        let prop_name = format!("test.float_special.{name}");
        rsproperties::set(&prop_name, special_value)?;

        let result: anyhow::Result<f64> = common::read_parsed(&prop_name);

        // The service stores the value unchanged, `read_with` hands it over
        // untrimmed, and `f64::from_str` accepts "inf", "-inf" and "NaN".
        match *name {
            "infinity" => {
                let val: f64 = result?;
                assert!(val.is_infinite() && val.is_sign_positive());
            }
            "negative_infinity" => {
                let val: f64 = result?;
                assert!(val.is_infinite() && val.is_sign_negative());
            }
            "nan" => {
                let val: f64 = result?;
                assert!(val.is_nan());
            }
            "scientific_notation" => {
                let val: f64 = result?;
                assert!((val - 1.23e10).abs() < 1e6);
            }
            "negative_scientific" => {
                let val: f64 = result?;
                assert!((val - (-4.56e-7)).abs() < 1e-10);
            }
            _ => {}
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_string_parsing_edge_cases() -> anyhow::Result<()> {
    setup_test_env().await;

    let edge_cases = [
        ("empty_string", ""),
        ("whitespace_only", "   "),
        ("tab_and_newline", "\t\n"),
        ("leading_zeros", "000123"),
        ("leading_plus", "+42"),
        ("multiple_signs", "++42"),
        ("decimal_only", "."),
        ("decimal_no_digits", ".123"),
        ("trailing_garbage", "42abc"),
        ("embedded_space", "4 2"),
    ];

    for (name, test_value) in edge_cases.iter() {
        let prop_name = format!("test.string_edge.{name}");
        rsproperties::set(&prop_name, test_value)?;

        // Try parsing as integer
        let int_result: Result<i32, _> = rsproperties::get(&prop_name);
        // The property was just set, so `get` finds it; a value that is
        // not an `i32` is reported as `Error::Parse`, never as another
        // variant.
        let is_error = matches!(int_result, Err(rsproperties::Error::Parse(_)));
        assert!(
            is_error || int_result.is_ok(),
            "{name}: unexpected error {int_result:?}"
        );

        // Check if the result makes sense
        match name {
            &"leading_zeros" => {
                let val = int_result.expect("leading_zeros should parse successfully");
                assert_eq!(val, 123);
            }
            &"leading_plus" => {
                let val = int_result.expect("leading_plus should parse successfully");
                assert_eq!(val, 42);
            }
            &"empty_string" | &"whitespace_only" | &"tab_and_newline" | &"multiple_signs"
            | &"decimal_only" | &"trailing_garbage" | &"embedded_space" => {
                assert!(is_error, "Should fail to parse '{test_value}' as integer");
            }
            &"decimal_no_digits" => {
                // This should fail for integer parsing
                assert!(is_error, "Should fail to parse '{test_value}' as integer");
            }
            &"very_long_number" => {
                // This might succeed or fail depending on the number size
                // We just ensure it doesn't panic
            }
            _ => {
                // For other cases, we just ensure no panic occurs
            }
        }

        // get_parsed_with_default should always succeed
        let with_default: i32 = rsproperties::get_or(&prop_name, 999);
        if is_error {
            assert_eq!(
                with_default, 999,
                "get_parsed_with_default should return default for invalid input: '{test_value}'"
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_property_name_edge_cases() -> anyhow::Result<()> {
    setup_test_env().await;

    // (case, name, accepted). The outcome follows from
    // `wire::validate_property_name`, which the client runs before
    // connecting: ASCII alphanumerics and `_ . - @ :` only, no leading,
    // trailing or consecutive `.`.
    //
    // The 500-byte name assumes the V2 protocol: this binary never sets
    // `PROPERTY_SERVICE_VERSION` and its store has no
    // `ro.property_service.version`, so the client uses V2. V1 would reject
    // names of PROP_NAME_MAX (32) bytes or more with `InvalidArgument`.
    let name_cases = [
        ("very_long_name", "a".repeat(500), true),
        ("unicode_name", "测试.property.名称".to_string(), false),
        (
            "special_chars_name",
            "test.@#$%.property".to_string(),
            false,
        ),
        ("numbers_only", "123.456.789".to_string(), true),
        ("single_char", "a".to_string(), true),
        ("dots_only", "...".to_string(), false),
        ("mixed_case", "Test.Property.Name".to_string(), true),
    ];

    for (test_name, prop_name, accepted) in name_cases.iter() {
        let test_value = "test_value";

        let set_result = rsproperties::set(prop_name, test_value);

        if *accepted {
            set_result.unwrap_or_else(|e| {
                panic!(
                    "{test_name}: V2 has no name-length rule beyond MAX_WIRE_NAME_LEN \
                     (1024) and the name passes validate_property_name, so set must \
                     succeed: {e}"
                )
            });
            let retrieved: String = rsproperties::get(prop_name)?;
            assert_eq!(
                retrieved, test_value,
                "Retrieved value doesn't match for property name case: {test_name}"
            );
        } else {
            assert!(
                matches!(set_result, Err(rsproperties::Error::InvalidArgument(_))),
                "{test_name}: the client must reject {prop_name:?} with InvalidArgument, \
                 got {set_result:?}"
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_property_value_size_limits() -> anyhow::Result<()> {
    setup_test_env().await;

    let size_cases = [
        ("small", "x".repeat(10)),
        ("medium", "x".repeat(100)),
        ("large", "x".repeat(1000)),
        ("very_large", "x".repeat(10000)),
        ("extremely_large", "x".repeat(100000)),
    ];

    // `test.size_limits.*` is not an `ro.` name, so values of
    // PROP_VALUE_MAX (92) bytes or more are rejected by the client
    // (`wire::validate_value_len`) and values are never truncated.
    for (test_name, test_value) in size_cases.iter() {
        let prop_name = format!("test.size_limits.{test_name}");

        let set_result = rsproperties::set(&prop_name, test_value);

        if test_value.len() < rsproperties::wire::PROP_VALUE_MAX {
            set_result?;
            let retrieved: String = rsproperties::get(&prop_name)?;
            assert_eq!(retrieved, *test_value, "value read back for {test_name}");
        } else {
            assert!(
                matches!(set_result, Err(rsproperties::Error::InvalidArgument(_))),
                "{test_name} ({} bytes) must be InvalidArgument, got {set_result:?}",
                test_value.len()
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_concurrent_error_scenarios() -> anyhow::Result<()> {
    setup_test_env().await;

    let num_tasks = 10;

    let handles: Vec<_> = (0..num_tasks)
        .map(|task_id| {
            tokio::spawn(async move {
                // Each task tries various error scenarios
                let base_name = format!("test.concurrent_errors.task_{task_id}");

                // Try parsing non-existent property
                let non_existent = format!("{base_name}.non_existent");
                let result: Result<i32, _> = rsproperties::get(&non_existent);
                assert!(
                    matches!(result, Err(rsproperties::Error::NotFound(_))),
                    "{result:?}"
                );

                // Set invalid value and try parsing
                let invalid_prop = format!("{base_name}.invalid");
                rsproperties::set(&invalid_prop, "not_a_number")?;
                let result: Result<i32, _> = rsproperties::get(&invalid_prop);
                assert!(
                    matches!(result, Err(rsproperties::Error::Parse(_))),
                    "{result:?}"
                );

                // But get_parsed_with_default should work
                let with_default: i32 = rsproperties::get_or(&invalid_prop, 42);
                assert_eq!(with_default, 42);

                anyhow::Ok(())
            })
        })
        .collect();

    for handle in handles {
        handle.await??;
    }

    Ok(())
}

#[tokio::test]
async fn test_api_contract_validation() -> anyhow::Result<()> {
    setup_test_env().await;

    // Test that API contracts are maintained

    // Contract 1: get_parsed_with_default never fails
    let test_cases = [
        ("non.existent.property", 42),
        ("test.contract.invalid", 999),
    ];

    // Set up invalid value for second test case
    rsproperties::set("test.contract.invalid", "invalid_number")?;

    for (prop_name, default_value) in test_cases.iter() {
        let result: i32 = rsproperties::get_or(prop_name, *default_value);
        assert_eq!(
            result, *default_value,
            "get_parsed_with_default should always return default for invalid cases"
        );
    }

    // Contract 2: get() reports a missing property, including the empty
    // name, as NotFound and returns a set value unchanged
    rsproperties::set("test.non.empty.value", "some_value")?;

    for prop_name in ["non.existent.property", ""] {
        let result: Result<String, _> = rsproperties::get(prop_name);
        assert!(
            matches!(result, Err(rsproperties::Error::NotFound(_))),
            "get({prop_name:?}) must be NotFound, got {result:?}"
        );
    }
    let value: String = rsproperties::get("test.non.empty.value")?;
    assert_eq!(value, "some_value");

    // Contract 3: set operations with valid strings should not fail
    let valid_operations = [
        ("test.contract.set1", "value1"),
        ("test.contract.set2", "123"),
        ("test.contract.set3", "true"),
        ("test.contract.set4", "non_empty_value"), // Changed from empty string
    ];

    for (prop_name, prop_value) in valid_operations.iter() {
        rsproperties::set(prop_name, prop_value).unwrap_or_else(|e| {
            panic!("Valid set operation should succeed: {prop_name}={prop_value}: {e}")
        });
        let value: String = rsproperties::get(prop_name)?;
        assert_eq!(value, *prop_value);
    }

    Ok(())
}

#[tokio::test]
async fn test_memory_usage_patterns() -> anyhow::Result<()> {
    setup_test_env().await;

    // Test that repeated operations don't cause memory issues
    let num_iterations = 100;

    for i in 0..num_iterations {
        let prop_name = format!("test.memory.iteration_{i}");

        // Create and destroy many property values
        rsproperties::set(&prop_name, i)?;
        let _retrieved: String = rsproperties::get(&prop_name)?;
        let _parsed: i32 = rsproperties::get(&prop_name)?;

        // Overwrite with different types
        rsproperties::set(&prop_name, i as f64 * 1.5)?;
        let _as_float: f64 = common::read_parsed(&prop_name)?;

        rsproperties::set(&prop_name, format!("string_{i}"))?;
        let _as_string: String = rsproperties::get(&prop_name)?;
    }

    // If we get here without crashing, memory management is working
    Ok(())
}

#[tokio::test]
async fn test_error_message_quality() -> anyhow::Result<()> {
    setup_test_env().await;

    // Test that error messages are informative

    // Test 1: Non-existent property
    let result: Result<i32, _> = rsproperties::get("non.existent.test.property");
    assert!(
        matches!(result, Err(rsproperties::Error::NotFound(_))),
        "{result:?}"
    );
    let error_msg = result.unwrap_err().to_string();
    // Error should mention the property name
    assert!(
        error_msg.contains("non.existent.test.property")
            || error_msg.to_lowercase().contains("not found")
            || error_msg.to_lowercase().contains("notfound"),
        "Error message should be informative: '{error_msg}'"
    );

    // Test 2: Parse error
    rsproperties::set("test.error_msg.parse", "invalid_for_integer")?;
    let result: Result<i32, _> = rsproperties::get("test.error_msg.parse");
    assert!(
        matches!(result, Err(rsproperties::Error::Parse(_))),
        "{result:?}"
    );
    let error_msg = result.unwrap_err().to_string();
    // Error should mention parsing failure and the value
    assert!(
        error_msg.to_lowercase().contains("parse")
            || error_msg.to_lowercase().contains("invalid")
            || error_msg.contains("invalid_for_integer"),
        "Parse error message should be informative: '{error_msg}'"
    );

    Ok(())
}

#[tokio::test]
async fn test_type_conversion_boundaries() -> anyhow::Result<()> {
    setup_test_env().await;

    // Test conversion at type boundaries
    let boundary_tests = [
        // (test_name, value, should_succeed_as_i32, should_succeed_as_u32)
        ("i32_max_positive", i32::MAX.to_string(), true, true),
        ("negative_value", "-1".to_string(), true, false), // Should fail for u32
        ("small_positive", "100".to_string(), true, true),
        ("u32_max_large", u32::MAX.to_string(), false, true), // Should fail for i32
    ];

    for (test_name, value_str, should_succeed_signed, should_succeed_unsigned) in
        boundary_tests.iter()
    {
        let prop_name = format!("test.boundaries.{test_name}");
        rsproperties::set(&prop_name, value_str)?;

        // Try parsing as signed
        let signed_result: Result<i32, _> = rsproperties::get(&prop_name);

        // Try parsing as unsigned
        let unsigned_result: Result<u32, _> = rsproperties::get(&prop_name);

        if *should_succeed_signed {
            assert_eq!(
                signed_result.as_ref().ok().map(i32::to_string).as_deref(),
                Some(value_str.as_str()),
                "Should parse successfully as i32 for {test_name}: {value_str}"
            );
        } else {
            assert!(
                matches!(signed_result, Err(rsproperties::Error::Parse(_))),
                "Should fail to parse as i32 for {test_name}: {value_str}: {signed_result:?}"
            );
        }

        if *should_succeed_unsigned {
            assert_eq!(
                unsigned_result.as_ref().ok().map(u32::to_string).as_deref(),
                Some(value_str.as_str()),
                "Should parse successfully as u32 for {test_name}: {value_str}"
            );
        } else {
            assert!(
                matches!(unsigned_result, Err(rsproperties::Error::Parse(_))),
                "Should fail to parse as u32 for {test_name}: {value_str}: {unsigned_result:?}"
            );
        }

        // Verify get_parsed_with_default behavior
        let default_signed: i32 = rsproperties::get_or(&prop_name, -999);
        let default_unsigned: u32 = rsproperties::get_or(&prop_name, 999);

        if signed_result.is_err() {
            assert_eq!(default_signed, -999);
        }
        if unsigned_result.is_err() {
            assert_eq!(default_unsigned, 999);
        }
    }

    Ok(())
}

/// The service replies with init's codes, which the client reports as
/// `SetRejected { reason }`; `ctl.*` is refused (no control handler in the
/// test service) and never stored.
#[tokio::test]
async fn test_service_rejections_carry_init_codes() -> anyhow::Result<()> {
    use rsproperties::SetRejection;
    setup_test_env().await;

    let reason_of = |result: rsproperties::Result<()>| match result {
        Err(rsproperties::Error::SetRejected { reason, .. }) => Some(reason),
        _ => None,
    };

    assert_eq!(
        reason_of(rsproperties::set("ctl.start", "some_service")),
        Some(SetRejection::ControlMessage)
    );
    assert!(matches!(
        rsproperties::get::<String>("ctl.start"),
        Err(rsproperties::Error::NotFound(_))
    ));

    rsproperties::set("ro.test.codes.once", "1")?;
    assert_eq!(
        reason_of(rsproperties::set("ro.test.codes.once", "2")),
        Some(SetRejection::ReadOnlyProperty)
    );

    // Set by the service at startup, like init.
    assert_eq!(
        rsproperties::get::<String>("ro.property_service.version")?,
        "2"
    );
    assert_eq!(
        rsproperties::get::<String>("ro.persistent_properties.ready")?,
        "true"
    );
    Ok(())
}
