// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{
    ffi::CStr,
    fmt::Debug,
    fs::{File, OpenOptions},
    mem,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::atomic::AtomicU32,
};

use crate::errors::*;
use log::{debug, error, info, warn};
use rustix::{fs, mm};

#[cfg(feature = "builder")]
use crate::property_info::init_name_with_trailing_data;
use crate::property_info::{name_from_trailing_data, PropertyInfo};

const PA_SIZE: u64 = 128 * 1024;
const PROP_AREA_MAGIC: u32 = 0x504f5250;
const PROP_AREA_VERSION: u32 = 0xfc6ed0ab;

#[repr(C, align(4))]
pub(crate) struct PropertyTrieNode {
    pub(crate) namelen: u32,
    pub(crate) prop: AtomicU32,
    pub(crate) left: AtomicU32,
    pub(crate) right: AtomicU32,
    pub(crate) children: AtomicU32,
}

impl PropertyTrieNode {
    #[cfg(feature = "builder")]
    fn init(&mut self, name: &str) {
        self.prop.store(0, std::sync::atomic::Ordering::Relaxed);
        self.left.store(0, std::sync::atomic::Ordering::Relaxed);
        self.right.store(0, std::sync::atomic::Ordering::Relaxed);
        self.children.store(0, std::sync::atomic::Ordering::Relaxed);

        self.namelen = name.len() as _;
        init_name_with_trailing_data(self, name);
    }

    pub(crate) fn name(&self) -> crate::errors::Result<&CStr> {
        name_from_trailing_data(self, Some(self.namelen as _))
    }
}

impl Debug for PropertyTrieNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PropertyTrieNode")
            .field("namelen", &self.namelen)
            .field("prop", &self.prop)
            .field("left", &self.left)
            .field("right", &self.right)
            .field("children", &self.children)
            .field(
                "name",
                &self
                    .name()
                    .map(|n| n.to_str().unwrap_or("<invalid>"))
                    .unwrap_or("<error>"),
            )
            .finish()
    }
}

fn cmp_prop_name(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[derive(Debug)]
#[repr(C, align(4))]
pub(crate) struct PropertyArea {
    bytes_used: u32,
    serial: AtomicU32,
    magic: u32,
    version: u32,
    reserved: [u32; 28],
}

impl PropertyArea {
    fn init(&mut self, magic: u32, version: u32) {
        self.serial.store(0, std::sync::atomic::Ordering::Relaxed);
        self.magic = magic;
        self.version = version;
        self.reserved = [0; 28];
        self.bytes_used = mem::size_of::<PropertyTrieNode>() as _;
        self.bytes_used += crate::bionic_align(crate::PROP_VALUE_MAX, mem::size_of::<u32>()) as u32;
    }

    pub(crate) fn serial(&self) -> &AtomicU32 {
        &self.serial
    }
}

#[derive(Debug)]
pub(crate) struct PropertyAreaMap {
    mmap: MemoryMap,
    data_offset: usize,
    #[allow(dead_code)]
    pa_data_size: usize,
}

impl PropertyAreaMap {
    // Initialize the property area map with the given file to create a new property area map.
    pub(crate) fn new_rw(
        filename: &Path,
        context: Option<&CStr>,
        fsetxattr_failed: &mut bool,
    ) -> Result<Self> {
        debug!("Creating new read-write property area map: {filename:?}");

        // A leftover area file from a previous writer instance would make
        // the O_EXCL create below fail — and the 0444 mode means it could
        // not be reopened read-write either. AOSP avoids this via the fresh
        // tmpfs mounted at /dev on every boot; that assumption doesn't hold
        // for an arbitrary properties dir, so treat `new_rw` as "build a
        // fresh area" and remove any stale file first. O_EXCL still guards
        // the create itself (no symlink / pre-created-file substitution
        // between the unlink and the open).
        match std::fs::remove_file(filename) {
            Ok(()) => debug!("Removed stale property area file: {filename:?}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to remove stale property area file {filename:?}: {e}"),
        }

        let file = OpenOptions::new()
            .read(true) // O_RDWR
            .write(true) // O_RDWR
            .create(true) // O_CREAT
            .custom_flags((fs::OFlags::NOFOLLOW.bits() | fs::OFlags::EXCL.bits()) as _) // additional flags
            .mode(0o444) // permission: 0444
            .open(filename)?;

        if let Some(context) = context {
            // Full xattr name required — the bare "selinux" (no namespace
            // prefix) is rejected by the kernel with EOPNOTSUPP, which made
            // this call fail unconditionally. bionic uses XATTR_NAME_SELINUX,
            // which is "security.selinux".
            if fs::fsetxattr(
                &file,
                "security.selinux",
                context.to_bytes_with_nul(),
                fs::XattrFlags::empty(),
            )
            .is_err()
            {
                warn!("Failed to set SELinux context for {filename:?}");
                *fsetxattr_failed = true;
            }
        }

        fs::ftruncate(&file, PA_SIZE).map_err(Error::from)?;

        let pa_size = PA_SIZE as usize;
        let pa_data_size = pa_size - std::mem::size_of::<PropertyArea>();

        let mut thiz = Self {
            mmap: MemoryMap::new(file, pa_size, true)?,
            data_offset: std::mem::size_of::<PropertyArea>(),
            pa_data_size,
        };

        thiz.property_area_mut()
            .init(PROP_AREA_MAGIC, PROP_AREA_VERSION);

        info!("Successfully created read-write property area map: {filename:?}");
        Ok(thiz)
    }

    // Initialize the property area map with the given file to read-only property area map.
    pub(crate) fn new_ro(filename: &Path) -> Result<Self> {
        debug!("Opening read-only property area map: {filename:?}");

        let file = OpenOptions::new()
            .read(true) // read only
            .custom_flags(fs::OFlags::NOFOLLOW.bits() as _) // additional flags
            .open(filename)
            .context_with_location(format!("Failed to open {filename:?}"))?;

        let metadata = file
            .metadata()
            .context_with_location("Failed to get metadata")?;

        // Validate file metadata using common utility function
        crate::errors::validate_file_metadata(
            &metadata,
            filename,
            mem::size_of::<PropertyArea>() as u64,
        )?;

        let pa_size = metadata.len() as usize;
        let pa_data_size = pa_size - std::mem::size_of::<PropertyArea>();

        let thiz = Self {
            mmap: MemoryMap::new(file, pa_size, false)?,
            data_offset: std::mem::size_of::<PropertyArea>(),
            pa_data_size,
        };

        let pa = thiz.property_area();

        if pa.magic != PROP_AREA_MAGIC || pa.version != PROP_AREA_VERSION {
            error!(
                "Invalid magic ({:#x} != {:#x}) or version ({:#x} != {:#x}) for {:?}",
                pa.magic, PROP_AREA_MAGIC, pa.version, PROP_AREA_VERSION, filename
            );
            Err(Error::FileValidation(
                "Invalid magic or version".to_string(),
            ))
        } else {
            info!("Successfully opened read-only property area map: {filename:?}");
            Ok(thiz)
        }
    }

    pub(crate) fn property_area(&self) -> &PropertyArea {
        self.mmap
            .to_object::<PropertyArea>(0, 0)
            .expect("PropertyArea's offset is zero. So, it must be valid.")
    }

    fn property_area_mut(&mut self) -> &mut PropertyArea {
        self.mmap
            .to_object_mut::<PropertyArea>(0, 0)
            .expect("PropertyArea's offset is zero. So, it must be valid.")
    }

    // Find the property information with the given name.
    pub(crate) fn find(&self, name: &str) -> Result<(&PropertyInfo, u32)> {
        let mut remaining_name = name;
        let mut current = self
            .mmap
            .to_object::<PropertyTrieNode>(0, self.data_offset)?;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                error!("Invalid property name (empty segment): '{name}'");
                return Err(Error::Parse(format!("Invalid property name: {name}")));
            }

            let subname = &remaining_name[0..substr_size];

            let children_offset = current.children.load(std::sync::atomic::Ordering::Acquire);
            if children_offset == 0 {
                return Err(Error::NotFound(name.to_owned()));
            }
            let root = self
                .mmap
                .to_object::<PropertyTrieNode>(children_offset as usize, self.data_offset)?;

            current = self.find_prop_trie_node(root, subname)?;

            if sep.is_none() {
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
        }

        let prop_offset = current.prop.load(std::sync::atomic::Ordering::Acquire);
        if prop_offset != 0 {
            Ok((
                self.mmap
                    .to_object(prop_offset as usize, self.data_offset)?,
                prop_offset,
            ))
        } else {
            Err(Error::NotFound(name.to_owned()))
        }
    }

    // Add the property information with the given name and value.
    #[cfg(feature = "builder")]
    pub(crate) fn add(&mut self, name: &str, value: &str) -> Result<()> {
        debug!("Adding property: '{name}' = '{value}'");

        let mut remaining_name = name;
        let mut current = 0;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                error!("Invalid property name (empty segment): '{name}'");
                return Err(Error::Parse(format!("Invalid property name: {name}")));
            }

            let subname = &remaining_name[0..substr_size];

            let children_offset = self
                .mmap
                .to_object::<PropertyTrieNode>(current, self.data_offset)?
                .children
                .load(std::sync::atomic::Ordering::Acquire);
            let root_offset = if children_offset != 0 {
                children_offset
            } else {
                let offset = self.new_prop_trie_node(subname)?;
                self.mmap
                    .to_object::<PropertyTrieNode>(current, self.data_offset)?
                    .children
                    .store(offset, std::sync::atomic::Ordering::Release);
                offset
            };

            current = self.add_prop_trie_node(root_offset, subname)? as _;

            if sep.is_none() {
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
        }

        let prop_offset = self
            .mmap
            .to_object::<PropertyTrieNode>(current, self.data_offset)?
            .prop
            .load(std::sync::atomic::Ordering::Acquire);

        if prop_offset == 0 {
            let offset = self.new_prop_info(name, value)?;
            let current_node = self
                .mmap
                .to_object_mut::<PropertyTrieNode>(current, self.data_offset)?;
            current_node
                .prop
                .store(offset, std::sync::atomic::Ordering::Release);
        }

        Ok(())
    }

    // Read the dirty backup area.
    pub(crate) fn dirty_backup_area(&self) -> Result<&CStr> {
        let result = self
            .mmap
            .to_cstr(mem::size_of::<PropertyTrieNode>(), self.data_offset);
        if result.is_err() {
            error!("Failed to read dirty backup area");
        }
        result
    }

    // Set the dirty backup area.
    // It is used to store the backup of the property area.
    //
    // Accepts raw bytes (not `&str`) so the caller can stream the current
    // property value directly from the byte-atomic mmap slot into the
    // backup area without first materialising a `String`. The reader side
    // already validates UTF-8 after the seqlock re-check, so the backup
    // area itself stores raw bytes verbatim.
    #[cfg(feature = "builder")]
    pub(crate) fn set_dirty_backup_area(&mut self, value: &[u8]) -> Result<()> {
        let offset = mem::size_of::<PropertyTrieNode>();
        // Checked arithmetic so a wrapping `usize` doesn't bypass the size
        // gate. Realistically `value.len()` is < PROP_VALUE_MAX (92), but
        // this function is `pub(crate)` and the rest of the module uses
        // `checked_*` throughout — keep the discipline.
        let total_len = value.len().checked_add(1).ok_or_else(|| {
            Error::FileValidation(format!("Backup value too long: {}", value.len()))
        })?;
        let end = total_len.checked_add(offset).ok_or_else(|| {
            Error::FileValidation(format!(
                "Backup area offset overflow: {total_len} + {offset}"
            ))
        })?;
        if end > self.pa_data_size {
            error!(
                "Backup area overflow: {total_len} + {offset} > {}",
                self.pa_data_size
            );
            return Err(Error::FileValidation("Invalid offset".to_string()));
        }

        let dst = self.mmap.data_mut(offset, self.data_offset, total_len)?;
        dst[..value.len()].copy_from_slice(value);
        dst[value.len()] = 0;
        Ok(())
    }

    // Add a new property trie node with the given name to the given trie node.
    // It uses trie offset to avoid the life time issue of the current trie node.
    #[cfg(feature = "builder")]
    fn add_prop_trie_node(&mut self, trie_offset: u32, name: &str) -> Result<u32> {
        let name_bytes = name.as_bytes();
        let mut current_offset = trie_offset;
        loop {
            let current_node = self
                .mmap
                .to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;

            let ordering = cmp_prop_name(name_bytes, current_node.name()?.to_bytes());
            let child_offset = match ordering {
                std::cmp::Ordering::Less => {
                    current_node.left.load(std::sync::atomic::Ordering::Acquire)
                }
                std::cmp::Ordering::Greater => current_node
                    .right
                    .load(std::sync::atomic::Ordering::Acquire),
                std::cmp::Ordering::Equal => break,
            };
            if child_offset != 0 {
                current_offset = child_offset;
                continue;
            }
            // Empty slot — allocate the new node, then re-borrow to store the
            // link (Release) before exiting the loop.
            let offset = self.new_prop_trie_node(name)?;
            let current_node = self
                .mmap
                .to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
            let link = match ordering {
                std::cmp::Ordering::Less => &current_node.left,
                std::cmp::Ordering::Greater => &current_node.right,
                std::cmp::Ordering::Equal => unreachable!(),
            };
            link.store(offset, std::sync::atomic::Ordering::Release);
            current_offset = offset;
            break;
        }
        Ok(current_offset)
    }

    fn find_prop_trie_node<'a>(
        &'a self,
        trie: &'a PropertyTrieNode,
        name: &str,
    ) -> Result<&'a PropertyTrieNode> {
        let name_bytes = name.as_bytes();
        let mut current = trie;
        loop {
            let next_offset = match cmp_prop_name(name_bytes, current.name()?.to_bytes()) {
                std::cmp::Ordering::Less => current.left.load(std::sync::atomic::Ordering::Acquire),
                std::cmp::Ordering::Greater => {
                    current.right.load(std::sync::atomic::Ordering::Acquire)
                }
                std::cmp::Ordering::Equal => break,
            };
            if next_offset == 0 {
                return Err(Error::NotFound(name.to_owned()));
            }
            current = self
                .mmap
                .to_object::<PropertyTrieNode>(next_offset as usize, self.data_offset)?;
        }
        Ok(current)
    }

    #[cfg(feature = "builder")]
    fn allocate_obj(&mut self, size: usize) -> Result<u32> {
        let aligned = crate::bionic_align(size, mem::size_of::<u32>());
        let offset = self.property_area().bytes_used;

        // Convert aligned to u32 with overflow check
        let aligned_u32 = u32::try_from(aligned).map_err(|_| {
            Error::FileSize(format!("Aligned size too large to fit in u32: {}", aligned))
        })?;

        // checked_add to prevent overflow
        let new_offset = offset.checked_add(aligned_u32).ok_or_else(|| {
            Error::FileSize(format!(
                "Offset overflow: {} + {} would exceed u32::MAX",
                offset, aligned_u32
            ))
        })?;

        // Bounds check
        if new_offset > self.pa_data_size as u32 {
            error!(
                "Out of memory: new_offset={} > pa_data_size={}",
                new_offset, self.pa_data_size
            );
            return Err(Error::FileSize(format!(
                "Out of memory: {} + {} = {} > {}",
                offset, aligned_u32, new_offset, self.pa_data_size
            )));
        }

        // Update bytes_used
        self.property_area_mut().bytes_used = new_offset;
        Ok(offset)
    }

    #[cfg(feature = "builder")]
    pub(crate) fn new_prop_trie_node(&mut self, name: &str) -> Result<u32> {
        let new_offset = self.allocate_obj(mem::size_of::<PropertyTrieNode>() + name.len() + 1)?;
        let node = self
            .mmap
            .to_object_mut::<PropertyTrieNode>(new_offset as usize, self.data_offset)?;
        node.init(name);
        Ok(new_offset)
    }

    #[cfg(feature = "builder")]
    pub(crate) fn new_prop_info(&mut self, name: &str, value: &str) -> Result<u32> {
        let new_offset = self.allocate_obj(mem::size_of::<PropertyInfo>() + name.len() + 1)?;

        if value.len() > crate::PROP_VALUE_MAX {
            let long_offset = self.allocate_obj(value.len() + 1)?;

            let target =
                self.mmap
                    .data_mut(long_offset as usize, self.data_offset, value.len() + 1)?;
            target[0..value.len()].copy_from_slice(value.as_bytes());
            target[value.len()] = 0; // Add null terminator

            let relative_offset = long_offset - new_offset;

            let info = self
                .mmap
                .to_object_mut::<PropertyInfo>(new_offset as usize, self.data_offset)?;
            info.init_with_long_offset(name, relative_offset as _);
        } else {
            let info = self
                .mmap
                .to_object_mut::<PropertyInfo>(new_offset as usize, self.data_offset)?;
            info.init_with_value(name, value);
        };

        Ok(new_offset)
    }

    pub(crate) fn property_info(&self, offset: u32) -> Result<&PropertyInfo> {
        self.mmap.to_object(offset as usize, self.data_offset)
    }

    /// Returns a `&mut PropertyInfo` for the entry at `offset`. Exposed only
    /// to the builder feature because the in-place update path is the only
    /// caller; together with `&mut PropertyAreaMap` it enforces single-writer
    /// inside one process via the borrow checker.
    #[cfg(feature = "builder")]
    pub(crate) fn property_info_mut(&mut self, offset: u32) -> Result<&mut PropertyInfo> {
        self.mmap.to_object_mut(offset as usize, self.data_offset)
    }

    /// Computes the maximum number of bytes that may be safely scanned past
    /// `pi` without leaving this mmap. Returned to callers that need to read
    /// long-property values whose length is not encoded in the header.
    pub(crate) fn max_value_bound(&self, pi: &PropertyInfo) -> usize {
        let pi_addr = pi as *const _ as usize;
        let mmap_start = self.mmap.data as usize;
        let mmap_end = mmap_start.saturating_add(self.mmap.size);
        // Require room for the header AND at least one trailing byte (the
        // NUL terminator slot). Without the `+ 1`, `pi_addr + header ==
        // mmap_end` would return `header` and let `PropertyInfo::name` scan
        // a single byte past the mapping.
        let header = std::mem::size_of::<PropertyInfo>();
        if pi_addr < mmap_start || pi_addr.saturating_add(header + 1) > mmap_end {
            return 0;
        }
        mmap_end - pi_addr
    }
}

// MemoryMap is a wrapper for the memory-mapped file.
// It provides the safe access to the memory-mapped file.
#[derive(Debug)]
pub(crate) struct MemoryMap {
    data: *mut u8,
    size: usize,
}

// SAFETY: The `data` pointer is owned by this MemoryMap and remains valid for
// `size` bytes until `Drop` calls `munmap`. The pointer itself is not mutated
// after construction. Higher-level invariants for the contents of the mapped
// region (atomic vs non-atomic writes) are the responsibility of the callers
// in this module — for shared writable mappings, the builder phase is expected
// to complete before any readers attach.
unsafe impl Send for MemoryMap {}

// SAFETY: See `Send` above. Concurrent readers via `&self` only touch atomic
// fields exposed through `to_object`. Mutations via `&mut self` are exclusive
// by Rust's borrow rules. For mmaps shared across processes, the same
// builder-phase precondition documented on `Send` applies.
unsafe impl Sync for MemoryMap {}

impl MemoryMap {
    pub(crate) fn new(file: File, size: usize, writable: bool) -> Result<Self> {
        debug!("Creating memory map: size={size}, writable={writable}");

        if size == 0 {
            return Err(Error::FileValidation(
                "Cannot mmap zero-sized region".into(),
            ));
        }

        let flags = if writable {
            mm::ProtFlags::READ.union(mm::ProtFlags::WRITE)
        } else {
            mm::ProtFlags::READ
        };

        // SAFETY: `file` is a valid owned `File`, `size > 0` is checked above,
        // and `mm::mmap` reports failure via `Result` rather than `MAP_FAILED`.
        let memory_area = unsafe {
            mm::mmap(
                std::ptr::null_mut(),
                size,
                flags,
                mm::MapFlags::SHARED,
                file,
                0,
            )
        }
        .map_err(Error::from)? as *mut u8;

        Ok(Self {
            data: memory_area,
            size,
        })
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    pub(crate) fn data(&self, offset: usize, base: usize, size: usize) -> Result<&[u8]> {
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, size)?;
        // SAFETY: `offset + size <= self.size`, so the resulting slice lies
        // entirely within the mmap region. `u8` has no alignment requirement.
        // Lifetime is tied to `&self`, matching the mmap's lifetime.
        Ok(unsafe { std::slice::from_raw_parts(self.data.add(offset) as *const u8, size) })
    }

    #[cfg(feature = "builder")]
    pub(crate) fn data_mut(
        &mut self,
        offset: usize,
        base: usize,
        size: usize,
    ) -> Result<&mut [u8]> {
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, size)?;
        // SAFETY: `offset + size <= self.size`. `&mut self` ensures exclusive
        // access to the mmap region. `u8` has no alignment requirement.
        Ok(unsafe { std::slice::from_raw_parts_mut(self.data.add(offset), size) })
    }

    /// Returns `offset + base` after verifying neither the addition nor the
    /// final value overflow `usize`. Wrapping behavior in release builds would
    /// otherwise let later bounds checks be silently bypassed.
    fn checked_offset(&self, offset: usize, base: usize) -> Result<usize> {
        offset
            .checked_add(base)
            .ok_or_else(|| Error::FileValidation(format!("Offset overflow: {offset} + {base}")))
    }

    fn check_size(&self, offset: usize, size: usize) -> Result<()> {
        let end = offset
            .checked_add(size)
            .ok_or_else(|| Error::FileValidation(format!("Size overflow: {offset} + {size}")))?;
        if end > self.size {
            error!(
                "Memory access out of bounds: {} + {} > {} (ptr={:p})",
                offset, size, self.size, self.data
            );
            return Err(Error::FileValidation(format!(
                "Invalid offset: {end} > {}",
                self.size
            )));
        }
        Ok(())
    }

    /// Verifies that `self.data.add(offset)` produces a pointer with the
    /// required alignment for `T`. The mmap base is page-aligned, so this
    /// reduces to a check on `offset % align_of::<T>()`.
    fn check_alignment<T>(&self, offset: usize) -> Result<()> {
        let align = mem::align_of::<T>();
        // SAFETY: `add(offset)` is only used to compute the address, not
        // dereferenced here. `offset <= self.size` is verified by the caller.
        let ptr_addr = unsafe { self.data.add(offset) } as usize;
        if ptr_addr % align != 0 {
            return Err(Error::FileValidation(format!(
                "Misaligned object at offset {offset}: required align={align}, addr={ptr_addr:#x}"
            )));
        }
        Ok(())
    }

    // Convert the memory-mapped file to the object with the given offset.
    // base is the base offset of the object.
    // offset is calculated by adding the base offset and the given offset.
    pub(crate) fn to_object<T>(&self, offset: usize, base: usize) -> Result<&T> {
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, mem::size_of::<T>())?;
        self.check_alignment::<T>(offset)?;
        // SAFETY: bounds and alignment are both verified above. The resulting
        // reference's lifetime is tied to `&self`, which owns the mmap.
        Ok(unsafe { &*(self.data.add(offset) as *const T) })
    }

    // Convert the memory-mapped file to the mutable object with the given offset.
    pub(crate) fn to_object_mut<T>(&mut self, offset: usize, base: usize) -> Result<&mut T> {
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, mem::size_of::<T>())?;
        self.check_alignment::<T>(offset)?;
        // SAFETY: bounds and alignment are both verified above. `&mut self`
        // ensures exclusive access for the lifetime of the returned reference.
        Ok(unsafe { &mut *(self.data.add(offset) as *mut T) })
    }

    // Convert the memory-mapped file to the CStr with the given offset.
    pub(crate) fn to_cstr(&self, offset: usize, base: usize) -> Result<&CStr> {
        let offset = self.checked_offset(offset, base)?;
        // Bound the NUL scan to the remaining mmap; `CStr::from_ptr` would
        // otherwise read past the mapping if no terminator is present.
        let remaining = self.size.checked_sub(offset).ok_or_else(|| {
            Error::FileValidation(format!("Offset past mmap: {offset} > {}", self.size))
        })?;
        // SAFETY: bounds checked above; `u8` has no alignment requirement.
        let bytes = unsafe { std::slice::from_raw_parts(self.data.add(offset), remaining) };
        CStr::from_bytes_until_nul(bytes).map_err(|e| {
            Error::FileValidation(format!("No NUL terminator at offset {offset}: {e}"))
        })
    }
}

impl std::ops::Drop for MemoryMap {
    fn drop(&mut self) {
        // SAFETY: `self.data` was returned by `mm::mmap` with `self.size`
        // bytes in `MemoryMap::new` and has not been unmapped since.
        unsafe {
            if let Err(e) = mm::munmap(self.data as _, self.size) {
                error!("Failed to unmap memory: {e:?}");
            }
        }
    }
}
