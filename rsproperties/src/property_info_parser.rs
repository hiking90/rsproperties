// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{cmp::Ordering, ffi::CStr, fs::File, mem::size_of, path::Path};

use log::{info, warn};

use zerocopy_derive::*;

use crate::errors::*;
use crate::property_area::MemoryMap;

/// Binary search returning the matching index or `None` for miss. Internal
/// indexing uses `usize` to avoid `u32 → i32` truncation when
/// `array_length > i32::MAX`. The callback is `FnMut` so it can record
/// out-of-band signals (e.g. a corrupted entry) for the caller to inspect.
fn find<F>(array_length: u32, mut f: F) -> Option<usize>
where
    F: FnMut(usize) -> Ordering,
{
    let len = array_length as usize;
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match f(mid) {
            Ordering::Equal => return Some(mid),
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
        }
    }
    None
}

/// Decodes a trie entry name into a non-empty UTF-8 string.
///
/// Returns `None` and skips silently for empty names (the corruption-fallback
/// `c""` returned by `cstr()`). Returns `None` and logs a warning on UTF-8
/// failure so corrupted entries are observable in logs.
fn entry_name_str<'a>(name: &'a CStr, kind: &str, idx: usize) -> Option<&'a str> {
    match name.to_str() {
        Ok(s) if !s.is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            warn!("{kind} entry {idx} has non-UTF-8 name: {e}");
            None
        }
    }
}

#[derive(FromBytes, KnownLayout, Immutable, Debug)]
#[repr(C, align(4))]
pub(crate) struct PropertyEntry {
    pub(crate) name_offset: u32,
    pub(crate) namelen: u32,
    pub(crate) context_index: u32,
    pub(crate) type_index: u32,
}

impl PropertyEntry {
    pub(crate) fn name<'a>(&'a self, property_info_area: &'a PropertyInfoArea) -> &'a CStr {
        property_info_area.cstr(self.name_offset as usize)
    }
}

#[derive(FromBytes, KnownLayout, Debug, Immutable)]
#[repr(C, align(4))]
pub(crate) struct TrieNodeData {
    pub(crate) property_entry: u32,
    pub(crate) num_child_nodes: u32,
    pub(crate) child_nodes: u32,
    pub(crate) num_prefixes: u32,
    pub(crate) prefix_entries: u32,
    pub(crate) num_exact_matches: u32,
    pub(crate) exact_match_entries: u32,
}

#[derive(FromBytes, KnownLayout, Immutable, Debug)]
#[repr(C, align(4))]
pub struct PropertyInfoAreaHeader {
    pub(crate) current_version: u32,
    pub(crate) minimum_supported_version: u32,
    pub(crate) size: u32,
    pub(crate) contexts_offset: u32,
    pub(crate) types_offset: u32,
    pub(crate) root_offset: u32,
}

#[derive(Debug)]
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

    pub(crate) fn name(&self) -> Result<&CStr> {
        let property_entry = self.property_entry()?;
        let name_offset = property_entry.name_offset as usize;
        Ok(self.property_info_area.cstr(name_offset))
    }

    fn data(&self) -> Result<&TrieNodeData> {
        self.property_info_area.ref_from(self.trie_node_offset)
    }

    fn property_entry(&self) -> Result<&PropertyEntry> {
        let data = self.data()?;
        self.property_info_area
            .ref_from(data.property_entry as usize)
    }

    pub(crate) fn context_index(&self) -> u32 {
        self.property_entry()
            .map(|pe| pe.context_index)
            .unwrap_or_else(|e| {
                warn!("Failed to read PropertyEntry: {e}");
                !0
            })
    }

    pub(crate) fn type_index(&self) -> u32 {
        self.property_entry()
            .map(|pe| pe.type_index)
            .unwrap_or_else(|e| {
                warn!("Failed to read PropertyEntry: {e}");
                !0
            })
    }

    pub(crate) fn num_child_nodes(&self) -> u32 {
        self.data().map(|d| d.num_child_nodes).unwrap_or_else(|e| {
            warn!(
                "Failed to read TrieNodeData at offset {}: {e}",
                self.trie_node_offset
            );
            0
        })
    }

    fn child_node(&'_ self, n: usize) -> Result<TrieNode<'_>> {
        let data = self.data()?;
        let slice = self
            .property_info_area
            .u32_slice_from(data.child_nodes as usize);
        let child_node_offset = slice.get(n).ok_or_else(|| {
            Error::FileValidation(format!(
                "Child node index {n} out of bounds: array length {}",
                slice.len()
            ))
        })?;
        Ok(TrieNode::new(
            &self.property_info_area,
            *child_node_offset as usize,
        ))
    }

    fn find_child_for_string(&'_ self, input: &str) -> Option<TrieNode<'_>> {
        // On corruption we return `Ordering::Equal`; `find` exits the binary
        // search immediately so the closure runs at most once after the flag
        // is set. `corrupted` then disqualifies the index because the
        // sorted-invariant can no longer be trusted.
        let mut corrupted = false;
        let node_index = find(self.num_child_nodes(), |i| match self.child_node(i) {
            Ok(child) => match child.name() {
                Ok(name) => match name.to_str() {
                    Ok(s) => s.cmp(input),
                    Err(e) => {
                        warn!("child node {i} has non-UTF-8 name: {e}");
                        corrupted = true;
                        Ordering::Equal
                    }
                },
                Err(e) => {
                    warn!("child node {i} name read failed: {e}");
                    corrupted = true;
                    Ordering::Equal
                }
            },
            Err(e) => {
                warn!("child node {i} read failed: {e}");
                corrupted = true;
                Ordering::Equal
            }
        });

        if corrupted {
            return None;
        }
        node_index.and_then(|i| self.child_node(i).ok())
    }

    pub(crate) fn num_prefixes(&self) -> u32 {
        self.data().map(|d| d.num_prefixes).unwrap_or_else(|e| {
            warn!(
                "Failed to read TrieNodeData at offset {}: {e}",
                self.trie_node_offset
            );
            0
        })
    }

    pub(crate) fn prefix(&self, n: usize) -> Result<&PropertyEntry> {
        let data = self.data()?;
        let slice = self
            .property_info_area
            .u32_slice_from(data.prefix_entries as usize);
        let offset = *slice.get(n).ok_or_else(|| {
            Error::FileValidation(format!(
                "Prefix index {n} out of bounds: array length {}",
                slice.len()
            ))
        })? as usize;
        self.property_info_area.ref_from(offset)
    }

    pub(crate) fn num_exact_matches(&self) -> u32 {
        self.data()
            .map(|d| d.num_exact_matches)
            .unwrap_or_else(|e| {
                warn!(
                    "Failed to read TrieNodeData at offset {}: {e}",
                    self.trie_node_offset
                );
                0
            })
    }

    pub(crate) fn exact_match(&self, n: usize) -> Result<&PropertyEntry> {
        let data = self.data()?;
        let slice = self
            .property_info_area
            .u32_slice_from(data.exact_match_entries as usize);
        let offset = *slice.get(n).ok_or_else(|| {
            Error::FileValidation(format!(
                "Exact match index {n} out of bounds: array length {}",
                slice.len()
            ))
        })? as usize;
        self.property_info_area.ref_from(offset)
    }
}

#[derive(Debug, Clone)]
pub struct PropertyInfoArea<'a> {
    data_base: &'a [u8],
}

impl<'a> PropertyInfoArea<'a> {
    pub(crate) fn new(data_base: &'a [u8]) -> Self {
        Self { data_base }
    }

    // To resolve lifetime issues, we need to clone the TrieNode.
    fn clone_trie_node(&'_ self, trie_node: &TrieNode) -> TrieNode<'_> {
        TrieNode::new(self, trie_node.trie_node_offset)
    }

    pub(crate) fn cstr(&self, offset: usize) -> &CStr {
        if offset >= self.data_base.len() {
            return c"";
        }
        match self.data_base[offset..].iter().position(|&x| x == 0) {
            Some(end) => {
                let end = end + offset + 1;
                CStr::from_bytes_with_nul(&self.data_base[offset..end])
                    .expect("null terminator verified by position search")
            }
            None => c"",
        }
    }

    #[inline]
    pub(crate) fn ref_from<T>(&self, offset: usize) -> Result<&T>
    where
        T: zerocopy::FromBytes + zerocopy::KnownLayout + zerocopy::Immutable,
    {
        let size_of = size_of::<T>();
        let end = offset.checked_add(size_of).ok_or_else(|| {
            Error::FileValidation(format!("Offset overflow: {offset} + {size_of}"))
        })?;
        let slice = self.data_base.get(offset..end).ok_or_else(|| {
            Error::FileValidation(format!(
                "Offset out of bounds: {end} > {}",
                self.data_base.len()
            ))
        })?;
        zerocopy::Ref::<&[u8], T>::from_bytes(slice)
            .map(zerocopy::Ref::into_ref)
            .map_err(|e| {
                Error::FileValidation(format!("Reference creation failed at offset {offset}: {e}"))
            })
    }

    #[inline]
    fn u32_slice_from(&self, offset: usize) -> &[u32] {
        // Check bounds first
        if offset >= self.data_base.len() {
            return &[];
        }

        let slice = &self.data_base[offset..];
        let (prefix, u32_slice, _suffix) = unsafe { slice.align_to::<u32>() };

        // Ensure proper alignment - prefix should be empty for u32-aligned data
        if !prefix.is_empty() {
            log::warn!("Data at offset {offset} is not properly aligned for u32");
            return &[];
        }

        // _suffix can contain trailing bytes which is normal
        u32_slice
    }

    #[inline]
    pub(crate) fn header(&self) -> &PropertyInfoAreaHeader {
        self.ref_from(0)
            .expect("header at offset 0; file size validated on load")
    }

    // #[inline]
    // pub fn current_version(&self) -> u32 {
    //     self.header().current_version
    // }

    // #[inline]
    // pub fn minimum_supported_version(&self) -> u32 {
    //     self.header().minimum_supported_version
    // }

    // #[inline]
    // pub fn size(&self) -> usize {
    //     self.header().size as _
    // }

    #[inline]
    pub(crate) fn num_contexts(&self) -> usize {
        self.u32_slice_from(self.header().contexts_offset as usize)
            .first()
            .copied()
            .unwrap_or(0) as _
    }

    #[cfg(feature = "builder")]
    #[inline]
    pub(crate) fn num_types(&self) -> usize {
        self.u32_slice_from(self.header().types_offset as usize)
            .first()
            .copied()
            .unwrap_or(0) as _
    }

    pub(crate) fn root_node(&'_ self) -> TrieNode<'_> {
        TrieNode::new(self, self.header().root_offset as usize)
    }

    pub(crate) fn context_offset(&self, index: usize) -> Result<usize> {
        let context_array_offset = self.header().contexts_offset as usize + size_of::<u32>();
        let slice = self.u32_slice_from(context_array_offset);
        let value = slice.get(index).ok_or_else(|| {
            Error::FileValidation(format!(
                "Context index {index} out of bounds: array length {} at offset {context_array_offset}",
                slice.len()
            ))
        })?;
        Ok(*value as _)
    }

    #[cfg(feature = "builder")]
    pub(crate) fn type_offset(&self, index: usize) -> Result<usize> {
        let type_array_offset = self.header().types_offset as usize + size_of::<u32>();
        let slice = self.u32_slice_from(type_array_offset);
        let value = slice.get(index).ok_or_else(|| {
            Error::FileValidation(format!(
                "Type index {index} out of bounds: array length {} at offset {type_array_offset}",
                slice.len()
            ))
        })?;
        Ok(*value as _)
    }

    fn check_prefix_match(
        &self,
        remaining_name: &str,
        trie_node: &TrieNode,
        context_index: &mut u32,
        type_index: &mut u32,
    ) {
        let remaining_name_size = remaining_name.len();
        for i in 0..trie_node.num_prefixes() {
            let prefix = match trie_node.prefix(i as _) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to read prefix entry {i}: {e}");
                    continue;
                }
            };
            if prefix.namelen > remaining_name_size as u32 {
                continue;
            }
            let Some(prefix_name) = entry_name_str(prefix.name(self), "Prefix", i as usize) else {
                continue;
            };
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

    pub(crate) fn get_property_info_indexes(&self, name: &str) -> (u32, u32) {
        let mut return_context_index: u32 = !0;
        let mut return_type_index: u32 = !0;
        let mut remaining_name = name;
        let mut trie_node = self.root_node();

        loop {
            if trie_node.context_index() != !0 {
                return_context_index = trie_node.context_index();
            }

            if trie_node.type_index() != !0 {
                return_type_index = trie_node.type_index();
            }

            self.check_prefix_match(
                remaining_name,
                &trie_node,
                &mut return_context_index,
                &mut return_type_index,
            );

            match remaining_name.find('.') {
                Some(index) => {
                    let segment = &remaining_name[..index];

                    match trie_node.find_child_for_string(segment) {
                        Some(node) => {
                            remaining_name = &remaining_name[index + 1..];
                            trie_node = self.clone_trie_node(&node);
                        }
                        None => {
                            break;
                        }
                    };
                }
                None => {
                    break;
                }
            }
        }

        for i in 0..trie_node.num_exact_matches() {
            let exact_match = match trie_node.exact_match(i as _) {
                Ok(em) => em,
                Err(e) => {
                    warn!("Failed to read exact_match entry {i}: {e}");
                    continue;
                }
            };
            let Some(exact_match_name) =
                entry_name_str(exact_match.name(self), "Exact match", i as usize)
            else {
                continue;
            };
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

                info!(
                    "Property '{name}' resolved: context_index={context_index}, type_index={type_index}"
                );
                return (context_index, type_index);
            }
        }

        self.check_prefix_match(
            remaining_name,
            &trie_node,
            &mut return_context_index,
            &mut return_type_index,
        );
        (return_context_index, return_type_index)
    }

    #[cfg(feature = "builder")]
    pub(crate) fn find_context_index(&self, context: &str) -> Option<usize> {
        self.find_string_index(self.num_contexts() as u32, context, "context", |i| {
            self.context_offset(i)
        })
    }

    #[cfg(feature = "builder")]
    pub(crate) fn find_type_index(&self, rtype: &str) -> Option<usize> {
        self.find_string_index(self.num_types() as u32, rtype, "type", |i| {
            self.type_offset(i)
        })
    }

    /// Shared binary search for context/type tables. Treats any entry that
    /// fails to read or is not valid UTF-8 as a corruption signal; once set,
    /// the search is short-circuited and returns `None` (the table's sorted
    /// invariant can no longer be trusted).
    #[cfg(feature = "builder")]
    fn find_string_index(
        &self,
        n: u32,
        needle: &str,
        kind: &str,
        offset_at: impl Fn(usize) -> Result<usize>,
    ) -> Option<usize> {
        // Same corruption-handling pattern as `find_child_for_string`: on
        // failure we return `Equal` (which terminates `find`'s binary
        // search) and surface the inability-to-trust-sort via `corrupted`.
        let mut corrupted = false;
        let idx = find(n, |i| {
            match offset_at(i).and_then(|off| {
                self.cstr(off)
                    .to_str()
                    .map_err(|e| Error::FileValidation(format!("{kind} entry {i} not UTF-8: {e}")))
            }) {
                Ok(s) => s.cmp(needle),
                Err(e) => {
                    warn!("{kind} entry {i} read failed: {e}");
                    corrupted = true;
                    Ordering::Equal
                }
            }
        });
        if corrupted {
            None
        } else {
            idx
        }
    }
}

pub struct PropertyInfoAreaFile {
    mmap: MemoryMap,
}

impl PropertyInfoAreaFile {
    pub(crate) fn load_default_path() -> Result<Self> {
        Self::load_path(Path::new(crate::system_properties::PROP_TREE_FILE))
    }

    pub(crate) fn load_path(path: &Path) -> Result<Self> {
        let file: File =
            File::open(path).context_with_location(format!("File open is failed in: {path:?}"))?;

        let metadata = file
            .metadata()
            .context_with_location(format!("File metadata is failed in: {path:?}"))?;

        // Validate file metadata using common utility function
        crate::errors::validate_file_metadata(
            &metadata,
            path,
            size_of::<PropertyInfoAreaHeader>() as u64,
        )?;

        Ok(Self {
            mmap: MemoryMap::new(file, metadata.len() as usize, false)?,
        })
    }

    pub(crate) fn property_info_area(&'_ self) -> PropertyInfoArea<'_> {
        PropertyInfoArea::new(
            self.mmap
                .data(0, 0, self.mmap.size())
                .expect("offset is 0. So, it must be valid."),
        )
    }
}
