// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::rc::Rc;

use crate::errors::{Error, Result};
use crate::property_info_parser::*;
use crate::trie_builder::*;
use crate::trie_node_arena::TrieNodeArena;

pub(crate) struct TrieSerializer {
    arena: TrieNodeArena,
}

/// Resolves an optional context/type name to a u32 index. Absent or empty
/// values map to the `u32::MAX` "no context" sentinel; a *lookup miss* for a
/// present name is an error — `TrieBuilder` inserts every context/type into
/// the string tables before serialization, so a miss means the serializer's
/// own invariant broke and folding it into the sentinel would silently
/// write a corrupt file.
fn resolve_index<F>(name: Option<&str>, mut lookup: F) -> Result<u32>
where
    F: FnMut(&str) -> Option<usize>,
{
    match name {
        Some(s) if !s.is_empty() => match lookup(s) {
            // `filter` on top of `try_from`: an index of exactly
            // `u32::MAX` would collide with the "no context" sentinel and
            // silently demote a real context on the reader side.
            Some(i) => u32::try_from(i)
                .ok()
                .filter(|&v| v != u32::MAX)
                .ok_or_else(|| {
                    Error::FileValidation(format!(
                        "String table index {i} collides with the u32::MAX sentinel"
                    ))
                }),
            None => Err(Error::FileValidation(format!(
                "'{s}' missing from the serialized string table (serializer invariant violation)"
            ))),
        },
        _ => Ok(u32::MAX),
    }
}

impl TrieSerializer {
    pub(crate) fn new(trie_builder: &TrieBuilder) -> Result<Self> {
        let mut this = Self {
            arena: TrieNodeArena::new(),
        };

        let header_offset = this.arena.allocate_object::<PropertyInfoAreaHeader>();
        {
            let header = this
                .arena
                .get_object::<PropertyInfoAreaHeader>(header_offset)?;
            header.current_version = 1;
            header.minimum_supported_version = 1;
        }

        // `arena.size()` is the running write position of the in-memory
        // `Vec<u8>` arena. The on-disk format stores offsets as `u32`;
        // arena offsets grow monotonically, so the single `u32::try_from`
        // on the *final* size below retroactively validates every
        // intermediate `as u32` cast in this module — if the total fits,
        // all earlier offsets did too.
        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)?
            .contexts_offset = this.arena.size() as u32;
        this.serialize_strings(&trie_builder.contexts)?;

        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)?
            .types_offset = this.arena.size() as u32;
        this.serialize_strings(&trie_builder.types)?;

        // AOSP parity: upstream stamps an intermediate `size` here because
        // its Find*Offset helpers consult it during trie writing. This
        // port's `find_context_index`/`find_type_index` never read
        // `header.size`, and the value is unconditionally overwritten with
        // the final size below — kept only to match the reference
        // serializer's write sequence.
        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)?
            .size = this.arena.size() as u32;

        let root_trie_offset = this.write_trie_node(&trie_builder.root, 0)?;
        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)?
            .root_offset = root_trie_offset;

        let final_size = this.arena.size();
        let final_size_u32 = u32::try_from(final_size).map_err(|_| {
            Error::FileValidation(format!(
                "Serialized property info exceeds the u32 offset space: {final_size} bytes"
            ))
        })?;
        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)?
            .size = final_size_u32;

        Ok(this)
    }

    fn write_property_entry(&mut self, property_entry: &PropertyEntryBuilder) -> Result<u32> {
        let context_index = resolve_index(property_entry.context.as_deref(), |s| {
            self.arena.info().find_context_index(s)
        })?;
        let type_index = resolve_index(property_entry.rtype.as_deref(), |s| {
            self.arena.info().find_type_index(s)
        })?;

        let entry_offset = self.arena.allocate_object::<PropertyEntry>();
        let name_offset = self.arena.allocate_and_write_string(&property_entry.name) as u32;
        let namelen = u32::try_from(property_entry.name.len()).map_err(|_| {
            Error::FileValidation(format!(
                "Property name too long: {} bytes",
                property_entry.name.len()
            ))
        })?;

        let entry = self.arena.get_object::<PropertyEntry>(entry_offset)?;
        entry.name_offset = name_offset;
        entry.namelen = namelen;
        entry.context_index = context_index;
        entry.type_index = type_index;

        Ok(entry_offset as u32)
    }

    fn write_trie_node(&mut self, builder_node: &TrieBuilderNode, depth: usize) -> Result<u32> {
        // Defense-in-depth alongside `MAX_NAME_SEGMENTS` in `TrieBuilder`:
        // this function recurses once per trie level, so an unbounded
        // depth would overflow the stack instead of returning an error.
        const MAX_TRIE_DEPTH: usize = 512;
        if depth > MAX_TRIE_DEPTH {
            return Err(Error::FileValidation(format!(
                "Trie deeper than {MAX_TRIE_DEPTH} levels — refusing to serialize"
            )));
        }
        let trie_offset = self.arena.allocate_object::<TrieNodeData>();

        let property_entry = self.write_property_entry(&builder_node.property_entry)?;
        self.arena
            .get_object::<TrieNodeData>(trie_offset)?
            .property_entry = property_entry;

        // Sort prefixes by length (longest first), tie-breaking equal
        // lengths by name: `prefixes` is a HashSet, so without the
        // tie-breaker the serialized byte output would vary run to run,
        // breaking reproducible builds. (AOSP's own tie order is likewise
        // unspecified — it length-sorts with an unstable std::sort — so
        // this makes *this* serializer deterministic rather than matching
        // AOSP's ties byte-for-byte.) Lookup semantics are unaffected —
        // two distinct equal-length prefixes can never both match one
        // name.
        let mut sorted_prefix_matches: Vec<_> = builder_node.prefixes.iter().collect();
        sorted_prefix_matches.sort_by(|a, b| {
            b.name
                .len()
                .cmp(&a.name.len())
                .then_with(|| a.name.cmp(&b.name))
        });

        // Counts are bounded by trie input size (≤ a few thousand entries per
        // pixel build), well below u32::MAX.
        self.arena
            .get_object::<TrieNodeData>(trie_offset)?
            .num_prefixes = sorted_prefix_matches.len() as u32;

        let prefix_entries_array_offset = self
            .arena
            .allocate_uint32_array(sorted_prefix_matches.len());
        self.arena
            .get_object::<TrieNodeData>(trie_offset)?
            .prefix_entries = prefix_entries_array_offset as u32;

        let prefix_count = sorted_prefix_matches.len();
        for (i, prefix_entry) in sorted_prefix_matches.iter().enumerate() {
            let offset = self.write_property_entry(prefix_entry)?;
            self.arena
                .uint32_array(prefix_entries_array_offset, prefix_count)?[i] = offset;
        }

        // Sort exact matches alphabetically
        let mut sorted_exact_matches: Vec<_> = builder_node.exact_matches.iter().collect();
        sorted_exact_matches.sort_by(|a, b| a.name.cmp(&b.name));

        self.arena
            .get_object::<TrieNodeData>(trie_offset)?
            .num_exact_matches = sorted_exact_matches.len() as u32;
        let exact_match_entries_array_offset =
            self.arena.allocate_uint32_array(sorted_exact_matches.len());
        self.arena
            .get_object::<TrieNodeData>(trie_offset)?
            .exact_match_entries = exact_match_entries_array_offset as u32;

        let exact_count = sorted_exact_matches.len();
        for (i, exact_entry) in sorted_exact_matches.iter().enumerate() {
            let offset = self.write_property_entry(exact_entry)?;
            self.arena
                .uint32_array(exact_match_entries_array_offset, exact_count)?[i] = offset;
        }

        // Sort children alphabetically
        let mut sorted_children: Vec<_> = builder_node.children.values().collect();
        sorted_children.sort_by(|a, b| a.property_entry.name.cmp(&b.property_entry.name));

        self.arena
            .get_object::<TrieNodeData>(trie_offset)?
            .num_child_nodes = sorted_children.len() as u32;
        let children_offset_array_offset = self.arena.allocate_uint32_array(sorted_children.len());
        self.arena
            .get_object::<TrieNodeData>(trie_offset)?
            .child_nodes = children_offset_array_offset as u32;

        let children_count = sorted_children.len();
        for (i, child_node) in sorted_children.iter().enumerate() {
            let child_offset = self.write_trie_node(child_node, depth + 1)?;
            self.arena
                .uint32_array(children_offset_array_offset, children_count)?[i] = child_offset;
        }

        Ok(trie_offset as u32)
    }

    fn serialize_strings(&mut self, strings: &BTreeSet<Rc<str>>) -> Result<()> {
        self.arena.allocate_and_write_uint32(strings.len() as u32);
        let n = strings.len();
        let offset_array_offset = self.arena.allocate_uint32_array(n);

        for (i, string) in strings.iter().enumerate() {
            let offset = self.arena.allocate_and_write_string(string);
            self.arena.uint32_array(offset_array_offset, n)?[i] = offset as u32;
        }

        Ok(())
    }

    pub(crate) fn into_data(self) -> Vec<u8> {
        self.arena.into_data()
    }
}
