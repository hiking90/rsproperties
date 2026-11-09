// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Regression tests on log content: adding a property must not log its
//! value, and the SELinux-label `warn` names the area's final path.
//!
//! The service masks property values in every log line (client-set values
//! can be secrets). `PropertyAreaMap::add` used to log `name = value` at
//! debug level for every new key, so `RUST_LOG=debug` defeated that
//! masking. This binary installs its own capturing logger (one per
//! process), so it lives in a separate test file.

#![cfg(all(feature = "builder", not(target_os = "android")))]

mod common;

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use rsproperties::{build_trie, AreaWriterLock, PropertyInfoEntry, SystemProperties};

struct CaptureLogger {
    lines: Mutex<Vec<String>>,
}

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("{}", record.args()));
    }

    fn flush(&self) {}
}

static LOGGER: CaptureLogger = CaptureLogger {
    lines: Mutex::new(Vec::new()),
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

/// One logger per process; every test of this binary calls this first.
fn install_logger() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        log::set_logger(&LOGGER).expect("only this binary's tests install a logger");
        log::set_max_level(log::LevelFilter::Trace);
    });
}

/// The SELinux-label `warn` of `new_area` names each area's final path, as
/// in 0.6.0, not the `.area.tmp-*` file the label is set on. For each area
/// the same label is first set on a probe file in the same dir; where that
/// fails, the exact 0.6.0 line for the area must be logged. An area whose
/// probe succeeds (a host that permits the label) is not checked.
#[test]
fn selinux_warn_names_the_area() {
    install_logger();
    let dir = common::TempDir::new("rsprops_selinux_warn");
    build_property_info(dir.path());

    // (area file name, label `new_area` sets on it)
    let areas = [
        ("u:object_r:test_prop:s0", "u:object_r:test_prop:s0"),
        ("u:object_r:default_prop:s0", "u:object_r:default_prop:s0"),
        ("properties_serial", "u:object_r:properties_serial:s0"),
    ];
    let probe_path = dir.path().join("xattr_probe");
    let probe = File::create(&probe_path).unwrap();
    let expected: Vec<String> = areas
        .iter()
        .filter(|(_, label)| {
            let value = format!("{label}\0");
            rustix::fs::fsetxattr(
                &probe,
                "security.selinux",
                value.as_bytes(),
                rustix::fs::XattrFlags::empty(),
            )
            .is_err()
        })
        .map(|(file, _)| {
            format!(
                "Failed to set SELinux context for {:?}",
                dir.path().join(file)
            )
        })
        .collect();
    drop(probe);
    std::fs::remove_file(&probe_path).unwrap();

    drop(SystemProperties::new_area(dir.path()).expect("new_area"));

    let dir_str = format!("{:?}", dir.path());
    let dir_str = dir_str.trim_matches('"');
    // Copied out so a failing assert does not poison the lock for the
    // other test.
    let lines: Vec<String> = LOGGER
        .lines
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("Failed to set SELinux context for") && l.contains(dir_str))
        .cloned()
        .collect();
    for want in &expected {
        assert!(lines.contains(want), "missing {want}; got {lines:?}");
    }
    assert!(lines.iter().all(|l| !l.contains(".area.tmp-")), "{lines:?}");
}

#[test]
fn add_does_not_log_value() {
    install_logger();

    let dir = common::TempDir::new("rsprops_mask");
    build_property_info(dir.path());

    const SECRET: &str = "s3cr3t-token-value";
    {
        let mut props = SystemProperties::new_area(dir.path()).expect("new_area");
        props.add("test.token", SECRET).unwrap();
        assert_eq!(props.get_with_result("test.token").unwrap(), SECRET);
    }

    // Copied out: asserting under the guard would poison the lock on
    // failure, and every later `log!` of this process would then panic.
    let lines: Vec<String> = LOGGER.lines.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l.contains("test.token")),
        "expected the add to be logged by name: {lines:?}"
    );
    let leaked: Vec<&String> = lines.iter().filter(|l| l.contains(SECRET)).collect();
    assert!(leaked.is_empty(), "value leaked into logs: {leaked:?}");
}
