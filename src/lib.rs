// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! # Android System Properties for Linux and Android both.
//!
//! This crate provides a way to access system properties on Linux and Android.
//!
//! ## Features
//!
//! - Get system properties.
//! - Set system properties.
//! - Wait for system properties.
//! - Serialize system properties.
//! - Deserialize system properties.
//!
//! ## Usage
//!
//! ```rust
//! use rsproperties;
//!
//! fn main() {
//!     // Initialize system properties.
//!     // It must be called before using other functions. And None means the default directory "/dev/__properties__".
//!     rsproperties::init(None);
//!
//!    // Get a value of the property.
//!    let value = rsproperties::get_with_default("ro.build.version.sdk", "0");
//!    println!("ro.build.version.sdk: {}", value);
//!
//!     // Set a value of the property.
//!     rsproperties::set("test.property", "test.value").unwrap();
//! }
//! ```

use std::{
    sync::{OnceLock, Mutex},
    path::PathBuf,
};

mod property_info_parser;
mod errors;
mod system_properties;
mod contexts_serialized;
mod property_area;
mod context_node;
mod property_info;
mod system_property_set;
#[cfg(feature = "builder")]
mod property_info_serializer;
#[cfg(feature = "builder")]
mod trie_builder;
#[cfg(feature = "builder")]
mod trie_serializer;
#[cfg(feature = "builder")]
mod trie_node_arena;
#[cfg(feature = "builder")]
mod build_property_parser;

pub use errors::*;
pub use system_properties::SystemProperties;
#[cfg(feature = "builder")]
pub use property_info_serializer::*;
#[cfg(feature = "builder")]
pub use build_property_parser::*;

pub const PROP_VALUE_MAX: usize = 92;
pub const PROP_DIRNAME: &str = "/dev/__properties__";

// System properties directory.
static SYSTEM_PROPERTIES_DIR: OnceLock<PathBuf> = OnceLock::new();
// Global system properties.
static SYSTEM_PROPERTIES: OnceLock<system_properties::SystemProperties> = OnceLock::new();
// Global system properties area.
static SYSTEM_PROPERTIES_AREA: OnceLock<Mutex<SystemProperties>> = OnceLock::new();

/// Initialize system properties.
/// It must be called before using other functions.
/// If dir is None, it uses the default directory "/dev/__properties__".
pub fn init(dir: Option<&str>) {
    let dir = dir.unwrap_or(PROP_DIRNAME);
    if let Err(e) = SYSTEM_PROPERTIES_DIR.set(PathBuf::from(dir)) {
        log::error!("Error setting system properties directory: {}", e.display());
    }
}

/// Get the system properties directory.
/// It returns None if init() is not called.
/// It returns Some(&PathBuf) if init() is called.
pub fn dirname() -> Option<&'static PathBuf> {
    SYSTEM_PROPERTIES_DIR.get()
}

/// Get the system properties.
/// Before calling this function, init() must be called.
/// It panics if init() is not called or the system properties cannot be opened.
pub fn system_properties() -> &'static system_properties::SystemProperties {
    SYSTEM_PROPERTIES.get_or_init(|| {
        let dir = dirname().expect(&format!("Call init() first."));
        system_properties::SystemProperties::new(dir)
            .expect(&format!("Cannot open system properties. Please check if \"{dir:?}\" exists."))
    })
}

/// Get the system properties area.
/// Before calling this function, init() must be called.
/// It is used for adding and updating system properties.
pub fn system_properties_area() -> &'static Mutex<SystemProperties> {
    SYSTEM_PROPERTIES_AREA.get_or_init(|| {
        let dir = dirname().expect(&format!("Call init() first."));
        Mutex::new(SystemProperties::new_area(dir)
            .expect(&format!("Cannot create system properties. Please check if \"{dir:?}\" exists.")))
    })
}

// Get alignment value for bionic style.
pub(crate) fn bionic_align(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

/// Get a value of the property.
/// If the property is not found, it returns the default value.
/// If an error occurs, it returns the default value.
pub fn get_with_default(name: &str, default: &str) -> String {
    system_properties().get(name).unwrap_or_else(|err| {
        log::error!("Error getting property {}: {}", name, err);
        default.to_string()
    })
}

/// Get a value of the property.
/// If the property is not found, it returns an empty string.
/// If an error occurs, it returns Err.
pub fn get(name: &str) -> Result<String> {
    system_properties().get(name)
}

/// Set a value of the property.
/// If an error occurs, it returns Err.
/// It uses socket communication to set the property. Because it is designed for client applications,
pub fn set(name: &str, value: &str) -> Result<()> {
    system_property_set::set(name, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, remove_dir_all, create_dir};
    use std::io::Write;
    use std::collections::HashMap;
    use std::path::Path;
    #[cfg(target_os = "android")]
    use android_system_properties::AndroidSystemProperties;

    const TEST_PROPERTY_DIR: &str = "__properties__";

    fn enable_logger() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[cfg(target_os = "android")]
    #[test]
    fn test_get() {
        const PROPERTIES: [&str; 40] = [
            "ro.build.version.sdk",
            "ro.build.version.release",
            "ro.product.model",
            "ro.product.manufacturer",
            "ro.product.name",
            "ro.serialno",
            "ro.bootloader",
            "ro.hardware",
            "ro.revision",
            "ro.kernel.qemu",
            "dalvik.vm.heapsize",
            "dalvik.vm.heapgrowthlimit",
            "dalvik.vm.heapstartsize",
            "dalvik.vm.heaptargetutilization",
            "dalvik.vm.heapminfree",
            "dalvik.vm.heapmaxfree",
            "net.bt.name",
            "net.change",
            "net.dns1",
            "net.dns2",
            "net.hostname",
            "net.tcp.default_init_rwnd",
            "persist.sys.timezone",
            "persist.sys.locale",
            "persist.sys.dalvik.vm.lib.2",
            "persist.sys.profiler_ms",
            "persist.sys.usb.config",
            "persist.service.acm.enable",
            "ril.ecclist",
            "ril.subscription.types",
            "service.adb.tcp.port",
            "service.bootanim.exit",
            "service.camera.running",
            "service.media.powersnd",
            "sys.boot_completed",
            "sys.usb.config",
            "sys.usb.state",
            "vold.post_fs_data_done",
            "wifi.interface",
            "wifi.supplicant_scan_interval"
        ];

        enable_logger();
        for prop in PROPERTIES.iter() {
            let value1 = get_with_default(prop, "");
            let value2 = AndroidSystemProperties::new().get(prop).unwrap_or_default();

            println!("{}: [{}], [{}]", prop, value1, value2);
            assert_eq!(value1, value2);
        }
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    #[test]
    fn test_set() -> Result<()> {
        enable_logger();

        #[cfg(target_os = "linux")]
        build_property_dir(TEST_PROPERTY_DIR);

        let prop = "test.property";
        let value = "test.value";

        set(prop, value)?;

        let value1: String = get(prop)?;
        assert_eq!(value1, value);

        Ok(())
    }

    fn load_properties() -> HashMap<String, String> {
        let build_prop_files = vec![
            "tests/android/product_build.prop",
            "tests/android/system_build.prop",
            "tests/android/system_dlkm_build.prop",
            "tests/android/system_ext_build.prop",
            "tests/android/vendor_build.prop",
            "tests/android/vendor_dlkm_build.prop",
            "tests/android/vendor_odm_build.prop",
            "tests/android/vendor_odm_dlkm_build.prop",
        ];

        let mut properties = HashMap::new();
        for file in build_prop_files {
            load_properties_from_file(Path::new(file), None, "u:r:init:s0", &mut properties).unwrap();
        }

        properties
    }

    fn build_property_dir(dir: &str) -> HashMap<String, String> {
        static INIT_PROPERTY_AREA: OnceLock<bool> = OnceLock::new();

        let _ = INIT_PROPERTY_AREA.get_or_init(|| {
            crate::init(Some(dir));

            let property_contexts_files = vec![
                "tests/android/plat_property_contexts",
                "tests/android/system_ext_property_contexts",
                "tests/android/vendor_property_contexts",
            ];

            let mut property_infos = Vec::new();
            for file in property_contexts_files {
                let (mut property_info, errors) = PropertyInfoEntry::parse_from_file(Path::new(file), false).unwrap();
                if errors.len() > 0 {
                    log::error!("{:?}", errors);
                }
                property_infos.append(&mut property_info);
            }

            let data = build_trie(&mut property_infos, "u:object_r:build_prop:s0", "string").unwrap();

            let dir = dirname().unwrap();
            remove_dir_all(dir).unwrap_or_default();
            create_dir(dir).unwrap_or_default();
            File::create(dir.join("property_info")).unwrap().write_all(&data).unwrap();

            let properties = load_properties();

            let mut system_properties = system_properties_area().lock().unwrap();
            for (key, value) in properties.iter() {
                match system_properties.find(key.as_str()).unwrap() {
                    Some(prop_ref) => {
                        system_properties.update(&prop_ref, value.as_str()).unwrap();
                    },
                    None => {
                        system_properties.add(key.as_str(), value.as_str()).unwrap();
                    }
                }
            }

            true
        });

        load_properties()
    }

    #[test]
    fn test_property_info() {
        enable_logger();

        let properties = build_property_dir(TEST_PROPERTY_DIR);

        let system_properties = system_properties();
        for (key, value) in properties.iter() {
            let prop_value = system_properties.get(key.as_str()).unwrap();
            assert_eq!(prop_value, value.as_str());
        }
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    #[test]
    fn test_wait() {
        enable_logger();

        let _properties = build_property_dir(TEST_PROPERTY_DIR);

        let test_prop = "test.property";
        let mut system_properties_area = system_properties_area().lock().unwrap();

        let wait_any = || {
            std::thread::spawn(move || {
                let system_properties = system_properties();
                system_properties.wait_any(system_properties.context_serial());
            })
        };

        let handle = wait_any();
        std::thread::sleep(std::time::Duration::from_millis(100));

        println!("Add property: {}", test_prop);
        system_properties_area.add(test_prop, "true").unwrap();
        handle.join().unwrap();
        println!("End add property: {}", test_prop);

        let handle = std::thread::spawn(move || {
            let system_properties = system_properties();
            let index = system_properties.find(&test_prop).unwrap();
            let serial = system_properties.serial(index.as_ref().unwrap());
            system_properties.wait(index.as_ref(), serial, None);
        });

        let handle_any = wait_any();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let index = system_properties_area.find(&test_prop).unwrap();
        system_properties_area.update(&index.unwrap(), "false").unwrap();

        handle.join().unwrap();
        handle_any.join().unwrap();
    }
}
