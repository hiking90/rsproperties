// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::rc::Rc;

use crate::property_info_parser::*;
use crate::trie_builder::*;
use crate::trie_node_arena::TrieNodeArena;

pub(crate) struct TrieSerializer {
    arena: TrieNodeArena,
}

impl TrieSerializer {
    pub(crate) fn new(trie_builder: &TrieBuilder) -> Self {
        let mut this = Self {
            arena: TrieNodeArena::new(),
        };

        let header_offset = this.arena.allocate_object::<PropertyInfoAreaHeader>();
        {
            let header = this
                .arena
                .get_object::<PropertyInfoAreaHeader>(header_offset);
            header.current_version = 1;
            header.minimum_supported_version = 1;
        }

        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)
            .contexts_offset = this.arena.size() as _;
        this.serialize_strings(&trie_builder.contexts);

        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)
            .types_offset = this.arena.size() as _;
        this.serialize_strings(&trie_builder.types);

        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)
            .size = this.arena.size() as _;

        let root_trie_offset = this.write_trie_node(&trie_builder.root);
        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)
            .root_offset = root_trie_offset as _;

        let final_size = this.arena.size();
        this.arena
            .get_object::<PropertyInfoAreaHeader>(header_offset)
            .size = final_size as _;

        this
    }

    fn write_property_entry(&mut self, property_entry: &PropertyEntryBuilder) -> u32 {
        let context_index = match property_entry.context {
            Some(ref context) => {
                if context.is_empty() {
                    !0
                } else {
                    let index = self.arena.info().find_context_index(context);
                    index
                }
            }
            None => !0,
        };

        let type_index = match property_entry.rtype {
            Some(ref rtype) => {
                if rtype.is_empty() {
                    !0
                } else {
                    let index = self.arena.info().find_type_index(rtype);
                    index
                }
            }
            None => !0,
        };

        let entry_offset = self.arena.allocate_object::<PropertyEntry>();
        let name_offset = self.arena.allocate_and_write_string(&property_entry.name) as _;

        let entry = self.arena.get_object::<PropertyEntry>(entry_offset);
        entry.name_offset = name_offset;
        entry.namelen = property_entry.name.len() as _;
        entry.context_index = context_index as _;
        entry.type_index = type_index as _;

        entry_offset as _
    }

    fn write_trie_node(&mut self, builder_node: &TrieBuilderNode) -> u32 {
        let trie_offset = self.arena.allocate_object::<TrieNodeData>();

        let property_entry = self.write_property_entry(&builder_node.property_entry);
        self.arena
            .get_object::<TrieNodeData>(trie_offset)
            .property_entry = property_entry as _;

        // Sort prefixes by length (longest first)
        let mut sorted_prefix_matches: Vec<_> = builder_node.prefixes.iter().collect();
        sorted_prefix_matches.sort_by(|a, b| b.name.len().cmp(&a.name.len()));

        self.arena
            .get_object::<TrieNodeData>(trie_offset)
            .num_prefixes = sorted_prefix_matches.len() as _;

        let prefix_entries_array_offset = self
            .arena
            .allocate_uint32_array(sorted_prefix_matches.len());
        self.arena
            .get_object::<TrieNodeData>(trie_offset)
            .prefix_entries = prefix_entries_array_offset as _;

        for (i, prefix_entry) in sorted_prefix_matches.iter().enumerate() {
            let offset = self.write_property_entry(prefix_entry);
            self.arena.uint32_array(prefix_entries_array_offset)[i] = offset;
        }

        // Sort exact matches alphabetically
        let mut sorted_exact_matches: Vec<_> = builder_node.exact_matches.iter().collect();
        sorted_exact_matches.sort_by(|a, b| a.name.cmp(&b.name));

        self.arena
            .get_object::<TrieNodeData>(trie_offset)
            .num_exact_matches = sorted_exact_matches.len() as _;
        let exact_match_entries_array_offset =
            self.arena.allocate_uint32_array(sorted_exact_matches.len());
        self.arena
            .get_object::<TrieNodeData>(trie_offset)
            .exact_match_entries = exact_match_entries_array_offset as _;

        for (i, exact_entry) in sorted_exact_matches.iter().enumerate() {
            let offset = self.write_property_entry(exact_entry);
            self.arena.uint32_array(exact_match_entries_array_offset)[i] = offset;
        }

        // Sort children alphabetically
        let mut sorted_children: Vec<_> = builder_node.children.values().collect();
        sorted_children.sort_by(|a, b| a.property_entry.name.cmp(&b.property_entry.name));

        self.arena
            .get_object::<TrieNodeData>(trie_offset)
            .num_child_nodes = sorted_children.len() as _;
        let children_offset_array_offset = self.arena.allocate_uint32_array(sorted_children.len());
        self.arena
            .get_object::<TrieNodeData>(trie_offset)
            .child_nodes = children_offset_array_offset as _;

        for (i, child_node) in sorted_children.iter().enumerate() {
            let child_offset = self.write_trie_node(child_node);
            self.arena.uint32_array(children_offset_array_offset)[i] = child_offset;
        }

        trie_offset as _
    }

    fn serialize_strings(&mut self, strings: &BTreeSet<Rc<String>>) {
        self.arena.allocate_and_write_uint32(strings.len() as _);
        let offset_array_offset = self.arena.allocate_uint32_array(strings.len());

        for (i, string) in strings.iter().enumerate() {
            let offset = self.arena.allocate_and_write_string(string);
            self.arena.uint32_array(offset_array_offset)[i] = offset as _;
        }
    }

    pub(crate) fn take_data(&mut self) -> Vec<u8> {
        self.arena.take_data()
    }
}
