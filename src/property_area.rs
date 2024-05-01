// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{
    ffi::CStr,
    fs::OpenOptions,
    mem,
    path::Path,
    ptr,
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

use crate::errors::*;
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
    if a.len() < b.len() {
        return std::cmp::Ordering::Less;
    } else if a.len() > b.len() {
        return std::cmp::Ordering::Greater;
    } else {
        return a.cmp(b);
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
}

#[derive(Debug)]
pub(crate) struct PropertyAreaMap {
    property_area: *mut PropertyArea,
    pa_size: usize,
    data: *mut u8,
    pa_data_size: usize,
}

unsafe impl Send for PropertyAreaMap {}
unsafe impl Sync for PropertyAreaMap {}

impl PropertyAreaMap {
    pub(crate) fn new_rw(filename: &Path, context: Option<&CStr>, fsetxattr_failed: &mut bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)               // O_RDWR
            .write(true)              // O_RDWR
            .create(true)             // O_CREAT
            .custom_flags((fs::OFlags::NOFOLLOW.bits() | fs::OFlags::EXCL.bits()) as _) // additional flags
            .mode(0o444)              // permission: 0444
            .open(filename)
            .map_err(Error::new_io)?;

        if let Some(context) = context {
            if fs::fsetxattr(&file, "selinux", context.to_bytes_with_nul(),
                fs::XattrFlags::empty()).is_err() {
                *fsetxattr_failed = true;
            }
        }

        fs::ftruncate(&file, PA_SIZE).map_err(Error::new_errno)?;

        let pa_size = PA_SIZE as usize;
        let pa_data_size = pa_size as usize - std::mem::size_of::<PropertyArea>();

        let memory_area = unsafe {
            mm::mmap(std::ptr::null_mut(),
                pa_size,
                mm::ProtFlags::READ.union(mm::ProtFlags::WRITE),
                mm::MapFlags::SHARED,
                file, 0)
        }.map_err(Error::new_errno)? as *mut u8;

        let thiz = Self {
            property_area: memory_area as _,
            pa_size,
            data: unsafe { memory_area.offset(std::mem::size_of::<PropertyArea>() as _) },
            pa_data_size,
        };

        thiz.property_area_mut().init(PROP_AREA_MAGIC, PROP_AREA_VERSION);

        Ok(thiz)
    }

    pub(crate) fn new_ro(filename: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)               // read only
            .custom_flags(fs::OFlags::NOFOLLOW.bits() as _) // additional flags
            .open(filename)
            .map_err(Error::new_io)?;

        let metadata = file.metadata().map_err(Error::new_io)?;
        if metadata.st_uid() != 0 || metadata.st_gid() != 0 ||
            metadata.st_mode() & (fs::Mode::WGRP.bits() | fs::Mode::WOTH.bits()) as u32 != 0 ||
            metadata.st_size() < mem::size_of::<PropertyArea>() as u64 {
            return Err(Error::new_invalid_data("Invalid file metadata".to_owned()));
        }

        let pa_size = metadata.st_size() as _;
        let pa_data_size = pa_size as usize - std::mem::size_of::<PropertyArea>();

        let memory_area = unsafe {
            mm::mmap(std::ptr::null_mut(),
                pa_size as usize,
                mm::ProtFlags::READ,
                mm::MapFlags::SHARED,
                file, 0)
        }.map_err(Error::new_errno)? as *mut u8;

        let thiz = Self {
            property_area: memory_area as _,
            pa_size,
            data: unsafe { memory_area.offset(std::mem::size_of::<PropertyArea>() as _) },
            pa_data_size,
        };

        if thiz.property_area().magic != PROP_AREA_MAGIC ||
           thiz.property_area().version != PROP_AREA_VERSION {
            return Err(Error::new_invalid_data("Invalid magic or version".to_owned()));
        }

        Ok(thiz)
    }

    pub(crate) fn property_area(&self) -> &PropertyArea {
        unsafe { &*self.property_area }
    }

    pub(crate) fn property_area_mut(&self) -> &mut PropertyArea {
        unsafe { &mut *self.property_area }
    }

    pub(crate) fn find(&self, name: &str) -> Result<&PropertyInfo> {
        self.find_property(self.property_area(), self.root_node()?, name, "", false)
    }

    pub(crate) fn dirty_backup_area(&self) -> Result<&CStr> {
        self.to_prop_cstr(mem::size_of::<PropertyTrieNode>() as _)
    }

    fn find_prop_trie_node<'a>(&'a self, trie: &'a PropertyTrieNode, name: &str, alloc_if_needed: bool) -> Result<&'a PropertyTrieNode> {
        let name_bytes = name.as_bytes();
        let mut current = trie;
        loop {
            match cmp_prop_name(name_bytes, current.name().to_bytes()) {
                std::cmp::Ordering::Less => {
                    let left_offset = current.left.load(std::sync::atomic::Ordering::Relaxed);
                    if left_offset != 0 {
                        current = self.to_prop_obj_from_atomic::<PropertyTrieNode>(&current.left)?;
                    } else if alloc_if_needed {
                        let (node, offset) = self.new_prop_trie_node(name)?;
                        current.left.store(offset, std::sync::atomic::Ordering::Release);
                        current = node;
                        break;
                    } else {
                        return Err(Error::new_invalid_data("Can't manage PropertyTrieNode".to_owned()));
                    }
                }
                std::cmp::Ordering::Greater => {
                    let right_offset = current.right.load(std::sync::atomic::Ordering::Relaxed);
                    if right_offset != 0 {
                        current = self.to_prop_obj_from_atomic::<PropertyTrieNode>(&current.right)?;
                    } else if alloc_if_needed {
                        let (node, offset) = self.new_prop_trie_node(name)?;
                        current.right.store(offset, std::sync::atomic::Ordering::Release);
                        current = node;
                        break;
                    } else {
                        return Err(Error::new_invalid_data("Can't manage PropertyTrieNode".to_owned()));
                    }
                }
                std::cmp::Ordering::Equal => {
                    break;
                }
            }
        }
        Ok(current)
    }

    pub(crate) fn find_property(&self, prop_area: &PropertyArea,
        trie: &PropertyTrieNode, name: &str, value: &str,
        alloc_if_needed: bool) -> Result<&PropertyInfo> {
        let mut remaining_name = name;
        let mut current = trie;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                return Err(Error::new_invalid_data("Invalid property name".to_owned()));
            }

            let subname = &remaining_name[0..substr_size];

            let children_offset = current.children.load(std::sync::atomic::Ordering::Relaxed);
            let root = if children_offset != 0 {
                self.to_prop_obj_from_atomic::<PropertyTrieNode>(&current.children)?
            } else if alloc_if_needed {
                let (node, offset) = self.new_prop_trie_node(subname)?;
                current.children.store(offset, std::sync::atomic::Ordering::Release);
                node
            }
            else {
                return Err(Error::new_invalid_data("Can't manage PropertyTrieNode".to_owned()));
            };

            current = self.find_prop_trie_node(root, subname, alloc_if_needed)?;

            if sep.is_none() {
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
        }

        let prop_offset = current.prop.load(std::sync::atomic::Ordering::Relaxed);

        if prop_offset != 0 {
            self.to_prop_obj_from_atomic::<PropertyInfo>(&current.prop)
        } else if alloc_if_needed {
            let (info, offset) = self.new_prop_info(name, value)?;
            current.prop.store(offset, std::sync::atomic::Ordering::Release);
            Ok(info)
        } else {
            return Err(Error::new_invalid_data("Can't manage PropertyInfo".to_owned()));
        }
    }

    fn allocate_obj(&self, size: usize) -> Result<u32> {
        let aligned = crate::bionic_align(size, mem::size_of::<u32>());
        let offset = self.property_area().bytes_used;
        if offset + (aligned as u32) > self.pa_data_size as u32 {
            return Err(Error::new_invalid_data("Out of memory".to_owned()));
        }

        self.property_area_mut().bytes_used += aligned as u32;
        Ok(offset)
    }

    pub(crate) fn new_prop_trie_node(&self, name: &str) -> Result<(&PropertyTrieNode, u32)> {
        let new_offset = self.allocate_obj(mem::size_of::<PropertyTrieNode>() + name.len() + 1)?;
        let node = self.to_prop_obj_mut::<PropertyTrieNode>(new_offset as _)?;
        node.init(name);

        Ok((node, new_offset))
    }

    pub(crate) fn new_prop_info(&self, name: &str, value: &str) -> Result<(&PropertyInfo, u32)> {
        let new_offset = self.allocate_obj(mem::size_of::<PropertyInfo>() + name.len() + 1)?;

        let info = if value.len() > crate::PROP_VALUE_MAX {
            let long_offset = self.allocate_obj(value.len() + 1)?;

            unsafe {
                let dest = self.data.add(long_offset as usize);
                ptr::copy_nonoverlapping(value.as_ptr(), dest, value.len());
                *dest.add(value.len()) = 0; // Add null terminator
            }

            let long_offset = long_offset - new_offset;

            let info = self.to_prop_obj_mut::<PropertyInfo>(new_offset as _)?;
            info.init_with_long_offset(name, long_offset as _);
            info
        } else {
            let info = self.to_prop_obj_mut::<PropertyInfo>(new_offset as _)?;
            info.init_with_value(name, value);
            info
        };

        Ok((info, new_offset))
    }

    fn to_prop_obj_from_atomic<T>(&self, offset: &AtomicU32) -> Result<&T> {
        let offset = offset.load(std::sync::atomic::Ordering::Acquire);
        self.to_prop_obj(offset as _)
    }

    fn to_prop_obj<T: Sized>(&self, offset: usize) -> Result<&T> {
        if offset + mem::size_of::<T>() > self.pa_data_size {
            return Err(Error::new_invalid_data("Invalid offset".to_owned()));
        }
        Ok(unsafe { &*(self.data.offset(offset as _) as *const T) })
    }

    fn to_prop_obj_mut<T: Sized>(&self, offset: usize) -> Result<&mut T> {
        if offset + mem::size_of::<T>() > self.pa_data_size {
            return Err(Error::new_invalid_data("Invalid offset".to_owned()));
        }
        Ok(unsafe { &mut *(self.data.offset(offset as _) as *mut T) })
    }

    fn to_prop_cstr(&self, offset: usize) -> Result<&CStr> {
        // TODO: Check if the size of the CStr is correct
        unsafe {
            let ptr = self.data.add(offset) as *const i8;
            Ok(CStr::from_ptr(ptr))
        }
    }

    fn root_node(&self) -> Result<&PropertyTrieNode> {
        self.to_prop_obj(0)
    }
}

impl std::ops::Drop for PropertyAreaMap {
    fn drop(&mut self) {
        unsafe {
            mm::munmap(self.property_area as _, self.pa_size).unwrap();
        }
    }
}
