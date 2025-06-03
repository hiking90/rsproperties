// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::ffi::CStr;

use rustix::fs;
use log::{trace, debug, info, warn, error};
use crate::errors::*;

use crate::context_node::{ContextNode, PropertyAreaGuard};
#[cfg(feature = "builder")]
use crate::context_node::PropertyAreaMutGuard;
use crate::property_area::{PropertyArea, PropertyAreaMap};
use crate::property_info_parser::PropertyInfoAreaFile;

// Pre-defined CStr constants to avoid unsafe code at runtime
// Using const_str macro or safer compile-time construction
const PROPERTIES_SERIAL_CONTEXT: &CStr = {
    // Safe compile-time CStr construction
    match CStr::from_bytes_with_nul(b"u:object_r:properties_serial:s0\0") {
        Ok(cstr) => cstr,
        Err(_) => panic!("Invalid CStr constant"),
    }
};

pub(crate) struct ContextsSerialized {
    property_info_area_file: PropertyInfoAreaFile,
    context_nodes: Vec<ContextNode>,
    serial_property_area_map: PropertyAreaMap,
}

impl ContextsSerialized {
    pub(crate) fn new(writable: bool, dirname: &Path, fsetxattr_failed: &mut bool, load_default_path: bool) -> Result<Self> {
        debug!("Creating ContextsSerialized with writable={}, dirname={:?}, load_default_path={}", writable, dirname, load_default_path);

        let dirname = dirname.to_path_buf();
        let tree_filename = dirname.join("property_info");
        let serial_filename = dirname.join("properties_serial");

        trace!("Property info file: {:?}, serial file: {:?}", tree_filename, serial_filename);

        let property_info_area_file = if load_default_path {
            debug!("Loading property info from default path");
            PropertyInfoAreaFile::load_default_path()
        } else {
            debug!("Loading property info from path: {:?}", tree_filename);
            PropertyInfoAreaFile::load_path(tree_filename.as_path())
        }?;

        let property_info_area = property_info_area_file.property_info_area();
        let num_context_nodes = property_info_area.num_contexts();
        debug!("Creating {} context nodes", num_context_nodes);
        let mut context_nodes = Vec::with_capacity(num_context_nodes);

        for i in 0..num_context_nodes {
            let context_offset = property_info_area.context_offset(i);
            let context_name = property_info_area.cstr(context_offset).to_str().unwrap();
            let filename = dirname.join(context_name);
            trace!("Creating context node {}: {} -> {:?}", i, context_name, filename);
            context_nodes.push(ContextNode::new(writable, context_offset, filename))
        }

        let serial_property_area_map = if writable {
            debug!("Setting up writable serial property area");
            if !dirname.is_dir() {
                info!("Creating directory: {:?}", dirname);
                fs::mkdir(dirname.as_path(), fs::Mode::RWXU | fs::Mode::XGRP | fs::Mode::XOTH)
                    .map_err(Error::from)?;
            }

            *fsetxattr_failed = false;

            debug!("Opening context nodes for writing");
            for (i, node) in context_nodes.iter_mut().enumerate() {
                trace!("Opening context node {} for writing", i);
                node.open(fsetxattr_failed)?;
            }

            Self::map_serial_property_area(serial_filename.as_path(), true, fsetxattr_failed)?
        } else {
            debug!("Setting up read-only serial property area");
            Self::map_serial_property_area(serial_filename.as_path(), false, fsetxattr_failed)?
        };

        info!("Successfully created ContextsSerialized with {} context nodes", context_nodes.len());
        Ok(Self {
            property_info_area_file,
            context_nodes,
            serial_property_area_map,
        })
    }

    fn map_serial_property_area(serial_filename: &Path, access_rw: bool, fsetxattr_failed: &mut bool) -> Result<PropertyAreaMap> {
        debug!("Mapping serial property area: {:?}, access_rw={}", serial_filename, access_rw);

        let result = if access_rw {
            trace!("Creating read-write serial property area with SELinux context");
            PropertyAreaMap::new_rw(serial_filename, Some(PROPERTIES_SERIAL_CONTEXT), fsetxattr_failed)
        } else {
            trace!("Creating read-only serial property area");
            PropertyAreaMap::new_ro(serial_filename)
        };

        match &result {
            Ok(_) => debug!("Successfully mapped serial property area: {:?}", serial_filename),
            Err(e) => error!("Failed to map serial property area {:?}: {}", serial_filename, e),
        }

        result
    }

    pub(crate) fn prop_area_for_name(&self, name: &str) -> Result<(PropertyAreaGuard<'_>, u32)> {
        trace!("Looking up property area for name: {}", name);

        let (index, _) = self.property_info_area_file
            .property_info_area()
            .get_property_info_indexes(name);

        debug!("Property {} mapped to context index: {}", name, index);

        if index == u32::MAX || index >= self.context_nodes.len() as u32 {
            warn!("Property {} not found: index={}, max_contexts={}", name, index, self.context_nodes.len());
            return Err(Error::new_not_found(name.to_owned()).into());
        }

        let context_node = &self.context_nodes[index as usize];
        trace!("Getting property area for context index {}", index);

        match context_node.property_area() {
            Ok(area) => {
                trace!("Successfully retrieved property area for {}", name);
                Ok((area, index))
            }
            Err(e) => {
                error!("Failed to get property area for {}: {}", name, e);
                Err(e)
            }
        }
    }

    #[cfg(feature = "builder")]
    pub(crate) fn prop_area_mut_for_name(&self, name: &str) -> Result<(PropertyAreaMutGuard<'_>, u32)> {
        trace!("Looking up mutable property area for name: {}", name);

        let (index, _) = self.property_info_area_file
            .property_info_area()
            .get_property_info_indexes(name);

        debug!("Property {} mapped to context index: {}", name, index);

        if index == u32::MAX || index >= self.context_nodes.len() as u32 {
            error!("Could not find context for property {}: index={}, max_contexts={}", name, index, self.context_nodes.len());
            return Err(Error::new_not_found(format!("Could not find context for property {name}")).into());
        }

        let context_node = &self.context_nodes[index as usize];
        trace!("Getting mutable property area for context index {}", index);

        match context_node.property_area_mut() {
            Ok(area) => {
                trace!("Successfully retrieved mutable property area for {}", name);
                Ok((area, index))
            }
            Err(e) => {
                error!("Failed to get mutable property area for {}: {}", name, e);
                Err(e)
            }
        }
    }

    // pub(crate) fn get_serial_prop_name(&self) -> Result<&PropertyAreaMap> {
    //     unimplemented!("get_serial_prop_name")
    // }

    pub(crate) fn serial_prop_area(&self) -> &PropertyArea {
        trace!("Accessing serial property area");
        self.serial_property_area_map.property_area()
    }

    pub(crate) fn prop_area_with_index(&self, context_index: u32) -> Result<PropertyAreaGuard<'_>> {
        trace!("Getting property area for context index: {}", context_index);

        if context_index >= self.context_nodes.len() as u32 {
            error!("Invalid context index {}: max={}", context_index, self.context_nodes.len());
            return Err(Error::new_parse(format!("Invalid context index: {}", context_index)).into());
        }

        let context_node = &self.context_nodes[context_index as usize];
        match context_node.property_area() {
            Ok(area) => {
                trace!("Successfully retrieved property area for context index {}", context_index);
                Ok(area)
            }
            Err(e) => {
                error!("Failed to get property area for context index {}: {}", context_index, e);
                Err(e)
            }
        }
    }

    #[cfg(feature = "builder")]
    pub(crate) fn prop_area_mut_with_index(&self, context_index: u32) -> Result<PropertyAreaMutGuard<'_>> {
        trace!("Getting mutable property area for context index: {}", context_index);

        if context_index >= self.context_nodes.len() as u32 {
            error!("Invalid context index {}: max={}", context_index, self.context_nodes.len());
            return Err(Error::new_parse(format!("Invalid context index: {}", context_index)).into());
        }

        let context_node = &self.context_nodes[context_index as usize];
        match context_node.property_area_mut() {
            Ok(area) => {
                trace!("Successfully retrieved mutable property area for context index {}", context_index);
                Ok(area)
            }
            Err(e) => {
                error!("Failed to get mutable property area for context index {}: {}", context_index, e);
                Err(e)
            }
        }
    }
}
