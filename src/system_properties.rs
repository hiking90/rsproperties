// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::atomic::{fence, Ordering, AtomicU32};

use rustix::{
    fs::Timespec,
    path::Arg,
};
#[cfg(any(target_os = "android", target_os = "linux"))]
use rustix::thread::futex;

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
        log::info!("Creating SystemProperties from path: {:?}", filename);

        let contexts = match ContextsSerialized::new(false, filename, &mut false, false) {
            Ok(contexts) => {
                log::info!("Successfully loaded contexts from: {:?}", filename);
                contexts
            },
            Err(e) => {
                log::error!("Failed to load contexts from {:?}: {}", filename, e);
                return Err(e);
            }
        };

        log::debug!("SystemProperties created successfully");
        Ok(Self {
            contexts,
        })
    }

    // Create a new area for system properties
    // The new area is used by the property service to store system properties.
    #[cfg(all(feature = "builder", target_os = "linux"))]
    pub fn new_area(dirname: &Path) -> Result<Self> {
        log::info!("Creating SystemProperties area from directory: {:?}", dirname);

        let contexts = match ContextsSerialized::new(true, dirname, &mut false, false) {
            Ok(contexts) => {
                log::info!("Successfully created area from: {:?}", dirname);
                contexts
            },
            Err(e) => {
                log::error!("Failed to create area from {:?}: {}", dirname, e);
                return Err(e);
            }
        };

        log::debug!("SystemProperties area created successfully");
        Ok(Self {
            contexts,
        })
    }

    fn read_mutable_property_value(&self, prop_info: &PropertyInfo) -> Result<(u32, String)> {
        log::debug!("Reading mutable property value for: {:?}", prop_info.name());

        loop {
            // Read current serial at the beginning of each iteration
            let serial = prop_info.serial.load(Ordering::Acquire);
            let _len: u32 = serial_value_len(serial);
            log::trace!("Property serial: {}, length: {}, dirty: {}", serial, _len, serial_dirty(serial));

            let value = if serial_dirty(serial) {
                log::debug!("Reading dirty property value from backup area");
                let res = match self.contexts.prop_area_for_name(prop_info.name().to_str()?) {
                    Ok(res) => res,
                    Err(e) => {
                        log::error!("Failed to get property area for name {:?}: {}", prop_info.name(), e);
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
                log::debug!("Reading property value from property info");
                let value = prop_info.value();
                value.as_str().map_err(Error::from)?.to_owned()
            };

            // Ensure all previous loads are completed before checking serial again
            fence(Ordering::Acquire);

            // Check if serial hasn't changed during our read operation
            let final_serial = prop_info.serial.load(Ordering::Acquire);
            if final_serial == serial {
                log::debug!("Successfully read property value: {} (length: {})", value, value.len());
                return Ok((serial, value));
            }

            log::trace!("Serial changed during read ({} -> {}), retrying...", serial, final_serial);
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

    /// Internal function to get property value that returns error for missing properties
    pub fn get_with_result(&self, name: &str) -> Result<String> {
        log::debug!("Getting property value for: {}", name);

        let res = match self.contexts.prop_area_for_name(name) {
            Ok(res) => {
                log::trace!("Found property area for: {}", name);
                res
            },
            Err(e) => {
                log::error!("Failed to find property area for {}: {}", name, e);
                return Err(e);
            }
        };
        let pa = res.0.property_area();

        match pa.find(name) {
            Ok(pi) => {
                log::trace!("Found property info for: {}", name);
                let (_name, value) = match self.read(pi.0, false) {
                    Ok(result) => result,
                    Err(e) => {
                        log::error!("Failed to read property {}: {}", name, e);
                        return Err(e);
                    }
                };
                log::debug!("Successfully retrieved property {}: {}", name, value);
                Ok(value)
            }
            Err(e) => {
                log::debug!("Property {} not found: {}", name, e);
                Err(e)
            }
        }
    }

    /// Get the value of a system property
    /// Returns empty string if property is not found (Android compatible behavior)
    pub fn get(&self, name: &str) -> String {
        match self.get_with_result(name) {
            Ok(value) => value,
            Err(_) => {
                log::warn!("Property {} not found, returning empty string", name);
                "".to_owned()
            }
        }
    }

    /// Get the value of a system property with a default value
    /// Returns the default value if property is not found or on error
    pub fn get_with_default(&self, name: &str, default: &str) -> String {
        match self.get_with_result(name) {
            Ok(value) => value,
            Err(_) => {
                log::debug!("Property {} not found, returning default: {}", name, default);
                default.to_owned()
            }
        }
    }

    /// Get the property index of a system property by name.
    /// The property index is used to update the property value.
    /// If the property is not found, it returns Ok(None)
    pub fn find(&self, name: &str) -> Result<Option<PropertyIndex>> {
        log::debug!("Finding property index for: {}", name);

        let res = match self.contexts.prop_area_for_name(name) {
            Ok(res) => {
                log::trace!("Found property area for: {}", name);
                res
            },
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
                log::debug!("Found property index for {}: context={}, property={}",
                           name, index.context_index, index.property_index);
                Ok(Some(index))
            }
            Err(e) => {
                log::debug!("Property {} not found: {}", name, e);
                Ok(None)
            }
        }
    }

    /// Set the value of a system property
    /// If the property is not found, it creates a new property.
    /// If the property value is too long, it returns an error.
    /// If the property is read-only, it returns an error.
    /// If the property is updated successfully, it returns Ok(()).
    #[cfg(all(feature = "builder", target_os = "linux"))]
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        log::info!("Setting property: {} = {}", key, value);

        match self.find(key)? {
            Some(prop_ref) => {
                log::debug!("Property {} exists, updating", key);
                match self.update(&prop_ref, value) {
                    Ok(_) => {
                        log::info!("Successfully updated property: {}", key);
                    },
                    Err(e) => {
                        log::error!("Failed to update property {}: {}", key, e);
                        return Err(e);
                    }
                }
            },
            None => {
                log::debug!("Property {} does not exist, creating new", key);
                match self.add(key, value) {
                    Ok(_) => {
                        log::info!("Successfully created property: {}", key);
                    },
                    Err(e) => {
                        log::error!("Failed to create property {}: {}", key, e);
                        return Err(e);
                    }
                }
            }
        }

        Ok(())
    }

    #[cfg(all(feature = "builder", target_os = "linux"))]
    pub fn update(&mut self, index: &PropertyIndex, value: &str) -> Result<bool> {
        log::debug!("Updating property at index context={}, property={} with value: {}",
                   index.context_index, index.property_index, value);

        if value.len() >= PROP_VALUE_MAX {
            let error_msg = format!("Value too long: {} (max: {})", value.len(), PROP_VALUE_MAX);
            log::error!("{}", error_msg);
            return Err(Error::new_context(error_msg).into());
        }

        let mut res = match self.contexts.prop_area_mut_with_index(index.context_index) {
            Ok(res) => res,
            Err(e) => {
                log::error!("Failed to get mutable property area for context {}: {}", index.context_index, e);
                return Err(e);
            }
        };
        let pa = res.property_area_mut();
        let pi = match pa.property_info(index.property_index) {
            Ok(pi) => pi,
            Err(e) => {
                log::error!("Failed to get property info for index {}: {}", index.property_index, e);
                return Err(e);
            }
        };

        let name = pi.name().to_bytes();
        if !name.is_empty() && &name[0..3] == b"ro." {
            let error_msg = format!("Try to update the read-only property: {name:?}");
            log::error!("{}", error_msg);
            return Err(Error::new_context(error_msg).into());
        }

        let mut serial = pi.serial.load(Ordering::Relaxed);
        let backup_value = pi.value().to_owned();
        log::trace!("Current serial: {}, backing up value: {:?}", serial, backup_value);

        // Before updating, the property value must be backed up
        match pa.set_dirty_backup_area(&backup_value) {
            Ok(_) => log::trace!("Backup area set successfully"),
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
        log::trace!("Set dirty flag, serial: {}", serial);

        // Set the new value
        pi.set_value(value);
        fence(Ordering::Release);

        // Set the new serial. It is cleared the dirty flag and set the new length of the value.
        let new_serial = (value.len() << 24) as u32 | ((serial + 1) & 0xffffff);
        pi.serial.store(new_serial, std::sync::atomic::Ordering::Relaxed);
        log::trace!("Updated value and serial: {}", new_serial);

        match futex_wake(&pi.serial) {
            Ok(_) => log::trace!("Property futex wake successful"),
            Err(e) => {
                log::error!("Failed to wake property futex: {}", e);
                return Err(e);
            }
        }

        let serial_pa = self.contexts.serial_prop_area();
        let old_serial = serial_pa.serial().load(Ordering::Relaxed);
        serial_pa.serial().store(old_serial + 1, Ordering::Release);
        log::trace!("Updated global serial from {} to {}", old_serial, old_serial + 1);

        match futex_wake(&serial_pa.serial()) {
            Ok(_) => log::trace!("Global serial futex wake successful"),
            Err(e) => {
                log::error!("Failed to wake global serial futex: {}", e);
                return Err(e);
            }
        }

        log::info!("Successfully updated property at index context={}, property={}",
                  index.context_index, index.property_index);
        Ok(true)
    }

    #[cfg(all(feature = "builder", target_os = "linux"))]
    pub fn add(&mut self, name: &str, value: &str) -> Result<()> {
        log::info!("Adding new property: {} = {}", name, value);

        if value.len() >= PROP_VALUE_MAX && !name.starts_with("ro.") {
            let error_msg = format!("Value too long: {} (max: {}) for property: {}",
                                   value.len(), PROP_VALUE_MAX, name);
            log::error!("{}", error_msg);
            return Err(Error::new_context(error_msg).into());
        }

        let mut res = match self.contexts.prop_area_mut_for_name(name) {
            Ok(res) => {
                log::trace!("Got mutable property area for: {}", name);
                res
            },
            Err(e) => {
                log::error!("Failed to get mutable property area for {}: {}", name, e);
                return Err(e);
            }
        };
        let pa = res.0.property_area_mut();

        match pa.add(name, value) {
            Ok(_) => {
                log::debug!("Successfully added property to area: {}", name);
            },
            Err(e) => {
                log::error!("Failed to add property {} to area: {}", name, e);
                return Err(e);
            }
        }

        let serial_pa = self.contexts.serial_prop_area();
        let old_serial = serial_pa.serial().load(Ordering::Relaxed);
        serial_pa.serial().store(old_serial + 1, Ordering::Release);
        log::trace!("Updated global serial from {} to {} after adding property", old_serial, old_serial + 1);

        match futex_wake(&serial_pa.serial()) {
            Ok(_) => {
                log::trace!("Global serial futex wake successful after adding property");
            },
            Err(e) => {
                log::error!("Failed to wake global serial futex after adding property: {}", e);
                return Err(e);
            }
        }

        log::info!("Successfully added new property: {}", name);
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
                    Some(pi) => {
                        pi.serial.load(Ordering::Acquire)
                    }
                    None => {
                        log::error!("Failed to get PropertyInfo for index: {}", idx.property_index);
                        0
                    }
                }
            }
            None => {
                log::error!("Failed to get PropertyArea for index: {}", idx.context_index);
                0
            }
        }
    }

    pub fn wait_any(&self) -> Option<u32> {
        self.wait(None, None)
    }

    pub fn wait(&self, index: Option<&PropertyIndex>, timeout: Option<&Timespec>) -> Option<u32> {
        match index {
            Some(idx) => {
                match self.contexts.prop_area_with_index(idx.context_index).ok() {
                    Some(guard) => {
                        let pa = guard.property_area();
                        match pa.property_info(idx.property_index).ok() {
                            Some(pi) => {
                                futex_wait(
                                    &pi.serial,
                                    pi.serial.load(Ordering::Acquire),
                                    timeout)
                            }
                            None => {
                                log::error!("Failed to get PropertyInfo for index: {}", idx.property_index);
                                None
                            }
                        }
                    }
                    None => {
                        log::error!("Failed to get PropertyArea for index: {}", idx.context_index);
                        None
                    }
                }
            }
            None => {
                let serial_pa = self.contexts.serial_prop_area().serial();
                futex_wait(
                    &serial_pa,
                    serial_pa.load(Ordering::Acquire),
                    timeout)
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
            let version1 = system_properties.get(VERSION_PROPERTY);
            let version2 = AndroidSystemProperties::new().get(VERSION_PROPERTY).unwrap();
            assert_eq!(version1, version2);
        });

        let _ = handle.join().unwrap();

        Ok(())
    }

    #[cfg(all(feature = "builder", target_os = "linux"))]
    #[test]
    fn test_property_update() -> Result<()> {
        Ok(())
    }
}
