// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{cmp::Ordering, ffi::CStr, fs::File, mem::size_of, path::Path};

use log::{trace, warn};

use zerocopy::FromBytes;
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
/// Returns `None` and logs a warning for corrupt names (out-of-range
/// offset, missing NUL, non-UTF-8) so damaged entries are observable in
/// logs; an empty name is skipped as well — no valid trie entry has one.
fn entry_name_str<'a>(name: Result<&'a CStr>, kind: &str, idx: usize) -> Option<&'a str> {
    let name = match name {
        Ok(name) => name,
        Err(e) => {
            warn!("{kind} entry {idx} name read failed: {e}");
            return None;
        }
    };
    match name.to_str() {
        Ok(s) if !s.is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            warn!("{kind} entry {idx} has non-UTF-8 name: {e}");
            None
        }
    }
}

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug)]
#[repr(C, align(4))]
pub(crate) struct PropertyEntry {
    pub(crate) name_offset: u32,
    pub(crate) namelen: u32,
    pub(crate) context_index: u32,
    pub(crate) type_index: u32,
}

impl PropertyEntry {
    pub(crate) fn name<'a>(&'a self, property_info_area: &'a PropertyInfoArea) -> Result<&'a CStr> {
        property_info_area.cstr(self.name_offset as usize)
    }
}

#[derive(FromBytes, IntoBytes, KnownLayout, Debug, Immutable)]
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

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug)]
#[repr(C, align(4))]
pub(crate) struct PropertyInfoAreaHeader {
    pub(crate) current_version: u32,
    pub(crate) minimum_supported_version: u32,
    pub(crate) size: u32,
    pub(crate) contexts_offset: u32,
    pub(crate) types_offset: u32,
    pub(crate) root_offset: u32,
}

#[derive(Debug)]
pub(crate) struct TrieNode<'a> {
    property_info_area: PropertyInfoArea<'a>,
    trie_node_offset: usize,
}

impl<'a> TrieNode<'a> {
    // Takes the (Copy) area by value so the returned node borrows the
    // underlying *data* (`'a`), not the `&self` reference it was created
    // through — child nodes can then be returned up the call stack without
    // re-wrapping.
    fn new(property_info_area: PropertyInfoArea<'a>, trie_node_offset: usize) -> Self {
        Self {
            property_info_area,
            trie_node_offset,
        }
    }

    // `&'a CStr`, not `&CStr`: like `child_node`, the name borrows the
    // underlying data, not this node value.
    pub(crate) fn name(&self) -> Result<&'a CStr> {
        let property_entry = self.property_entry()?;
        let name_offset = property_entry.name_offset as usize;
        self.property_info_area.cstr(name_offset)
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

    fn child_node(&self, n: usize) -> Result<TrieNode<'a>> {
        let data = self.data()?;
        let offset =
            self.entry_offset_at(data.child_nodes, data.num_child_nodes, n, "Child node")?;
        Ok(TrieNode::new(self.property_info_area, offset))
    }

    /// Reads the `n`-th offset out of a `count`-sized u32 offset array at
    /// `array_offset` — the shared skeleton of `child_node` / `prefix` /
    /// `exact_match`. Bounds are validated against the *declared* count, so
    /// a corrupt count field fails loudly instead of silently reinterpreting
    /// adjacent data as entries.
    fn entry_offset_at(
        &self,
        array_offset: u32,
        count: u32,
        n: usize,
        kind: &str,
    ) -> Result<usize> {
        let slice = self
            .property_info_area
            .u32_slice_from(array_offset as usize, count as usize)?;
        let offset = slice.get(n).ok_or_else(|| {
            Error::FileValidation(format!(
                "{kind} index {n} out of bounds: array length {}",
                slice.len()
            ))
        })?;
        Ok(*offset as usize)
    }

    fn find_child_for_string(&self, input: &str) -> Option<TrieNode<'a>> {
        // On corruption we return `Ordering::Equal`; `find` exits the binary
        // search immediately so the closure runs at most once after the flag
        // is set. `corrupted` then disqualifies the index because the
        // sorted-invariant can no longer be trusted.
        let mut corrupted = false;
        let node_index = find(self.num_child_nodes(), |i| match self.child_node(i) {
            Ok(child) => match child.name() {
                Ok(name) => match name.to_str() {
                    // No valid trie node has an empty name — treat it as
                    // corruption and disqualify the search like the other
                    // corruption arms (otherwise `"".cmp(input) == Less`
                    // silently steers the binary search with a broken sort
                    // invariant).
                    Ok("") => {
                        warn!("child node {i} has empty (corruption-fallback) name");
                        corrupted = true;
                        Ordering::Equal
                    }
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
        let offset = self.entry_offset_at(data.prefix_entries, data.num_prefixes, n, "Prefix")?;
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
        let offset = self.entry_offset_at(
            data.exact_match_entries,
            data.num_exact_matches,
            n,
            "Exact match",
        )?;
        self.property_info_area.ref_from(offset)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PropertyInfoArea<'a> {
    data_base: &'a [u8],
}

impl<'a> PropertyInfoArea<'a> {
    pub(crate) fn new(data_base: &'a [u8]) -> Self {
        // `header()` relies on this holding for both construction paths
        // (mmap: validated by `load_path`; builder: header pre-allocated by
        // `TrieNodeArena`) — assert it locally so a future third path can't
        // silently violate it.
        debug_assert!(
            data_base.len() >= size_of::<PropertyInfoAreaHeader>(),
            "property_info area smaller than its header"
        );
        Self { data_base }
    }

    /// NUL-terminated string at `offset`. Corruption (out-of-range offset,
    /// missing NUL terminator) is a typed error, *not* an in-band `c""` —
    /// an empty string is valid data and must stay distinguishable from a
    /// damaged file.
    pub(crate) fn cstr(&self, offset: usize) -> Result<&'a CStr> {
        let tail = self.data_base.get(offset..).ok_or_else(|| {
            Error::FileValidation(format!(
                "string offset {offset} out of bounds ({} bytes)",
                self.data_base.len()
            ))
        })?;
        CStr::from_bytes_until_nul(tail)
            .map_err(|_| Error::FileValidation(format!("no NUL terminator after offset {offset}")))
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

    /// `len`-element u32 array at `offset`. The caller passes the *declared*
    /// element count (from a header/node field) so a corrupt count larger
    /// than the file fails here as a validation error — slicing "to the end
    /// of the buffer" instead would silently reinterpret adjacent unrelated
    /// data as entries.
    ///
    /// zerocopy rather than `align_to`: `align_to`'s middle-slice length is
    /// documented as a performance property, not a correctness guarantee
    /// ("it is permissible for all of the input data to be returned as the
    /// prefix"), while `ref_from_bytes` *contractually* fails on
    /// misalignment and returns exactly `byte_len / 4` elements.
    #[inline]
    fn u32_slice_from(&self, offset: usize, len: usize) -> Result<&'a [u32]> {
        let byte_len = len.checked_mul(size_of::<u32>()).ok_or_else(|| {
            Error::FileValidation(format!(
                "u32 array length overflow: {len} at offset {offset}"
            ))
        })?;
        let end = offset.checked_add(byte_len).ok_or_else(|| {
            Error::FileValidation(format!("u32 array end overflow: {offset} + {byte_len}"))
        })?;
        let slice = self.data_base.get(offset..end).ok_or_else(|| {
            Error::FileValidation(format!(
                "u32 array out of bounds: {offset}..{end} > {}",
                self.data_base.len()
            ))
        })?;
        <[u32]>::ref_from_bytes(slice).map_err(|_| {
            Error::FileValidation(format!(
                "u32 array at offset {offset} is not 4-byte aligned"
            ))
        })
    }

    #[inline]
    pub(crate) fn header(&self) -> &PropertyInfoAreaHeader {
        // Both construction paths guarantee room and alignment for the
        // header at offset 0: the mmap path validates the file size on
        // load (`load_path`), and the builder path (`TrieNodeArena::info`)
        // allocates the header before anything else (asserted in `new`).
        // `ref_from` re-checks both at runtime, so a violated invariant
        // panics here rather than reading garbage.
        self.ref_from(0)
            .expect("header at offset 0; guaranteed by load-time validation / arena layout")
    }

    /// Element count stored at the head of the u32 table at `table_offset`
    /// (contexts/types tables both lead with their count). Corruption reads
    /// as 0 — every consumer treats "no entries" as the safe degenerate —
    /// but is logged so a damaged table doesn't silently look empty.
    #[inline]
    fn table_count(&self, table_offset: u32) -> usize {
        match self.u32_slice_from(table_offset as usize, 1) {
            Ok(s) => s.first().copied().unwrap_or(0) as _,
            Err(e) => {
                warn!("table count read failed at offset {table_offset}: {e}");
                0
            }
        }
    }

    #[inline]
    pub(crate) fn num_contexts(&self) -> usize {
        self.table_count(self.header().contexts_offset)
    }

    #[cfg(feature = "builder")]
    #[inline]
    pub(crate) fn num_types(&self) -> usize {
        self.table_count(self.header().types_offset)
    }

    pub(crate) fn root_node(&self) -> TrieNode<'a> {
        TrieNode::new(*self, self.header().root_offset as usize)
    }

    pub(crate) fn context_offset(&self, index: usize) -> Result<usize> {
        // `contexts_offset` is untrusted file data — checked arithmetic so
        // a corrupt value can't overflow (a debug-build panic on 32-bit).
        let context_array_offset = (self.header().contexts_offset as usize)
            .checked_add(size_of::<u32>())
            .ok_or_else(|| {
                Error::FileValidation(format!(
                    "contexts_offset overflow: {}",
                    self.header().contexts_offset
                ))
            })?;
        let slice = self.u32_slice_from(context_array_offset, self.num_contexts())?;
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
        // See `context_offset`: untrusted offset, checked arithmetic.
        let type_array_offset = (self.header().types_offset as usize)
            .checked_add(size_of::<u32>())
            .ok_or_else(|| {
                Error::FileValidation(format!(
                    "types_offset overflow: {}",
                    self.header().types_offset
                ))
            })?;
        let slice = self.u32_slice_from(type_array_offset, self.num_types())?;
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
            // Widen the untrusted field instead of truncating the local
            // length with `as u32`.
            if prefix.namelen as usize > remaining_name_size {
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
            // Cache each index: every accessor call re-validates the whole
            // TrieNodeData → PropertyEntry chain (`ref_from` twice), so
            // calling them twice per field doubled the per-level cost of
            // this lookup hot path.
            let context_index = trie_node.context_index();
            if context_index != !0 {
                return_context_index = context_index;
            }

            let type_index = trie_node.type_index();
            if type_index != !0 {
                return_type_index = type_index;
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
                            trie_node = node;
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

                // `trace!`, not `info!`: this fires on every successful
                // lookup on the property-get hot path.
                trace!(
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
            match offset_at(i).and_then(|off| self.cstr(off)).and_then(|s| {
                s.to_str()
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

pub(crate) struct PropertyInfoAreaFile {
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

        // `metadata.len()` is u64; a plain `as usize` would truncate on
        // 32-bit targets, letting a 2^32 + k byte file pass the minimum-
        // size validation above but map only k bytes — turning `header()`'s
        // validated-on-load invariant into a reachable panic.
        let size = usize::try_from(metadata.len()).map_err(|_| {
            Error::FileValidation(format!(
                "File too large to map on this platform: {} bytes in {path:?}",
                metadata.len()
            ))
        })?;

        Ok(Self {
            mmap: MemoryMap::new(file, size, false)?,
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
