// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::property_area::PropertyAreaMap;
use crate::errors::*;

pub(crate) struct ContextNode {
    access_rw: bool,
    filename: PathBuf,
    _context_offset: usize,
    property_area: RwLock<Option<PropertyAreaMap>>,
    _no_access: bool,
}

impl ContextNode {
    pub(crate) fn new(access_rw: bool, _context_offset: usize, filename: PathBuf) -> Self {
        Self {
            access_rw,
            filename: filename,
            _context_offset,
            property_area: RwLock::new(None),
            _no_access: false,
        }
    }

    pub(crate) fn open(&self, fsetxattr_failed: &mut bool) -> Result<()> {
        if self.access_rw == false {
            panic!("open() must be called with access_rw == true");
        }

        let mut prop_area = self.property_area.write().unwrap();
        if prop_area.is_some() {
            return Ok(());
        }

        *prop_area = Some(PropertyAreaMap::new_rw(self.filename.as_path(), None, fsetxattr_failed)?);

        Ok(())
    }

    // pub(crate) fn context_offset(&self) -> usize {
    //     self.context_offset
    // }

    pub(crate) fn property_area(&self) -> Result<PropertyAreaGuard<'_>> {
        loop {
            {
                let guard = self.property_area.read().unwrap();
                if guard.is_some() {
                    return Ok(PropertyAreaGuard { guard });
                }
            }
            let mut guard = self.property_area.write().unwrap();
            if guard.is_none() {
                *guard = Some(PropertyAreaMap::new_ro(self.filename.as_path())?);
            }
        }
    }

    pub(crate) fn property_area_mut(&self) -> Result<PropertyAreaMutGuard<'_>> {
        self.property_area()?;
        Ok(PropertyAreaMutGuard { guard: self.property_area.write().unwrap() })
    }
}

pub(crate) struct PropertyAreaGuard<'a> {
    guard: RwLockReadGuard<'a, Option<PropertyAreaMap>>
}

impl<'a> PropertyAreaGuard<'a> {
    pub(crate) fn property_area(&self) -> &PropertyAreaMap {
        self.guard.as_ref().expect("PropertyAreaMap is not initialized")
    }
}

pub(crate) struct PropertyAreaMutGuard<'a> {
    guard: RwLockWriteGuard<'a, Option<PropertyAreaMap>>
}

impl<'a> PropertyAreaMutGuard<'a> {
    pub(crate) fn property_area_mut(&mut self) -> &mut PropertyAreaMap {
        self.guard.as_mut().expect("PropertyAreaMap is not initialized")
    }
}