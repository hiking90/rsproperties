// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use crate::property_area::PropertyAreaMap;
use crate::errors::*;

pub(crate) struct ContextNode {
    filename: PathBuf,
    _context_offset: usize,
    property_area: Option<PropertyAreaMap>,
    _no_access: bool,
}

impl ContextNode {
    pub(crate) fn new(_context_offset: usize, filename: PathBuf) -> Self {
        Self {
            filename: filename,
            _context_offset,
            property_area: None,
            _no_access: false,
        }
    }

    pub(crate) fn open(&mut self, access_rw: bool, fsetxattr_failed: &mut bool) -> Result<()> {
        if self.property_area.is_some() {
            return Ok(());
        }
        let pa = if access_rw {
            PropertyAreaMap::new_rw(self.filename.as_path(), None, fsetxattr_failed)?
        } else {
            PropertyAreaMap::new_ro(self.filename.as_path())?
        };

        self.property_area = Some(pa);
        Ok(())
    }

    // pub(crate) fn context_offset(&self) -> usize {
    //     self.context_offset
    // }

    pub(crate) fn property_area(&self) -> Option<&PropertyAreaMap> {
        self.property_area.as_ref()
    }

    pub(crate) fn property_area_mut(&mut self) -> Option<&mut PropertyAreaMap> {
        self.property_area.as_mut()
    }
}