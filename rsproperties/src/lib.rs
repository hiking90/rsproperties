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
//! #[cfg(target_os = "android")]
//! {
//!     use rsproperties::{init, PropertyConfig};
//!     use std::path::PathBuf;
//!
//!     // Initialize with defaults
//!     rsproperties::init(None);
//!
//!     // Initialize with custom directories
//!     let config = PropertyConfig {
//!         properties_dir: Some(PathBuf::from("/custom/properties")),
//!         socket_dir: Some(PathBuf::from("/custom/socket")),
//!     };
//!     rsproperties::init(config);
//!
//!     // Get a value of the property.
//!     let value = rsproperties::get_with_default("ro.build.version.sdk", "0");
//!     println!("ro.build.version.sdk: {}", value);
//!
//!     // Set a value of the property.
//!     rsproperties::set("test.property", "test.value").unwrap();
//! }
//! ```

use std::{
    sync::OnceLock,
    path::{PathBuf, Path},
};

/// Configuration for initializing the property system
#[derive(Debug, Clone, Default)]
pub struct PropertyConfig {
    /// Directory for reading system properties (default: "/dev/__properties__")
    pub properties_dir: Option<PathBuf>,
    /// Directory for property service sockets (default: "/dev/socket")
    pub socket_dir: Option<PathBuf>,
}

// Implement From traits for backward compatibility and convenience
impl From<PathBuf> for PropertyConfig {
    fn from(path: PathBuf) -> Self {
        Self {
            properties_dir: Some(path),
            socket_dir: None,
        }
    }
}

impl From<String> for PropertyConfig {
    fn from(path: String) -> Self {
        Self {
            properties_dir: Some(PathBuf::from(path)),
            socket_dir: None,
        }
    }
}

impl From<&str> for PropertyConfig {
    fn from(path: &str) -> Self {
        Self {
            properties_dir: Some(PathBuf::from(path)),
            socket_dir: None,
        }
    }
}

impl PropertyConfig {
    /// Create config from optional PathBuf (for backward compatibility)
    pub fn from_optional_path(path: Option<PathBuf>) -> Self {
        match path {
            Some(path) => Self::from(path),
            None => Self::default(),
        }
    }

    /// Create config with only properties directory
    pub fn with_properties_dir<P: Into<PathBuf>>(dir: P) -> Self {
        Self {
            properties_dir: Some(dir.into()),
            socket_dir: None,
        }
    }

    /// Create config with only socket directory
    pub fn with_socket_dir<P: Into<PathBuf>>(dir: P) -> Self {
        Self {
            properties_dir: None,
            socket_dir: Some(dir.into()),
        }
    }

    /// Create config with both directories
    pub fn with_both_dirs<P1: Into<PathBuf>, P2: Into<PathBuf>>(
        properties_dir: P1,
        socket_dir: P2
    ) -> Self {
        Self {
            properties_dir: Some(properties_dir.into()),
            socket_dir: Some(socket_dir.into()),
        }
    }
}

pub mod errors;
pub use errors::{Error, Result, ContextWithLocation};

mod property_info_parser;
mod system_properties;
mod contexts_serialized;
mod property_area;
mod context_node;
mod property_info;
mod system_property_set;
#[cfg(all(feature = "builder", target_os = "linux"))]
mod property_info_serializer;
#[cfg(all(feature = "builder", target_os = "linux"))]
mod trie_builder;
#[cfg(all(feature = "builder", target_os = "linux"))]
mod trie_serializer;
#[cfg(all(feature = "builder", target_os = "linux"))]
mod trie_node_arena;
#[cfg(all(feature = "builder", target_os = "linux"))]
mod build_property_parser;

pub use system_properties::SystemProperties;
#[cfg(all(feature = "builder", target_os = "linux"))]
pub use property_info_serializer::*;
#[cfg(all(feature = "builder", target_os = "linux"))]
pub use build_property_parser::*;
pub use system_property_set::socket_dir;

pub use system_property_set::{
    PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME,
    PROPERTY_SERVICE_SOCKET_NAME,
};

pub const PROP_VALUE_MAX: usize = 92;
pub const PROP_DIRNAME: &str = "/dev/__properties__";

// System properties directory.
static SYSTEM_PROPERTIES_DIR: OnceLock<PathBuf> = OnceLock::new();
// Global system properties.
static SYSTEM_PROPERTIES: OnceLock<system_properties::SystemProperties> = OnceLock::new();

/// Initialize system properties with flexible configuration options.
///
/// # Arguments
/// * `config` - Can be:
///   - `None` - Use default directories
///   - `Some(PathBuf)` - Set only properties directory (backward compatibility)
///   - `Some(PropertyConfig)` - Full configuration
///
/// # Examples
/// ```rust
/// use rsproperties::{init, PropertyConfig};
/// use std::path::PathBuf;
///
/// // Set only properties directory (backward compatible)
/// init(PropertyConfig::from(PathBuf::from("/custom/properties")));
///
/// // Full configuration
/// let config = PropertyConfig {
///     properties_dir: Some(PathBuf::from("/custom/properties")),
///     socket_dir: Some(PathBuf::from("/custom/socket")),
/// };
/// init(config);
/// ```
pub fn init(config: PropertyConfig) {
    // Initialize properties directory
    let props_dir = config.properties_dir.unwrap_or_else(|| {
        log::info!("Using default properties directory: {}", PROP_DIRNAME);
        PathBuf::from(PROP_DIRNAME)
    });

    match SYSTEM_PROPERTIES_DIR.set(props_dir.clone()) {
        Ok(_) => {
            log::info!("Successfully set system properties directory to: {:?}", props_dir);
        },
        Err(_) => {
            log::warn!("System properties directory already set, ignoring new value");
        }
    }

    // Initialize socket directory if specified
    if let Some(socket_dir) = config.socket_dir {
        let success = system_property_set::set_socket_dir(&socket_dir);
        if success {
            log::info!("Successfully set socket directory to: {:?}", socket_dir);
        } else {
            log::warn!("Socket directory already set, ignoring new value");
        }
    }
}

/// Get the system properties directory.
/// It returns None if init() is not called.
/// It returns Some(&PathBuf) if init() is called.
pub fn properties_dir() -> &'static Path {
    let path = SYSTEM_PROPERTIES_DIR.get().expect("Call init() first.").as_path();
    log::trace!("Getting system properties directory: {:?}", path);
    path
}

/// Get the system properties.
/// Before calling this function, init() must be called.
/// It panics if init() is not called or the system properties cannot be opened.
pub fn system_properties() -> &'static system_properties::SystemProperties {
    SYSTEM_PROPERTIES.get_or_init(|| {
        let dir = properties_dir();
        log::info!("Initializing global SystemProperties instance from: {:?}", dir);

        match system_properties::SystemProperties::new(dir) {
            Ok(props) => {
                log::info!("Successfully initialized global SystemProperties instance");
                props
            },
            Err(e) => {
                panic!("Failed to initialize SystemProperties from {:?}: {}", dir, e);
            }
        }
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
    system_properties().get_with_default(name, default)
}

/// Get a value of the property.
/// If the property is not found, it returns an empty string.
/// If an error occurs, it returns Err.
pub fn get(name: &str) -> String {
    system_properties().get(name)
}

pub fn get_with_result(name: &str) -> Result<String> {
    system_properties().get_with_result(name)
}

/// Set a value of the property.
/// If an error occurs, it returns Err.
/// It uses socket communication to set the property. Because it is designed for client applications,
pub fn set(name: &str, value: &str) -> Result<()> {
    system_property_set::set(name, value)
}

#[cfg(test)]
mod tests {
    #![allow(unused_imports)]
    use super::*;
    use std::fs::{File, remove_dir_all, create_dir};
    use std::io::Write;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};
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

    #[cfg(all(feature = "builder", target_os = "linux"))]
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

    #[cfg(all(feature = "builder", target_os = "linux"))]
    fn system_properties_area() -> MutexGuard<'static, Option<SystemProperties>> {
        static SYSTEM_PROPERTIES: Mutex<Option<SystemProperties>> = Mutex::new(None);
        let mut system_properties_guard = SYSTEM_PROPERTIES.lock().unwrap();

        if let None = *system_properties_guard {
            *system_properties_guard = Some(build_property_dir(TEST_PROPERTY_DIR));
        }
        system_properties_guard
    }

    #[cfg(all(feature = "builder", target_os = "linux"))]
    fn build_property_dir(dir: &str) -> SystemProperties {
        crate::init(PropertyConfig::from(PathBuf::from(dir)));

        let property_contexts_files = vec![
            "tests/android/plat_property_contexts",
            "tests/android/system_ext_property_contexts",
            "tests/android/vendor_property_contexts",
        ];

        let mut property_infos = Vec::new();
        for file in property_contexts_files {
            let (mut property_info, errors) = PropertyInfoEntry::parse_from_file(Path::new(file), false).unwrap();
            if !errors.is_empty() {
                log::error!("{:?}", errors);
            }
            property_infos.append(&mut property_info);
        }

        let data: Vec<u8> = build_trie(&property_infos, "u:object_r:build_prop:s0", "string").unwrap();

        let dir = properties_dir();
        remove_dir_all(dir).unwrap_or_default();
        create_dir(dir).unwrap_or_default();
        File::create(dir.join("property_info")).unwrap().write_all(&data).unwrap();

        let properties = load_properties();

        let dir = properties_dir();
        let mut system_properties = SystemProperties::new_area(dir)
            .unwrap_or_else(|e| panic!("Cannot create system properties: {}. Please check if {dir:?} exists.", e));
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

        system_properties
    }

    #[cfg(all(feature = "builder", target_os = "linux"))]
    #[test]
    fn test_property_info() {
        enable_logger();

        let _guard = system_properties_area();

        let system_properties = system_properties();

        let properties = load_properties();

        for (key, value) in properties.iter() {
            let prop_value = system_properties.get(key.as_str());
            assert_eq!(prop_value, value.as_str());
        }
    }

    #[cfg(all(feature = "builder", target_os = "linux"))]
    #[test]
    fn test_wait() {
        enable_logger();

        let mut guard = system_properties_area();

        let system_properties_area = guard.as_mut().unwrap();

        let test_prop = "test.property";

        let wait_any = || {
            std::thread::spawn(move || {
                let system_properties = system_properties();
                system_properties.wait_any();
            })
        };

        let handle = wait_any();
        std::thread::sleep(std::time::Duration::from_millis(100));

        system_properties_area.add(test_prop, "true").unwrap();
        handle.join().unwrap();

        let handle = std::thread::spawn(move || {
            let system_properties = system_properties();
            let index = system_properties.find(test_prop).unwrap();
            // let serial = system_properties.serial(index.as_ref().unwrap());
            system_properties.wait(index.as_ref(), None);
        });

        let handle_any = wait_any();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let index = system_properties_area.find(test_prop).unwrap();
        system_properties_area.update(&index.unwrap(), "false").unwrap();

        handle.join().unwrap();
        handle_any.join().unwrap();
    }
}
