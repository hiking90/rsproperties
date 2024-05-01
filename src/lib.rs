// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

#[macro_use]
extern crate zerocopy;

mod property_info_parser;
mod errors;
mod system_properties;
mod contexts_serialized;
mod property_area;
mod context_node;
mod property_info;

pub use errors::*;

pub const PROP_VALUE_MAX: usize = 92;
pub const PROP_DIRNAME: &str = "/dev/__properties__";

lazy_static::lazy_static! {
    pub static ref SYSTEM_PROPERTIES: system_properties::SystemProperties =
        system_properties::SystemProperties::new(&std::path::Path::new(PROP_DIRNAME))
        .expect("Cannot open system properties. Please check if \"/dev/__properties__\" exists.");
}

pub(crate) fn bionic_align(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

pub fn get_with_default(name: &str, default: &str) -> String {
    SYSTEM_PROPERTIES.get(name).unwrap_or_else(|err| {
        log::error!("Error getting property {}: {}", name, err);
        default.to_string()
    })
}

pub fn get(name: &str) -> Result<String> {
    SYSTEM_PROPERTIES.get(name)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn test_get() {
        for prop in PROPERTIES.iter() {
            let value1 = get_with_default(prop, "");
            let value2 = AndroidSystemProperties::new().get(prop).unwrap_or_default();

            println!("{}: [{}], [{}]", prop, value1, value2);
            assert_eq!(value1, value2);
        }
    }
}
