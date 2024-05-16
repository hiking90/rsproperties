// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet, BTreeSet};
use std::rc::Rc;

use crate::trie_node_arena::TrieNodeArena;
use crate::trie_builder::*;
use crate::property_info_parser::*;

pub(crate) struct TrieSerializer {
    arena: TrieNodeArena,
}

impl TrieSerializer {
    pub(crate) fn new(trie_builder: &TrieBuilder) -> Self {
        let mut this = Self {
            arena: TrieNodeArena::new(),
        };

        let header = this.arena.allocate_object::<PropertyInfoAreaHeader>();
        header.as_mut().current_version = 1;
        header.as_mut().minimum_supported_version = 1;

        header.as_mut().contexts_offset = this.arena.size() as _;
        this.serialize_strings(&trie_builder.contexts);

        header.as_mut().types_offset = this.arena.size() as _;
        this.serialize_strings(&trie_builder.types);

        header.as_mut().size = this.arena.size() as _;

        let root_trie_offset = this.write_trie_node(&trie_builder.root);
        header.as_mut().root_offset = root_trie_offset as _;

        header.as_mut().size = this.arena.size() as _;

        this
    }

    fn write_property_entry(&mut self, property_entry: &PropertyEntryBuilder) -> u32 {
        let context_index = match property_entry.context {
            Some(ref context) => {
                if context.is_empty() {
                    !0
                } else {
                    self.arena.info().find_context_index(context)
                }
            }
            None => !0,
        };

        let type_index = match property_entry.rtype {
            Some(ref rtype) => {
                if rtype.is_empty() {
                    !0
                } else {
                    self.arena.info().find_type_index(rtype)
                }
            }
            None => !0,
        };

        let entry = self.arena.allocate_object::<PropertyEntry>();

        entry.as_mut().name_offset = self.arena.allocate_and_write_string(&property_entry.name) as _;
        entry.as_mut().namelen = property_entry.name.len() as _;
        entry.as_mut().context_index = context_index as _;
        entry.as_mut().type_index = type_index as _;

        return entry.offset() as _;
    }

    fn write_trie_node(&mut self, builder_node: &TrieBuilderNode) -> u32 {
        let trie = self.arena.allocate_object::<TrieNodeData>();

        trie.as_mut().property_entry = self.write_property_entry(&builder_node.property_entry);
        let mut sorted_prefix_matches: Vec<_> = builder_node.prefixes.iter().collect();
        sorted_prefix_matches.sort_by(|a, b| b.name.len().cmp(&a.name.len()));

        trie.as_mut().num_prefixes = sorted_prefix_matches.len() as _;
        let prefix_entries_array_offset = self.arena.allocate_uint32_array(sorted_prefix_matches.len());
        trie.as_mut().prefix_entries = prefix_entries_array_offset as _;

        for (i, prefix_entry) in sorted_prefix_matches.iter().enumerate() {
            self.arena.uint32_array(prefix_entries_array_offset)[i] = self.write_property_entry(prefix_entry);
        }

        let mut sorted_exact_matches: Vec<_> = builder_node.exact_matches.iter().collect();
        sorted_exact_matches.sort_by(|a, b| a.name.len().cmp(&b.name.len()));

        trie.as_mut().num_exact_matches = sorted_exact_matches.len() as _;

        let exact_match_entries_array_offset = self.arena.allocate_uint32_array(sorted_exact_matches.len());
        trie.as_mut().exact_match_entries = exact_match_entries_array_offset as _;

        for (i, exact_entry) in sorted_exact_matches.iter().enumerate() {
            self.arena.uint32_array(exact_match_entries_array_offset)[i] = self.write_property_entry(exact_entry);
        }

        let mut sorted_children: Vec<_> = builder_node.children.values().collect();
        sorted_children.sort_by(|a, b| a.property_entry.name.len().cmp(&b.property_entry.name.len()));

        trie.as_mut().num_child_nodes = sorted_children.len() as _;
        let children_offset_array_offset = self.arena.allocate_uint32_array(sorted_children.len());

        trie.as_mut().child_nodes = children_offset_array_offset as _;

        for (i, child_node) in sorted_children.iter().enumerate() {
            self.arena.uint32_array(children_offset_array_offset)[i] = self.write_trie_node(child_node);
        }

        return trie.offset() as _;
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