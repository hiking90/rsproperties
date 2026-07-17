// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::ffi::CStr;
use std::path::Path;

use crate::errors::*;
use log::{debug, error, info, warn};
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
    // `cstr()` reports out-of-range offsets and missing NUL terminators as
    // errors; an *empty* name is still rejected here — it would produce a
    // plausible-looking node whose filename is the directory itself,
    // failing `open()` far from the actual corruption.
    let context_cstr = area.cstr(context_offset)?;
    if context_cstr.is_empty() {
        return Err(Error::FileValidation(format!(
            "context entry {i}: empty context name at offset {context_offset}"
        )));
    }
    let context_name = context_cstr.to_str()?;
    // The context name comes from file content and becomes a filename via
    // `dirname.join()` — which *replaces* `dirname` entirely when handed an
    // absolute path, and descends on `/` or `..`. On the writable path the
    // resulting file is later `remove_file`d by `PropertyAreaMap::new_rw`,
    // so a corrupt or malicious property_info must not be able to point a
    // node outside the properties directory. Legitimate SELinux context
    // names (`u:object_r:...:s0`) never contain a path separator, so
    // requiring a single normal component loses nothing.
    {
        use std::path::Component;
        let mut components = Path::new(context_name).components();
        // The explicit `contains('/')` check closes what `components()`
        // normalizes away: `"a/"`, `"a//"`, and `"a/."` all yield a single
        // `Normal("a")` component yet are not plain filenames.
        if context_name.contains('/')
            || !matches!(
                (components.next(), components.next()),
                (Some(Component::Normal(_)), None)
            )
        {
            return Err(Error::FileValidation(format!(
                "context entry {i}: context name {context_name:?} is not a plain filename"
            )));
        }
    }
    // The owned context is only consumed by `open()` (writable path) for
    // SELinux labeling; read-only nodes skip the allocation.
    let context = writable.then(|| context_cstr.to_owned());
    Ok(ContextNode::new(
        writable,
        context,
        dirname.join(context_name),
    ))
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
    /// Exclusive `flock` held for the lifetime of a writable instance
    /// (`None` when read-only). `PropertyAreaMap::new_rw` unlinks and
    /// recreates stale area files, so without this lock a second writer
    /// would silently destroy the files the first one still owns; with it,
    /// the loser fails fast before touching anything. The kernel drops the
    /// lock when the `File` closes — including on crash.
    _writer_lock: Option<std::fs::File>,
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

        let (writer_lock, serial_property_area_map) = if writable {
            if !dirname.is_dir() {
                info!("Creating directory: {dirname:?}");
                match fs::mkdir(
                    dirname.as_path(),
                    fs::Mode::RWXU | fs::Mode::XGRP | fs::Mode::XOTH,
                ) {
                    Ok(()) => {}
                    // Lost a create race with a concurrent writer — the
                    // flock below is the real single-writer arbiter, so an
                    // already-existing *directory* is not an error here.
                    // Re-check with is_dir(): EEXIST for a plain file at
                    // this path is a real configuration error and deferring
                    // it would surface later as a confusing ENOTDIR.
                    Err(rustix::io::Errno::EXIST) if dirname.is_dir() => {}
                    Err(e) => return Err(Error::from(e)),
                }
            }

            // Must precede the `open()` calls below: they unlink and
            // recreate area files, so a losing second writer has to bail
            // out *before* touching anything the winner owns.
            let lock = Self::acquire_writer_lock(dirname.as_path())?;

            *fsetxattr_failed = false;

            for node in context_nodes.iter_mut().flatten() {
                node.open(fsetxattr_failed)?;
            }

            (
                Some(lock),
                Self::map_serial_property_area(serial_filename.as_path(), true, fsetxattr_failed)?,
            )
        } else {
            (
                None,
                Self::map_serial_property_area(serial_filename.as_path(), false, fsetxattr_failed)?,
            )
        };

        Ok(Self {
            property_info_area_file,
            context_nodes,
            serial_property_area_map,
            _writer_lock: writer_lock,
        })
    }

    /// Opens (creating if needed) `<dirname>/.writer_lock` and takes a
    /// non-blocking exclusive `flock`. The lock lives exactly as long as
    /// the returned `File`, so holding it in the struct scopes single-writer
    /// ownership of the directory to the instance's lifetime.
    fn acquire_writer_lock(dirname: &Path) -> Result<std::fs::File> {
        let lock_path = dirname.join(".writer_lock");
        let lock_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .context_with_location(format!("Failed to open writer lock {lock_path:?}"))?;
        fs::flock(&lock_file, fs::FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            error!("Another writer holds the property area lock {lock_path:?}: {e}");
            Error::Lock(format!(
                "Writable property area already owned by another instance ({lock_path:?}): {e}"
            ))
        })?;
        Ok(lock_file)
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

        result
            .inspect_err(|e| error!("Failed to map serial property area {serial_filename:?}: {e}"))
    }

    /// Shared slot lookup for the accessors below: one bounds check via
    /// `.get()` (which also covers the parser's `u32::MAX` "no context"
    /// sentinel, since `len <= u32::MAX`) plus the `None`-slot
    /// (skipped-at-init) case.
    ///
    /// `miss_is_expected` picks the log level of the out-of-range case:
    /// a name lookup missing is the normal fallback flow on the get hot
    /// path (`debug!`), while an out-of-range *explicit* `context_index` —
    /// which this crate itself produced earlier — signals internal
    /// inconsistency or corruption (`warn!`). A `None` slot is always an
    /// `error!`: the entry existed in property_info but was corrupt at
    /// init.
    fn context_node_at(
        &self,
        index: u32,
        what: &dyn std::fmt::Display,
        miss_is_expected: bool,
    ) -> Result<&ContextNode> {
        match self.context_nodes.get(index as usize) {
            Some(Some(node)) => Ok(node),
            Some(None) => {
                error!("Context entry {index} for {what} was skipped during init");
                Err(Error::NotFound(format!(
                    "context entry {index} for {what} unavailable (corrupt at init)"
                )))
            }
            None => {
                if miss_is_expected {
                    debug!(
                        "No context for {what}: index={index}, max_contexts={}",
                        self.context_nodes.len()
                    );
                } else {
                    warn!(
                        "Context index out of range for {what}: index={index}, max_contexts={}",
                        self.context_nodes.len()
                    );
                }
                Err(Error::NotFound(format!("no context for {what}")))
            }
        }
    }

    pub(crate) fn prop_area_for_name(&self, name: &str) -> Result<(PropertyAreaGuard<'_>, u32)> {
        let (index, _) = self
            .property_info_area_file
            .property_info_area()
            .get_property_info_indexes(name);
        let node = self.context_node_at(index, &format_args!("property {name}"), true)?;
        let area = node
            .property_area()
            .inspect_err(|e| error!("Failed to get property area for {name}: {e}"))?;
        Ok((area, index))
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
        let node = self.context_node_at(index, &format_args!("property {name}"), true)?;
        let area = node
            .property_area_mut()
            .inspect_err(|e| error!("Failed to get mutable property area for {name}: {e}"))?;
        Ok((area, index))
    }

    pub(crate) fn serial_prop_area(&self) -> &PropertyArea {
        self.serial_property_area_map.property_area()
    }

    pub(crate) fn prop_area_with_index(&self, context_index: u32) -> Result<PropertyAreaGuard<'_>> {
        self.context_node_at(
            context_index,
            &format_args!("context index {context_index}"),
            false,
        )?
        .property_area()
        .inspect_err(|e| {
            error!("Failed to get property area for context index {context_index}: {e}")
        })
    }

    #[cfg(feature = "builder")]
    pub(crate) fn prop_area_mut_with_index(
        &self,
        context_index: u32,
    ) -> Result<PropertyAreaMutGuard<'_>> {
        self.context_node_at(
            context_index,
            &format_args!("context index {context_index}"),
            false,
        )?
        .property_area_mut()
        .inspect_err(|e| {
            error!("Failed to get mutable property area for context index {context_index}: {e}")
        })
    }
}
