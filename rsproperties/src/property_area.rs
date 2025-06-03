// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{
    ffi::CStr,
    fs::{OpenOptions, File},
    mem,
    path::Path,
    sync::atomic::AtomicU32,
    os::unix::fs::OpenOptionsExt,
    fmt::Debug,
};

use rustix::{fs, mm};
use log::{debug, info, warn, error, trace};
use crate::errors::*;

use crate::property_info::{
    PropertyInfo,
    name_from_trailing_data,
};
#[cfg(feature = "builder")]
use crate::property_info::init_name_with_trailing_data;

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

    pub(crate) fn name(&self) -> &CStr {
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
            .field("name", &self.name().to_str().unwrap())
            .finish()
    }
}

fn cmp_prop_name(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Less => std::cmp::Ordering::Less,
        std::cmp::Ordering::Greater => std::cmp::Ordering::Greater,
        _ => a.cmp(b),
    }
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
    pub(crate) fn new_rw(filename: &Path, context: Option<&CStr>, fsetxattr_failed: &mut bool) -> Result<Self> {
        debug!("Creating new read-write property area map: {:?}", filename);

        let file = OpenOptions::new()
            .read(true)               // O_RDWR
            .write(true)              // O_RDWR
            .create(true)             // O_CREAT
            .custom_flags((fs::OFlags::NOFOLLOW.bits() | fs::OFlags::EXCL.bits()) as _) // additional flags
            .mode(0o444)              // permission: 0444
            .open(filename)
            .map_err(|e| Error::new_io(e))?;

        if let Some(context) = context {
            debug!("Setting SELinux context: {:?}", context);
            if fs::fsetxattr(&file, "selinux", context.to_bytes_with_nul(),
                fs::XattrFlags::empty()).is_err() {
                warn!("Failed to set SELinux context for {:?}", filename);
                *fsetxattr_failed = true;
            } else {
                trace!("Successfully set SELinux context for {:?}", filename);
            }
        }

        trace!("Truncating file to {} bytes", PA_SIZE);
        fs::ftruncate(&file, PA_SIZE).map_err(Error::from)?;

        let pa_size = PA_SIZE as usize;
        let pa_data_size = pa_size - std::mem::size_of::<PropertyArea>();

        debug!("Creating memory map: size={}, data_size={}", pa_size, pa_data_size);
        let mut thiz = Self {
            mmap: MemoryMap::new(file, pa_size, true)?,
            data_offset: std::mem::size_of::<PropertyArea>(),
            pa_data_size,
        };

        trace!("Initializing property area with magic={:#x}, version={:#x}", PROP_AREA_MAGIC, PROP_AREA_VERSION);
        thiz.property_area_mut().init(PROP_AREA_MAGIC, PROP_AREA_VERSION);

        info!("Successfully created read-write property area map: {:?}", filename);
        Ok(thiz)
    }

    // Initialize the property area map with the given file to read-only property area map.
    pub(crate) fn new_ro(filename: &Path) -> Result<Self> {
        debug!("Opening read-only property area map: {:?}", filename);

        let file = OpenOptions::new()
            .read(true)               // read only
            .custom_flags(fs::OFlags::NOFOLLOW.bits() as _) // additional flags
            .open(filename)
            .context_with_location("Failed to open to {filename:?}")?;

        let metadata = file.metadata()
            .context_with_location("Failed to get metadata")?;

        // Validate file metadata using common utility function
        crate::errors::validate_file_metadata(&metadata, filename, mem::size_of::<PropertyArea>() as u64)?;

        let pa_size = metadata.len() as usize;
        let pa_data_size = pa_size - std::mem::size_of::<PropertyArea>();

        debug!("Creating read-only memory map: size={}, data_size={}", pa_size, pa_data_size);
        let thiz = Self {
            mmap: MemoryMap::new(file, pa_size, false)?,
            data_offset: std::mem::size_of::<PropertyArea>(),
            pa_data_size,
        };

        let pa = thiz.property_area();
        trace!("Verifying property area: magic={:#x}, version={:#x}", pa.magic, pa.version);

        if thiz.property_area().magic != PROP_AREA_MAGIC ||
           thiz.property_area().version != PROP_AREA_VERSION {
            error!("Invalid magic ({:#x} != {:#x}) or version ({:#x} != {:#x}) for {:?}",
                   pa.magic, PROP_AREA_MAGIC, pa.version, PROP_AREA_VERSION, filename);
            Err(Error::new_file_validation("Invalid magic or version".to_string()).into())
        } else {
            info!("Successfully opened read-only property area map: {:?}", filename);
            Ok(thiz)
        }
    }

    pub(crate) fn property_area(&self) -> &PropertyArea {
        self.mmap.to_object::<PropertyArea>(0, 0)
            .expect("PropertyArea's offset is zero. So, it must be valid.")
    }

    fn property_area_mut(&mut self) -> &mut PropertyArea {
        self.mmap.to_object_mut::<PropertyArea>(0, 0)
            .expect("PropertyArea's offset is zero. So, it must be valid.")
    }

    // Find the property information with the given name.
    pub(crate) fn find(&self, name: &str) -> Result<(&PropertyInfo, u32)> {
        trace!("Finding property: '{}'", name);

        let mut remaining_name = name;
        let mut current = self.mmap.to_object::<PropertyTrieNode>(0, self.data_offset)?;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                error!("Invalid property name (empty segment): '{}'", name);
                return Err(Error::new_parse(format!("Invalid property name: {name}")));
            }

            let subname = &remaining_name[0..substr_size];
            trace!("Searching for property segment: '{}'", subname);

            let children_offset = current.children.load(std::sync::atomic::Ordering::Relaxed);
            let root = if children_offset != 0 {
                trace!("Found children at offset: {}", children_offset);
                self.to_prop_obj_from_atomic::<PropertyTrieNode>(&current.children)?
            } else {
                debug!("No children found for property segment '{}', property '{}' not found", subname, name);
                return Err(Error::new_not_found(name.to_owned()).into());
            };

            current = self.find_prop_trie_node(root, subname)?;

            if sep.is_none() {
                trace!("Reached final segment of property '{}'", name);
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
            trace!("Continuing with remaining name: '{}'", remaining_name);
        }

        let prop_offset = current.prop.load(std::sync::atomic::Ordering::Relaxed);

        if prop_offset != 0 {
            let offset = &current.prop.load(std::sync::atomic::Ordering::Acquire);
            trace!("Found property '{}' at offset: {}", name, offset);
            Ok((self.mmap.to_object(*offset as usize, self.data_offset)?, *offset))
        } else {
            debug!("Property '{}' found in trie but has no value", name);
            Err(Error::new_not_found(name.to_owned()).into())
        }
    }

    // Add the property information with the given name and value.
    #[cfg(feature = "builder")]
    pub(crate) fn add(&mut self, name: &str, value: &str) -> Result<()> {
        debug!("Adding property: '{}' = '{}'", name, value);

        let mut remaining_name = name;
        let mut current = 0;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                error!("Invalid property name (empty segment): '{}'", name);
                return Err(Error::new_parse(format!("Invalid property name: {name}")).into());
            }

            let subname = &remaining_name[0..substr_size];
            trace!("Processing property segment: '{}'", subname);

            let children_offset = {
                let current_node = self.mmap.to_object::<PropertyTrieNode>(current, self.data_offset)?;
                current_node.children.load(std::sync::atomic::Ordering::Relaxed)
            };
            let root_offset = if children_offset != 0 {
                let current_node = self.mmap.to_object::<PropertyTrieNode>(current, self.data_offset)?;
                trace!("Found existing children at offset: {}", children_offset);
                current_node.children.load(std::sync::atomic::Ordering::Acquire)
            } else {
                trace!("Creating new trie node for segment: '{}'", subname);
                let offset = self.new_prop_trie_node(subname)?;
                let current_node = self.mmap.to_object::<PropertyTrieNode>(current, self.data_offset)?;
                current_node.children.store(offset, std::sync::atomic::Ordering::Release);
                trace!("Created new trie node at offset: {}", offset);
                offset
            };

            current = self.add_prop_trie_node(root_offset, subname)? as _;

            if sep.is_none() {
                trace!("Reached final segment of property '{}'", name);
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
            trace!("Continuing with remaining name: '{}'", remaining_name);
        }

        let prop_offset = {
            let current_node = self.mmap.to_object_mut::<PropertyTrieNode>(current, self.data_offset)?;
            current_node.prop.load(std::sync::atomic::Ordering::Relaxed)
        };

        if prop_offset == 0 {
            trace!("Creating new property info for: '{}'", name);
            let offset = self.new_prop_info(name, value)?;
            let current_node = self.mmap.to_object_mut::<PropertyTrieNode>(current, self.data_offset)?;
            current_node.prop.store(offset, std::sync::atomic::Ordering::Release);
            info!("Successfully added property: '{}' at offset {}", name, offset);
        } else {
            debug!("Property '{}' already exists at offset {}", name, prop_offset);
        }

        Ok(())
    }

    // Read the dirty backup area.
    pub(crate) fn dirty_backup_area(&self) -> Result<&CStr> {
        trace!("Reading dirty backup area");
        let result = self.mmap.to_cstr(mem::size_of::<PropertyTrieNode>(), self.data_offset);
        match &result {
            Ok(cstr) => trace!("Dirty backup area: {:?}", cstr),
            Err(e) => error!("Failed to read dirty backup area: {}", e),
        }
        result
    }

    // Set the dirty backup area.
    // It is used to store the backup of the property area.
    #[cfg(feature = "builder")]
    pub(crate) fn set_dirty_backup_area(&mut self, value: &CStr) -> Result<()> {
        debug!("Setting dirty backup area: {:?}", value);

        let offset = mem::size_of::<PropertyTrieNode>();
        let bytes = value.to_bytes_with_nul();

        trace!("Backup area: offset={}, size={}, data_size={}", offset, bytes.len(), self.pa_data_size);

        if bytes.len() + offset > self.pa_data_size {
            error!("Backup area overflow: {} + {} > {}", bytes.len(), offset, self.pa_data_size);
            return Err(Error::new_file_validation("Invalid offset".to_string()).into());
        }

        self.mmap.data_mut(offset, self.data_offset, bytes.len())?.copy_from_slice(bytes);
        trace!("Successfully set dirty backup area");

        Ok(())
    }

    // Add a new property trie node with the given name to the given trie node.
    // It uses trie offset to avoid the life time issue of the current trie node.
    #[cfg(feature = "builder")]
    fn add_prop_trie_node(&mut self, trie_offset: u32, name: &str) -> Result<u32> {
        trace!("Adding trie node '{}' at offset {}", name, trie_offset);

        let name_bytes = name.as_bytes();
        let mut current_offset = trie_offset;
        loop {
            let current_node = self.mmap.to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
            let current_name = current_node.name().to_str().unwrap_or("<invalid>");

            match cmp_prop_name(name_bytes, current_node.name().to_bytes()) {
                std::cmp::Ordering::Less => {
                    trace!("'{}' < '{}', checking left branch", name, current_name);
                    let left_offset = current_node.left.load(std::sync::atomic::Ordering::Relaxed);
                    if left_offset != 0 {
                        current_offset = current_node.left.load(std::sync::atomic::Ordering::Acquire);
                        trace!("Following left branch to offset {}", current_offset);
                    } else {
                        trace!("Creating new left branch for '{}'", name);
                        let offset = self.new_prop_trie_node(name)?;

                        // To avoid the life time issue of current trie node.
                        let current_node = self.mmap.to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
                        current_node.left.store(offset, std::sync::atomic::Ordering::Release);
                        current_offset = offset;
                        trace!("Created left branch at offset {}", offset);
                        break;
                    }
                }
                std::cmp::Ordering::Greater => {
                    trace!("'{}' > '{}', checking right branch", name, current_name);
                    let right_offset = current_node.right.load(std::sync::atomic::Ordering::Relaxed);
                    if right_offset != 0 {
                        current_offset = current_node.right.load(std::sync::atomic::Ordering::Acquire);
                        trace!("Following right branch to offset {}", current_offset);
                    } else {
                        trace!("Creating new right branch for '{}'", name);
                        let offset = self.new_prop_trie_node(name)?;
                        // To avoid the life time issue of current trie node.
                        let current_node = self.mmap.to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
                        current_node.right.store(offset, std::sync::atomic::Ordering::Release);
                        current_offset = offset;
                        trace!("Created right branch at offset {}", offset);
                        break;
                    }
                }
                std::cmp::Ordering::Equal => {
                    trace!("Found existing node for '{}'", name);
                    break;
                }
            }
        }
        trace!("Trie node operation completed, final offset: {}", current_offset);
        Ok(current_offset)
    }

    fn find_prop_trie_node<'a>(&'a self, trie: &'a PropertyTrieNode, name: &str) -> Result<&'a PropertyTrieNode> {
        trace!("Finding trie node for name: '{}'", name);

        let name_bytes = name.as_bytes();
        let mut current = trie;
        loop {
            let current_name = current.name().to_str().unwrap_or("<invalid>");

            match cmp_prop_name(name_bytes, current.name().to_bytes()) {
                std::cmp::Ordering::Less => {
                    trace!("'{}' < '{}', checking left branch", name, current_name);
                    let left_offset = current.left.load(std::sync::atomic::Ordering::Relaxed);
                    if left_offset != 0 {
                        current = self.to_prop_obj_from_atomic::<PropertyTrieNode>(&current.left)?;
                        trace!("Following left branch to node '{}'", current.name().to_str().unwrap_or("<invalid>"));
                    } else {
                        debug!("Left branch empty, '{}' not found", name);
                        return Err(Error::new_not_found(name.to_owned()).into());
                    }
                }
                std::cmp::Ordering::Greater => {
                    trace!("'{}' > '{}', checking right branch", name, current_name);
                    let right_offset = current.right.load(std::sync::atomic::Ordering::Relaxed);
                    if right_offset != 0 {
                        current = self.to_prop_obj_from_atomic::<PropertyTrieNode>(&current.right)?;
                        trace!("Following right branch to node '{}'", current.name().to_str().unwrap_or("<invalid>"));
                    } else {
                        debug!("Right branch empty, '{}' not found", name);
                        return Err(Error::new_not_found(name.to_owned()).into());
                    }
                }
                std::cmp::Ordering::Equal => {
                    trace!("Found exact match for '{}'", name);
                    break;
                }
            }
        }
        Ok(current)
    }

    #[cfg(feature = "builder")]
    fn allocate_obj(&mut self, size: usize) -> Result<u32> {
        let aligned = crate::bionic_align(size, mem::size_of::<u32>());
        let offset = self.property_area().bytes_used;

        trace!("Allocating object: size={}, aligned={}, current_offset={}", size, aligned, offset);

        if offset + (aligned as u32) > self.pa_data_size as u32 {
            error!("Out of memory: {} + {} > {}", offset, aligned, self.pa_data_size);
            return Err(Error::new_file_size("Out of memory".to_string()).into());
        }

        self.property_area_mut().bytes_used += aligned as u32;
        trace!("Allocated object at offset {}, new bytes_used: {}", offset, self.property_area().bytes_used);
        Ok(offset)
    }

    #[cfg(feature = "builder")]
    pub(crate) fn new_prop_trie_node(&mut self, name: &str) -> Result<u32> {
        debug!("Creating new property trie node: '{}'", name);

        let new_offset = self.allocate_obj(mem::size_of::<PropertyTrieNode>() + name.len() + 1)?;
        let node = self.mmap.to_object_mut::<PropertyTrieNode>(new_offset as usize, self.data_offset)?;
        node.init(name);

        trace!("Created trie node '{}' at offset {}", name, new_offset);
        Ok(new_offset)
    }

    #[cfg(feature = "builder")]
    pub(crate) fn new_prop_info(&mut self, name: &str, value: &str) -> Result<u32> {
        debug!("Creating new property info: '{}' = '{}' (value_len={})", name, value, value.len());

        let new_offset = self.allocate_obj(mem::size_of::<PropertyInfo>() + name.len() + 1)?;

        if value.len() > crate::PROP_VALUE_MAX {
            info!("Property '{}' has long value ({}), allocating separate storage", name, value.len());
            let long_offset = self.allocate_obj(value.len() + 1)?;

            let target = self.mmap.data_mut(long_offset as usize, self.data_offset, value.len() + 1)?;
            target[0..value.len()].copy_from_slice(value.as_bytes());
            target[value.len()] = 0; // Add null terminator

            let relative_offset = long_offset - new_offset;
            trace!("Long value stored at offset {} (relative: {})", long_offset, relative_offset);

            let info = self.mmap.to_object_mut::<PropertyInfo>(new_offset as usize, self.data_offset)?;
            info.init_with_long_offset(name, relative_offset as _);
        } else {
            trace!("Property '{}' has normal value, storing inline", name);
            let info = self.mmap.to_object_mut::<PropertyInfo>(new_offset as usize, self.data_offset)?;
            info.init_with_value(name, value);
        };

        trace!("Created property info '{}' at offset {}", name, new_offset);
        Ok(new_offset)
    }

    fn to_prop_obj_from_atomic<T>(&self, offset: &AtomicU32) -> Result<&T> {
        let offset = offset.load(std::sync::atomic::Ordering::Acquire);
        self.mmap.to_object(offset as usize, self.data_offset)
    }

    pub(crate) fn property_info(&self, offset: u32) -> Result<&PropertyInfo> {
        self.mmap.to_object(offset as usize, self.data_offset)
    }
}

// MemoryMap is a wrapper for the memory-mapped file.
// It provides the safe access to the memory-mapped file.
#[derive(Debug)]
pub(crate) struct MemoryMap {
    data: *mut u8,
    size: usize,
}

unsafe impl Send for MemoryMap {}
unsafe impl Sync for MemoryMap {}

impl MemoryMap {
    pub(crate) fn new(file: File, size: usize, wriable: bool) -> Result<Self> {
        debug!("Creating memory map: size={}, writable={}", size, wriable);

        let flags = if wriable {
            mm::ProtFlags::READ.union(mm::ProtFlags::WRITE)
        } else {
            mm::ProtFlags::READ
        };

        trace!("Memory map flags: {:?}", flags);

        let memory_area = unsafe {
            mm::mmap(std::ptr::null_mut(),
                size, flags, mm::MapFlags::SHARED,
                file, 0)
        }.map_err(Error::from)? as *mut u8;

        info!("Successfully created memory map: ptr={:p}, size={}", memory_area, size);

        Ok(Self {
            data: memory_area,
            size,
        })
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    pub(crate) fn data(&self, offset: usize, base: usize, size: usize) -> Result<&[u8]> {
        let offset = offset + base;
        self.check_size(offset, size)?;
        Ok(unsafe {
            std::slice::from_raw_parts(self.data.add(offset) as *const u8, size)
        })
    }

    #[cfg(feature = "builder")]
    pub(crate) fn data_mut(&mut self, offset: usize, base: usize, size: usize) -> Result<&mut [u8]> {
        let offset = offset + base;
        self.check_size(offset, size)?;

        Ok(unsafe {
            std::slice::from_raw_parts_mut(self.data.add(offset), size)
        })
    }

    fn check_size(&self, offset: usize, size: usize) -> Result<()> {
        if offset + size > self.size {
            error!("Memory access out of bounds: {} + {} > {} (ptr={:p})", offset, size, self.size, self.data);
            return Err(Error::new_file_validation(format!("Invalid offset: {} > {}", offset + size, self.size)).into());
        }
        trace!("Memory access check passed: offset={}, size={}, total_size={}", offset, size, self.size);
        Ok(())
    }

    // Convert the memory-mapped file to the object with the given offset.
    // base is the base offset of the object.
    // offset is calculated by adding the base offset and the given offset.
    pub(crate) fn to_object<T>(&self, offset: usize, base: usize) -> Result<&T> {
        let offset = offset + base;
        self.check_size(offset, mem::size_of::<T>())?;
        Ok(unsafe { &*(self.data.add(offset as _) as *const T) })
    }

    // Convert the memory-mapped file to the mutable object with the given offset.
    pub(crate) fn to_object_mut<T>(&mut self, offset: usize, base: usize) -> Result<&mut T> {
        let offset = offset + base;
        self.check_size(offset, mem::size_of::<T>())?;
        Ok(unsafe { &mut *(self.data.add(offset) as *mut T) })
    }

    // Convert the memory-mapped file to the CStr with the given offset.
    pub(crate) fn to_cstr(&self, offset: usize, base: usize) -> Result<&CStr> {
        let offset = offset + base;
        self.check_size(offset, 1)?;
        unsafe {
            let ptr = self.data.add(offset) as *const i8;
            Ok(CStr::from_ptr(ptr))
        }
    }
}

impl std::ops::Drop for MemoryMap {
    fn drop(&mut self) {
        trace!("Dropping memory map: ptr={:p}, size={}", self.data, self.size);
        unsafe {
            if let Err(e) = mm::munmap(self.data as _, self.size) {
                error!("Failed to unmap memory: {:?}", e);
            } else {
                trace!("Successfully unmapped memory");
            }
        }
    }
}
