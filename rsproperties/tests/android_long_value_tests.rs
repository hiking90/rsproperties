// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Android client V2 length caps against AOSP init.
//!
//! On Android `set` talks to init, whose `RecvString` accepts names and
//! values of up to 0xffff bytes (`wire::AOSP_MAX_WIRE_STRING_LEN`); the
//! client must not reject what init accepts with this crate's service caps
//! (`MAX_WIRE_NAME_LEN` 1024, `MAX_WIRE_VALUE_LEN` 8192).
//!
//! Run as root (`adb root`): whether the shell domain may set `ro.*`
//! properties depends on the SELinux policy. Run once on a freshly booted
//! emulator and reboot afterwards: `ro.` values are write-once and the
//! long value and long name stay in the property areas (tmpfs) until the
//! next boot. `--test-threads=1`.

#![cfg(target_os = "android")]

use rsproperties::Error;

/// A `ro.` value longer than `MAX_WIRE_VALUE_LEN` (8192) is sent to init
/// and stored as a long property.
#[test]
fn long_ro_value_above_service_cap_is_set() {
    let name = format!("ro.rsproperties.test.long{}", std::process::id());
    let value = "v".repeat(10_000);
    rsproperties::set(&name, &value).expect("init accepts a 10000-byte ro. value");
    let read: String = rsproperties::get(&name).expect("long ro. value reads back");
    assert_eq!(read.len(), value.len());
    assert_eq!(read, value);
}

/// One byte above init's limit is rejected by the client before it
/// connects (init would close the connection with `PROP_ERROR_READ_DATA`).
#[test]
fn value_above_aosp_cap_is_rejected_by_client() {
    let name = format!("ro.rsproperties.test.huge{}", std::process::id());
    let value = "v".repeat(rsproperties::wire::AOSP_MAX_WIRE_STRING_LEN + 1);
    let err = rsproperties::set(&name, &value).expect_err("65536-byte value must be rejected");
    assert!(
        matches!(&err, Error::InvalidArgument(msg) if msg.contains("wire cap")),
        "unexpected error: {err:?}"
    );
}

/// A name longer than `MAX_WIRE_NAME_LEN` (1024) is sent to init, which
/// accepts it.
#[test]
fn name_above_service_cap_is_set() {
    let prefix = format!("debug.rsproperties.test.{}.", std::process::id());
    let name = format!("{prefix}{}", "n".repeat(1025 - prefix.len()));
    assert_eq!(name.len(), 1025);
    rsproperties::set(&name, "1").expect("init accepts a 1025-byte name");
    let read: String = rsproperties::get(&name).expect("long name reads back");
    assert_eq!(read, "1");
}
