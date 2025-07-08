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
//! ```rust,no_run
//! #[cfg(target_os = "android")]
//! {
//!     // Get a value of the property.
//!     let value: String = rsproperties::get_or("ro.build.version.sdk", "0".to_owned());
//!     println!("ro.build.version.sdk: {}", value);
//!
//!     // Set a value of the property - use string literals for compatibility
//!     rsproperties::set("test.property", "test.value").unwrap();
//!
//!     // For Android system properties, prefer string format used by the system
//!     rsproperties::set("ro.debuggable", "1").unwrap();  // Not &true
//! }
//! ```

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
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
        socket_dir: P2,
    ) -> Self {
        Self {
            properties_dir: Some(properties_dir.into()),
            socket_dir: Some(socket_dir.into()),
        }
    }

    /// Create a new builder for PropertyConfig
    pub fn builder() -> PropertyConfigBuilder {
        PropertyConfigBuilder::default()
    }
}

/// Builder for PropertyConfig with validation
#[derive(Debug, Clone, Default)]
pub struct PropertyConfigBuilder {
    properties_dir: Option<PathBuf>,
    socket_dir: Option<PathBuf>,
}

impl PropertyConfigBuilder {
    /// Set the properties directory
    pub fn properties_dir<P: Into<PathBuf>>(mut self, dir: P) -> Self {
        self.properties_dir = Some(dir.into());
        self
    }

    /// Set the socket directory
    pub fn socket_dir<P: Into<PathBuf>>(mut self, dir: P) -> Self {
        self.socket_dir = Some(dir.into());
        self
    }

    /// Build the PropertyConfig
    pub fn build(self) -> PropertyConfig {
        PropertyConfig {
            properties_dir: self.properties_dir,
            socket_dir: self.socket_dir,
        }
    }
}

pub mod errors;
pub use errors::{ContextWithLocation, Error, Result};

#[cfg(feature = "builder")]
mod build_property_parser;
mod context_node;
mod contexts_serialized;
mod property_area;
mod property_info;
mod property_info_parser;
#[cfg(feature = "builder")]
mod property_info_serializer;
mod system_properties;
mod system_property_set;
#[cfg(feature = "builder")]
mod trie_builder;
#[cfg(feature = "builder")]
mod trie_node_arena;
#[cfg(feature = "builder")]
mod trie_serializer;

#[cfg(feature = "builder")]
pub use build_property_parser::*;
#[cfg(feature = "builder")]
pub use property_info_serializer::*;
pub use system_properties::SystemProperties;
pub use system_property_set::socket_dir;

pub use system_property_set::{
    PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME, PROPERTY_SERVICE_SOCKET_NAME,
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
/// ```rust,no_run
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
        log::info!("Using default properties directory: {PROP_DIRNAME}");
        PathBuf::from(PROP_DIRNAME)
    });

    match SYSTEM_PROPERTIES_DIR.set(props_dir.clone()) {
        Ok(_) => {
            log::info!("Successfully set system properties directory to: {props_dir:?}");
        }
        Err(_) => {
            log::warn!("System properties directory already set, ignoring new value");
        }
    }

    // Initialize socket directory if specified
    if let Some(socket_dir) = config.socket_dir {
        let success = system_property_set::set_socket_dir(&socket_dir);
        if success {
            log::info!("Successfully set socket directory to: {socket_dir:?}");
        } else {
            log::warn!("Socket directory already set, ignoring new value");
        }
    }
}

/// Get the system properties directory.
/// Returns the configured directory if init() was called,
/// otherwise returns the default PROP_DIRNAME (/dev/__properties__).
pub fn properties_dir() -> &'static Path {
    let path = SYSTEM_PROPERTIES_DIR
        .get_or_init(|| {
            log::info!("Using default properties directory: {PROP_DIRNAME}");
            PathBuf::from(PROP_DIRNAME)
        })
        .as_path();
    path
}

/// Get the system properties.
/// Before calling this function, init() must be called.
/// It panics if init() is not called or the system properties cannot be opened.
pub fn system_properties() -> &'static system_properties::SystemProperties {
    SYSTEM_PROPERTIES.get_or_init(|| {
        let dir = properties_dir();
        log::debug!("Initializing global SystemProperties instance from: {dir:?}");

        match system_properties::SystemProperties::new(dir) {
            Ok(props) => {
                log::debug!("Successfully initialized global SystemProperties instance");
                props
            }
            Err(e) => {
                log::error!("Failed to initialize SystemProperties from {dir:?}: {e}");
                panic!("Failed to initialize SystemProperties from {dir:?}: {e}");
            }
        }
    })
}

// Get alignment value for bionic style.
pub(crate) fn bionic_align(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

/// Get a property value parsed to specified type
/// Returns Err if property not found, system error, or parse error occurs
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::get;
///
/// let sdk_version: i32 = get("ro.build.version.sdk").unwrap();
/// let is_debuggable: bool = get("ro.debuggable").unwrap();
/// let version: String = get("ro.build.version.release").unwrap();
///
/// // With fallback
/// let sdk_version: i32 = get("ro.build.version.sdk").unwrap_or(0);
/// let version: String = get("ro.build.version.release").unwrap_or_default();
/// ```
pub fn get<T>(name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = system_properties().get_with_result(name)?;
    value.parse().map_err(|e| {
        Error::Parse(format!(
            "Failed to parse '{value}' for property '{name}': {e}"
        ))
    })
}

/// Get a property value with default fallback
/// Never fails - always returns a valid value
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::get_or;
///
/// let sdk_version: i32 = get_or("ro.build.version.sdk", 0);
/// let is_debuggable: bool = get_or("ro.debuggable", false);
/// let version: String = get_or("ro.build.version.release", "unknown".to_owned());
/// ```
pub fn get_or<T>(name: &str, default: T) -> T
where
    T: std::str::FromStr + Clone,
{
    match system_properties().get_with_result(name) {
        Ok(value) if !value.is_empty() => value.parse().unwrap_or(default),
        _ => default,
    }
}

/// Set a value of the property with any Display type.
///
/// **Important**: All values are converted to strings using the `Display` trait before being stored.
/// This means that when reading properties set by other applications or systems, you should be aware
/// of potential format differences. For example:
/// - Boolean values are stored as "true"/"false" (Rust format)
/// - Numbers may have different precision or formatting
/// - Different applications may use different string representations for the same logical value
///
/// For maximum compatibility with existing Android properties, consider using string literals
/// when setting well-known system properties that may be read by other applications.
///
/// If an error occurs, it returns Err.
/// It uses socket communication to set the property. Because it is designed for client applications.
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::set;
///
/// // Setting various types (all converted to strings)
/// set("test.int.property", &42).unwrap();           // Stored as "42"
/// set("test.bool.property", &true).unwrap();        // Stored as "true"
/// set("test.float.property", &3.14).unwrap();       // Stored as "3.14"
/// set("test.string.property", &"hello").unwrap();   // Stored as "hello"
///
/// // For Android system properties, prefer string literals for compatibility
/// set("ro.debuggable", "1").unwrap();               // Better than set("ro.debuggable", &1)
/// set("persist.sys.timezone", "Asia/Seoul").unwrap();
/// ```
///
/// # Compatibility Notes
/// - Android system properties typically use "0"/"1" for boolean values, not "true"/"false"
/// - Numeric properties may have specific formatting requirements
/// - Always test compatibility when setting properties that will be read by other applications
pub fn set<T: std::fmt::Display + ?Sized>(name: &str, value: &T) -> Result<()> {
    system_property_set::set(name, &value.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(unused_imports)]
    use super::*;
    #[cfg(target_os = "android")]
    use android_system_properties::AndroidSystemProperties;
    use std::collections::HashMap;
    use std::fs::{create_dir, remove_dir_all, File};
    use std::io::Write;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    const TEST_PROPERTY_DIR: &str = "__properties__";

    #[cfg(any(feature = "builder", target_os = "android"))]
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
            "wifi.supplicant_scan_interval",
        ];

        enable_logger();
        for prop in PROPERTIES.iter() {
            let value1: String = get_or(prop, "".to_owned());
            let value2 = AndroidSystemProperties::new().get(prop).unwrap_or_default();

            println!("{}: [{}], [{}]", prop, value1, value2);
            assert_eq!(value1, value2);
        }
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
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
            load_properties_from_file(Path::new(file), None, "u:r:init:s0", &mut properties)
                .unwrap();
        }

        properties
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    fn system_properties_area() -> MutexGuard<'static, Option<SystemProperties>> {
        static SYSTEM_PROPERTIES: Mutex<Option<SystemProperties>> = Mutex::new(None);
        let mut system_properties_guard = SYSTEM_PROPERTIES.lock().unwrap();

        if system_properties_guard.is_none() {
            *system_properties_guard = Some(build_property_dir(TEST_PROPERTY_DIR));
        }
        system_properties_guard
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    fn build_property_dir(dir: &str) -> SystemProperties {
        crate::init(PropertyConfig::from(PathBuf::from(dir)));

        let property_contexts_files = vec![
            "tests/android/plat_property_contexts",
            "tests/android/system_ext_property_contexts",
            "tests/android/vendor_property_contexts",
        ];

        let mut property_infos = Vec::new();
        for file in property_contexts_files {
            let (mut property_info, errors) =
                PropertyInfoEntry::parse_from_file(Path::new(file), false).unwrap();
            if !errors.is_empty() {
                log::error!("{errors:?}");
            }
            property_infos.append(&mut property_info);
        }

        let data: Vec<u8> =
            build_trie(&property_infos, "u:object_r:build_prop:s0", "string").unwrap();

        let dir = properties_dir();
        remove_dir_all(dir).unwrap_or_default();
        create_dir(dir).unwrap_or_default();
        File::create(dir.join("property_info"))
            .unwrap()
            .write_all(&data)
            .unwrap();

        let properties = load_properties();

        let dir = properties_dir();
        let mut system_properties = SystemProperties::new_area(dir).unwrap_or_else(|e| {
            panic!("Cannot create system properties: {e}. Please check if {dir:?} exists.")
        });
        for (key, value) in properties.iter() {
            match system_properties.find(key.as_str()).unwrap() {
                Some(prop_ref) => {
                    system_properties.update(&prop_ref, value.as_str()).unwrap();
                }
                None => {
                    system_properties.add(key.as_str(), value.as_str()).unwrap();
                }
            }
        }

        system_properties
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    #[test]
    fn test_property_info() {
        enable_logger();

        let _guard = system_properties_area();

        let system_properties = system_properties();

        let properties = load_properties();

        for (key, value) in properties.iter() {
            let prop_value = system_properties
                .get_with_result(key.as_str())
                .unwrap_or_default();
            assert_eq!(prop_value, value.as_str());
        }
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
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
        system_properties_area
            .update(&index.unwrap(), "false")
            .unwrap();

        handle.join().unwrap();
        handle_any.join().unwrap();
    }
}
