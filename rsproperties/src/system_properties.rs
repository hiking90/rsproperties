// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::atomic::{fence, AtomicU32, Ordering};

#[cfg(any(target_os = "android", target_os = "linux"))]
use rustix::thread::futex;
use rustix::{fs::Timespec, path::Arg};

use crate::errors::*;

use crate::contexts_serialized::ContextsSerialized;
use crate::property_info::PropertyInfo;

pub(crate) const PROP_VALUE_MAX: usize = 92;
pub(crate) const PROP_TREE_FILE: &str = "/dev/__properties__/property_info";

#[inline(always)]
fn serial_value_len(serial: u32) -> u32 {
    serial >> 24
}

#[inline(always)]
fn serial_dirty(serial: u32) -> bool {
    (serial & 1) != 0
}

#[cfg(feature = "builder")]
fn futex_wake(_addr: &AtomicU32) -> Result<usize> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        futex::wake(_addr, futex::Flags::empty(), i32::MAX as u32)
            .context_with_location("Failed to wake futex")
        // .map_err(Error::new_errno)
    }
    #[cfg(target_os = "macos")]
    Ok(0)
}

fn futex_wait(_serial: &AtomicU32, _value: u32, _timeout: Option<&Timespec>) -> Option<u32> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    loop {
        match futex::wait(_serial, futex::Flags::empty(), _value as _, _timeout) {
            Ok(_) => {
                let new_serial = _serial.load(Ordering::Acquire);
                if _value != new_serial {
                    return Some(new_serial);
                }
            }
            Err(e) => {
                log::error!("Failed to wait for property change: {}", e);
                return None;
            }
        }
    }
    #[cfg(target_os = "macos")]
    None
}

// To avoid lifetime issues, the property index is used to access the property value.
pub struct PropertyIndex {
    pub(crate) context_index: u32,
    pub(crate) property_index: u32,
}

/// System properties
/// It can't be created directly. Use `system_properties()` or `system_properties_area()` instead.
pub struct SystemProperties {
    contexts: ContextsSerialized,
}

impl SystemProperties {
    // Create a new system properties to read system properties from a file or a directory.
    pub(crate) fn new(filename: &Path) -> Result<Self> {
        let contexts = match ContextsSerialized::new(false, filename, &mut false, false) {
            Ok(contexts) => contexts,
            Err(e) => {
                log::error!("Failed to load contexts from {:?}: {}", filename, e);
                return Err(e);
            }
        };

        Ok(Self { contexts })
    }

    // Create a new area for system properties
    // The new area is used by the property service to store system properties.
    #[cfg(feature = "builder")]
    pub fn new_area(dirname: &Path) -> Result<Self> {
        let contexts = match ContextsSerialized::new(true, dirname, &mut false, false) {
            Ok(contexts) => contexts,
            Err(e) => {
                log::error!("Failed to create area from {:?}: {}", dirname, e);
                return Err(e);
            }
        };

        Ok(Self { contexts })
    }

    fn read_mutable_property_value(&self, prop_info: &PropertyInfo) -> Result<(u32, String)> {
        loop {
            // Read current serial at the beginning of each iteration
            let serial = prop_info.serial.load(Ordering::Acquire);
            let _len: u32 = serial_value_len(serial);

            let value = if serial_dirty(serial) {
                let res = match self.contexts.prop_area_for_name(prop_info.name().to_str()?) {
                    Ok(res) => res,
                    Err(e) => {
                        log::error!(
                            "Failed to get property area for name {:?}: {}",
                            prop_info.name(),
                            e
                        );
                        return Err(e);
                    }
                };
                let pa = res.0.property_area();
                let value = match pa.dirty_backup_area() {
                    Ok(value) => value,
                    Err(e) => {
                        log::error!("Failed to read dirty backup area: {}", e);
                        return Err(e);
                    }
                };
                value.as_str().map_err(Error::from)?.to_owned()
            } else {
                let value = prop_info.value();
                value.as_str().map_err(Error::from)?.to_owned()
            };

            // Ensure all previous loads are completed before checking serial again
            fence(Ordering::Acquire);

            // Check if serial hasn't changed during our read operation
            let final_serial = prop_info.serial.load(Ordering::Acquire);
            if final_serial == serial {
                return Ok((serial, value));
            }

            // No need for additional fence here as we'll acquire again at loop start
        }
    }

    fn read(&self, prop_info: &PropertyInfo, is_name: bool) -> Result<(Option<String>, String)> {
        let (_serial, value) = self.read_mutable_property_value(prop_info)?;
        let name_cstr = prop_info.name();

        let name = if is_name {
            Some(name_cstr.to_str()?.to_owned())
        } else {
            None
        };

        Ok((name, value))
    }

    /// Get property value that returns error for missing properties
    pub fn get_with_result(&self, name: &str) -> Result<String> {
        let res = match self.contexts.prop_area_for_name(name) {
            Ok(res) => res,
            Err(e) => {
                log::error!("Failed to find property area for {}: {}", name, e);
                return Err(e);
            }
        };
        let pa = res.0.property_area();

        match pa.find(name) {
            Ok(pi) => {
                let (_name, value) = match self.read(pi.0, false) {
                    Ok(result) => result,
                    Err(e) => {
                        log::error!("Failed to read property {}: {}", name, e);
                        return Err(e);
                    }
                };
                Ok(value)
            }
            Err(e) => Err(e),
        }
    }

    /// Get the property index of a system property by name.
    /// The property index is used to update the property value.
    /// If the property is not found, it returns Ok(None)
    pub fn find(&self, name: &str) -> Result<Option<PropertyIndex>> {
        let res = match self.contexts.prop_area_for_name(name) {
            Ok(res) => res,
            Err(e) => {
                log::error!("Failed to find property area for {}: {}", name, e);
                return Err(e);
            }
        };
        let pa = res.0.property_area();
        match pa.find(name) {
            Ok(pi) => {
                let index = PropertyIndex {
                    context_index: res.1,
                    property_index: pi.1,
                };
                Ok(Some(index))
            }
            Err(_) => Ok(None),
        }
    }

    /// Set the value of a system property
    /// If the property is not found, it creates a new property.
    /// If the property value is too long, it returns an error.
    /// If the property is read-only, it returns an error.
    /// If the property is updated successfully, it returns Ok(()).
    #[cfg(feature = "builder")]
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match self.find(key)? {
            Some(prop_ref) => match self.update(&prop_ref, value) {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Failed to update property {}: {}", key, e);
                    return Err(e);
                }
            },
            None => match self.add(key, value) {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Failed to create property {}: {}", key, e);
                    return Err(e);
                }
            },
        }

        Ok(())
    }

    #[cfg(feature = "builder")]
    pub fn update(&mut self, index: &PropertyIndex, value: &str) -> Result<bool> {
        if value.len() >= PROP_VALUE_MAX {
            let error_msg = format!("Value too long: {} (max: {})", value.len(), PROP_VALUE_MAX);
            log::error!("{}", error_msg);
            return Err(Error::new_file_validation(error_msg));
        }

        let mut res = match self.contexts.prop_area_mut_with_index(index.context_index) {
            Ok(res) => res,
            Err(e) => {
                log::error!(
                    "Failed to get mutable property area for context {}: {}",
                    index.context_index,
                    e
                );
                return Err(e);
            }
        };
        let pa = res.property_area_mut();
        let pi = match pa.property_info(index.property_index) {
            Ok(pi) => pi,
            Err(e) => {
                log::error!(
                    "Failed to get property info for index {}: {}",
                    index.property_index,
                    e
                );
                return Err(e);
            }
        };

        let name = pi.name().to_bytes();
        if !name.is_empty() && &name[0..3] == b"ro." {
            let error_msg = format!("Try to update the read-only property: {name:?}");
            log::error!("{}", error_msg);
            return Err(Error::new_permission_denied(error_msg));
        }

        let mut serial = pi.serial.load(Ordering::Relaxed);
        let backup_value = pi.value().to_owned();

        // Before updating, the property value must be backed up
        match pa.set_dirty_backup_area(&backup_value) {
            Ok(_) => {}
            Err(e) => {
                log::error!("Failed to set backup area: {}", e);
                return Err(e);
            }
        }
        fence(Ordering::Release);

        // Set dirty flag
        serial |= 1;
        let pi = match pa.property_info(index.property_index) {
            Ok(pi) => pi,
            Err(e) => {
                log::error!("Failed to get property info after backup: {}", e);
                return Err(e);
            }
        };
        pi.serial.store(serial, Ordering::Relaxed);

        // Set the new value
        pi.set_value(value);
        fence(Ordering::Release);

        // Set the new serial. It is cleared the dirty flag and set the new length of the value.
        let new_serial = (value.len() << 24) as u32 | ((serial + 1) & 0xffffff);
        pi.serial
            .store(new_serial, std::sync::atomic::Ordering::Relaxed);

        match futex_wake(&pi.serial) {
            Ok(_) => {}
            Err(e) => {
                log::error!("Failed to wake property futex: {}", e);
                return Err(e);
            }
        }

        let serial_pa = self.contexts.serial_prop_area();
        let old_serial = serial_pa.serial().load(Ordering::Relaxed);
        serial_pa.serial().store(old_serial + 1, Ordering::Release);

        match futex_wake(serial_pa.serial()) {
            Ok(_) => {}
            Err(e) => {
                log::error!("Failed to wake global serial futex: {}", e);
                return Err(e);
            }
        }

        Ok(true)
    }

    #[cfg(feature = "builder")]
    pub fn add(&mut self, name: &str, value: &str) -> Result<()> {
        if value.len() >= PROP_VALUE_MAX && !name.starts_with("ro.") {
            let error_msg = format!(
                "Value too long: {} (max: {}) for property: {}",
                value.len(),
                PROP_VALUE_MAX,
                name
            );
            log::error!("{}", error_msg);
            return Err(Error::new_file_validation(error_msg));
        }

        let mut res = match self.contexts.prop_area_mut_for_name(name) {
            Ok(res) => res,
            Err(e) => {
                log::error!("Failed to get mutable property area for {}: {}", name, e);
                return Err(e);
            }
        };
        let pa = res.0.property_area_mut();

        match pa.add(name, value) {
            Ok(_) => {}
            Err(e) => {
                log::error!("Failed to add property {} to area: {}", name, e);
                return Err(e);
            }
        }

        let serial_pa = self.contexts.serial_prop_area();
        let old_serial = serial_pa.serial().load(Ordering::Relaxed);
        serial_pa.serial().store(old_serial + 1, Ordering::Release);

        match futex_wake(serial_pa.serial()) {
            Ok(_) => {}
            Err(e) => {
                log::error!(
                    "Failed to wake global serial futex after adding property: {}",
                    e
                );
                return Err(e);
            }
        }

        Ok(())
    }

    pub fn context_serial(&self) -> u32 {
        let serial_pa = self.contexts.serial_prop_area();
        serial_pa.serial().load(Ordering::Acquire)
    }

    pub fn serial(&self, idx: &PropertyIndex) -> u32 {
        match self.contexts.prop_area_with_index(idx.context_index).ok() {
            Some(guard) => {
                let pa = guard.property_area();
                match pa.property_info(idx.property_index).ok() {
                    Some(pi) => pi.serial.load(Ordering::Acquire),
                    None => {
                        log::error!(
                            "Failed to get PropertyInfo for index: {}",
                            idx.property_index
                        );
                        0
                    }
                }
            }
            None => {
                log::error!(
                    "Failed to get PropertyArea for index: {}",
                    idx.context_index
                );
                0
            }
        }
    }

    pub fn wait_any(&self) -> Option<u32> {
        self.wait(None, None)
    }

    pub fn wait(&self, index: Option<&PropertyIndex>, timeout: Option<&Timespec>) -> Option<u32> {
        match index {
            Some(idx) => match self.contexts.prop_area_with_index(idx.context_index).ok() {
                Some(guard) => {
                    let pa = guard.property_area();
                    match pa.property_info(idx.property_index).ok() {
                        Some(pi) => {
                            futex_wait(&pi.serial, pi.serial.load(Ordering::Acquire), timeout)
                        }
                        None => {
                            log::error!(
                                "Failed to get PropertyInfo for index: {}",
                                idx.property_index
                            );
                            None
                        }
                    }
                }
                None => {
                    log::error!(
                        "Failed to get PropertyArea for index: {}",
                        idx.context_index
                    );
                    None
                }
            },
            None => {
                let serial_pa = self.contexts.serial_prop_area().serial();
                futex_wait(serial_pa, serial_pa.load(Ordering::Acquire), timeout)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_imports)]
    use super::*;

    #[cfg(target_os = "android")]
    use android_system_properties::AndroidSystemProperties;

    #[cfg(target_os = "android")]
    const VERSION_PROPERTY: &str = "ro.build.version.release";

    #[cfg(target_os = "android")]
    #[test]
    fn test_system_properties() -> Result<()> {
        let system_properties = SystemProperties::new(&Path::new(crate::PROP_DIRNAME)).unwrap();

        let handle = std::thread::spawn(move || {
            let version1 = system_properties
                .get_with_result(VERSION_PROPERTY)
                .unwrap_or_default();
            let version2 = AndroidSystemProperties::new()
                .get(VERSION_PROPERTY)
                .unwrap_or_default();
            assert_eq!(version1, version2);
        });

        let _ = handle.join().unwrap();

        Ok(())
    }
}
