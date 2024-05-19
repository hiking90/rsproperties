// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::RwLock;
use std::sync::atomic::{fence, Ordering, AtomicU32};

use rustix::{
    fs::Timespec,
    path::Arg,
};
#[cfg(any(target_os = "android", target_os = "linux"))]
use rustix::thread::{futex, FutexOperation, FutexFlags};

use crate::property_area::{PropertyAreaMap, PropertyArea};
use crate::contexts_serialized::ContextsSerialized;
use crate::errors::*;
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

fn futex_wake(addr: &AtomicU32, _count: i32) -> Result<usize> {
    let _addr = addr.as_ptr() as *mut u32;
    #[cfg(any(target_os = "android", target_os = "linux"))]
    unsafe {
        futex(_addr, FutexOperation::Wake, FutexFlags::PRIVATE, _count as _, std::ptr::null(), std::ptr::null_mut(), 0)
            .map_err(Error::new_errno)
    }
    #[cfg(target_os = "macos")]
    Ok(0)
}

fn futex_wait(addr: &AtomicU32, _value: i32, _timeout: Option<&Timespec>) -> Result<usize> {
    let _addr = addr.as_ptr() as *mut u32;
    #[cfg(any(target_os = "android", target_os = "linux"))]
    unsafe {
        let timeout = match _timeout {
            Some(timeout) => timeout as *const Timespec,
            None => std::ptr::null_mut(),
        };
        futex(_addr, FutexOperation::Wait, FutexFlags::PRIVATE, _value as _, timeout, std::ptr::null_mut(), 0)
            .map_err(Error::new_errno)
    }
    #[cfg(target_os = "macos")]
    Ok(0)
}

pub trait Contexts : Send + Sync {
    fn get_prop_area_for_name(&mut self, name: &str) -> Result<Option<&mut PropertyAreaMap>>;
    fn get_serial_prop_name(&self) -> Result<&PropertyAreaMap>;
    fn get_serial_prop_area(&self) -> &PropertyArea;
    fn reset_access(&mut self);
}

/// System properties
pub struct SystemProperties {
    contexts: RwLock<Box<dyn Contexts>>,
}

impl SystemProperties {
    /// Create a new system properties to read system properties from a file or a directory.
    pub fn new(filename: &Path) -> Result<Self> {
        let contexts = if filename.is_dir() {
            match ContextsSerialized::new(false, filename, &mut false, false) {
                Ok(contexts) => Box::new(contexts),
                Err(e) => {
                    log::error!("Failed to create ContextsSerialized: {e:?}");
                    unimplemented!("ContextsSplit")
                }
            }
        } else {
            unimplemented!("ContextsPreSplit")
        };

        Ok(Self {
            contexts: RwLock::new(contexts),
        })
    }

    /// Create a new area for system properties
    /// The new area is used by the property service to store system properties.
    pub fn new_area(filename: &Path) -> Result<Self> {
        let contexts = Box::new(ContextsSerialized::new(true, filename, &mut false, false)?);

        Ok(Self {
            contexts: RwLock::new(contexts),
        })
    }

    fn read_mutable_property_value(&self, prop_info: &PropertyInfo) -> Result<(u32, String)> {
        let new_serial = prop_info.serial.load(std::sync::atomic::Ordering::Acquire);
        let mut serial;
        loop {
            serial = new_serial;
            let _len: u32 = serial_value_len(serial);
            let value = if serial_dirty(serial) {
                let mut guard = self.contexts.write().unwrap();
                let pa = guard
                    .get_prop_area_for_name(prop_info.name().to_str().map_err(Error::new_utf8)?)?
                    .ok_or(Error::new_custom("Invalid PropertyInfo".to_owned()))?;
                let value = pa.dirty_backup_area()?;
                value.as_str().map_err(Error::new_errno)?.to_owned()
            } else {
                let value = prop_info.value();
                value.as_str().map_err(Error::new_errno)?.to_owned()
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
            Some(name_cstr.to_str().map_err(Error::new_utf8)?.to_owned())
        } else {
            None
        };

        Ok((name, value))
    }

    /// Get the value of a system property
    pub fn get(&self, name: &str) -> Result<String> {
        let mut contexts = self.contexts.write().unwrap();
        match contexts.get_prop_area_for_name(name)? {
            Some(pa) => {
                match pa.find(name) {
                    Ok(pi) => {
                        let (_name, value) = self.read(pi, false)?;
                        Ok(value)
                    }
                    Err(_) => {
                        Ok("".to_owned())
                    }
                }
            }
            None => {
                Ok("".to_owned())
            }
        }
    }

    pub fn update(&mut self, name: &str, value: &str) -> Result<bool> {
        if value.len() > PROP_VALUE_MAX {
            return Err(Error::new_custom(format!("Value too long: {name}:{}", value.len())));
        }

        let mut contexts = self.contexts.write().unwrap();

        let pa = match contexts.get_prop_area_for_name(name)? {
            Some(pa) => {
                pa
            }
            None => {
                return Ok(false);
            }
        };

        let pi = match pa.find(name) {
            Ok(pi) => {
                pi
            }
            Err(_) => {
                return Ok(false);
            }
        };

        if name.starts_with("ro.") {
            return Err(Error::new_custom(format!("Try to update the read-only property: {name}")));
        }

        let mut serial = pi.serial.load(Ordering::Relaxed);

        // Before updating, the property value must be backed up
        pa.set_dirty_backup_area(pi.value())?;
        fence(Ordering::Release);

        // Set dirty flag
        serial |= 1;
        pi.serial.store(serial, Ordering::Relaxed);
        // Set the new value
        pi.set_value(value);
        fence(Ordering::Release);
        // Set the new serial. It is cleared the dirty flag and set the new length of the value.
        pi.serial.store((value.len() << 24) as u32 | ((serial + 1) & 0xffffff), std::sync::atomic::Ordering::Relaxed);
        futex_wake(&pi.serial, i32::MAX)?;

        let serial_pa = contexts.get_serial_prop_area();
        serial_pa.serial().store(serial_pa.serial().load(Ordering::Relaxed) + 1, Ordering::Release);
        futex_wake(serial_pa.serial(), i32::MAX)?;

        Ok(true)
    }

    pub fn add(&mut self, name: &str, value: &str) -> Result<()> {
        if value.len() > PROP_VALUE_MAX && name.starts_with("ro.") == false {
            return Err(Error::new_custom(format!("Value too long: {}", value.len())));
        }

        let mut contexts = self.contexts.write().unwrap();

        let pa = contexts.get_prop_area_for_name(name)?
            .ok_or(Error::new_custom(format!("Can't find a PropertyArea for {name}")))?;
        pa.add(name, value)?;

        let serial_pa = contexts.get_serial_prop_area();
        serial_pa.serial().store(serial_pa.serial().load(Ordering::Relaxed) + 1, Ordering::Release);
        futex_wake(serial_pa.serial(), i32::MAX)?;

        Ok(())
    }

    pub fn wait_any(&self, old_serial: u32) -> Option<u32> {
        self.wait(None, old_serial, None)
    }

    pub fn wait(&self, pi: Option<&PropertyInfo>, old_serial: u32, timeout: Option<&Timespec>) -> Option<u32> {
        let contexts = self.contexts.read().unwrap();
        let serial = match pi {
            Some(pi) => {
                &pi.serial
            }
            None => {
                let serial_pa = contexts.get_serial_prop_area();
                serial_pa.serial()
            }
        };

        loop {
            match futex_wait(&serial, old_serial as _, timeout) {
                Ok(_) => {
                    let new_serial = serial.load(Ordering::Acquire);
                    if old_serial != new_serial {
                        return Some(new_serial);
                    }
                }
                Err(e) => {
                    log::error!("Failed to wait for property change: {}", e);
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
