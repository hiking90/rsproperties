// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::ffi::CStr;
use std::path::Path;

use crate::errors::*;
use log::{error, info, warn};
use rustix::fs;

#[cfg(feature = "builder")]
use crate::context_node::PropertyAreaMutGuard;
use crate::context_node::{ContextNode, PropertyAreaGuard};
use crate::property_area::{PropertyArea, PropertyAreaMap};
use crate::property_info_parser::{PropertyInfoArea, PropertyInfoAreaFile};

/// Decodes one `ContextNode` entry from the property-info area. Returns
/// `Err` on corrupt offset, missing NUL terminator, or non-UTF-8 name —
/// callers tag the slot as `None` so the surrounding `Vec<Option<_>>`
/// indices stay aligned with the parser's `context_index` values.
fn try_build_context_node(
    area: &PropertyInfoArea<'_>,
    dirname: &Path,
    writable: bool,
    i: usize,
) -> Result<ContextNode> {
    let context_offset = area.context_offset(i)?;
    let context_name = area.cstr(context_offset).to_str()?;
    Ok(ContextNode::new(writable, dirname.join(context_name)))
}

// Pre-defined CStr constants to avoid unsafe code at runtime
// Using const_str macro or safer compile-time construction
const PROPERTIES_SERIAL_CONTEXT: &CStr = c"u:object_r:properties_serial:s0";

pub(crate) struct ContextsSerialized {
    property_info_area_file: PropertyInfoAreaFile,
    /// `None` slots are corrupt context entries that were skipped during init.
    /// We keep the slot so that `context_index` values produced by the
    /// property-info parser line up with the vector's indices.
    context_nodes: Vec<Option<ContextNode>>,
    serial_property_area_map: PropertyAreaMap,
}

impl ContextsSerialized {
    pub(crate) fn new(
        writable: bool,
        dirname: &Path,
        fsetxattr_failed: &mut bool,
        load_default_path: bool,
    ) -> Result<Self> {
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
        let mut context_nodes: Vec<Option<ContextNode>> = Vec::with_capacity(num_context_nodes);

        for i in 0..num_context_nodes {
            match try_build_context_node(&property_info_area, dirname.as_path(), writable, i) {
                Ok(n) => context_nodes.push(Some(n)),
                Err(e) => {
                    warn!("context entry {i} skipped: {e}");
                    context_nodes.push(None);
                }
            }
        }

        let serial_property_area_map = if writable {
            if !dirname.is_dir() {
                info!("Creating directory: {dirname:?}");
                fs::mkdir(
                    dirname.as_path(),
                    fs::Mode::RWXU | fs::Mode::XGRP | fs::Mode::XOTH,
                )
                .map_err(Error::from)?;
            }

            *fsetxattr_failed = false;

            for node in context_nodes.iter_mut().flatten() {
                node.open(fsetxattr_failed)?;
            }

            Self::map_serial_property_area(serial_filename.as_path(), true, fsetxattr_failed)?
        } else {
            Self::map_serial_property_area(serial_filename.as_path(), false, fsetxattr_failed)?
        };

        Ok(Self {
            property_info_area_file,
            context_nodes,
            serial_property_area_map,
        })
    }

    fn map_serial_property_area(
        serial_filename: &Path,
        access_rw: bool,
        fsetxattr_failed: &mut bool,
    ) -> Result<PropertyAreaMap> {
        let result = if access_rw {
            PropertyAreaMap::new_rw(
                serial_filename,
                Some(PROPERTIES_SERIAL_CONTEXT),
                fsetxattr_failed,
            )
        } else {
            PropertyAreaMap::new_ro(serial_filename)
        };

        match &result {
            Ok(_) => {}
            Err(e) => error!("Failed to map serial property area {serial_filename:?}: {e}"),
        }

        result
    }

    pub(crate) fn prop_area_for_name(&self, name: &str) -> Result<(PropertyAreaGuard<'_>, u32)> {
        let (index, _) = self
            .property_info_area_file
            .property_info_area()
            .get_property_info_indexes(name);

        if index == u32::MAX || index >= self.context_nodes.len() as u32 {
            warn!(
                "Property {} not found: index={}, max_contexts={}",
                name,
                index,
                self.context_nodes.len()
            );
            return Err(Error::NotFound(name.to_owned()));
        }

        let context_node = self.context_nodes[index as usize].as_ref().ok_or_else(|| {
            error!("Context entry {index} for property {name} was skipped during init");
            Error::NotFound(name.to_owned())
        })?;

        match context_node.property_area() {
            Ok(area) => Ok((area, index)),
            Err(e) => {
                error!("Failed to get property area for {name}: {e}");
                Err(e)
            }
        }
    }

    #[cfg(feature = "builder")]
    pub(crate) fn prop_area_mut_for_name(
        &self,
        name: &str,
    ) -> Result<(PropertyAreaMutGuard<'_>, u32)> {
        let (index, _) = self
            .property_info_area_file
            .property_info_area()
            .get_property_info_indexes(name);

        if index == u32::MAX || index >= self.context_nodes.len() as u32 {
            error!(
                "Could not find context for property {}: index={}, max_contexts={}",
                name,
                index,
                self.context_nodes.len()
            );
            return Err(Error::NotFound(format!(
                "Could not find context for property {name}"
            )));
        }

        let context_node = self.context_nodes[index as usize].as_ref().ok_or_else(|| {
            error!("Context entry {index} for property {name} was skipped during init");
            Error::NotFound(format!("Could not find context for property {name}"))
        })?;

        match context_node.property_area_mut() {
            Ok(area) => Ok((area, index)),
            Err(e) => {
                error!("Failed to get mutable property area for {name}: {e}");
                Err(e)
            }
        }
    }

    pub(crate) fn serial_prop_area(&self) -> &PropertyArea {
        self.serial_property_area_map.property_area()
    }

    pub(crate) fn prop_area_with_index(&self, context_index: u32) -> Result<PropertyAreaGuard<'_>> {
        if context_index >= self.context_nodes.len() as u32 {
            error!(
                "Invalid context index {}: max={}",
                context_index,
                self.context_nodes.len()
            );
            return Err(Error::Parse(format!(
                "Invalid context index: {context_index}"
            )));
        }

        let context_node = self.context_nodes[context_index as usize]
            .as_ref()
            .ok_or_else(|| {
                error!("Context entry {context_index} was skipped during init");
                Error::NotFound(format!(
                    "Context entry {context_index} is unavailable (corrupt at init)"
                ))
            })?;
        match context_node.property_area() {
            Ok(area) => Ok(area),
            Err(e) => {
                error!("Failed to get property area for context index {context_index}: {e}");
                Err(e)
            }
        }
    }

    #[cfg(feature = "builder")]
    pub(crate) fn prop_area_mut_with_index(
        &self,
        context_index: u32,
    ) -> Result<PropertyAreaMutGuard<'_>> {
        if context_index >= self.context_nodes.len() as u32 {
            error!(
                "Invalid context index {}: max={}",
                context_index,
                self.context_nodes.len()
            );
            return Err(Error::Parse(format!(
                "Invalid context index: {context_index}"
            )));
        }

        let context_node = self.context_nodes[context_index as usize]
            .as_ref()
            .ok_or_else(|| {
                error!("Context entry {context_index} was skipped during init");
                Error::NotFound(format!(
                    "Context entry {context_index} is unavailable (corrupt at init)"
                ))
            })?;
        match context_node.property_area_mut() {
            Ok(area) => Ok(area),
            Err(e) => {
                error!(
                    "Failed to get mutable property area for context index {context_index}: {e}"
                );
                Err(e)
            }
        }
    }
}
