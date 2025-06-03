// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use log::{debug, info, error, trace};

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
        debug!("Creating new ContextNode: path={:?}, access_rw={}, context_offset={}",
               filename, access_rw, _context_offset);
        Self {
            access_rw,
            filename,
            _context_offset,
            property_area: RwLock::new(None),
            _no_access: false,
        }
    }

    pub(crate) fn open(&self, fsetxattr_failed: &mut bool) -> Result<()> {
        debug!("Opening context node: {:?}", self.filename);

        if !self.access_rw {
            error!("Attempted to open context node without write access: {:?}", self.filename);
            panic!("open() must be called with access_rw == true");
        }

        let mut prop_area = self.property_area.write().unwrap();
        if prop_area.is_some() {
            debug!("Context node already open: {:?}", self.filename);
            return Ok(());
        }

        trace!("Creating new read-write property area map for context: {:?}", self.filename);
        *prop_area = Some(PropertyAreaMap::new_rw(self.filename.as_path(), None, fsetxattr_failed)?);

        info!("Successfully opened context node: {:?}", self.filename);
        Ok(())
    }

    // pub(crate) fn context_offset(&self) -> usize {
    //     self.context_offset
    // }

    pub(crate) fn property_area(&self) -> Result<PropertyAreaGuard<'_>> {
        trace!("Accessing property area for context: {:?}", self.filename);

        loop {
            {
                let guard = self.property_area.read().unwrap();
                if guard.is_some() {
                    trace!("Property area already initialized for: {:?}", self.filename);
                    return Ok(PropertyAreaGuard { guard });
                }
            }
            debug!("Initializing property area for context: {:?}", self.filename);
            let mut guard = self.property_area.write().unwrap();
            if guard.is_none() {
                trace!("Creating read-only property area map for: {:?}", self.filename);
                *guard = Some(PropertyAreaMap::new_ro(self.filename.as_path())?);
                info!("Successfully initialized property area for: {:?}", self.filename);
            }
        }
    }

    pub(crate) fn property_area_mut(&self) -> Result<PropertyAreaMutGuard<'_>> {
        debug!("Accessing mutable property area for context: {:?}", self.filename);
        self.property_area()?;
        trace!("Obtained mutable access to property area: {:?}", self.filename);
        Ok(PropertyAreaMutGuard { guard: self.property_area.write().unwrap() })
    }
}

// PropertyAreaGuard is used to get a reference to the PropertyAreaMap.
pub(crate) struct PropertyAreaGuard<'a> {
    guard: RwLockReadGuard<'a, Option<PropertyAreaMap>>
}

impl<'a> PropertyAreaGuard<'a> {
    pub(crate) fn property_area(&self) -> &PropertyAreaMap {
        trace!("Getting property area reference from guard");
        self.guard.as_ref().expect("PropertyAreaMap is not initialized")
    }
}

// PropertyAreaMutGuard is used to get a mutable reference to the PropertyAreaMap.
// Option<PropertyAreaMap> is initialized when the PropertyAreaMap is first accessed.
// After that, the PropertyAreaMap is never replaced.
// This is to ensure that the PropertyAreaMap is not replaced by another PropertyAreaMap.
pub(crate) struct PropertyAreaMutGuard<'a> {
    guard: RwLockWriteGuard<'a, Option<PropertyAreaMap>>
}

impl<'a> PropertyAreaMutGuard<'a> {
    pub(crate) fn property_area_mut(&mut self) -> &mut PropertyAreaMap {
        trace!("Getting mutable property area reference from guard");
        self.guard.as_mut().expect("PropertyAreaMap is not initialized")
    }
}