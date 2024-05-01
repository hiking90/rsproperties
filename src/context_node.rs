use std::sync::RwLock;
use std::path::PathBuf;

use crate::property_area::PropertyAreaMap;
use crate::errors::*;

pub struct ContextNode {
    filename: PathBuf,
    context_offset: usize,
    property_area: Option<PropertyAreaMap>,
    no_access: bool,
}

impl ContextNode {
    pub fn new(context_offset: usize, filename: PathBuf) -> Self {
        Self {
            filename: filename,
            context_offset,
            property_area: None,
            no_access: false,
        }
    }

    pub fn open(&mut self, access_rw: bool, fsetxattr_failed: &mut bool) -> Result<()> {
        let mut property_area = &mut self.property_area;
        if property_area.is_some() {
            return Ok(());
        }
        let pa = if access_rw {
            PropertyAreaMap::new_rw(self.filename.as_path(), None, fsetxattr_failed)?
        } else {
            PropertyAreaMap::new_ro(self.filename.as_path())?
        };

        *property_area = Some(pa);
        Ok(())
    }

    pub fn context_offset(&self) -> usize {
        self.context_offset
    }

    pub fn get_property_area(&self) -> Option<&PropertyAreaMap> {
        self.property_area.as_ref()
        // let mut property_area = self.property_area.;
        // match *property_area {
        //     Some(pa) => Some(&pa),
        //     None => {
        //         *property_area = PropertyAreaMap::new_ro(self.filename.as_path()).ok();
        //         property_area.as_ref()
        //     }
        // }
    }

    // pub fn reset_access(&self) {
    //     self.no_access = false;
    // }
}