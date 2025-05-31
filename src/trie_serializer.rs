// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::rc::Rc;
use log::{trace, debug, info};

use crate::trie_node_arena::TrieNodeArena;
use crate::trie_builder::*;
use crate::property_info_parser::*;

pub(crate) struct TrieSerializer {
    arena: TrieNodeArena,
}

impl TrieSerializer {
    pub(crate) fn new(trie_builder: &TrieBuilder) -> Self {
        info!("Creating TrieSerializer for trie with {} contexts and {} types",
              trie_builder.contexts.len(), trie_builder.types.len());

        let mut this = Self {
            arena: TrieNodeArena::new(),
        };

        debug!("Allocating property info area header");
        let header_offset = this.arena.allocate_object::<PropertyInfoAreaHeader>();
        {
            let header = this.arena.to_object::<PropertyInfoAreaHeader>(header_offset);
            header.current_version = 1;
            header.minimum_supported_version = 1;
        }
        trace!("Header allocated at offset {}", header_offset);

        debug!("Serializing {} contexts", trie_builder.contexts.len());
        this.arena.to_object::<PropertyInfoAreaHeader>(header_offset).contexts_offset = this.arena.size() as _;
        this.serialize_strings(&trie_builder.contexts);

        debug!("Serializing {} types", trie_builder.types.len());
        this.arena.to_object::<PropertyInfoAreaHeader>(header_offset).types_offset = this.arena.size() as _;
        this.serialize_strings(&trie_builder.types);

        this.arena.to_object::<PropertyInfoAreaHeader>(header_offset).size = this.arena.size() as _;

        debug!("Writing trie structure starting from root");
        let root_trie_offset = this.write_trie_node(&trie_builder.root);
        this.arena.to_object::<PropertyInfoAreaHeader>(header_offset).root_offset = root_trie_offset as _;

        let final_size = this.arena.size();
        this.arena.to_object::<PropertyInfoAreaHeader>(header_offset).size = final_size as _;

        info!("TrieSerializer created successfully with total size: {} bytes", final_size);
        this
    }

    fn write_property_entry(&mut self, property_entry: &PropertyEntryBuilder) -> u32 {
        trace!("Writing property entry: {}", property_entry.name);

        let context_index = match property_entry.context {
            Some(ref context) => {
                if context.is_empty() {
                    trace!("Property {} has empty context", property_entry.name);
                    !0
                } else {
                    let index = self.arena.info().find_context_index(context);
                    trace!("Property {} mapped to context '{}' (index={})", property_entry.name, context, index);
                    index
                }
            }
            None => {
                trace!("Property {} has no context", property_entry.name);
                !0
            }
        };

        let type_index = match property_entry.rtype {
            Some(ref rtype) => {
                if rtype.is_empty() {
                    trace!("Property {} has empty type", property_entry.name);
                    !0
                } else {
                    let index = self.arena.info().find_type_index(rtype);
                    trace!("Property {} mapped to type '{}' (index={})", property_entry.name, rtype, index);
                    index
                }
            }
            None => {
                trace!("Property {} has no type", property_entry.name);
                !0
            }
        };

        let entry_offset = self.arena.allocate_object::<PropertyEntry>();
        let name_offset = self.arena.allocate_and_write_string(&property_entry.name) as _;

        let entry = self.arena.to_object::<PropertyEntry>(entry_offset);
        entry.name_offset = name_offset;
        entry.namelen = property_entry.name.len() as _;
        entry.context_index = context_index as _;
        entry.type_index = type_index as _;

        trace!("Property entry '{}' written at offset {} (name_offset={}, len={}, context={}, type={})",
               property_entry.name, entry_offset, name_offset, property_entry.name.len(),
               context_index, type_index);

        entry_offset as _
    }    fn write_trie_node(&mut self, builder_node: &TrieBuilderNode) -> u32 {
        debug!("Writing trie node for property '{}'", builder_node.property_entry.name);
        debug!("Node has {} prefixes, {} exact matches, {} children",
               builder_node.prefixes.len(), builder_node.exact_matches.len(), builder_node.children.len());

        let trie_offset = self.arena.allocate_object::<TrieNodeData>();

        let property_entry = self.write_property_entry(&builder_node.property_entry);
        self.arena.to_object::<TrieNodeData>(trie_offset).property_entry = property_entry as _;

        // Sort prefixes by length (longest first)
        let mut sorted_prefix_matches: Vec<_> = builder_node.prefixes.iter().collect();
        sorted_prefix_matches.sort_by(|a, b| b.name.len().cmp(&a.name.len()));
        trace!("Sorted prefixes: {:?}", sorted_prefix_matches.iter().map(|p| &p.name).collect::<Vec<_>>());

        self.arena.to_object::<TrieNodeData>(trie_offset).num_prefixes = sorted_prefix_matches.len() as _;

        let prefix_entries_array_offset = self.arena.allocate_uint32_array(sorted_prefix_matches.len());
        self.arena.to_object::<TrieNodeData>(trie_offset).prefix_entries = prefix_entries_array_offset as _;

        for (i, prefix_entry) in sorted_prefix_matches.iter().enumerate() {
            let offset = self.write_property_entry(prefix_entry);
            self.arena.uint32_array(prefix_entries_array_offset)[i] = offset;
            trace!("Prefix {}: '{}' at offset {}", i, prefix_entry.name, offset);
        }

        // Sort exact matches alphabetically
        let mut sorted_exact_matches: Vec<_> = builder_node.exact_matches.iter().collect();
        sorted_exact_matches.sort_by(|a, b| a.name.cmp(&b.name));
        trace!("Sorted exact matches: {:?}", sorted_exact_matches.iter().map(|e| &e.name).collect::<Vec<_>>());

        self.arena.to_object::<TrieNodeData>(trie_offset).num_exact_matches = sorted_exact_matches.len() as _;
        let exact_match_entries_array_offset = self.arena.allocate_uint32_array(sorted_exact_matches.len());
        self.arena.to_object::<TrieNodeData>(trie_offset).exact_match_entries = exact_match_entries_array_offset as _;

        for (i, exact_entry) in sorted_exact_matches.iter().enumerate() {
            let offset = self.write_property_entry(exact_entry);
            self.arena.uint32_array(exact_match_entries_array_offset)[i] = offset;
            trace!("Exact match {}: '{}' at offset {}", i, exact_entry.name, offset);
        }

        // Sort children alphabetically
        let mut sorted_children: Vec<_> = builder_node.children.values().collect();
        sorted_children.sort_by(|a, b| a.property_entry.name.cmp(&b.property_entry.name));
        trace!("Sorted children: {:?}", sorted_children.iter().map(|c| &c.property_entry.name).collect::<Vec<_>>());

        self.arena.to_object::<TrieNodeData>(trie_offset).num_child_nodes = sorted_children.len() as _;
        let children_offset_array_offset = self.arena.allocate_uint32_array(sorted_children.len());
        self.arena.to_object::<TrieNodeData>(trie_offset).child_nodes = children_offset_array_offset as _;

        for (i, child_node) in sorted_children.iter().enumerate() {
            let child_offset = self.write_trie_node(child_node);
            self.arena.uint32_array(children_offset_array_offset)[i] = child_offset;
            trace!("Child {}: '{}' at offset {}", i, child_node.property_entry.name, child_offset);
        }

        debug!("Trie node '{}' written at offset {}", builder_node.property_entry.name, trie_offset);
        trie_offset as _
    }

    fn serialize_strings(&mut self, strings: &BTreeSet<Rc<String>>) {
        debug!("Serializing {} strings", strings.len());

        self.arena.allocate_and_write_uint32(strings.len() as _);
        let offset_array_offset = self.arena.allocate_uint32_array(strings.len());

        for (i, string) in strings.iter().enumerate() {
            let offset = self.arena.allocate_and_write_string(string);
            self.arena.uint32_array(offset_array_offset)[i] = offset as _;
            trace!("String {}: '{}' at offset {}", i, string, offset);
        }

        debug!("Successfully serialized {} strings", strings.len());
    }

    pub(crate) fn take_data(&mut self) -> Vec<u8> {
        let data = self.arena.take_data();
        info!("Extracted serialized trie data: {} bytes", data.len());
        data
    }
}