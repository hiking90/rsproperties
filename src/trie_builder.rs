// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, BTreeSet, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::errors::*;

#[derive(Debug)]
pub(crate) struct PropertyEntryBuilder {
    pub(crate) name: Rc<String>,
    pub(crate) context: Option<Rc<String>>,
    pub(crate) rtype: Option<Rc<String>>,
}

impl PartialEq for PropertyEntryBuilder {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for PropertyEntryBuilder {}

impl Hash for PropertyEntryBuilder {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

pub(crate) struct TrieBuilderNode {
    pub(crate) property_entry: PropertyEntryBuilder,
    pub(crate) prefixes: HashSet<PropertyEntryBuilder>,
    pub(crate) exact_matches: HashSet<PropertyEntryBuilder>,
    pub(crate) children: HashMap<Rc<String>, TrieBuilderNode>,
}

impl TrieBuilderNode {
    fn new(name: Rc<String>) -> Self {
        TrieBuilderNode {
            property_entry: PropertyEntryBuilder {
                name,
                context: None,
                rtype: None,
            },
            children: HashMap::new(),
            prefixes: HashSet::new(),
            exact_matches: HashSet::new(),
        }
    }

    fn set_context(&mut self, context: Rc<String>) {
        self.property_entry.context = Some(context);
    }

    fn set_rtype(&mut self, rtype: Rc<String>) {
        self.property_entry.rtype = Some(rtype);
    }

    fn add_exact_match_context(&mut self, name: Rc<String>, context: Rc<String>, rtype: Rc<String>) -> Result<()> {
        let entry = PropertyEntryBuilder {
            name: Rc::clone(&name),
            context: Some(context),
            rtype: Some(rtype),
        };

        if self.exact_matches.insert(entry) {
            Ok(())
        } else {
            Err(Error::new_custom(format!("Exact match already exists for '{}'", name)))
        }
    }

    fn add_prefix_context(&mut self, name: Rc<String>, context: Rc<String>, rtype: Rc<String>) -> Result<()> {
        let entry = PropertyEntryBuilder {
            name: Rc::clone(&name),
            context: Some(context),
            rtype: Some(rtype),
        };

        if self.prefixes.insert(entry) {
            Ok(())
        } else {
            Err(Error::new_custom(format!("Prefix already exists for '{}'", name)))
        }
    }

    fn context(&self) -> Option<&String> {
        self.property_entry.context.as_deref()
    }

    fn rtype(&self) -> Option<&String> {
        self.property_entry.rtype.as_deref()
    }
}

pub(crate) struct TrieBuilder {
    pub(crate) root: TrieBuilderNode,
    pub(crate) contexts: BTreeSet<Rc<String>>,
    pub(crate) types: BTreeSet<Rc<String>>,
}

impl TrieBuilder {
    pub(crate) fn new(default_context: &str, default_type: &str) -> Self {
        let mut contexts = BTreeSet::new();
        let mut types = BTreeSet::new();

        let context = Rc::new(default_context.to_owned());
        let rtypes = Rc::new(default_type.to_owned());

        contexts.insert(context.clone());
        types.insert(rtypes.clone());

        let mut root = TrieBuilderNode::new(Rc::new("root".to_owned()));
        root.set_context(context);
        root.set_rtype(rtypes);

        TrieBuilder {
            root,
            contexts,
            types,
        }
    }

    pub(crate) fn add_to_trie(&mut self, name: &str, context: &str, rtype: &str, exact: bool) -> Result<()> {
        let context = Rc::new(context.to_owned());
        let rtype = Rc::new(rtype.to_owned());

        self.contexts.insert(context.clone());
        self.types.insert(rtype.clone());

        let mut current_node = &mut self.root;
        let mut name_parts = name.split('.').collect::<Vec<&str>>();

        let ends_with_dot = if name_parts.last() == Some(&"") {
            name_parts.pop();
            true
        } else {
            false
        };

        let last_name = name_parts.pop()
            .ok_or(Error::new_custom(format!("No name parts for '{}'", name)))?;

        for part in name_parts {
            let part = Rc::new(part.to_owned());
            current_node = current_node.children.entry(Rc::clone(&part))
                .or_insert_with(|| TrieBuilderNode::new(part));
        }

        let last_name = Rc::new(last_name.to_owned());

        if exact {
            current_node.add_exact_match_context(last_name, Rc::clone(&context), Rc::clone(&rtype))?;
        } else if ends_with_dot == false {
            current_node.add_prefix_context(last_name, Rc::clone(&context), Rc::clone(&rtype))?;
        } else {
            let child = current_node.children.entry(Rc::clone(&last_name))
                .or_insert_with(|| TrieBuilderNode::new(last_name));
            if child.context() != None || child.rtype() != None {
                return Err(Error::new_custom(format!("Duplicate prefix match detected for '{}'", name)));
            }

            child.set_context(context);
            child.set_rtype(rtype);
        }
        Ok(())
    }
}