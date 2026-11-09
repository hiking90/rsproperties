// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Simple test to debug compilation issues

extern crate rsproperties;

use rsproperties::{PROP_DIRNAME, PROP_VALUE_MAX};

mod common;
#[cfg(any(feature = "builder", target_os = "android"))]
use common::init_test;

#[test]
fn test_constants() {
    assert_eq!(PROP_VALUE_MAX, 92);
    assert_eq!(PROP_DIRNAME, "/dev/__properties__");
    println!("Constants test passed");
}

#[cfg(any(feature = "builder", target_os = "android"))]
#[test]
fn test_basic_functionality() {
    // Loads this binary's store (Android: the device store).
    init_test();

    // Test get_or
    let result = rsproperties::get_or("test.nonexistent", "default".to_string());
    assert_eq!(result, "default");

    println!("Basic functionality test passed");
}
