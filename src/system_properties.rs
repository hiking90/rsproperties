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

use rserror::*;

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
        let contexts = ContextsSerialized::new(false, filename, &mut false, false)?;

        Ok(Self {
            contexts,
        })
    }

    // Create a new area for system properties
    // The new area is used by the property service to store system properties.
    #[cfg(feature = "builder")]
    pub fn new_area(dirname: &Path) -> Result<Self> {
        let contexts = ContextsSerialized::new(true, dirname, &mut false, false)?;

        Ok(Self {
            contexts,
        })
    }

    fn read_mutable_property_value(&self, prop_info: &PropertyInfo) -> Result<(u32, String)> {
        let new_serial = prop_info.serial.load(std::sync::atomic::Ordering::Acquire);
        let mut serial;
        loop {
            serial = new_serial;
            let _len: u32 = serial_value_len(serial);
            let value = if serial_dirty(serial) {
                let res = self.contexts.prop_area_for_name(prop_info.name().to_str()?)?;
                let pa = res.0.property_area();
                let value = pa.dirty_backup_area()?;
                value.as_str().map_err(Error::from)?.to_owned()
            } else {
                let value = prop_info.value();
                value.as_str().map_err(Error::from)?.to_owned()
            };
            fence(Ordering::Acquire);
            let new_serial = prop_info.serial.load(std::sync::atomic::Ordering::Acquire);
            if new_serial == serial {
                return Ok((serial, value));
            }
            fence(Ordering::Acquire);
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

    /// Get the value of a system property
    pub fn get(&self, name: &str) -> Result<String> {
        let res = self.contexts.prop_area_for_name(name)?;
        let pa = res.0.property_area();

        match pa.find(name) {
            Ok(pi) => {
                let (_name, value) = self.read(pi.0, false)?;
                Ok(value)
            }
            Err(_) => {
                Ok("".to_owned())
            }
        }
    }

    /// Get the property index of a system property by name.
    /// The property index is used to update the property value.
    /// If the property is not found, it returns Ok(None)
    pub fn find(&self, name: &str) -> Result<Option<PropertyIndex>> {
        let res = self.contexts.prop_area_for_name(name)?;
        let pa = res.0.property_area();
        match pa.find(name) {
            Ok(pi) => {
                Ok(Some(PropertyIndex {
                    context_index: res.1,
                    property_index: pi.1,
                }))
            }
            Err(_) => {
                Ok(None)
            }
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
            Some(prop_ref) => {
                self.update(&prop_ref, value)?;
            },
            None => {
                self.add(key, value)?;
            }
        }

        Ok(())
    }

    #[cfg(feature = "builder")]
    pub fn update(&mut self, index: &PropertyIndex, value: &str) -> Result<bool> {
        if value.len() >= PROP_VALUE_MAX {
            return Err(rserror!("Value too long: {value}"));
        }

        let mut res = self.contexts.prop_area_mut_with_index(index.context_index)?;
        let pa = res.property_area_mut();
        let pi = pa.property_info(index.property_index)?;

        let name = pi.name().to_bytes();
        if !name.is_empty() && &name[0..3] == b"ro." {
            return Err(rserror!("Try to update the read-only property: {name:?}"));
        }

        let mut serial = pi.serial.load(Ordering::Relaxed);
        let backup_value = pi.value().to_owned();

        // Before updating, the property value must be backed up
        pa.set_dirty_backup_area(&backup_value)?;
        fence(Ordering::Release);

        // Set dirty flag
        serial |= 1;
        let pi = pa.property_info(index.property_index)?;
        pi.serial.store(serial, Ordering::Relaxed);
        // Set the new value
        pi.set_value(value);
        fence(Ordering::Release);
        // Set the new serial. It is cleared the dirty flag and set the new length of the value.
        pi.serial.store((value.len() << 24) as u32 | ((serial + 1) & 0xffffff), std::sync::atomic::Ordering::Relaxed);
        futex_wake(&pi.serial)?;

        let serial_pa = self.contexts.serial_prop_area();
        serial_pa.serial().store(serial_pa.serial().load(Ordering::Relaxed) + 1, Ordering::Release);
        futex_wake(&serial_pa.serial())?;

        Ok(true)
    }

    #[cfg(feature = "builder")]
    pub fn add(&mut self, name: &str, value: &str) -> Result<()> {
        if value.len() >= PROP_VALUE_MAX && !name.starts_with("ro.") {
            return Err(rserror!("Value too long: {}", value.len()));
        }

        let mut res = self.contexts.prop_area_mut_for_name(name)?;
        let pa = res.0.property_area_mut();
        pa.add(name, value)?;

        let serial_pa = self.contexts.serial_prop_area();
        serial_pa.serial().store(serial_pa.serial().load(Ordering::Relaxed) + 1, Ordering::Release);
        futex_wake(&serial_pa.serial())?;

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
            let version1 = system_properties.get(VERSION_PROPERTY).unwrap();
            let version2 = AndroidSystemProperties::new().get(VERSION_PROPERTY).unwrap();
            assert_eq!(version1, version2);
        });

        let _ = handle.join().unwrap();

        Ok(())
    }

    #[cfg(feature = "builder")]
    #[test]
    fn test_property_update() -> Result<()> {
        Ok(())
    }
}
