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

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use rsproperties::{build_trie, PropertyConfig, PropertyInfoEntry, SystemProperties, Timespec};

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
    File::create(dir.join("property_info"))
        .unwrap()
        .write_all(&data)
        .unwrap();
}

fn timespec(d: Duration) -> Timespec {
    Timespec {
        tv_sec: d.as_secs() as _,
        tv_nsec: d.subsec_nanos() as _,
    }
}

#[test]
fn test_wait_wake_across_instances() {
    let dir = std::env::temp_dir().join(format!("rsprops_waitwake_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    build_property_info(&dir);

    let mut writer = SystemProperties::new_area(&dir).expect("writer new_area");
    writer.add("test.wait.prop", "0").unwrap();

    rsproperties::init(PropertyConfig::with_properties_dir(&dir));
    let reader = rsproperties::system_properties();
    let idx = reader
        .find("test.wait.prop")
        .unwrap()
        .expect("property added by the writer must be visible to the reader");

    // Phase 1 — wake: a waiter parked on the property's serial must observe
    // a cross-instance write. The 300ms delay before the set spans several
    // 100ms lock slices, so this also exercises the re-acquire/re-check
    // loop, not just the first futex wait.
    let old = reader.serial(&idx).expect("initial serial");
    let waiter = std::thread::spawn(move || {
        let reader = rsproperties::system_properties();
        reader.wait(
            Some(&idx),
            Some(old),
            Some(&timespec(Duration::from_secs(10))),
        )
    });
    std::thread::sleep(Duration::from_millis(300));
    writer.set("test.wait.prop", "1").unwrap();
    let woken = waiter.join().expect("waiter thread panicked");
    let new_serial = woken.expect("wait must return the post-update serial, not time out");
    assert_ne!(new_serial, old, "serial must advance on update");
    assert_eq!(reader.get_with_result("test.wait.prop").unwrap(), "1");

    // Phase 2 — timeout: with no writer activity the wait must expire close
    // to the requested bound (deadline math, clamped final slice), not hang
    // and not return early.
    let old = reader.serial(&idx).unwrap();
    let start = Instant::now();
    let res = reader.wait(
        Some(&idx),
        Some(old),
        Some(&timespec(Duration::from_millis(300))),
    );
    let elapsed = start.elapsed();
    assert!(res.is_none(), "nothing changed — wait must report timeout");
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
    let res = reader.wait(
        Some(&idx),
        Some(old),
        Some(&timespec(Duration::from_secs(10))),
    );
    let elapsed = start.elapsed();
    let new_serial = res.expect("already-changed serial must return immediately");
    assert_ne!(new_serial, old);
    assert!(
        elapsed < Duration::from_secs(2),
        "wait should have returned without parking: {elapsed:?}"
    );

    // Phase 4 — global serial: wait_any-style wait with an explicit
    // old_serial from the global serial area, woken by any update.
    let old_global = reader.context_serial();
    let waiter = std::thread::spawn(move || {
        let reader = rsproperties::system_properties();
        reader.wait(
            None,
            Some(old_global),
            Some(&timespec(Duration::from_secs(10))),
        )
    });
    std::thread::sleep(Duration::from_millis(200));
    writer.set("test.wait.prop", "3").unwrap();
    let woken = waiter.join().expect("global waiter panicked");
    assert!(
        woken.is_some(),
        "global-serial wait must observe the update"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
