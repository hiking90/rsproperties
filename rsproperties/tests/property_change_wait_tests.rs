// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Tests for property change wait functionality on Android
//!
//! A waiter thread reads the serial *before* it signals the main thread,
//! then waits with that serial as `old_serial`. A change the main thread
//! makes before the waiter enters `wait` then returns at once instead of
//! being missed. The wait has a 10 s timeout so a lost wakeup fails the
//! test instead of hanging it: set → futex wake takes milliseconds, so the
//! bound is only reached on failure.
//!
//! Names are under `debug.`: the shell SELinux domain may set `debug.*`
//! but not `test.*`, so the tests also pass after `adb unroot`. The pid
//! suffix keeps concurrent runs apart. `wait_any` is not tested: it takes
//! no `old_serial`, so no test of it can be free of the lost-wakeup race
//! (it is `wait(None, None, None)`).

#![cfg(target_os = "android")]

use std::sync::{Arc, Barrier};
use std::thread;

use rsproperties::Timespec;

mod common;
use common::init_test;

const WAIT_TIMEOUT: Timespec = Timespec {
    tv_sec: 10,
    tv_nsec: 0,
};

/// Helper function to initialize test environment
fn setup_android_test() {
    let _ = env_logger::builder().is_test(true).try_init();
    init_test();
}

/// Test waiting for a property change
#[test]
fn test_wait_for_property_change() {
    setup_android_test();

    let test_prop = format!("debug.rsproperties.wait.{}", std::process::id());
    let initial_value = "initial";
    let changed_value = "changed";

    rsproperties::set(&test_prop, initial_value).expect("set initial value");
    assert_eq!(
        rsproperties::get::<String>(&test_prop).expect("get after set"),
        initial_value
    );

    let barrier = Arc::new(Barrier::new(2));
    let waiter = {
        let barrier = Arc::clone(&barrier);
        let prop_name = test_prop.clone();
        thread::spawn(move || {
            let system_properties = rsproperties::system_properties();
            let index = system_properties
                .find(&prop_name)
                .expect("find")
                .expect("property exists after set");
            let old_serial = system_properties.serial(&index).expect("serial");

            barrier.wait();

            system_properties
                .wait(Some(&index), Some(old_serial), Some(&WAIT_TIMEOUT))
                .expect("wait timed out: the change was not observed");
            rsproperties::get::<String>(&prop_name).expect("get after wait")
        })
    };

    barrier.wait();
    rsproperties::set(&test_prop, changed_value).expect("set changed value");

    let seen = waiter.join().expect("waiter thread panicked");
    assert_eq!(seen, changed_value);
}

/// Test waiting for any property change (global serial)
#[test]
fn test_wait_for_any_property_change() {
    setup_android_test();

    let test_prop = format!("debug.rsproperties.wait.any.{}", std::process::id());
    let changed_value = "any_changed";

    let barrier = Arc::new(Barrier::new(2));
    let waiter = {
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            let system_properties = rsproperties::system_properties();
            let old_serial = system_properties.context_serial();

            barrier.wait();

            system_properties.wait(None, Some(old_serial), Some(&WAIT_TIMEOUT))
        })
    };

    barrier.wait();
    rsproperties::set(&test_prop, changed_value).expect("set property");

    let woke = waiter.join().expect("waiter thread panicked");
    assert!(woke.is_some(), "wait timed out: no global serial change");
    assert_eq!(
        rsproperties::get_or(&test_prop, "not_set".to_string()),
        changed_value
    );
}
