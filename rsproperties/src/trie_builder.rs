// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use log::error;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::errors::*;

#[derive(Debug)]
pub(crate) struct PropertyEntryBuilder {
    // `Rc<str>` over `Rc<String>`: one level of indirection less, and
    // `Rc<str>: Borrow<str>` enables allocation-free map/set lookups with
    // borrowed segments (see `add_to_trie`).
    pub(crate) name: Rc<str>,
    pub(crate) context: Option<Rc<str>>,
    pub(crate) rtype: Option<Rc<str>>,
}

// Eq/Hash key on `name` only — the prefix/exact sets deduplicate by
// property name, so two entries differing only in context/type compare
// equal on purpose (that *is* the duplicate being detected).
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
    pub(crate) children: HashMap<Rc<str>, TrieBuilderNode>,
}

impl TrieBuilderNode {
    fn new(name: Rc<str>) -> Self {
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

    fn set_context(&mut self, context: Rc<str>) {
        self.property_entry.context = Some(context);
    }

    fn set_rtype(&mut self, rtype: Rc<str>) {
        self.property_entry.rtype = Some(rtype);
    }

    /// `name` is the trie-level key (the last segment); `full_name` is the
    /// caller's complete property name, used for diagnostics — an error
    /// naming only the segment ("already exists for 'b'") is useless for
    /// locating the offending `a.b` line. AOSP reports the full name here
    /// too.
    fn add_exact_match_context(
        &mut self,
        name: Rc<str>,
        context: Rc<str>,
        rtype: Rc<str>,
        full_name: &str,
    ) -> Result<()> {
        let entry = PropertyEntryBuilder {
            name,
            context: Some(context),
            rtype: Some(rtype),
        };

        if self.exact_matches.insert(entry) {
            Ok(())
        } else {
            error!("Exact match already exists for '{full_name}'");
            Err(Error::FileValidation(format!(
                "Exact match already exists for '{full_name}'"
            )))
        }
    }

    /// See [`Self::add_exact_match_context`] for the `full_name` contract.
    fn add_prefix_context(
        &mut self,
        name: Rc<str>,
        context: Rc<str>,
        rtype: Rc<str>,
        full_name: &str,
    ) -> Result<()> {
        let entry = PropertyEntryBuilder {
            name,
            context: Some(context),
            rtype: Some(rtype),
        };

        if self.prefixes.insert(entry) {
            Ok(())
        } else {
            error!("Prefix already exists for '{full_name}'");
            Err(Error::FileValidation(format!(
                "Prefix already exists for '{full_name}'"
            )))
        }
    }

    fn context(&self) -> Option<&str> {
        self.property_entry.context.as_deref()
    }

    fn rtype(&self) -> Option<&str> {
        self.property_entry.rtype.as_deref()
    }
}

/// Returns the interned copy of `s` from `set`, inserting it on first
/// sight. Repeated contexts/types (the common case — a handful of contexts
/// across thousands of lines) allocate exactly once.
fn intern(set: &mut BTreeSet<Rc<str>>, s: &str) -> Rc<str> {
    match set.get(s) {
        Some(existing) => Rc::clone(existing),
        None => {
            let rc: Rc<str> = Rc::from(s);
            set.insert(Rc::clone(&rc));
            rc
        }
    }
}

/// Upper bound on dot-separated segments per property name. Each segment
/// becomes one trie level, and both `TrieSerializer::write_trie_node` and
/// the `TrieBuilderNode` drop glue recurse per level — an unbounded input
/// (`a.a.a.…`) would overflow the stack instead of failing cleanly. Real
/// property names use fewer than ten segments.
const MAX_NAME_SEGMENTS: usize = 256;

pub(crate) struct TrieBuilder {
    pub(crate) root: TrieBuilderNode,
    pub(crate) contexts: BTreeSet<Rc<str>>,
    pub(crate) types: BTreeSet<Rc<str>>,
}

impl TrieBuilder {
    pub(crate) fn new(default_context: &str, default_type: &str) -> Self {
        let mut contexts = BTreeSet::new();
        let mut types = BTreeSet::new();

        let context: Rc<str> = Rc::from(default_context);
        let rtypes: Rc<str> = Rc::from(default_type);

        contexts.insert(Rc::clone(&context));
        types.insert(Rc::clone(&rtypes));

        let mut root = TrieBuilderNode::new(Rc::from("root"));
        root.set_context(context);
        root.set_rtype(rtypes);

        TrieBuilder {
            root,
            contexts,
            types,
        }
    }

    pub(crate) fn add_to_trie(
        &mut self,
        name: &str,
        context: &str,
        rtype: &str,
        exact: bool,
    ) -> Result<()> {
        // The serialized string table and the trailing node names are C
        // strings; an interior NUL desyncs the recorded lengths from what
        // NUL-scanning readers see (exact-match lookups of the truncated
        // prefix would resolve to this entry's context). This gate covers
        // every per-entry string; the defaults, which bypass `add_to_trie`
        // (interned directly by `TrieBuilder::new`), get the same check at
        // `build_trie`'s entry.
        crate::wire::validate_no_interior_nul("property name", name)?;
        crate::wire::validate_no_interior_nul("context", context)?;
        crate::wire::validate_no_interior_nul("type", rtype)?;

        let mut name_parts = name.split('.').collect::<Vec<&str>>();

        let ends_with_dot = if name_parts.last() == Some(&"") {
            name_parts.pop();
            true
        } else {
            false
        };

        // Checked after the trailing-dot pop so prefix (`a.b.`) and exact
        // names get the same effective limit — the empty trailing segment
        // never becomes a trie level.
        if name_parts.len() > MAX_NAME_SEGMENTS {
            error!("Property name '{name}' exceeds {MAX_NAME_SEGMENTS} segments");
            return Err(Error::Parse(format!(
                "Property name has more than {MAX_NAME_SEGMENTS} segments"
            )));
        }

        // Reject empty segments ("a..b", "a..") — AOSP's IsLegalPropertyName
        // forbids consecutive dots, and the parser side relies on "no empty
        // node names" as an invariant: `cstr()` returns an empty string as
        // its corruption fallback, so a legitimately-empty trie node name
        // would be indistinguishable from a corrupt one during lookup.
        if name_parts.iter().any(|p| p.is_empty()) {
            error!("Property name '{name}' contains an empty segment");
            return Err(Error::Parse(format!(
                "Property name contains an empty segment: '{name}'"
            )));
        }

        let last_name: &str = name_parts
            .pop()
            .ok_or(Error::Parse(format!("No name parts for '{name}'")))?;

        // Intern only after the name has passed validation so rejected
        // lines don't leave their context/type behind in the string
        // tables. Duplicate-entry rejections further down still intern
        // first — the duplicate is only discoverable after the trie walk —
        // so this ordering covers name-shape errors only. Harmless today
        // (build aborts on the first error); a skip-and-continue caller
        // would serialize orphans from the duplicate paths.
        let context = intern(&mut self.contexts, context);
        let rtype = intern(&mut self.types, rtype);

        let mut current_node = &mut self.root;

        for part in name_parts {
            // `Rc<str>: Borrow<str>` lets the existence check use the
            // borrowed segment directly — interior segments allocate only
            // the first time they are seen.
            if !current_node.children.contains_key(part) {
                let key: Rc<str> = Rc::from(part);
                current_node
                    .children
                    .insert(Rc::clone(&key), TrieBuilderNode::new(key));
            }
            current_node = current_node
                .children
                .get_mut(part)
                .expect("child inserted above");
        }

        let last_name: Rc<str> = Rc::from(last_name);

        // The three branches are mutually exclusive, so each can consume
        // `context`/`rtype` directly — no refcount traffic needed.
        if exact {
            current_node.add_exact_match_context(last_name, context, rtype, name)?;
        } else if !ends_with_dot {
            current_node.add_prefix_context(last_name, context, rtype, name)?;
        } else {
            let child = current_node
                .children
                .entry(Rc::clone(&last_name))
                .or_insert_with(|| TrieBuilderNode::new(last_name));

            if child.context().is_some() || child.rtype().is_some() {
                error!("Duplicate prefix match detected for '{name}'");
                return Err(Error::FileValidation(format!(
                    "Duplicate prefix match detected for '{name}'"
                )));
            }

            child.set_context(context);
            child.set_rtype(rtype);
        }

        Ok(())
    }
}
