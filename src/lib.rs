// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::OnceLock;

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

static SYSTEM_PROPERTIES: OnceLock<system_properties::SystemProperties> = OnceLock::new();

fn system_properties() -> &'static system_properties::SystemProperties {
    SYSTEM_PROPERTIES.get_or_init(|| {
        system_properties::SystemProperties::new(&std::path::Path::new(PROP_DIRNAME))
            .expect("Cannot open system properties. Please check if \"/dev/__properties__\" exists.")
    })
}

pub(crate) fn bionic_align(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

pub fn get_with_default(name: &str, default: &str) -> String {
    system_properties().get(name).unwrap_or_else(|err| {
        log::error!("Error getting property {}: {}", name, err);
        default.to_string()
    })
}

pub fn get(name: &str) -> Result<String> {
    system_properties().get(name)
}

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
    use std::mem;
    use android_system_properties::AndroidSystemProperties;

    #[cfg(test)]
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

    fn enable_logger() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[test]
    fn test_get() {
        enable_logger();
        for prop in PROPERTIES.iter() {
            let value1 = get_with_default(prop, "");
            let value2 = AndroidSystemProperties::new().get(prop).unwrap_or_default();

            println!("{}: [{}], [{}]", prop, value1, value2);
            assert_eq!(value1, value2);
        }
    }

    #[test]
    fn test_set() -> Result<()> {
        enable_logger();
        let prop = "test.property";
        let value = "test.value";

        set(prop, value)?;

        let value1: String = get(prop)?;
        assert_eq!(value1, value);

        Ok(())
    }

    #[test]
    fn test_property_info() {
        enable_logger();

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

        let target_dir = Path::new("__properties__");
        remove_dir_all(&target_dir).unwrap_or_default();
        create_dir(target_dir).unwrap_or_default();
        File::create(target_dir.join("property_info")).unwrap().write_all(&data).unwrap();

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

        let mut system_properties = SystemProperties::new_area(target_dir).unwrap();
        for (key, value) in properties.iter() {
            match system_properties.update(key.as_str(), value.as_str()) {
                Ok(true) => {},
                Ok(false) => {
                    system_properties.add(key.as_str(), value.as_str()).unwrap();
                },
                Err(err) => {
                    println!("Error updating property {}: {}", key, err);
                }
            }
        }
        mem::drop(system_properties);

        let system_properties = SystemProperties::new(target_dir).unwrap();
        for (key, value) in properties.iter() {
            let prop_value = system_properties.get(key.as_str()).unwrap();
            if value.len() > PROP_VALUE_MAX {
                assert_eq!(prop_value, "");
            } else {
                assert_eq!(prop_value, value.as_str());
            }
        }
    }
}
