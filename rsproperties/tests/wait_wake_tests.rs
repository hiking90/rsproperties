// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Linux regression tests for `SystemProperties::wait` / `serial`.
//!
//! The wait path (sliced futex waits, deadline math, the lost-wakeup
//! re-check) was previously exercised only by the Android-gated tests in
//! `property_change_wait_tests.rs`, i.e. never in CI. These tests run the
//! same-process service arrangement on plain Linux: a builder writer
//! (`SystemProperties::new_area`) and the global read-only instance mapping
//! the same files.
//!
//! All phases share one properties dir because `rsproperties::init` latches
//! globals once per process — hence a single #[test] fn with sequential
//! phases instead of independent tests racing on the latch.

#![cfg(all(feature = "builder", target_os = "linux"))]

mod common;

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use rsproperties::{
    build_trie, AreaWriterLock, PropertyConfig, PropertyInfoEntry, SystemProperties,
};

fn build_property_info(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();

    let contexts_path = dir.join("property_contexts");
    File::create(&contexts_path)
        .unwrap()
        .write_all(b"test. u:object_r:test_prop:s0 prefix string\n")
        .unwrap();

    let (entries, errors) = PropertyInfoEntry::parse_from_file(&contexts_path, false).unwrap();
    assert!(errors.is_empty(), "parse errors: {errors:?}");

    let data = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();
    // Mode 0444 whatever the umask; `File::create` under umask 002 makes
    // a group-writable file that readers refuse.
    AreaWriterLock::acquire(dir)
        .unwrap()
        .publish_property_info(&data)
        .unwrap();
}

#[test]
fn test_wait_wake_across_instances() {
    let dir = common::TempDir::new("rsprops_waitwake");
    build_property_info(dir.path());

    let mut writer = SystemProperties::new_area(dir.path()).expect("writer new_area");
    writer.add("test.wait.prop", "0").unwrap();

    rsproperties::init(PropertyConfig::new().properties_dir(dir.path())).expect("init");
    let reader = rsproperties::system_properties();
    let idx = reader
        .find("test.wait.prop")
        .unwrap()
        .expect("property added by the writer must be visible to the reader");

    // Phase 1 — cross-instance update observed across lock slices (the
    // re-check path): the 300ms delay before the set spans several 100ms
    // slices, and the waiter compares the serial again on each one, so this
    // passes without the per-property futex wake. The wake itself is
    // checked by `system_properties::wait_tests::update_wakes_property_futex`,
    // which waits without slicing.
    let old = reader.serial(&idx).expect("initial serial");
    let waiter = std::thread::spawn(move || {
        let reader = rsproperties::system_properties();
        reader.wait(Some(&idx), Some(old), Some(Duration::from_secs(10)))
    });
    std::thread::sleep(Duration::from_millis(300));
    writer.set("test.wait.prop", "1").unwrap();
    let woken = waiter.join().expect("waiter thread panicked");
    let new_serial = woken
        .expect("wait must not fail")
        .expect("wait must return the post-update serial, not time out");
    assert_ne!(new_serial, old, "serial must advance on update");
    assert_eq!(reader.get("test.wait.prop").unwrap(), "1");

    // Phase 2 — timeout: with no writer activity the wait must expire close
    // to the requested bound (deadline math, clamped final slice), not hang
    // and not return early.
    let old = reader.serial(&idx).unwrap();
    let start = Instant::now();
    let res = reader.wait(Some(&idx), Some(old), Some(Duration::from_millis(300)));
    let elapsed = start.elapsed();
    assert!(
        matches!(res, Ok(None)),
        "nothing changed — wait must report timeout, got {res:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(250),
        "returned before the timeout: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "overshot the timeout bound: {elapsed:?}"
    );

    // Phase 3 — lost-wakeup contract: if the property already changed since
    // `old_serial`, wait must return immediately with the new serial
    // instead of parking until the *next* change.
    let old = reader.serial(&idx).unwrap();
    writer.set("test.wait.prop", "2").unwrap();
    let start = Instant::now();
    let res = reader.wait(Some(&idx), Some(old), Some(Duration::from_secs(10)));
    let elapsed = start.elapsed();
    let new_serial = res
        .unwrap()
        .expect("already-changed serial must return immediately");
    assert_ne!(new_serial, old);
    assert!(
        elapsed < Duration::from_secs(2),
        "wait should have returned without parking: {elapsed:?}"
    );

    // Phase 4 — global serial: a wait with an explicit old_serial from the
    // global serial area, woken by any update.
    let old_global = reader.context_serial();
    let waiter = std::thread::spawn(move || {
        let reader = rsproperties::system_properties();
        reader.wait(None, Some(old_global), Some(Duration::from_secs(10)))
    });
    std::thread::sleep(Duration::from_millis(200));
    writer.set("test.wait.prop", "3").unwrap();
    let woken = waiter.join().expect("global waiter panicked");
    assert!(
        matches!(woken, Ok(Some(_))),
        "global-serial wait must observe the update, got {woken:?}"
    );

    // Phase 5 — a zero timeout still compares `old_serial` first. bionic
    // gets the same answer from the kernel: FUTEX_WAIT returns EAGAIN for a
    // changed word with a zero timeout, and bionic reloads the serial.
    let old = reader.serial(&idx).unwrap();
    writer.set("test.wait.prop", "4").unwrap();
    let got = reader.wait(Some(&idx), Some(old), Some(Duration::ZERO));
    assert!(
        matches!(got, Ok(Some(s)) if s != old),
        "already-changed serial must be reported, got {got:?}"
    );
    // An unchanged serial with a zero timeout returns `Ok(None)` without
    // waiting.
    let cur = reader.serial(&idx).unwrap();
    let got = reader.wait(Some(&idx), Some(cur), Some(Duration::ZERO));
    assert!(matches!(got, Ok(None)), "{got:?}");
    // Regression only (passes before the fix): the global-serial path
    // compared before the futex already, and gives the same answer.
    let g = reader.context_serial();
    writer.set("test.wait.prop", "5").unwrap();
    let got = reader.wait(None, Some(g), Some(Duration::ZERO));
    assert!(matches!(got, Ok(Some(_))), "{got:?}");
}
