// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Simple test to debug compilation issues

extern crate rsproperties;

use rsproperties::{PROP_VALUE_MAX, PROP_DIRNAME};

#[path = "common.rs"]
mod common;
use common::init_test;

#[test]
fn test_constants() {
    assert_eq!(PROP_VALUE_MAX, 92);
    assert_eq!(PROP_DIRNAME, "/dev/__properties__");
    println!("Constants test passed");
}

#[test]
fn test_basic_functionality() {
    // Initialize with the existing __properties__ directory
    init_test();

    // Test get_with_default
    let result = rsproperties::get_with_default("test.nonexistent", "default");
    assert_eq!(result, "default");

    println!("Basic functionality test passed");
}
