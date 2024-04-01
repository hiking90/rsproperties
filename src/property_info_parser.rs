// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{
    ffi::{c_void, CStr},
    fs::File,
    mem::size_of,
    path::Path,
    ptr::NonNull,
};

// This is a workaround for the fact that the `MetadataExt` trait is not implemented for `std::fs::Metadata` on all platforms.
#[cfg(target_os = "macos")]
use std::os::macos::fs::MetadataExt;
#[cfg(target_os = "android")]
use std::os::android::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::os::linux::fs::MetadataExt;

use zerocopy::FromBytes;
use zerocopy_derive::{FromBytes, FromZeroes};

use crate::errors::*;

#[derive(FromZeroes, FromBytes, Debug)]
#[repr(C, align(4))]
pub struct PropertyEntry {
    name_offset: u32,
    namelen: u32,
    context_index: u32,
    type_index: u32,
}

#[derive(FromZeroes, FromBytes, Debug)]
#[repr(C, align(4))]
pub(crate) struct TrieNodeInternal {
    property_entry: u32,
    num_child_nodes: u32,
    child_nodes: u32,
    num_prefixes: u32,
    prefix_entries: u32,
    num_exact_matches: u32,
    exact_match_entries: u32,
}

#[derive(FromZeroes, FromBytes, Debug)]
#[repr(C, align(4))]
pub struct PropertyInfoAreaHeader {
    current_version: u32,
    minimum_supported_version: u32,
    size: u32,
    contexts_offset: u32,
    types_offset: u32,
    root_offset: u32,
}

pub struct TrieNode<'a> {
    data_base: &'a [u8],
    trie_node_offset: usize,
}

impl<'a> TrieNode<'a> {
    fn new(data_base: &'a [u8], trie_node_offset: usize) -> Self {
        Self {
            data_base,
            trie_node_offset,
        }
    }

    pub fn name(&self) -> &CStr {
        let name_offset = self.property_entry().name_offset as usize;
        CStr::from_bytes_with_nul(&self.data_base[name_offset..]).unwrap()
    }

    fn internal(&self) -> &TrieNodeInternal {
        let size_of = size_of::<TrieNodeInternal>();
        TrieNodeInternal::ref_from(&self.data_base[self.trie_node_offset..size_of]).unwrap()
    }

    fn property_entry(&self) -> &PropertyEntry {
        let size_of = size_of::<PropertyEntry>();
        PropertyEntry::ref_from(&self.data_base[self.internal().property_entry as usize..size_of]).unwrap()
    }

    pub fn context_index(&self) -> u32 {
        self.property_entry().context_index
    }

    pub fn type_index(&self) -> u32 {
        self.property_entry().type_index
    }

    pub fn num_child_nodes(&self) -> u32 {
        self.internal().num_child_nodes
    }

    pub fn child_node(&self, n: usize) -> TrieNode {
        let child_node_offset = u32::slice_from(&self.data_base[self.internal().child_nodes as usize..]).unwrap()[n];
        TrieNode::new(self.data_base, child_node_offset as usize)
    }

    pub fn find_child_for_string(&self, input: &str, child: &TrieNode) -> bool {
        unimplemented!()
    }

    pub fn num_prefixes(&self) -> u32 {
        self.internal().num_prefixes
    }

    pub fn prefix(&self, n: usize) -> &PropertyEntry {
        let prefix_entry_offset = u32::slice_from(&self.data_base[self.internal().prefix_entries as usize..]).unwrap()[n];
        let size_of = size_of::<PropertyEntry>();
        PropertyEntry::ref_from(&self.data_base[prefix_entry_offset as usize..size_of]).unwrap()
    }

    pub fn num_exact_matches(&self) -> u32 {
        self.internal().num_exact_matches
    }

    pub fn exact_match(&self, n: usize) -> &PropertyEntry {
        let exact_match_entry_offset = u32::slice_from(&self.data_base[self.internal().exact_match_entries as usize..]).unwrap()[n];
        let size_of = size_of::<PropertyEntry>();
        PropertyEntry::ref_from(&self.data_base[exact_match_entry_offset as usize..size_of]).unwrap()
    }
}

#[derive(Debug, Clone)]
pub struct PropertyInfoArea<'a> {
    data_base: &'a [u8],
}

impl<'a> PropertyInfoArea<'a> {
    fn new(data_base: &'a [u8]) -> Self {
        Self {
            data_base,
        }
    }

    fn header(&self) -> &PropertyInfoAreaHeader {
        let size_of = size_of::<PropertyInfoAreaHeader>();
        PropertyInfoAreaHeader::ref_from(&self.data_base[0..size_of]).unwrap()
    }

    pub fn current_version(&self) -> u32 {
        self.header().current_version
    }

    pub fn minimum_supported_version(&self) -> u32 {
        self.header().minimum_supported_version
    }

    pub fn size(&self) -> u32 {
        self.header().size
    }

    pub fn num_contexts(&self) -> u32 {
        u32::slice_from(&self.data_base[self.header().contexts_offset as usize..]).unwrap()[0]
    }

    pub fn num_types(&self) -> u32 {
        u32::slice_from(&self.data_base[self.header().types_offset as usize..]).unwrap()[0]
    }

    pub fn root_node(&self) -> TrieNode {
        TrieNode::new(self.data_base, self.header().root_offset as usize)
    }
}

pub struct PropertyInfoAreaFile {
    mmap_base: NonNull<c_void>,
    mmap_size: usize,
}

impl PropertyInfoAreaFile {
    pub fn load_default_path() -> Result<Self> {
        Self::load_path(Path::new("/dev/__properties__/property_info"))
    }

    pub fn load_path(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(Error::new_io)?;

        let metadata = file.metadata().map_err(Error::new_io)?;
        if metadata.st_uid() != 0 || metadata.st_gid() != 0 ||
            metadata.st_mode() & (nix::sys::stat::Mode::S_IWGRP.bits() | nix::sys::stat::Mode::S_IWOTH.bits()) as u32 != 0 ||
            metadata.st_size() < size_of::<PropertyInfoAreaHeader>() as u64 {
            return Err(Error::new_invalid_data("Invalid file metadata"));
        }

        let mmap_size = metadata.st_size();
        let map_result = unsafe {
            nix::sys::mman::mmap(None,
                std::num::NonZeroUsize::new(mmap_size as usize).unwrap(),
                nix::sys::mman::ProtFlags::PROT_READ,
                nix::sys::mman::MapFlags::MAP_SHARED,
                file, 0)
        }.map_err(Error::new_errno)?;

        Ok(Self {
            mmap_base: map_result,
            mmap_size: mmap_size as usize,
        })
    }

    pub fn property_info_area(&self) -> PropertyInfoArea {
        let data_base = unsafe {
            std::slice::from_raw_parts(self.mmap_base.as_ptr() as *const u8, self.mmap_size)
        };

        PropertyInfoArea::new(data_base)
    }
}

impl std::ops::Drop for PropertyInfoAreaFile {
    fn drop(&mut self) {
        unsafe {
            nix::sys::mman::munmap(self.mmap_base, self.mmap_size).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_property_info_area_file() -> Result<()> {
        let info_area_file = PropertyInfoAreaFile::load_default_path()?;

        let info_area = info_area_file.property_info_area();

        assert_eq!(info_area.current_version(), 1);
        assert_eq!(info_area.minimum_supported_version(), 1);
        assert_ne!(info_area.size(), 0);
        assert_ne!(info_area.num_contexts(), 0);
        assert_ne!(info_area.num_types(), 0);

        Ok(())
    }
}
