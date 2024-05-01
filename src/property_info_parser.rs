// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{
    cmp::Ordering, ffi::{c_void, CStr}, fs::File, mem::size_of, path::Path, ptr::NonNull
};

// This is a workaround for the fact that the `MetadataExt` trait is not implemented for `std::fs::Metadata` on all platforms.
#[cfg(target_os = "macos")]
use std::os::macos::fs::MetadataExt;
#[cfg(target_os = "android")]
use std::os::android::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::os::linux::fs::MetadataExt;

use rustix::{fs, mm};

use zerocopy::FromBytes;
use zerocopy_derive::{FromBytes, FromZeroes};

use crate::errors::*;

fn find<F>(array_length: u32, f: F) -> i32
where
    F: Fn(i32) -> Ordering,
{
    let mut bottom = 0;
    let mut top = array_length as i32 - 1;
    while top >= bottom {
        let search = (top + bottom) / 2;
        let cmp = f(search);

        match f(search) {
            Ordering::Equal => return search,
            Ordering::Less => bottom = search + 1,
            Ordering::Greater => top = search - 1,
        };
    }
    -1
}


#[derive(FromZeroes, FromBytes, Debug)]
#[repr(C, align(4))]
pub struct PropertyEntry {
    name_offset: u32,
    namelen: u32,
    context_index: u32,
    type_index: u32,
}

impl PropertyEntry {
    pub fn name<'a>(&'a self, property_info_area: &'a PropertyInfoArea) -> &'a CStr {
        property_info_area.cstr(self.name_offset as usize)
    }
}

#[derive(FromZeroes, FromBytes, Debug)]
#[repr(C, align(4))]
pub(crate) struct TrieNodeData {
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
    property_info_area: PropertyInfoArea<'a>,
    trie_node_offset: usize,
}

impl<'a> TrieNode<'a> {
    fn new(property_info_area: &'a PropertyInfoArea, trie_node_offset: usize) -> Self {
        Self {
            property_info_area: property_info_area.clone(),
            trie_node_offset,
        }
    }

    pub fn name(&self) -> &CStr {
        let name_offset = self.property_entry().name_offset as usize;
        self.property_info_area.cstr(name_offset)
    }

    fn data(&self) -> &TrieNodeData {
        self.property_info_area.ref_from(self.trie_node_offset)
    }

    fn property_entry(&self) -> &PropertyEntry {
        self.property_info_area.ref_from(self.data().property_entry as usize)
    }

    pub fn context_index(&self) -> u32 {
        self.property_entry().context_index
    }

    pub fn type_index(&self) -> u32 {
        self.property_entry().type_index
    }

    pub fn num_child_nodes(&self) -> u32 {
        self.data().num_child_nodes
    }

    pub fn child_node(&self, n: usize) -> TrieNode {
        let child_node_offset = u32::slice_from(&self.property_info_area.data_base[self.data().child_nodes as usize..]).unwrap()[n];
        TrieNode::new(&self.property_info_area, child_node_offset as usize)
    }

    pub fn find_child_for_string(&self, input: &str) -> Option<TrieNode> {
        let node_index = find(self.num_child_nodes(), |i| {
            let child = self.child_node(i as _);
            child.name().to_str().unwrap().cmp(input)
        });
        if node_index < 0 {
            None
        } else {
            Some(self.child_node(node_index as _))
        }
    }

    pub fn num_prefixes(&self) -> u32 {
        self.data().num_prefixes
    }

    pub fn prefix(&self, n: usize) -> &PropertyEntry {
        let offset = self.property_info_area.u32_slice_from(self.data().prefix_entries as usize)[n] as usize;
        self.property_info_area.ref_from(offset)
    }

    pub fn num_exact_matches(&self) -> u32 {
        self.data().num_exact_matches
    }

    pub fn exact_match(&self, n: usize) -> &PropertyEntry {
        let offset = self.property_info_area.u32_slice_from(self.data().exact_match_entries as usize)[n] as usize;
        self.property_info_area.ref_from(offset)
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

    // To resolve lifetime issues, we need to clone the TrieNode.
    fn clone_trie_node(&self, trie_node: &TrieNode) -> TrieNode {
        TrieNode::new(self, trie_node.trie_node_offset)
    }

    pub(crate) fn cstr(&self, offset: usize) -> &CStr {
        match self.data_base[offset..].iter().position(|&x| x == 0) {
            Some(end) => {
                let end = end + offset + 1;
                // println!("{offset}\n{:?}", &self.data_base[offset .. end].hex_dump());
                CStr::from_bytes_with_nul(&self.data_base[offset .. end]).unwrap()
            }
            None => {
                return CStr::from_bytes_with_nul(b"\0").unwrap();
            }
        }
    }

    #[inline]
    pub(crate) fn ref_from<T: FromBytes>(&self, offset: usize) -> &T {
        let size_of = size_of::<T>();
        T::ref_from(&self.data_base[offset..offset + size_of]).unwrap()
    }

    #[inline]
    fn u32_slice_from(&self, offset: usize) -> &[u32] {
        u32::slice_from(&self.data_base[offset..]).unwrap()
    }

    #[inline]
    fn header(&self) -> &PropertyInfoAreaHeader {
        self.ref_from(0)
    }

    #[inline]
    pub fn current_version(&self) -> u32 {
        self.header().current_version
    }

    #[inline]
    pub fn minimum_supported_version(&self) -> u32 {
        self.header().minimum_supported_version
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.header().size as _
    }

    #[inline]
    pub fn num_contexts(&self) -> usize {
        self.u32_slice_from(self.header().contexts_offset as usize)[0] as _
    }

    #[inline]
    pub fn num_types(&self) -> usize {
        self.u32_slice_from(self.header().types_offset as usize)[0] as _
    }

    pub fn root_node(&self) -> TrieNode {
        TrieNode::new(self, self.header().root_offset as usize)
    }

    pub fn context_offset(&self, index: usize) -> usize {
        let context_array_offset = self.header().contexts_offset as usize + size_of::<u32>();
        self.u32_slice_from(context_array_offset)[index] as _
    }

    pub fn type_offset(&self, index: usize) -> usize {
        let type_array_offset = self.header().types_offset as usize + size_of::<u32>();
        self.u32_slice_from(type_array_offset)[index] as _
    }

    fn check_prefix_match(&self, remaining_name: &str, trie_node: &TrieNode,
        context_index: &mut u32, type_index: &mut u32) {
        let mut remaining_name_size = remaining_name.len();
        for i in 0..trie_node.num_prefixes() {
            let prefix = trie_node.prefix(i as _);
            if prefix.namelen > remaining_name_size as u32 {
                continue;
            }
            let prefix_name = prefix.name(self).to_str().unwrap();
            if remaining_name.starts_with(prefix_name) {
                if prefix.context_index != !0 {
                    *context_index = prefix.context_index;
                }

                if prefix.type_index != !0 {
                    *type_index = prefix.type_index;
                }
                return;
            }
        }
    }

    pub fn get_property_info_indexes(&self, name: &str) -> (u32, u32) {
        let mut return_context_index: u32 = !0;
        let mut return_type_index: u32 = !0;
        let mut remaining_name = name;
        let mut trie_node = self.root_node();
        while true {
            if trie_node.context_index() != !0 {
                return_context_index = trie_node.context_index();
            }

            if trie_node.type_index() != !0 {
                return_type_index = trie_node.type_index();
            }

            self.check_prefix_match(remaining_name, &trie_node, &mut return_context_index, &mut return_type_index);
            match remaining_name.find('.') {
                Some(index) => {
                    match trie_node.find_child_for_string(&remaining_name[..index]) {
                        Some(node) => {
                            remaining_name = &remaining_name[index + 1..];
                            trie_node = self.clone_trie_node(&node);
                        }
                        None => break,
                    };
                }
                None => break,
            }
        }

        for i in 0..trie_node.num_exact_matches() {
            let exact_match = trie_node.exact_match(i as _);
            let exact_match_name = exact_match.name(self).to_str().unwrap();
            if exact_match_name == remaining_name {
                let context_index = if exact_match.context_index != !0 {
                    exact_match.context_index
                } else {
                    return_context_index
                };

                let type_index = if exact_match.type_index != !0 {
                    exact_match.type_index
                } else {
                    return_type_index
                };

                return (context_index, type_index);
            }
        }

        self.check_prefix_match(remaining_name, &trie_node, &mut return_context_index, &mut return_type_index);
        return (return_context_index, return_type_index);
    }

    pub fn get_property_info(&self, name: &str) -> (Option<&CStr>, Option<&CStr>) {
        let (context_index, type_index) = self.get_property_info_indexes(name);
        let context_cstr = if context_index == !0 {
            None
        } else {
            Some(self.cstr(self.context_offset(context_index as _) as _))
        };

        let type_cstr = if type_index == !0 {
            None
        } else {
            Some(self.cstr(self.type_offset(type_index as _) as _))
        };
        (context_cstr, type_cstr)
    }
}

pub struct PropertyInfoAreaFile {
    mmap_base: *const c_void,
    mmap_size: usize,
}

unsafe impl Send for PropertyInfoAreaFile {}
unsafe impl Sync for PropertyInfoAreaFile {}

impl PropertyInfoAreaFile {
    pub fn load_default_path() -> Result<Self> {
        Self::load_path(Path::new(crate::system_properties::PROP_TREE_FILE))
    }

    pub fn load_path(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(Error::new_io)?;

        let metadata = file.metadata().map_err(Error::new_io)?;
        if metadata.st_uid() != 0 || metadata.st_gid() != 0 ||
            metadata.st_mode() & (fs::Mode::WGRP.bits() | fs::Mode::WOTH.bits()) as u32 != 0 ||
            metadata.st_size() < size_of::<PropertyInfoAreaHeader>() as u64 {
            return Err(Error::new_invalid_data("Invalid file metadata".to_owned()));
        }

        let mmap_size = metadata.st_size();
        let map_result = unsafe {
            mm::mmap(std::ptr::null_mut(),
                mmap_size as usize,
                mm::ProtFlags::READ,
                mm::MapFlags::SHARED,
                file, 0)
        }.map_err(Error::new_errno)?;

        Ok(Self {
            mmap_base: map_result,
            // mmap_base: NonNull::new(map_result).unwrap(),
            mmap_size: mmap_size as usize,
        })
    }

    pub fn property_info_area(&self) -> PropertyInfoArea {
        let data_base = unsafe {
            std::slice::from_raw_parts(self.mmap_base as *const u8, self.mmap_size)
        };

        PropertyInfoArea::new(data_base)
    }
}

impl std::ops::Drop for PropertyInfoAreaFile {
    fn drop(&mut self) {
        unsafe {
            mm::munmap(self.mmap_base as _, self.mmap_size).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use android_system_properties::AndroidSystemProperties;

    #[test]
    fn test_property_info_area_file() -> Result<()> {
        let info_area_file = PropertyInfoAreaFile::load_default_path()?;

        let info_area = info_area_file.property_info_area();

        assert_eq!(info_area.current_version(), 1);
        assert_eq!(info_area.minimum_supported_version(), 1);

        println!("info_area.header() = {:?}", info_area.header());

        let num_context_nodes = info_area.num_contexts();
        println!("num_context_nodes = {:?}", num_context_nodes);

        // for i in 0..num_context_nodes {
        //     println!("context: {:?}", info_area.cstr(info_area.context_offset(i)));
        // }

        let (context_cstr, type_cstr) = info_area.get_property_info("ro.build.version.sdk");
        println!("context_cstr = {:?}, type_cstr = {:?}", context_cstr, type_cstr);
        assert_eq!(context_cstr.unwrap().to_str().unwrap(), "ro");

        Ok(())
    }
}
