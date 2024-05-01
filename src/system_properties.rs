// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::RwLock;
use std::sync::atomic::{fence, Ordering};

use rustix::fs;
use rustix::path::Arg;

use crate::property_area::{PropertyArea, PropertyAreaMap};
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


pub trait Contexts : Send + Sync {
    fn get_prop_area_for_name(&mut self, name: &str) -> Result<Option<&PropertyAreaMap>>;
    fn get_serial_prop_name(&self) -> Result<&PropertyArea>;
    fn reset_access(&mut self);
}

pub struct SystemProperties {
    contexts: RwLock<Box<dyn Contexts>>,
}

impl SystemProperties {
    pub fn new(filename: &Path) -> Result<Self> {
        let contexts = if filename.is_dir() {
            if fs::access(Path::new(PROP_TREE_FILE), fs::Access::READ_OK).is_ok() {
                Box::new(ContextsSerialized::new(false, filename, &mut false, false)?)
            } else {
                unimplemented!()
            }
        } else {
            unimplemented!()
        };

        Ok(Self {
            contexts: RwLock::new(contexts),
        })
    }

    fn find_unlocked<'a>(&'a self, name: &'a str, contexts: &'a mut Box<dyn Contexts>) -> Result<Option<&'a PropertyInfo>> {
        let pa = contexts.get_prop_area_for_name(name)?;
        match pa {
            Some(pa) => {
                Ok(Some(pa.find(name)?))
            }
            None => Ok(None),
        }
    }

    // pub(crate) fn find<'a>(&'a self, name: &'a str) -> Result<Option<&'a PropertyInfo>> {
    //     let contexts: std::sync::RwLockWriteGuard<'_, Box<dyn Contexts>> = self.contexts.write().unwrap();
    //     let res = self.find_unlocked(name, &mut contexts)?;
    //     res
    // }

    fn read_mutable_property_value(&self, prop_info: &PropertyInfo) -> Result<(u32, String)> {
        let new_serial = prop_info.serial.load(std::sync::atomic::Ordering::Acquire);
        let mut serial;
        loop {
            serial = new_serial;
            let len: u32 = serial_value_len(serial);
            let value = if serial_dirty(serial) {
                let mut guard = self.contexts.write().unwrap();
                let pa = guard
                    .get_prop_area_for_name(prop_info.name().to_str().map_err(Error::new_utf8)?)?
                    .ok_or(Error::new_invalid_data("Invalid PropertyInfo".to_owned()))?;
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

    pub fn get(&self, name: &str) -> Result<String> {
        match self.find_unlocked(name, &mut self.contexts.write().unwrap())? {
            Some(prop_info) => {
                return self.read(prop_info, false).map(|(_, value)| value);
            }
            None => {
                Ok("".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use android_system_properties::AndroidSystemProperties;

    #[cfg(test)]
    const VERSION_PROPERTY: &str = "ro.build.version.release";

    #[test]
    fn test_system_properties() -> Result<()> {
        let system_properties = SystemProperties::new(&Path::new(crate::PROP_DIRNAME)).unwrap();

        let handle = thread::spawn(move || {
            let version1 = system_properties.get(VERSION_PROPERTY).unwrap();
            let version2 = AndroidSystemProperties::new().get(VERSION_PROPERTY).unwrap();
            assert_eq!(version1, version2);
        });

        let _ = handle.join().unwrap();

        Ok(())
    }
}
