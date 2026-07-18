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

/// Filenames the property directory reserves for its own bookkeeping. A
/// context named after one of these would make `ContextNode::open()` (which
/// unlinks and recreates its file via `PropertyAreaMap::new_rw`) destroy
/// that bookkeeping: `.writer_lock` → the flock the writer just acquired
/// keeps guarding an orphan inode, so a second writer can "win" the lock
/// and both writers unlink each other's areas; `properties_serial` → the
/// serial mapping created right after the node loop re-unlinks the node's
/// file, leaving the node on an orphan inode invisible to readers;
/// `property_info` → the trie file itself is destroyed.
///
/// Compared (like the duplicate check) on the ASCII-case-folded name: on a
/// case-insensitive filesystem (macOS APFS default, where this crate's
/// tests run writable) `"PROPERTY_INFO"` would otherwise pass the exact
/// match yet unlink the real `property_info`.
const RESERVED_FILENAMES: &[&str] = &[".writer_lock", "properties_serial", "property_info"];

/// Decodes one `ContextNode` entry from the property-info area. Returns
/// `Err` on corrupt offset, missing NUL terminator, or non-UTF-8 name —
/// callers tag the slot as `None` so the surrounding `Vec<Option<_>>`
/// indices stay aligned with the parser's `context_index` values.
fn try_build_context_node(
    area: &PropertyInfoArea<'_>,
    dirname: &Path,
    writable: bool,
    i: usize,
    seen_names: &mut std::collections::HashSet<String>,
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
    // Legitimate SELinux context names are pure ASCII. Rejecting anything
    // else closes what the ASCII case-fold below cannot: macOS APFS also
    // aliases Unicode case variants and NFC/NFD normalization forms onto
    // one file, so two "different" non-ASCII names could pass the
    // duplicate check yet unlink each other's area files — exactly the
    // destructive collision these checks exist to prevent.
    if !context_name.is_ascii() {
        return Err(Error::FileValidation(format!(
            "context entry {i}: context name {context_name:?} contains non-ASCII characters"
        )));
    }
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
    // Same-directory collisions are as destructive as directory escape —
    // see `RESERVED_FILENAMES`. Both checks compare the ASCII-case-folded
    // name so a case-insensitive filesystem cannot be used to alias two
    // "different" names onto one file (the fold is also why the seen-set
    // key is an owned `String` rather than a borrow of the mmap'd name).
    let folded_name = context_name.to_ascii_lowercase();
    if RESERVED_FILENAMES.contains(&folded_name.as_str()) {
        return Err(Error::FileValidation(format!(
            "context entry {i}: context name {context_name:?} collides with a reserved filename"
        )));
    }
    // Two entries with the same name would share one file: the later
    // node's `open()` unlinks the earlier node's area, silently losing its
    // properties within the same instance. First entry wins; later
    // duplicates are skipped like any other corrupt entry.
    if !seen_names.insert(folded_name) {
        return Err(Error::FileValidation(format!(
            "context entry {i}: duplicate context name {context_name:?}"
        )));
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
    pub(crate) fn new(writable: bool, dirname: &Path, load_default_path: bool) -> Result<Self> {
        let tree_filename = dirname.join("property_info");
        let serial_filename = dirname.join("properties_serial");

        let property_info_area_file = if load_default_path {
            PropertyInfoAreaFile::load_default_path()
        } else {
            PropertyInfoAreaFile::load_path(tree_filename.as_path())
        }?;

        let property_info_area = property_info_area_file.property_info_area();
        let num_context_nodes = property_info_area.num_contexts();
        // The count is untrusted file data; it sizes the allocation below
        // AND bounds the loop. Two gates before allocating:
        // - the table-bounds check rejects a count whose declared table
        //   doesn't fit in the file (e.g. u32::MAX), so a small corrupt
        //   file can't drive a giant `Vec` allocation;
        // - the absolute cap bounds the ~25x amplification (4 table bytes
        //   → one `Option<ContextNode>` slot) still reachable from a
        //   *genuinely large* file that really contains its table. Real
        //   Android property_info files declare a few thousand contexts;
        //   the cap is far above any legitimate build.
        const MAX_CONTEXTS: usize = 65_536;
        if num_context_nodes > MAX_CONTEXTS {
            return Err(Error::FileValidation(format!(
                "context table declares {num_context_nodes} entries (max {MAX_CONTEXTS})"
            )));
        }
        if num_context_nodes > 0 {
            property_info_area
                .context_offset(num_context_nodes - 1)
                .map_err(|e| {
                    Error::FileValidation(format!(
                        "context table ({num_context_nodes} entries) exceeds property_info bounds: {e}"
                    ))
                })?;
        }
        let mut context_nodes: Vec<Option<ContextNode>> = Vec::with_capacity(num_context_nodes);

        let mut seen_names = std::collections::HashSet::new();
        for i in 0..num_context_nodes {
            match try_build_context_node(&property_info_area, dirname, writable, i, &mut seen_names)
            {
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
                match fs::mkdir(dirname, fs::Mode::RWXU | fs::Mode::XGRP | fs::Mode::XOTH) {
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
            let lock = Self::acquire_writer_lock(dirname)?;

            // `open()` takes `&self` (interior mutability via its RwLock) —
            // a `&mut` walk here would misread as structural mutation.
            for node in context_nodes.iter().flatten() {
                node.open()?;
            }

            (
                Some(lock),
                Self::map_serial_property_area(serial_filename.as_path(), true)?,
            )
        } else {
            (
                None,
                Self::map_serial_property_area(serial_filename.as_path(), false)?,
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
        use std::os::unix::fs::OpenOptionsExt;
        let lock_path = dirname.join(".writer_lock");
        // O_NOFOLLOW + explicit mode, like the area files opened by
        // `PropertyAreaMap::new_rw`: this file is the single-writer
        // arbiter, so a symlink planted at `.writer_lock` must not be able
        // to redirect the `flock` to a different inode (two writers each
        // locking a different file would both "win"). Mode 0600, not 0644:
        // `flock(LOCK_EX)` succeeds on a read-only fd, so any user who can
        // open the file could otherwise squat the exclusive lock and block
        // the legitimate writer forever — nobody but the owner ever needs
        // to open this file.
        let lock_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(fs::OFlags::NOFOLLOW.bits() as _)
            .mode(0o600)
            .open(&lock_path)
            .context_with_location(format!("Failed to open writer lock {lock_path:?}"))?;
        // `.mode(0o600)` above only applies when the file is *created*; a
        // leftover lock file with wider permissions (e.g. 0644 from an
        // older version) would keep them and defeat the anti-squat
        // rationale. Re-assert the mode on the open fd before taking the
        // lock. (An attacker who already holds an open fd is not stopped —
        // this closes the window for every open that comes after.)
        fs::fchmod(&lock_file, fs::Mode::RUSR | fs::Mode::WUSR)
            .context_with_location(format!("Failed to restrict mode of {lock_path:?}"))?;
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
    ) -> Result<PropertyAreaMap> {
        let result = if access_rw {
            PropertyAreaMap::new_rw(serial_filename, Some(PROPERTIES_SERIAL_CONTEXT))
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
    /// inconsistency or corruption (`warn!`). A `None` slot (entry existed
    /// in property_info but was corrupt at init) logs at `debug!` here:
    /// the corruption was already reported once with `warn!` during init,
    /// and this accessor sits on the get hot path.
    fn context_node_at(
        &self,
        index: u32,
        what: &dyn std::fmt::Display,
        miss_is_expected: bool,
    ) -> Result<&ContextNode> {
        match self.context_nodes.get(index as usize) {
            Some(Some(node)) => Ok(node),
            Some(None) => {
                // `debug!`, not `error!`: the corruption was already
                // reported once (warn) at init; this arm sits on the get
                // hot path and would otherwise log on every lookup of an
                // affected property. The returned error still carries the
                // full context.
                debug!("Context entry {index} for {what} was skipped during init");
                // `FileValidation`, NOT `NotFound`: callers fold `NotFound`
                // into "property absent" (`SystemProperties::find` returns
                // `Ok(None)`), and a context that *exists* in property_info
                // but was corrupt at init must not masquerade as absence —
                // it has to propagate as an error.
                Err(Error::FileValidation(format!(
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
