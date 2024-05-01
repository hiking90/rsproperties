use std::path::{Path, PathBuf};
use std::ffi::CStr;

use rustix::fs;

use crate::system_properties::Contexts;
use crate::context_node::ContextNode;
use crate::property_area::{PropertyArea, PropertyAreaMap};
use crate::errors::*;
use crate::property_info_parser::PropertyInfoAreaFile;

pub(crate) struct ContextsSerialized {
    dirname: PathBuf,
    tree_filename: PathBuf,
    serial_filename: PathBuf,
    property_info_area_file: PropertyInfoAreaFile,
    context_nodes: Vec<ContextNode>,
    serial_property_area_map: PropertyAreaMap,
}

impl ContextsSerialized {
    pub(crate) fn new(writable: bool, dirname: &Path, fsetxattr_failed: &mut bool, load_default_path: bool) -> Result<Self> {
        let dirname = dirname.to_path_buf();
        let tree_filename = dirname.join("property_info");
        let serial_filename = dirname.join("properties_serial");

        let property_info_area_file = if load_default_path {
            PropertyInfoAreaFile::load_default_path()
        } else {
            PropertyInfoAreaFile::load_path(tree_filename.as_path())
        }?;

        let property_info_area = property_info_area_file.property_info_area();
        let num_context_nodes = property_info_area.num_contexts();
        let mut context_nodes = Vec::with_capacity(num_context_nodes);

        for i in 0..num_context_nodes {
            let context_offset = property_info_area.context_offset(i);
            let filename = dirname.join(property_info_area.cstr(context_offset).to_str().unwrap());
            context_nodes.push(ContextNode::new(context_offset, filename))
        }

        let serial_property_area_map = if writable {
            fs::mkdir(dirname.as_path(), fs::Mode::RWXU | fs::Mode::XGRP | fs::Mode::XOTH)
                .map_err(Error::new_errno)?;

            *fsetxattr_failed = false;

            for node in &mut context_nodes {
                let mut fsetxattr_failed = false;
                // let filename = dirname.join(property_info_area.cstr(node.context_offset()).to_str().unwrap());
                node.open(true, &mut fsetxattr_failed)?;
            }

            Self::map_serial_property_area(serial_filename.as_path(), true, fsetxattr_failed)?
        } else {
            Self::map_serial_property_area(serial_filename.as_path(), false, fsetxattr_failed)?
        };

        Ok(Self {
            dirname,
            tree_filename,
            serial_filename,
            property_info_area_file,
            context_nodes,
            serial_property_area_map,
        })
    }

    fn map_serial_property_area(serial_filename: &Path, access_rw: bool, fsetxattr_failed: &mut bool) -> Result<PropertyAreaMap> {
        if access_rw {
            let context = unsafe { CStr::from_bytes_with_nul_unchecked(b"u:object_r:properties_serial:s0\0") };
            PropertyAreaMap::new_rw(serial_filename, Some(context), fsetxattr_failed)
        } else {
            PropertyAreaMap::new_ro(serial_filename)
        }
    }
}

impl Contexts for ContextsSerialized {
    fn get_prop_area_for_name(&mut self, name: &str) -> Result<Option<&PropertyAreaMap>> {
        let (index, _) = self.property_info_area_file
            .property_info_area()
            .get_property_info_indexes(name);
        if index == u32::MAX || index >= self.context_nodes.len() as u32 {
            return Err(Error::new_invalid_data(format!("Could not find context for property {name}")));
        }

        let context_node = &mut self.context_nodes[index as usize];
        context_node.open(false, &mut false)?;

        Ok(context_node.get_property_area())
    }

    fn get_serial_prop_name(&self) -> Result<&PropertyArea> {
        unimplemented!()
    }

    fn reset_access(&mut self) {

    }
}
