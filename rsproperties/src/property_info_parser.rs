// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{cmp::Ordering, ffi::CStr, fs::File, mem::size_of, path::Path};

use log::{trace, warn};

use zerocopy::FromBytes;
use zerocopy_derive::*;

use crate::errors::*;
use crate::property_area::ImmutableFileMap;

/// In-band "no context/type" sentinel used by the serialized format
/// (bionic uses `~0u` the same way). A `context_index`/`type_index` of
/// this value means "not set at this node — inherit from the walk so
/// far". Named so the `!0` magic doesn't scatter across comparisons.
pub(crate) const NO_INDEX: u32 = !0;

/// Binary search returning the matching index or `None` for miss. Takes
/// `usize` directly — callers hold `usize` lengths, and round-tripping
/// through `u32` would add a silent truncation point if a count's type
/// ever widened. The callback is `FnMut` so it can record out-of-band
/// signals (e.g. a corrupted entry) for the caller to inspect.
fn find<F>(len: usize, mut f: F) -> Option<usize>
where
    F: FnMut(usize) -> Ordering,
{
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

/// Decodes a trie entry name into a UTF-8 string.
///
/// Returns `None` and logs a warning for corrupt names (out-of-range
/// offset, missing NUL, non-UTF-8) so damaged entries are observable in
/// logs. An empty name is returned as is: AOSP `AddToTrie` writes an empty
/// exact entry for an exact `a..` line (shapes: `add_to_trie` in
/// trie_builder.rs). The prefix loop rejects an empty name itself.
fn entry_name_str<'a>(name: Result<&'a CStr>, kind: &str, idx: usize) -> Option<&'a str> {
    let name = match name {
        Ok(name) => name,
        Err(e) => {
            warn!("{kind} entry {idx} name read failed: {e}");
            return None;
        }
    };
    match name.to_str() {
        Ok(s) => Some(s),
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
    // `&'d CStr` tied to the *data* lifetime, not `&self` — same convention
    // as `TrieNode::name` / `cstr` / `ref_from` throughout this file: the
    // name borrows the underlying buffer, so callers may return it up the
    // stack past this entry reference.
    pub(crate) fn name<'d>(&self, property_info_area: &PropertyInfoArea<'d>) -> Result<&'d CStr> {
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

    /// Reads `context_index` and `type_index` together through a single
    /// TrieNodeData → PropertyEntry validation chain. Separate accessors
    /// would re-run `ref_from` twice each — four validations per node on
    /// the lookup hot path for two adjacent fields.
    pub(crate) fn context_and_type_indexes(&self) -> (u32, u32) {
        self.property_entry()
            .map(|pe| (pe.context_index, pe.type_index))
            .unwrap_or_else(|e| {
                warn!("Failed to read PropertyEntry: {e}");
                (NO_INDEX, NO_INDEX)
            })
    }

    /// Validated offset array of this node's children — one node-data
    /// validation + one array slice for the whole binary search, mirroring
    /// `prefix_offsets` / `exact_match_offsets`. Bounds are validated
    /// against the *declared* count, so a corrupt count field fails loudly
    /// instead of silently reinterpreting adjacent data as entries.
    fn child_offsets(&self) -> Result<&'a [u32]> {
        let data = self.data()?;
        self.property_info_area
            .u32_slice_from(data.child_nodes as usize, data.num_child_nodes as usize)
    }

    fn find_child_for_string(&self, input: &str) -> Option<TrieNode<'a>> {
        // Hoisted once for the whole search: the previous per-probe
        // `child_node(i)` re-validated the node data and re-sliced the
        // offset array on every iteration of this lookup hot path.
        let offsets = match self.child_offsets() {
            Ok(o) => o,
            Err(e) => {
                warn!("Failed to read child node offsets: {e}");
                return None;
            }
        };
        let child_at = |i: usize| TrieNode::new(self.property_info_area, offsets[i] as usize);

        // On corruption we return `Ordering::Equal`; `find` exits the binary
        // search immediately so the closure runs at most once after the flag
        // is set. `corrupted` then disqualifies the index because the
        // sorted-invariant can no longer be trusted.
        let mut corrupted = false;
        let node_index = find(offsets.len(), |i| match child_at(i).name() {
            Ok(name) => match name.to_str() {
                // AOSP `AddToTrie` writes an empty-named child for a `.a`,
                // `a..b` or prefix `a..` line (shapes: `add_to_trie` in
                // trie_builder.rs), and its serializer sorts children by
                // name, so that child is child 0. There the empty name is
                // compared like any other, as in AOSP
                // `FindChildForString`: it sorts before every non-empty
                // segment, and only an empty segment descends into it. An
                // empty name at any other index means the children are out
                // of order; disqualify the search like the other
                // corruption arms.
                Ok("") if i != 0 => {
                    warn!("child node {i} has an empty name but is not child 0 (children out of order)");
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
        });

        if corrupted {
            return None;
        }
        node_index.map(child_at)
    }

    /// Validated offset array of this node's prefix entries. Fetched once
    /// per node — the previous per-index accessor re-validated the node
    /// data and re-sliced the array on every iteration of the lookup hot
    /// path.
    fn prefix_offsets(&self) -> Result<&'a [u32]> {
        let data = self.data()?;
        self.property_info_area
            .u32_slice_from(data.prefix_entries as usize, data.num_prefixes as usize)
    }

    /// Validated offset array of this node's exact-match entries; see
    /// [`Self::prefix_offsets`].
    fn exact_match_offsets(&self) -> Result<&'a [u32]> {
        let data = self.data()?;
        self.property_info_area.u32_slice_from(
            data.exact_match_entries as usize,
            data.num_exact_matches as usize,
        )
    }

    fn entry_at(&self, offset: u32) -> Result<&'a PropertyEntry> {
        self.property_info_area.ref_from(offset as usize)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PropertyInfoArea<'a> {
    data_base: &'a [u8],
}

impl<'a> PropertyInfoArea<'a> {
    pub(crate) fn new(data_base: &'a [u8]) -> Self {
        // `header()` relies on both properties holding for every
        // construction path (mmap: page-aligned base, size validated by
        // `load_path`; builder: 4-aligned `Vec<u32>` backing, header
        // pre-allocated by `TrieNodeArena`) — assert them locally so a
        // future third path can't silently violate either.
        debug_assert!(
            data_base.len() >= size_of::<PropertyInfoAreaHeader>(),
            "property_info area smaller than its header"
        );
        debug_assert_eq!(
            data_base.as_ptr().align_offset(size_of::<u32>()),
            0,
            "property_info base not 4-byte aligned"
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

    // `&'a T`, not `&T`: like `cstr` and `u32_slice_from`, the reference
    // borrows the underlying data, not the (Copy) `self` value it was
    // reached through — callers can return it up the stack.
    #[inline]
    pub(crate) fn ref_from<T>(&self, offset: usize) -> Result<&'a T>
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
        // Both construction paths guarantee room for the header at offset 0
        // AND a 4-aligned base (asserted in `new`): the mmap path validates
        // the file size on load (`load_path`) and mmap bases are
        // page-aligned; the builder path allocates the header before
        // anything else out of `TrieNodeArena`'s `Vec<u32>` backing, whose
        // base alignment is a language-level guarantee — not an allocator
        // observation. `ref_from` re-checks both properties on the actual
        // pointer at runtime, so a violated assumption panics here rather
        // than reading garbage.
        self.ref_from(0)
            .expect("header at offset 0; size/alignment guaranteed by construction paths")
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

    /// AOSP `PropertyInfoAreaFile::LoadPath`'s checks of the header
    /// against the `size` bytes it came with: a newer format, or a size
    /// that is not the one the header records, is an error. `origin`
    /// names the bytes in the message.
    fn check_header(&self, size: usize, origin: std::fmt::Arguments<'_>) -> Result<()> {
        let header = self.header();
        if header.minimum_supported_version > 1 {
            return Err(Error::FileValidation(format!(
                "Unsupported property_info version in {origin}: minimum_supported_version={} (max supported 1)",
                header.minimum_supported_version
            )));
        }
        if header.size as usize != size {
            return Err(Error::FileValidation(format!(
                "property_info header size {} does not match file size {size} in {origin}",
                header.size
            )));
        }
        Ok(())
    }

    /// The `property_contexts` type of `name` (`"string"`, `"int"`,
    /// `"enum a b"`, ...), or `None` when no entry and no default gives it
    /// one.
    #[cfg(feature = "builder")]
    pub(crate) fn property_type(&self, name: &str) -> Result<Option<&'a str>> {
        let (_, type_index) = self.get_property_info_indexes(name);
        if type_index == NO_INDEX {
            return Ok(None);
        }
        let offset = self.type_offset(type_index as usize)?;
        Ok(Some(self.cstr(offset)?.to_str()?))
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

    /// Applies the first (longest, by serialization order) prefix entry
    /// matching `remaining_name`.
    ///
    /// Deliberately stricter than AOSP on inconsistent files: AOSP
    /// compares only `namelen` bytes (`strncmp`), trusting the field,
    /// while this port length-gates on `namelen` but matches against the
    /// full NUL-terminated name — so a corrupt `namelen < strlen(name)`
    /// *misses* here instead of matching on a silently truncated prefix.
    /// Consistent files (the builder always writes `namelen ==
    /// strlen(name)`) behave identically.
    fn check_prefix_match(
        &self,
        remaining_name: &str,
        trie_node: &TrieNode,
        context_index: &mut u32,
        type_index: &mut u32,
    ) {
        let remaining_name_size = remaining_name.len();
        // One node-data validation + one array slice for the whole loop;
        // the per-entry `ref_from` below is the only per-iteration check.
        let offsets = match trie_node.prefix_offsets() {
            Ok(o) => o,
            Err(e) => {
                warn!("Failed to read prefix entries: {e}");
                return;
            }
        };
        for (i, &entry_offset) in offsets.iter().enumerate() {
            let prefix = match trie_node.entry_at(entry_offset) {
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
            let Some(prefix_name) = entry_name_str(prefix.name(self), "Prefix", i) else {
                continue;
            };
            // AOSP `AddToTrie` never writes an empty prefix entry (a prefix
            // line without a trailing dot keeps its non-empty last piece),
            // so only a damaged file has one. AOSP `CheckPrefixMatch` would
            // match every name with it (`strncmp` of 0 bytes); skip it, as
            // 0.6.0 did.
            if prefix_name.is_empty() {
                warn!("Prefix entry {i} has an empty name");
                continue;
            }
            if remaining_name.starts_with(prefix_name) {
                if prefix.context_index != NO_INDEX {
                    *context_index = prefix.context_index;
                }
                if prefix.type_index != NO_INDEX {
                    *type_index = prefix.type_index;
                }
                return;
            }
        }
    }

    pub(crate) fn get_property_info_indexes(&self, name: &str) -> (u32, u32) {
        let mut return_context_index: u32 = NO_INDEX;
        let mut return_type_index: u32 = NO_INDEX;
        let mut remaining_name = name;
        let mut trie_node = self.root_node();

        loop {
            // Single TrieNodeData → PropertyEntry validation per node for
            // both indexes — separate accessors would double the per-level
            // cost of this lookup hot path.
            let (context_index, type_index) = trie_node.context_and_type_indexes();
            if context_index != NO_INDEX {
                return_context_index = context_index;
            }
            if type_index != NO_INDEX {
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

        // One node-data validation + one array slice for the whole loop, as
        // in `check_prefix_match`.
        let exact_offsets = match trie_node.exact_match_offsets() {
            Ok(o) => o,
            Err(e) => {
                warn!("Failed to read exact_match entries: {e}");
                &[][..]
            }
        };
        for (i, &entry_offset) in exact_offsets.iter().enumerate() {
            let exact_match = match trie_node.entry_at(entry_offset) {
                Ok(em) => em,
                Err(e) => {
                    warn!("Failed to read exact_match entry {i}: {e}");
                    continue;
                }
            };
            let Some(exact_match_name) = entry_name_str(exact_match.name(self), "Exact match", i)
            else {
                continue;
            };
            // An empty name (AOSP: an exact `a..` line, entry 0 of node
            // `a`) is compared like any other, as AOSP
            // `GetPropertyInfoIndexes` does with `strcmp`: it matches only
            // an empty `remaining_name`, i.e. a name ending in `.`.
            if exact_match_name == remaining_name {
                let context_index = if exact_match.context_index != NO_INDEX {
                    exact_match.context_index
                } else {
                    return_context_index
                };

                let type_index = if exact_match.type_index != NO_INDEX {
                    exact_match.type_index
                } else {
                    return_type_index
                };

                // `trace!`, not `info!`: this fires on every successful
                // lookup on the property-get hot path.
                trace!(
                    "Property {name:?} resolved: context_index={context_index}, type_index={type_index}"
                );
                return (context_index, type_index);
            }
        }

        // AOSP parity: upstream re-runs the prefix scan here even though
        // the loop's final iteration already covered this node with the
        // same arguments (same first match, same overwrite) — kept for
        // behavioral parity with the reference walk, not because it can
        // change the result.
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
        self.find_string_index(self.num_contexts(), context, "context", |i| {
            self.context_offset(i)
        })
    }

    #[cfg(feature = "builder")]
    pub(crate) fn find_type_index(&self, rtype: &str) -> Option<usize> {
        self.find_string_index(self.num_types(), rtype, "type", |i| self.type_offset(i))
    }

    /// Shared binary search for context/type tables. Treats any entry that
    /// fails to read or is not valid UTF-8 as a corruption signal; once set,
    /// the search is short-circuited and returns `None` (the table's sorted
    /// invariant can no longer be trusted).
    #[cfg(feature = "builder")]
    fn find_string_index(
        &self,
        n: usize,
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

/// `property_info`, mapped read-only and shared on every target, like
/// bionic. Sound only while writers follow \[IMM\] (property_area.rs).
pub(crate) struct PropertyInfoAreaFile {
    mmap: ImmutableFileMap,
}

impl PropertyInfoAreaFile {
    pub(crate) fn load_default_path() -> Result<Self> {
        Self::load_path(Path::new(crate::system_properties::PROP_TREE_FILE))
    }

    pub(crate) fn load_path(path: &Path) -> Result<Self> {
        // `O_NOFOLLOW` like AOSP `PropertyInfoAreaFile::LoadPath`: a
        // symlink as the last path component fails with `ELOOP` instead of
        // mapping the file it points to. std adds `O_CLOEXEC`.
        use std::os::unix::fs::OpenOptionsExt;
        let file: File = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as _)
            .open(path)
            .context_with_location(format!("File open is failed in: {path:?}"))?;

        let metadata = file
            .metadata()
            .context_with_location(format!("File metadata is failed in: {path:?}"))?;

        // Validate file metadata using common utility function
        crate::file_validation::validate_file_metadata(
            &metadata,
            path,
            size_of::<PropertyInfoAreaHeader>() as u64,
        )?;
        // [IMM]: a file its producer left owner-writable may be rewritten
        // in place while mapped.
        crate::file_validation::check_owner_write(
            &metadata,
            path,
            cfg!(feature = "strict-file-validation"),
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

        let this = Self {
            mmap: ImmutableFileMap::open(file, size)?,
        };

        // AOSP parity (`PropertyInfoAreaFile::LoadPath`): reject files this
        // parser cannot be trusted to interpret. Without the version gate a
        // future-format file parses "successfully" into garbage lookups;
        // without the size cross-check a truncated or concatenated file
        // degrades into per-lookup warnings instead of one load-time error.
        this.property_info_area()
            .check_header(size, format_args!("{path:?}"))?;

        Ok(this)
    }

    pub(crate) fn property_info_area(&'_ self) -> PropertyInfoArea<'_> {
        PropertyInfoArea::new(self.mmap.bytes())
    }
}

/// The `property_contexts` type of `name` (`"string"`, `"int"`,
/// `"enum a b"`, ...) in `trie`, the bytes [`crate::build_trie`] returns,
/// or `None` when no entry and no default gives it one.
///
/// This is the lookup [`crate::SystemProperties::property_type`] does on a
/// published `property_info`, for a trie that is not published yet: AOSP
/// init checks each build.prop value against its type while it reads the
/// files, and a service that reads its input files before it writes to the
/// properties dir has only the bytes at that point (see
/// [`crate::load_properties_from_file_with`]).
///
/// `trie` may start at any address. The lookup reads 4-byte aligned
/// records in place, so bytes that are not 4-byte aligned are copied
/// first, once per call. A `Vec<u8>` from the global allocator is aligned
/// on the supported targets (glibc and bionic `malloc` return 8- or
/// 16-byte aligned blocks), so `build_trie`'s result is read in place.
///
/// # Errors
///
/// `Error::FileValidation` when `trie` is shorter than its header, names a
/// format newer than this crate reads, or records a size other than
/// `trie.len()`; the errors of the lookup itself when an offset inside
/// `trie` is out of range.
#[cfg(feature = "builder")]
pub fn property_type_in_trie(trie: &[u8], name: &str) -> Result<Option<String>> {
    use zerocopy::IntoBytes;

    if trie.len() < size_of::<PropertyInfoAreaHeader>() {
        return Err(Error::FileValidation(format!(
            "property_info trie of {} bytes is smaller than its header ({} bytes)",
            trie.len(),
            size_of::<PropertyInfoAreaHeader>()
        )));
    }
    let lookup = |bytes: &[u8]| -> Result<Option<String>> {
        let area = PropertyInfoArea::new(bytes);
        area.check_header(bytes.len(), format_args!("the trie bytes"))?;
        Ok(area.property_type(name)?.map(str::to_owned))
    };
    if trie.as_ptr().align_offset(size_of::<u32>()) == 0 {
        return lookup(trie);
    }
    // `Vec<u32>` backing: its 4-byte alignment is a language guarantee.
    let mut words = vec![0u32; trie.len().div_ceil(size_of::<u32>())];
    words.as_mut_bytes()[..trie.len()].copy_from_slice(trie);
    lookup(&words.as_bytes()[..trie.len()])
}

#[cfg(all(test, feature = "builder", not(target_os = "android")))]
mod tests {
    use super::*;
    use crate::property_info_serializer::{build_trie, PropertyInfoEntry};
    use std::os::unix::fs::OpenOptionsExt;

    /// Removes the dir on drop; made before the dir is created, so a
    /// failing assert still removes it.
    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// AOSP opens `property_info` with `O_NOFOLLOW`: a symlink as the last
    /// path component is refused even when it points to a valid file.
    #[test]
    fn load_path_refuses_symlinked_property_info() {
        let guard = TempDir(
            std::env::temp_dir().join(format!("rsprops_pinfo_link_{}", std::process::id())),
        );
        let dir = &guard.0;
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();

        let entries = vec![PropertyInfoEntry::prefix(
            "test.".to_owned(),
            "u:object_r:test_prop:s0".to_owned(),
            "string",
        )
        .unwrap()];
        let trie = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();
        let target = dir.join("property_info.real");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&target)
            .and_then(|mut f| std::io::Write::write_all(&mut f, &trie))
            .unwrap();
        let link = dir.join("property_info");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        PropertyInfoAreaFile::load_path(&target).expect("the target itself loads");
        let err = match PropertyInfoAreaFile::load_path(&link) {
            Ok(_) => panic!("symlinked property_info must not be opened"),
            Err(e) => e,
        };
        let Error::Context { source, .. } = &err else {
            panic!("expected Context, got {err:?}");
        };
        match &**source {
            Error::Io(io) => assert_eq!(
                io.raw_os_error(),
                Some(rustix::io::Errno::LOOP.raw_os_error()),
                "{err}"
            ),
            other => panic!("expected Io(ELOOP), got {other:?}"),
        }
    }

    /// `property_type_in_trie` reads `build_trie`'s bytes as
    /// `SystemProperties::property_type` reads the published file, at any
    /// alignment of the slice, and refuses bytes whose length is not the
    /// one the header records.
    #[test]
    fn property_type_in_trie_reads_unpublished_bytes() {
        let entries = vec![
            PropertyInfoEntry::prefix("typed.", "u:object_r:typed_prop:s0", "string").unwrap(),
            PropertyInfoEntry::exact("typed.int", "u:object_r:typed_prop:s0", "int").unwrap(),
            PropertyInfoEntry::exact("typed.enum", "u:object_r:typed_prop:s0", "enum a b").unwrap(),
        ];
        let trie = build_trie(&entries, "u:object_r:default_prop:s0", "double").unwrap();
        let cases = [
            ("typed.int", "int"),
            ("typed.enum", "enum a b"),
            ("typed.other", "string"),
            // No entry: the default type.
            ("other.name", "double"),
        ];
        for (name, expected) in cases {
            assert_eq!(
                property_type_in_trie(&trie, name).unwrap().as_deref(),
                Some(expected),
                "{name}"
            );
        }

        // Four consecutive start addresses: at most one is 4-byte aligned,
        // so the copying path runs for at least three of them.
        let mut shifted = vec![0u8; trie.len() + 3];
        for start in 0..4 {
            shifted[start..start + trie.len()].copy_from_slice(&trie);
            let bytes = &shifted[start..start + trie.len()];
            for (name, expected) in cases {
                assert_eq!(
                    property_type_in_trie(bytes, name).unwrap().as_deref(),
                    Some(expected),
                    "{name} at +{start}"
                );
            }
        }

        for bad in [&trie[..4], &trie[..trie.len() - 4]] {
            assert!(
                matches!(
                    property_type_in_trie(bad, "typed.int"),
                    Err(Error::FileValidation(_))
                ),
                "{} bytes",
                bad.len()
            );
        }
    }

    /// AOSP `AddToTrie` shapes that `add_to_trie` rejects: an empty-named
    /// child of `a` (prefix `a..`) and an empty exact entry in `a` (exact
    /// `a..`). Lookups resolve them like AOSP `GetPropertyInfoIndexes`; an
    /// empty name that is not child 0 disqualifies the child search.
    #[test]
    fn empty_names_resolve_like_aosp() {
        use crate::trie_builder::{PropertyEntryBuilder, TrieBuilder, TrieBuilderNode};
        use crate::trie_serializer::TrieSerializer;
        use std::collections::{HashMap, HashSet};
        use std::rc::Rc;
        use zerocopy::IntoBytes;

        const A: &str = "u:object_r:a_prop:s0";
        const Z: &str = "u:object_r:z_prop:s0";
        const EMPTY_CHILD: &str = "u:object_r:empty_child_prop:s0";
        const EMPTY_EXACT: &str = "u:object_r:empty_exact_prop:s0";

        fn context_of<'a>(area: &PropertyInfoArea<'a>, name: &str) -> &'a str {
            let (index, _) = area.get_property_info_indexes(name);
            let offset = area.context_offset(index as usize).unwrap();
            area.cstr(offset).unwrap().to_str().unwrap()
        }

        let mut trie = TrieBuilder::new("u:object_r:default_prop:s0", "string");
        trie.add_to_trie("a.", A, "string", false).unwrap();
        trie.add_to_trie("a.m.", A, "string", false).unwrap();
        trie.add_to_trie("a.z.", Z, "string", false).unwrap();
        for context in [EMPTY_CHILD, EMPTY_EXACT] {
            trie.contexts.insert(Rc::from(context));
        }
        let entry = |name: &str, context: &str| PropertyEntryBuilder {
            name: Rc::from(name),
            context: Some(Rc::from(context)),
            rtype: Some(Rc::from("string")),
        };
        let a = trie.root.children.get_mut("a").unwrap();
        a.children.insert(
            Rc::from(""),
            TrieBuilderNode {
                property_entry: entry("", EMPTY_CHILD),
                prefixes: HashSet::new(),
                exact_matches: HashSet::new(),
                children: HashMap::new(),
            },
        );
        a.exact_matches.insert(entry("", EMPTY_EXACT));

        let bytes = TrieSerializer::new(&trie).unwrap().into_data();
        let mut words = vec![0u32; bytes.len() / 4];
        words.as_mut_bytes().copy_from_slice(&bytes);

        let children_of_a = {
            let area = PropertyInfoArea::new(words.as_bytes());
            // Only an empty segment descends into the empty-named child.
            assert_eq!(context_of(&area, "a..b"), EMPTY_CHILD);
            // Only a name ending in `.` matches the empty exact entry.
            assert_eq!(context_of(&area, "a."), EMPTY_EXACT);
            // Legal names: `b` probes the empty-named child 0 and misses.
            assert_eq!(context_of(&area, "a.b"), A);
            assert_eq!(context_of(&area, "a.b.c"), A);
            assert_eq!(context_of(&area, "a.z.q"), Z);
            let a = area.root_node().find_child_for_string("a").unwrap();
            a.data().unwrap().child_nodes as usize / 4
        };

        // Children of `a` reordered to `m`, ``, `z`: the search for `z`
        // probes child 1 first and stops there (AOSP would go on to `z`).
        words.swap(children_of_a, children_of_a + 1);
        let area = PropertyInfoArea::new(words.as_bytes());
        assert_eq!(context_of(&area, "a.z.q"), A);
    }
}
