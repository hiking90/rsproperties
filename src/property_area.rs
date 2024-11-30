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

// This is a workaround for the fact that the `MetadataExt` trait is not implemented for `std::fs::Metadata` on all platforms.
#[cfg(target_os = "macos")]
use std::os::macos::fs::MetadataExt;
#[cfg(target_os = "android")]
use std::os::android::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::os::linux::fs::MetadataExt;

use rustix::{fs, mm};
use anyhow::Context;
use rserror::*;

use crate::property_info::{
    PropertyInfo,
    name_from_trailing_data,
    init_name_with_trailing_data,
};

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
    pa_data_size: usize,
}

impl PropertyAreaMap {
    // Initialize the property area map with the given file to create a new property area map.
    pub(crate) fn new_rw(filename: &Path, context: Option<&CStr>, fsetxattr_failed: &mut bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)               // O_RDWR
            .write(true)              // O_RDWR
            .create(true)             // O_CREAT
            .custom_flags((fs::OFlags::NOFOLLOW.bits() | fs::OFlags::EXCL.bits()) as _) // additional flags
            .mode(0o444)              // permission: 0444
            .open(filename)
            .context(format!("Failed to open to {filename:?}"))?;

        if let Some(context) = context {
            if fs::fsetxattr(&file, "selinux", context.to_bytes_with_nul(),
                fs::XattrFlags::empty()).is_err() {
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

        thiz.property_area_mut().init(PROP_AREA_MAGIC, PROP_AREA_VERSION);

        Ok(thiz)
    }

    // Initialize the property area map with the given file to read-only property area map.
    pub(crate) fn new_ro(filename: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)               // read only
            .custom_flags(fs::OFlags::NOFOLLOW.bits() as _) // additional flags
            .open(filename)
            .context_with_location("Failed to open to {filename:?}")?;

        let metadata = file.metadata()
            .context_with_location("Failed to get metadata")?;
        if cfg!(test) {
            if metadata.st_mode() & (fs::Mode::WGRP.bits() | fs::Mode::WOTH.bits()) as u32 != 0 ||
                metadata.st_size() < mem::size_of::<PropertyArea>() as u64 {
                anyhow::bail!("Invalid file metadata");
            }
        } else if metadata.st_uid() != 0 || metadata.st_gid() != 0 ||
            metadata.st_mode() & (fs::Mode::WGRP.bits() | fs::Mode::WOTH.bits()) as u32 != 0 ||
            metadata.st_size() < mem::size_of::<PropertyArea>() as u64 {
            anyhow::bail!("Invalid file metadata");
        }

        let pa_size = metadata.st_size() as usize;

        let pa_data_size = pa_size - std::mem::size_of::<PropertyArea>();

        let thiz = Self {
            mmap: MemoryMap::new(file, pa_size, false)?,
            data_offset: std::mem::size_of::<PropertyArea>(),
            pa_data_size,
        };

        if thiz.property_area().magic != PROP_AREA_MAGIC ||
           thiz.property_area().version != PROP_AREA_VERSION {
            Err(anyhow::anyhow!("Invalid magic or version"))
        } else {
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
        let mut remaining_name = name;
        let mut current = self.mmap.to_object::<PropertyTrieNode>(0, self.data_offset)?;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                anyhow::bail!("Invalid property name: {name}");
            }

            let subname = &remaining_name[0..substr_size];

            let children_offset = current.children.load(std::sync::atomic::Ordering::Relaxed);
            let root = if children_offset != 0 {
                self.to_prop_obj_from_atomic::<PropertyTrieNode>(&current.children)?
            } else {
                return Err(Error::new_not_found(name.to_owned()).into());
            };

            current = self.find_prop_trie_node(root, subname)?;

            if sep.is_none() {
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
        }

        let prop_offset = current.prop.load(std::sync::atomic::Ordering::Relaxed);

        if prop_offset != 0 {
            let offset = &current.prop.load(std::sync::atomic::Ordering::Acquire);
            Ok((self.mmap.to_object(*offset as usize, self.data_offset)?, *offset))
        } else {
            Err(Error::new_not_found(name.to_owned()).into())
        }
    }

    // Add the property information with the given name and value.
    pub(crate) fn add(&mut self, name: &str, value: &str) -> Result<()> {
        let mut remaining_name = name;
        let mut current = 0;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                return Err(rserror!("Invalid property name: {name}"));
            }

            let subname = &remaining_name[0..substr_size];

            let children_offset = {
                let current_node = self.mmap.to_object::<PropertyTrieNode>(current, self.data_offset)?;
                current_node.children.load(std::sync::atomic::Ordering::Relaxed)
            };
            let root_offset = if children_offset != 0 {
                let current_node = self.mmap.to_object::<PropertyTrieNode>(current, self.data_offset)?;
                current_node.children.load(std::sync::atomic::Ordering::Acquire)
            } else {
                let offset = self.new_prop_trie_node(subname)?;
                let current_node = self.mmap.to_object::<PropertyTrieNode>(current, self.data_offset)?;
                current_node.children.store(offset, std::sync::atomic::Ordering::Release);
                offset
            };

            current = self.add_prop_trie_node(root_offset, subname)? as _;

            if sep.is_none() {
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
        }

        let prop_offset = {
            let current_node = self.mmap.to_object_mut::<PropertyTrieNode>(current, self.data_offset)?;
            current_node.prop.load(std::sync::atomic::Ordering::Relaxed)
        };

        if prop_offset == 0 {
            let offset = self.new_prop_info(name, value)?;
            let current_node = self.mmap.to_object_mut::<PropertyTrieNode>(current, self.data_offset)?;
            current_node.prop.store(offset, std::sync::atomic::Ordering::Release);
        }

        Ok(())
    }

    // Read the dirty backup area.
    pub(crate) fn dirty_backup_area(&self) -> Result<&CStr> {
        self.mmap.to_cstr(mem::size_of::<PropertyTrieNode>(), self.data_offset)
    }

    // Set the dirty backup area.
    // It is used to store the backup of the property area.
    pub(crate) fn set_dirty_backup_area(&mut self, value: &CStr) -> Result<()> {
        let offset = mem::size_of::<PropertyTrieNode>();
        let bytes = value.to_bytes_with_nul();
        if bytes.len() + offset > self.pa_data_size {
            return Err(rserror!("Invalid offset"));
        }

        self.mmap.data_mut(offset, self.data_offset, bytes.len())?.copy_from_slice(bytes);

        Ok(())
    }

    // Add a new property trie node with the given name to the given trie node.
    // It uses trie offset to avoid the life time issue of the current trie node.
    fn add_prop_trie_node(&mut self, trie_offset: u32, name: &str) -> Result<u32> {
        let name_bytes = name.as_bytes();
        let mut current_offset = trie_offset;
        loop {
            let current_node = self.mmap.to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
            match cmp_prop_name(name_bytes, current_node.name().to_bytes()) {
                std::cmp::Ordering::Less => {
                    let left_offset = current_node.left.load(std::sync::atomic::Ordering::Relaxed);
                    if left_offset != 0 {
                        current_offset = current_node.left.load(std::sync::atomic::Ordering::Acquire);
                    } else {
                        let offset = self.new_prop_trie_node(name)?;

                        // To avoid the life time issue of current trie node.
                        let current_node = self.mmap.to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
                        current_node.left.store(offset, std::sync::atomic::Ordering::Release);
                        current_offset = offset;
                        break;
                    }
                }
                std::cmp::Ordering::Greater => {
                    let right_offset = current_node.right.load(std::sync::atomic::Ordering::Relaxed);
                    if right_offset != 0 {
                        current_offset = current_node.right.load(std::sync::atomic::Ordering::Acquire);
                    } else {
                        let offset = self.new_prop_trie_node(name)?;
                        // To avoid the life time issue of current trie node.
                        let current_node = self.mmap.to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
                        current_node.right.store(offset, std::sync::atomic::Ordering::Release);
                        current_offset = offset;
                        break;
                    }
                }
                std::cmp::Ordering::Equal => {
                    break;
                }
            }
        }
        Ok(current_offset)
    }

    fn find_prop_trie_node<'a>(&'a self, trie: &'a PropertyTrieNode, name: &str) -> Result<&'a PropertyTrieNode> {
        let name_bytes = name.as_bytes();
        let mut current = trie;
        loop {
            match cmp_prop_name(name_bytes, current.name().to_bytes()) {
                std::cmp::Ordering::Less => {
                    let left_offset = current.left.load(std::sync::atomic::Ordering::Relaxed);
                    if left_offset != 0 {
                        current = self.to_prop_obj_from_atomic::<PropertyTrieNode>(&current.left)?;
                    } else {
                        return Err(Error::new_not_found(name.to_owned()).into());
                    }
                }
                std::cmp::Ordering::Greater => {
                    let right_offset = current.right.load(std::sync::atomic::Ordering::Relaxed);
                    if right_offset != 0 {
                        current = self.to_prop_obj_from_atomic::<PropertyTrieNode>(&current.right)?;
                    } else {
                        return Err(Error::new_not_found(name.to_owned()).into());
                    }
                }
                std::cmp::Ordering::Equal => {
                    break;
                }
            }
        }
        Ok(current)
    }

    fn allocate_obj(&mut self, size: usize) -> Result<u32> {
        let aligned = crate::bionic_align(size, mem::size_of::<u32>());
        let offset = self.property_area().bytes_used;
        if offset + (aligned as u32) > self.pa_data_size as u32 {
            return Err(rserror!("Out of memory"));
        }

        self.property_area_mut().bytes_used += aligned as u32;
        Ok(offset)
    }

    pub(crate) fn new_prop_trie_node(&mut self, name: &str) -> Result<u32> {
        let new_offset = self.allocate_obj(mem::size_of::<PropertyTrieNode>() + name.len() + 1)?;
        let node = self.mmap.to_object_mut::<PropertyTrieNode>(new_offset as usize, self.data_offset)?;
        node.init(name);

        Ok(new_offset)
    }

    pub(crate) fn new_prop_info(&mut self, name: &str, value: &str) -> Result<u32> {
        let new_offset = self.allocate_obj(mem::size_of::<PropertyInfo>() + name.len() + 1)?;

        if value.len() > crate::PROP_VALUE_MAX {
            let long_offset = self.allocate_obj(value.len() + 1)?;

            let target = self.mmap.data_mut(long_offset as usize, self.data_offset, value.len() + 1)?;
            target[0..value.len()].copy_from_slice(value.as_bytes());
            target[value.len()] = 0; // Add null terminator

            let long_offset = long_offset - new_offset;

            let info = self.mmap.to_object_mut::<PropertyInfo>(new_offset as usize, self.data_offset)?;
            info.init_with_long_offset(name, long_offset as _);
        } else {
            let info = self.mmap.to_object_mut::<PropertyInfo>(new_offset as usize, self.data_offset)?;
            info.init_with_value(name, value);
        };

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
        let flags = if wriable {
            mm::ProtFlags::READ.union(mm::ProtFlags::WRITE)
        } else {
            mm::ProtFlags::READ
        };

        let memory_area = unsafe {
            mm::mmap(std::ptr::null_mut(),
                size, flags, mm::MapFlags::SHARED,
                file, 0)
        }.map_err(Error::from)? as *mut u8;

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

    pub(crate) fn data_mut(&mut self, offset: usize, base: usize, size: usize) -> Result<&mut [u8]> {
        let offset = offset + base;
        self.check_size(offset, size)?;

        Ok(unsafe {
            std::slice::from_raw_parts_mut(self.data.add(offset), size)
        })
    }

    fn check_size(&self, offset: usize, size: usize) -> Result<()> {
        if offset + size > self.size {
            return Err(rserror!("Invalid offset: {} > {}", offset + size, self.size));
        }
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
        unsafe {
            mm::munmap(self.data as _, self.size).unwrap();
        }
    }
}
