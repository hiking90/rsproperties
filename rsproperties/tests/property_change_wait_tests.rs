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
//! suffix keeps concurrent runs apart.

#![cfg(target_os = "android")]

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

mod common;
use common::init_test;

const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

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
            let system_properties = rsproperties::system_properties().expect("global reader");
            let index = system_properties
                .find(&prop_name)
                .expect("find")
                .expect("property exists after set");
            let old_serial = system_properties.serial(index).expect("serial");

            barrier.wait();

            system_properties
                .wait(Some(index), old_serial, Some(WAIT_TIMEOUT))
                .expect("wait failed")
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
            let system_properties = rsproperties::system_properties().expect("global reader");
            let old_serial = system_properties.area_serial();

            barrier.wait();

            system_properties.wait(None, old_serial, Some(WAIT_TIMEOUT))
        })
    };

    barrier.wait();
    rsproperties::set(&test_prop, changed_value).expect("set property");

    let woke = waiter.join().expect("waiter thread panicked");
    assert!(
        matches!(woke, Ok(Some(_))),
        "wait timed out or failed: {woke:?}"
    );
    assert_eq!(
        rsproperties::get_or(&test_prop, "not_set".to_string()),
        changed_value
    );
}

/// `PropertyWatcher` against init: the creation and one update each end
/// one `wait_for_change`, with the value init stored, and the update is
/// not reported a second time.
#[test]
fn test_watcher_follows_creation_and_update() {
    setup_android_test();

    // `debug.*` properties outlive the process, so the pid alone could
    // name a property an earlier run created.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .subsec_nanos();
    let test_prop = format!("debug.rsproperties.watch.{}.{nanos}", std::process::id());

    let barrier = Arc::new(Barrier::new(2));
    let waiter = {
        let barrier = Arc::clone(&barrier);
        let prop_name = test_prop.clone();
        thread::spawn(move || {
            let props = rsproperties::system_properties().expect("global reader");
            let mut watcher = props.watch(&prop_name);
            assert_eq!(watcher.value().expect("value before creation"), None);

            barrier.wait();
            let created = watcher
                .wait_for_change(Some(WAIT_TIMEOUT))
                .expect("creation wait failed");
            assert!(created, "creation wait timed out");
            let first = watcher.value().expect("value").map(str::to_owned);

            barrier.wait();
            let updated = watcher
                .wait_for_change(Some(WAIT_TIMEOUT))
                .expect("update wait failed");
            assert!(updated, "update wait timed out");
            let second = watcher.value().expect("value").map(str::to_owned);

            let again = watcher
                .wait_for_change(Some(Duration::ZERO))
                .expect("poll failed");
            (first, second, again)
        })
    };

    barrier.wait();
    rsproperties::set(&test_prop, "created").expect("create property");
    barrier.wait();
    rsproperties::set(&test_prop, "updated").expect("update property");

    let (first, second, again) = waiter.join().expect("waiter thread panicked");
    assert_eq!(first.as_deref(), Some("created"));
    assert_eq!(second.as_deref(), Some("updated"));
    assert!(!again, "one update was reported twice");
}
