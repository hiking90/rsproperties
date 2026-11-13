// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Property area files (one per SELinux context, plus `properties_serial`)
//! and the shared mappings through which they and `property_info` are read.
//!
//! # Shared-mapping contract
//!
//! SAFETY comments in this module and in `property_info.rs` state only what
//! the block itself checks and cite the rule IDs below; rule text lives
//! only here.
//!
//! - \[SM1\] Other mappings. Every byte of an area file may be accessed at
//!   the same time through other mappings of the same file: other
//!   processes, and other `MemoryMap`s in this process (the global reader
//!   and a builder writer map the same files). A borrow of one `MemoryMap`
//!   (`&self` / `&mut self`) excludes only accesses through that
//!   `MemoryMap`; it is never evidence of exclusive access to file bytes.
//! - \[SM2\] Published ranges. A range is published once an offset that
//!   reaches it has been Release-stored into a reachable link word
//!   (`children`, `left`, `right`, `prop`); an area header is published
//!   when its file is renamed to its final name. This crate accesses
//!   published bytes only (a) through atomics — header `bytes_used` /
//!   `serial`, link words, `PropertyInfo::serial`, value slots, the dirty
//!   backup slot — or (b) by non-atomic reads of write-once bytes — header
//!   magic/version, node `namelen` and name, entry name, long value —
//!   reached through an acquire load (`load_acquire`, \[R-RO\]) paired
//!   with the publishing Release (for the header: an `open` that follows
//!   the rename).
//! - \[SM3\] Fresh ranges. `&mut` references and non-atomic writes are made
//!   only on ranges no published offset reaches: an allocation between
//!   `allocate_obj` and `Fresh::link` (the `Fresh` token), and an area
//!   header in `create_unpublished` before `publish` renames the temp
//!   file. Readers open only names taken from `property_info` and
//!   `properties_serial`; context names starting with `.` are rejected, so
//!   no reader opens a `.area.tmp-*` file.
//! - \[SM4\] Single writer. At most one writable mapping per area file. On
//!   Linux with this crate as writer: the `.writer_lock` flock held by
//!   `AreaWriterLock` for the writer's lifetime, and within the process
//!   `&mut PropertyAreaMap` behind the context node's write guard. On
//!   Android: init is the only writer (system design, not checked here).
//!   \[SM3\] depends on it: `bytes_used` grows only under the single
//!   writer, so no allocation is handed out twice.
//! - \[SM5\] Untrusted offsets. Offsets read from a file are bounds- and
//!   alignment-checked only. A corrupt offset that passes those checks can
//!   make an access overlap bytes a writer — another process, or a writer
//!   mapping in this process \[SM1\] — stores concurrently (a value slot,
//!   the backup slot, a link word, a fresh allocation): an \[SM2\](b)
//!   non-atomic read, or an atomic load of another width than the store
//!   (a `u32` link word, `serial` or long `offset` loaded over a value
//!   slot that is stored byte-wise). That combination (corruption plus a
//!   concurrent writer) is the accepted residual gap; every other claim in
//!   this contract and in the SAFETY comments that cite it holds outside
//!   this overlap only. A `PropertyIndex` from another `SystemProperties`
//!   instance is rejected before it reaches these accessors, so the gap is
//!   limited to file corruption.
//! - \[R-OFF\] Offset sources. `MemoryMap::{to_object, cstr_at,
//!   atomic_data}` take an `AreaOffset<K>` (module `offset`), whose kind
//!   `K` is what the offset addresses: a `PropertyArea` header, a
//!   `PropertyTrieNode`, a `PropertyInfo` entry (`EntryOffset`), a
//!   `WriteOnceStr` (a node name, an entry name, a long value) or the
//!   `BackupSlot`. `to_object::<T>` accepts only `AreaOffset<T>`,
//!   `cstr_at` only `WriteOnceStr`, and `atomic_data` only `BackupSlot`,
//!   for at most the slot's length. The raw value is private to `offset`,
//!   and every constructor is visible only in this file and names one
//!   source: (1) layout constants — `HEADER`, `ROOT`, `BACKUP_SLOT`; (2)
//!   an acquire load \[R-RO\] of a link word of a `&PropertyTrieNode`
//!   (`children_offset`, `left_offset`, `right_offset` → node,
//!   `prop_offset` → entry), and the name trailing the node or entry an
//!   offset addresses (`name`; `ROOT.name()` is an error, since the bytes
//!   after the root are `BACKUP_SLOT`); (3) a long entry's value
//!   (`EntryOffset::long_value`), called only by
//!   `PropertyAreaMap::long_property_value` after its \[R-UNION\] name
//!   check; (4) `Fresh::link`, which returns the offset of the object it
//!   has just published. Link words are loaded only through a
//!   `&PropertyTrieNode` from `to_object` (the `&mut` from
//!   `fresh_object_mut` only initializes a fresh node), so (2) reads link
//!   words of published nodes or of nodes this writer linked. Code outside
//!   this file gets an `EntryOffset` only from `find` or `entry_offsets`
//!   — directly (`SystemProperties::read_with`) or stored in a
//!   `PropertyIndex` — and can copy it but not make one. The type does not record which area an
//!   offset belongs to: a `PropertyIndex` pairs it with its
//!   `context_index` and is accepted only by the instance whose `find` or
//!   `foreach` made it (`owns_index`). Bounds and alignment are checked at every
//!   access. An offset from any other source would be a bug in this file,
//!   not the \[SM5\] residual; \[SM5\] covers only a file whose link words
//!   or long offsets are corrupt.
//! - \[R-SIZE\] File size. A file is not truncated below its mapped size
//!   while a mapping exists; a violation raises SIGBUS, which no bounds
//!   check can intercept. This crate's writer never truncates a published
//!   area file (it renames a new file over it). Applies to area files and
//!   to `property_info`.
//! - \[R-UNION\] Union variant. The active `PropertyInfo` union variant is
//!   chosen by the LONG flag (bit 16) of `serial`, and only for names
//!   starting with `ro.` (bionic `system_properties.cpp` `ReadCallback`).
//!   `ro.` entries are never updated; bionic's writer counter
//!   `(serial + 1) & 0xffffff` sets bit 16 on the 32768th update of a
//!   mutable property, so on any other name bit 16 is counter. This
//!   crate's writer sets LONG only on new `ro.` entries
//!   (`PropertyAreaMap::add` rejects longer non-`ro.` values) and never
//!   publishes bit 16 on an update (`apply_write` carries it into bit 17,
//!   `SERIAL_COUNTER_MASK`). Entries written in place are short:
//!   `PropertyAreaMap::backup_and_apply_write` — which makes the only
//!   `AreaWriteAccess`, so it is the only route to
//!   `PropertyInfoWriter::apply_write` — refuses an entry whose own
//!   trailing name starts with `ro.`, whatever its caller checked; a
//!   long entry's bytes (its `offset` read as `AtomicU32`, its out-of-line
//!   value read non-atomically) are therefore never rewritten after
//!   publication.
//!
//!   Accesses to the union bytes of a published entry, all of this
//!   crate's (\[SM5\] aside):
//!   - the single writer \[SM4\] stores the `value` variant byte-wise
//!     (`AtomicU8`), only on entries whose trailing name is not `ro.`;
//!   - readers load the `value` variant byte-wise (`AtomicU8`) when they
//!     decide the entry is short — also on a long entry reached by a
//!     non-`ro.` lookup (below);
//!   - the long `offset` (`AtomicU32`, union bytes 56..60) is loaded only
//!     by `PropertyAreaMap::long_property_value`, after it has checked that
//!     the trailing name starts with `ro.` — so only on entries without
//!     stores, and a `u32` load never races a byte store;
//!   - `LongProperty::error_message` (non-atomic) is written only while
//!     the entry is fresh \[SM3\] and never read as that field; after
//!     publication its bytes are read only through the `value` variant's
//!     atomics, when a long entry is read as short (below).
//!
//!   Readers pick the variant by the name they looked up, which equals the
//!   entry's trailing name unless the file is corrupt, so `offset` is
//!   loaded only when both the lookup name and the trailing name start
//!   with `ro.`. In a corrupt file the two differ in two ways: a `ro.`
//!   lookup that reaches an entry not stored under `ro.` with bit 16 set
//!   fails with `Error::FileValidation`; a non-`ro.` lookup that reaches a
//!   LONG `ro.` entry reads its union as the short slot (the legacy error
//!   string) — byte-wise atomic loads of bytes without stores, so only
//!   the interpretation is wrong. bionic's `ReadCallback` decides both
//!   cases by the trailing name alone (short value, long value).
//! - \[R-INIT\] Foreign writers. On Android the writer is init, which
//!   writes value slots and the dirty backup slot with non-atomic copies.
//!   \[SM2\] describes this crate's own accesses; against init this crate
//!   has the same access pattern as bionic's reader, nothing stronger.
//! - \[R-WRITE\] Stores need PROT_WRITE. Atomic stores and
//!   read-modify-writes (link words, `bytes_used`, serials, value slots,
//!   the backup slot) are made only through a mapping created writable:
//!   `allocate_obj` and `AreaWriteAccess::new` check it, and the
//!   serial-area `fetch_add` in `SystemProperties::{add, update}` runs only
//!   after `property_area_mut` returned a writable area, which only
//!   instances created by `new_area` / `new_area_locked` have (a
//!   `debug_assert!` checks that their `properties_serial` mapping is
//!   writable). A violation faults with SIGSEGV on the supported targets;
//!   std leaves it undefined behavior \[R-RO\].
//! - \[R-RO\] Loads on read-only mappings. Readers map area files and
//!   `properties_serial` `PROT_READ`. std (`core::sync::atomic`, "Atomic
//!   accesses to read-only memory") defines atomic accesses to read-only
//!   memory only for `Relaxed` loads up to a per-target size: 4 bytes on
//!   arm, x86 and powerpc, 8 bytes on x86_64 and aarch64 (std's table
//!   lists the rest); other orderings, and every RMW, are
//!   undefined behavior there even when the codegen emits no store. Every
//!   atomic access on a mapping that can be read-only is therefore a
//!   `Relaxed` `AtomicU32`/`AtomicU8` load. Where the protocol needs
//!   Acquire — link words \[SM2\], and `serial` words for the seqlock read
//!   and the serials `serial`/`wait` return — `load_acquire` follows the
//!   `Relaxed` load with `fence(Acquire)`: the fence synchronizes with the
//!   Release store (or the writer's Release fence before it) whose value
//!   the load read, the same edge an Acquire load gives. The futex
//!   syscalls read the word in the kernel and are not Rust atomic
//!   accesses. bionic loads `serial` with `memory_order_acquire` on the
//!   same `PROT_READ` mappings (C11 makes no read-only exception); this
//!   crate is bound by the std contract instead.
//! - \[IMM\] property_info. `property_info` is mapped `PROT_READ` /
//!   `MAP_SHARED` on every target, like bionic (`ImmutableFileMap`). A
//!   writer replaces the file only by renaming a complete new file over it
//!   (`AreaWriterLock::publish_property_info`; AOSP init writes it once,
//!   before any reader exists) and never modifies a file in place. This is
//!   a precondition on every writer, including tools outside this crate,
//!   which this crate cannot check; an in-place rewrite (0.6.0's service,
//!   `std::fs::write`) raises SIGBUS past a new EOF or yields a torn read.
//!   Readers refuse group/other-writable files (as bionic does) and warn on
//!   an owner-writable one — refused with the `strict-file-validation`
//!   feature. The owner-write bit does not stop a writer (root ignores
//!   it); it shows that the producer did not mark the file read-only.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::{
    ffi::CStr,
    fmt::Debug,
    fs::{File, OpenOptions},
    mem,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::errors::*;
use log::{debug, error, info, warn};
use rustix::{fs, mm};

use crate::property_info::PropertyInfo;
#[cfg(feature = "builder")]
use fresh::Fresh;
pub(crate) use offset::EntryOffset;
use offset::{AreaOffset, BackupSlot, NodeOffset, WriteOnceStr, BACKUP_SLOT, HEADER, ROOT};

const PA_SIZE: u64 = 128 * 1024;
const PROP_AREA_MAGIC: u32 = 0x504f5250;
const PROP_AREA_VERSION: u32 = 0xfc6ed0ab;

/// Name prefix of a writer's unpublished area file:
/// `.area.tmp-<pid>-<seq>`. Independent of the area's basename so the temp
/// name never exceeds `NAME_MAX` for a long context name, and so the sweep
/// in `AreaWriterLock::acquire` needs no context list.
pub(crate) const AREA_TMP_PREFIX: &str = ".area.tmp-";

/// Per-process sequence number for [`AREA_TMP_PREFIX`] names; the pid
/// alone repeats for every area one writer creates.
static AREA_TMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// Acquire load of a word that may live in a `PROT_READ` mapping
/// \[R-RO\]: a `Relaxed` load, which std defines on read-only memory,
/// followed by `fence(Acquire)`. Use this, not `load(Acquire)`, for every
/// link word and `serial` word a reader can reach.
#[inline]
pub(crate) fn load_acquire(word: &AtomicU32) -> u32 {
    let value = word.load(Ordering::Relaxed);
    std::sync::atomic::fence(Ordering::Acquire);
    value
}

/// Marker for types that may be materialized in-place from property-area
/// mmap bytes via `MemoryMap::to_object` / `MemoryMap::to_object_mut`.
///
/// # Safety
///
/// Implementors must be valid for **every** bit pattern (no niches — no
/// `bool`/enum/reference fields; `repr(C)` with primitive/atomic fields,
/// or unions/`UnsafeCell` thereof) and tolerate arbitrary padding-byte
/// values. Non-atomic fields are written only while fresh \[SM3\]; after
/// publication they are read as that field only as \[SM2\](b), and where a
/// union overlays them with an atomic field, their bytes may also be loaded
/// through that atomic (\[R-UNION\] lists the accesses). Without this bound
/// `to_object` would let any safe caller conjure UB by picking a type with
/// invalid-bit-pattern niches.
pub(crate) unsafe trait MmapObject {}

// SAFETY: `repr(C, align(4))` of u32/AtomicU32 fields, every bit pattern
// valid. Non-atomic fields: `magic`, `version`, `reserved` — written by
// `PropertyArea::init` while the header is fresh [SM3], read as [SM2](b).
unsafe impl MmapObject for PropertyArea {}
// SAFETY: `repr(C, align(4))` of u32/AtomicU32 fields, every bit pattern
// valid. Non-atomic field: `namelen` — written by `init_header` while the
// node is fresh [SM3], read as [SM2](b) after an acquire link load (or by
// the writer that allocated the node). The root node at offset 0 is reached
// through the published header instead; its `namelen` is the zero fill of
// the temp file and is never written.
unsafe impl MmapObject for PropertyTrieNode {}
// SAFETY: `repr(C, align(4))`: AtomicU32 serial + `UnsafeCell` of a union
// of an AtomicU8 array and (u8 array, AtomicU32); every bit pattern valid.
// Non-atomic field: `LongProperty::error_message` — written by
// `init_with_long_offset` while the entry is fresh [SM3]; after
// publication its bytes are read only through the `value` variant's
// atomics (a long entry read as short), never as `u8` [R-UNION]. The value
// slot and the long offset are atomics.
unsafe impl MmapObject for PropertyInfo {}

// Byte-for-byte compatibility with bionic's on-disk layout: these structs
// overlay files produced (and consumed) by native Android. A field added,
// removed, or reordered must fail the build, not corrupt lookups at
// runtime — hence per-field offsets, not just sizes (a size-only assert
// would let two same-width fields swap silently). `PropertyInfo`'s size is
// asserted next to its definition in property_info.rs.
const _: () = {
    assert!(mem::size_of::<PropertyTrieNode>() == 20);
    assert!(mem::offset_of!(PropertyTrieNode, namelen) == 0);
    assert!(mem::offset_of!(PropertyTrieNode, prop) == 4);
    assert!(mem::offset_of!(PropertyTrieNode, left) == 8);
    assert!(mem::offset_of!(PropertyTrieNode, right) == 12);
    assert!(mem::offset_of!(PropertyTrieNode, children) == 16);

    assert!(mem::size_of::<PropertyArea>() == 128);
    assert!(mem::offset_of!(PropertyArea, bytes_used) == 0);
    assert!(mem::offset_of!(PropertyArea, serial) == 4);
    assert!(mem::offset_of!(PropertyArea, magic) == 8);
    assert!(mem::offset_of!(PropertyArea, version) == 12);
    assert!(mem::offset_of!(PropertyArea, reserved) == 16);
};

// Fields private to this file: link words are loaded only by the `offset`
// constructors [R-OFF](2) and stored only by `Fresh::link` (and zeroed by
// `init_header` while fresh).
#[repr(C, align(4))]
pub(crate) struct PropertyTrieNode {
    namelen: u32,
    prop: AtomicU32,
    left: AtomicU32,
    right: AtomicU32,
    children: AtomicU32,
}

impl PropertyTrieNode {
    /// Initializes the fixed-size header of a fresh node \[SM3\] — plain
    /// writes, the node is not reachable yet. The trailing name bytes are
    /// written by `PropertyAreaMap::new_prop_trie_node` through the mmap
    /// base pointer — writing them through `&mut self` would step outside
    /// this reference's provenance (it covers exactly
    /// `size_of::<PropertyTrieNode>()` bytes).
    #[cfg(feature = "builder")]
    fn init_header(&mut self, namelen: u32) {
        *self.prop.get_mut() = 0;
        *self.left.get_mut() = 0;
        *self.right.get_mut() = 0;
        *self.children.get_mut() = 0;
        self.namelen = namelen;
    }
}

impl Debug for PropertyTrieNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The trailing name is not printed here: reading it requires the
        // enclosing mmap (see `PropertyAreaMap::trie_node_name`), which a
        // bare node reference cannot reach without leaving its provenance.
        f.debug_struct("PropertyTrieNode")
            .field("namelen", &self.namelen)
            .field("prop", &self.prop)
            .field("left", &self.left)
            .field("right", &self.right)
            .field("children", &self.children)
            .finish()
    }
}

fn cmp_prop_name(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[derive(Debug)]
#[repr(C, align(4))]
pub(crate) struct PropertyArea {
    // Atomic: `allocate_obj` rewrites it on every `add` while readers
    // (other processes, or another in-process mapping of the same file)
    // have the header mapped.
    bytes_used: AtomicU32,
    serial: AtomicU32,
    magic: u32,
    version: u32,
    reserved: [u32; 28],
}

impl PropertyArea {
    /// Initializes a fresh header \[SM3\] (temp file, before `publish`).
    fn init(&mut self, magic: u32, version: u32) {
        *self.serial.get_mut() = 0;
        self.magic = magic;
        self.version = version;
        self.reserved = [0; 28];
        *self.bytes_used.get_mut() = mem::size_of::<PropertyTrieNode>() as u32
            + crate::bionic_align(crate::wire::PROP_VALUE_MAX, mem::size_of::<u32>()) as u32;
    }

    pub(crate) fn serial(&self) -> &AtomicU32 {
        &self.serial
    }
}

/// Proof that the mapping of the entry being written was created
/// `PROT_WRITE` \[R-WRITE\]. Created only by `backup_and_apply_write` from
/// its own mapping; required by `PropertyInfo::writer` and
/// `set_dirty_backup_area`, whose stores go through `&self` accessors.
#[cfg(feature = "builder")]
pub(crate) struct AreaWriteAccess<'a> {
    _map: std::marker::PhantomData<&'a MemoryMap>,
}

#[cfg(feature = "builder")]
impl<'a> AreaWriteAccess<'a> {
    fn new(map: &'a MemoryMap) -> Result<Self> {
        map.require_writable()?;
        Ok(Self {
            _map: std::marker::PhantomData,
        })
    }

    /// For `property_info` tests on a heap entry, which has no mapping.
    #[cfg(test)]
    pub(crate) fn for_heap_test() -> Self {
        Self {
            _map: std::marker::PhantomData,
        }
    }
}

/// Typed offsets into an area file \[R-OFF\]. The raw value is private to
/// this module; the constructors are `pub(super)`, so only this file can
/// make an offset.
mod offset {
    use std::fmt;
    use std::marker::PhantomData;
    use std::mem;
    use std::sync::atomic::AtomicU32;

    use super::{PropertyArea, PropertyTrieNode};
    use crate::errors::*;
    use crate::property_info::PropertyInfo;

    /// Kind of a NUL-terminated string written once, before it is
    /// published: a trie node name, an entry name, a long value.
    pub(crate) enum WriteOnceStr {}

    /// Kind of the area's dirty backup slot, accessed only byte-wise
    /// atomically.
    pub(crate) enum BackupSlot {}

    /// Offset of a `K` in an area file, relative to the data region
    /// ([`HEADER`]: to the start of the file). The sources are listed in
    /// \[R-OFF\].
    pub(crate) struct AreaOffset<K> {
        raw: u32,
        // `fn() -> K`: `Send`/`Sync`/`Copy` whatever `K` is, so
        // `PropertyIndex` keeps its auto traits.
        kind: PhantomData<fn() -> K>,
    }

    pub(crate) type NodeOffset = AreaOffset<PropertyTrieNode>;
    pub(crate) type EntryOffset = AreaOffset<PropertyInfo>;
    pub(crate) type StrOffset = AreaOffset<WriteOnceStr>;

    /// (1) The area header, at file offset 0.
    pub(super) const HEADER: AreaOffset<PropertyArea> = AreaOffset::new(0);
    /// (1) The root trie node, at the start of the data region.
    pub(super) const ROOT: NodeOffset = AreaOffset::new(0);
    /// (1) The dirty backup slot, right after the root node (see
    /// `PropertyArea::init`).
    pub(super) const BACKUP_SLOT: AreaOffset<BackupSlot> =
        AreaOffset::new(mem::size_of::<PropertyTrieNode>() as u32);

    // Manual impls: derives would bound `K`. `Debug` prints the number
    // only, as `PropertyIndex`'s `Debug` did before this type existed.
    impl<K> Clone for AreaOffset<K> {
        fn clone(&self) -> Self {
            *self
        }
    }

    impl<K> Copy for AreaOffset<K> {}

    // By number. Two offsets of one kind are comparable only within one
    // area; `PropertyIndex` pairs the offset with its context and instance.
    impl<K> PartialEq for AreaOffset<K> {
        fn eq(&self, other: &Self) -> bool {
            self.raw == other.raw
        }
    }

    impl<K> Eq for AreaOffset<K> {}

    impl<K> std::hash::Hash for AreaOffset<K> {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            self.raw.hash(state);
        }
    }

    impl<K> fmt::Debug for AreaOffset<K> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Debug::fmt(&self.raw, f)
        }
    }

    impl<K> fmt::Display for AreaOffset<K> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Display::fmt(&self.raw, f)
        }
    }

    impl<K> AreaOffset<K> {
        /// Private to this module: the constructors below name the source.
        const fn new(raw: u32) -> Self {
            Self {
                raw,
                kind: PhantomData,
            }
        }

        /// (4) The offset of the object `Fresh::link` has just
        /// Release-stored into a link word. Only `Fresh::link` calls this.
        #[cfg(feature = "builder")]
        pub(super) fn linked(raw: u32) -> Self {
            Self::new(raw)
        }

        pub(super) fn get(self) -> usize {
            self.raw as usize
        }

        /// (2) The name trailing the `header`-byte object at `self`.
        fn trailing_name(self, header: usize, what: &str) -> Result<StrOffset> {
            u32::try_from(header)
                .ok()
                .and_then(|h| self.raw.checked_add(h))
                .map(AreaOffset::new)
                .ok_or_else(|| {
                    Error::FileValidation(format!("{what} name offset overflow: {}", self.raw))
                })
        }
    }

    impl NodeOffset {
        /// (2) The name trailing this node. Not `ROOT`: the root has no
        /// name, and the bytes after it are the mutable `BACKUP_SLOT`.
        pub(super) fn name(self) -> Result<StrOffset> {
            if self.raw == 0 {
                return Err(Error::FileValidation(
                    "The root trie node has no name".into(),
                ));
            }
            self.trailing_name(mem::size_of::<PropertyTrieNode>(), "Trie node")
        }
    }

    impl EntryOffset {
        /// (2) The name trailing this entry.
        pub(super) fn name(self) -> Result<StrOffset> {
            self.trailing_name(mem::size_of::<PropertyInfo>(), "PropertyInfo")
        }

        /// (3) The out-of-line value `rel` bytes past this entry. Only
        /// `PropertyAreaMap::long_property_value` calls this, with the
        /// `offset` it loaded after the \[R-UNION\] name check.
        pub(super) fn long_value(self, rel: u32) -> Result<StrOffset> {
            self.raw
                .checked_add(rel)
                .map(AreaOffset::new)
                .ok_or_else(|| {
                    Error::FileValidation(format!(
                        "Long value offset overflow: {} + {rel}",
                        self.raw
                    ))
                })
        }
    }

    /// (2) Acquire load \[R-RO\] of a link word; 0 means no link.
    fn load<K>(word: &AtomicU32) -> Option<AreaOffset<K>> {
        match super::load_acquire(word) {
            0 => None,
            raw => Some(AreaOffset::new(raw)),
        }
    }

    impl PropertyTrieNode {
        /// (2) This node's `children` link.
        pub(super) fn children_offset(&self) -> Option<NodeOffset> {
            load(&self.children)
        }

        /// (2) This node's `left` link.
        pub(super) fn left_offset(&self) -> Option<NodeOffset> {
            load(&self.left)
        }

        /// (2) This node's `right` link.
        pub(super) fn right_offset(&self) -> Option<NodeOffset> {
            load(&self.right)
        }

        /// (2) This node's `prop` link.
        pub(super) fn prop_offset(&self) -> Option<EntryOffset> {
            load(&self.prop)
        }
    }
}

#[cfg(feature = "builder")]
mod fresh {
    use std::marker::PhantomData;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::offset::AreaOffset;

    /// An allocation for a `K` that no link word holds yet \[SM3\]. Not
    /// Clone/Copy. Only `PropertyAreaMap::allocate_obj` calls `alloc`;
    /// only `link` and `offset_from` consume a token.
    #[must_use = "an unlinked allocation leaks area space"]
    pub(super) struct Fresh<K> {
        offset: u32,
        len: usize,
        map_base: usize,
        kind: PhantomData<fn() -> K>,
    }

    impl<K> Fresh<K> {
        pub(super) fn alloc(offset: u32, len: usize, map_base: usize) -> Self {
            Self {
                offset,
                len,
                map_base,
                kind: PhantomData,
            }
        }

        /// (offset, len) for the `fresh_*` accessors; `None` if the token
        /// belongs to another mapping.
        pub(super) fn range_in(&self, map_base: usize) -> Option<(usize, usize)> {
            (self.map_base == map_base).then_some((self.offset as usize, self.len))
        }

        /// Release-stores the offset into `slot` and consumes the token.
        /// Returns the offset for further *reads* of the now-published
        /// object \[R-OFF\](4).
        pub(super) fn link(self, slot: &AtomicU32) -> AreaOffset<K> {
            slot.store(self.offset, Ordering::Release);
            AreaOffset::linked(self.offset)
        }

        /// Long value: consumes `self`, returns its offset relative to
        /// `entry` (same mapping, `self` allocated after `entry`). The long
        /// bytes become reachable when `entry` is linked.
        pub(super) fn offset_from<E>(self, entry: &Fresh<E>) -> Option<u32> {
            if self.map_base != entry.map_base {
                return None;
            }
            self.offset.checked_sub(entry.offset)
        }
    }
}

#[derive(Debug)]
pub(crate) struct PropertyAreaMap {
    mmap: MemoryMap,
    data_offset: usize,
    pa_data_size: usize,
}

impl PropertyAreaMap {
    /// Creates a fresh, empty area at `filename`: builds it under a temp
    /// name in the same directory and renames it into place, so a reader
    /// opening `filename` finds either the previous file or the complete
    /// new header, never an empty or zeroed file.
    ///
    /// Precondition \[SM4\]: the caller holds the directory's
    /// `AreaWriterLock`. A previous writer's file at `filename` is replaced
    /// by the rename (AOSP relies on a fresh tmpfs `/dev` per boot instead
    /// and fails its `O_EXCL` create); readers that mapped the old file
    /// keep the old inode — they stop seeing updates but never fault
    /// \[R-SIZE\]. No directory fsync: areas are recreated on every writer
    /// start and need no durability.
    pub(crate) fn new_rw(filename: &Path, context: Option<&CStr>) -> Result<Self> {
        debug!("Creating new read-write property area map: {filename:?}");
        let (thiz, tmp) = Self::create_unpublished(filename, context)?;
        Self::publish(&tmp, filename)?;
        info!("Successfully created read-write property area map: {filename:?}");
        Ok(thiz)
    }

    /// Creates and initializes the area under `.area.tmp-<pid>-<seq>` next
    /// to `filename`; returns the mapping and the temp path. The temp file
    /// is removed on every failure after it was created.
    fn create_unpublished(filename: &Path, context: Option<&CStr>) -> Result<(Self, PathBuf)> {
        let dir = filename.parent().ok_or_else(|| {
            Error::InvalidArgument(format!("property area path has no parent: {filename:?}"))
        })?;
        let seq = AREA_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!("{AREA_TMP_PREFIX}{}-{seq}", std::process::id()));

        let file = OpenOptions::new()
            .read(true) // O_RDWR
            .write(true) // O_RDWR
            .create_new(true) // O_CREAT | O_EXCL — atomic create, never an existing file
            // O_EXCL alone already fails on a symlink at the final
            // component (even a dangling one, with EEXIST); NOFOLLOW is
            // redundant belt-and-suspenders kept so the intent survives a
            // future change away from create_new.
            .custom_flags(fs::OFlags::NOFOLLOW.bits() as _)
            .mode(0o444) // permission: 0444
            .open(&tmp)
            .context_with_location(format!("Failed to create property area {tmp:?}"))?;

        match Self::init_unpublished(file, &tmp, filename, context) {
            Ok(thiz) => Ok((thiz, tmp)),
            Err(e) => {
                if let Err(rm) = std::fs::remove_file(&tmp) {
                    warn!("Failed to remove unpublished property area {tmp:?}: {rm}");
                }
                Err(e)
            }
        }
    }

    /// `filename` is the final path, for the SELinux `warn` (0.6.0 text);
    /// the errors name `tmp`, the file they concern.
    fn init_unpublished(
        file: File,
        tmp: &Path,
        filename: &Path,
        context: Option<&CStr>,
    ) -> Result<Self> {
        if let Some(context) = context {
            // Full xattr name required — the bare "selinux" (no namespace
            // prefix) is rejected by the kernel with EOPNOTSUPP, which made
            // this call fail unconditionally. bionic uses XATTR_NAME_SELINUX,
            // which is "security.selinux".
            //
            // Labeling failure is a warning, NOT fatal — a deliberate
            // deviation from bionic, where init treats it as fatal. This
            // crate's primary deployments (non-Android hosts, dev
            // containers) hit EOPNOTSUPP as the normal case; on an SELinux
            // enforcing system an unlabeled area instead surfaces later as
            // reader-side denials. The label moves with the inode on rename.
            if fs::fsetxattr(
                &file,
                "security.selinux",
                context.to_bytes_with_nul(),
                fs::XattrFlags::empty(),
            )
            .is_err()
            {
                warn!("Failed to set SELinux context for {filename:?}");
            }
        }

        // The umask may have cleared read bits of 0444; readers of other
        // uids need all three, like `publish_property_info` (AOSP init runs
        // with umask 0).
        fs::fchmod(&file, fs::Mode::RUSR | fs::Mode::RGRP | fs::Mode::ROTH)
            .context_with_location(format!("Failed to set mode of {tmp:?}"))?;

        fs::ftruncate(&file, PA_SIZE)
            .context_with_location(format!("Failed to size property area {tmp:?}"))?;

        let pa_size = PA_SIZE as usize;
        let pa_data_size = pa_size - std::mem::size_of::<PropertyArea>();

        let mut thiz = Self {
            mmap: MemoryMap::new(file, pa_size, true)?,
            data_offset: std::mem::size_of::<PropertyArea>(),
            pa_data_size,
        };

        // SAFETY: [SM3] — the header lies in the temp file, which no reader
        // opens before `publish` renames it.
        let header = unsafe { thiz.mmap.to_object_mut::<PropertyArea>(0, 0) }?;
        header.init(PROP_AREA_MAGIC, PROP_AREA_VERSION);
        Ok(thiz)
    }

    /// Publishes the area header \[SM2\]: renames the temp file over
    /// `filename`. On failure the temp file is removed.
    fn publish(tmp: &Path, filename: &Path) -> Result<()> {
        std::fs::rename(tmp, filename)
            .inspect_err(|_| {
                if let Err(rm) = std::fs::remove_file(tmp) {
                    warn!("Failed to remove unpublished property area {tmp:?}: {rm}");
                }
            })
            .context_with_location(format!("Failed to rename {tmp:?} to {filename:?}"))
    }

    // Initialize the property area map with the given file to read-only property area map.
    //
    // [R-SIZE]: the file must not be truncated below the validated size
    // while this mapping is alive.
    pub(crate) fn new_ro(filename: &Path) -> Result<Self> {
        debug!("Opening read-only property area map: {filename:?}");

        let file = OpenOptions::new()
            .read(true) // read only
            .custom_flags(fs::OFlags::NOFOLLOW.bits() as _) // additional flags
            .open(filename)
            .context_with_location(format!("Failed to open {filename:?}"))?;

        let metadata = file
            .metadata()
            .context_with_location("Failed to get metadata")?;

        // Validate file metadata using common utility function
        crate::file_validation::validate_file_metadata(
            &metadata,
            filename,
            mem::size_of::<PropertyArea>() as u64,
        )?;

        // See `PropertyInfoAreaFile::load_path`: `as usize` would truncate
        // on 32-bit targets and desync the mapped size from the validated
        // file size.
        let pa_size = usize::try_from(metadata.len()).map_err(|_| {
            Error::FileValidation(format!(
                "File too large to map on this platform: {} bytes in {filename:?}",
                metadata.len()
            ))
        })?;
        let pa_data_size = pa_size - std::mem::size_of::<PropertyArea>();

        let thiz = Self {
            mmap: MemoryMap::new(file, pa_size, false)?,
            data_offset: std::mem::size_of::<PropertyArea>(),
            pa_data_size,
        };

        let pa = thiz.property_area();

        if pa.magic != PROP_AREA_MAGIC || pa.version != PROP_AREA_VERSION {
            error!(
                "Invalid magic ({:#x} != {:#x}) or version ({:#x} != {:#x}) for {:?}",
                pa.magic, PROP_AREA_MAGIC, pa.version, PROP_AREA_VERSION, filename
            );
            Err(Error::FileValidation(
                "Invalid magic or version".to_string(),
            ))
        } else {
            info!("Successfully opened read-only property area map: {filename:?}");
            Ok(thiz)
        }
    }

    pub(crate) fn property_area(&self) -> &PropertyArea {
        self.mmap
            .to_object(HEADER, 0)
            .expect("PropertyArea's offset is zero. So, it must be valid.")
    }

    /// Whether the underlying mapping was created read-write.
    pub(crate) fn is_writable(&self) -> bool {
        self.mmap.writable
    }

    // Find the property information with the given name.
    pub(crate) fn find(&self, name: &str) -> Result<(&PropertyInfo, EntryOffset)> {
        let mut remaining_name = name;
        let mut current_offset = ROOT;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                // bionic parity: an empty segment never names a stored
                // property (`add` rejects such names).
                return Err(Error::NotFound(name.to_owned()));
            }

            let subname = &remaining_name[0..substr_size];

            let Some(children_offset) = self
                .mmap
                .to_object(current_offset, self.data_offset)?
                .children_offset()
            else {
                return Err(Error::NotFound(name.to_owned()));
            };

            // `find_prop_trie_node` only knows the segment; report the full
            // name like the other `NotFound` paths of this function.
            current_offset = match self.find_prop_trie_node(children_offset, subname)? {
                Some(offset) => offset,
                None => return Err(Error::NotFound(name.to_owned())),
            };

            if sep.is_none() {
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
        }

        match self
            .mmap
            .to_object(current_offset, self.data_offset)?
            .prop_offset()
        {
            Some(prop_offset) => Ok((
                self.mmap.to_object(prop_offset, self.data_offset)?,
                prop_offset,
            )),
            None => Err(Error::NotFound(name.to_owned())),
        }
    }

    /// Offsets of every entry in this area, in bionic
    /// `prop_area::foreach_property` order: for each node, its `left`
    /// subtree, its `prop` entry, its `children` subtree, then its `right`
    /// subtree. Links are loaded like `find` loads them \[R-OFF\](2), and
    /// each entry offset is checked with `to_object` before it is returned.
    ///
    /// An explicit stack instead of bionic's recursion: a corrupt file can
    /// link nodes into a cycle or a chain as deep as the area, and
    /// recursion would overflow the thread stack before any bound fired.
    /// A walk over distinct nodes visits at most as many nodes as fit in
    /// the data region; more visits than that fail with
    /// `Error::FileValidation`, which also bounds the stack (each visit
    /// pushes at most four steps). bionic stops the area's walk silently
    /// on a bad offset instead.
    pub(crate) fn entry_offsets(&self) -> Result<Vec<EntryOffset>> {
        enum Step {
            Node(NodeOffset),
            Entry(EntryOffset),
        }
        let max_nodes = self.pa_data_size / mem::size_of::<PropertyTrieNode>();
        let mut visited = 0usize;
        let mut entries = Vec::new();
        let mut stack = vec![Step::Node(ROOT)];
        while let Some(step) = stack.pop() {
            match step {
                Step::Entry(offset) => {
                    self.property_info(offset)?;
                    entries.push(offset);
                }
                Step::Node(offset) => {
                    visited += 1;
                    if visited > max_nodes {
                        return Err(Error::FileValidation(
                            "Trie node cycle detected (corrupt property area)".into(),
                        ));
                    }
                    let node = self.mmap.to_object(offset, self.data_offset)?;
                    // Pushed in reverse, so they pop in bionic's order.
                    if let Some(right) = node.right_offset() {
                        stack.push(Step::Node(right));
                    }
                    if let Some(children) = node.children_offset() {
                        stack.push(Step::Node(children));
                    }
                    if let Some(prop) = node.prop_offset() {
                        stack.push(Step::Entry(prop));
                    }
                    if let Some(left) = node.left_offset() {
                        stack.push(Step::Node(left));
                    }
                }
            }
        }
        Ok(entries)
    }

    // Add the property information with the given name and value.
    #[cfg(feature = "builder")]
    pub(crate) fn add(&mut self, name: &str, value: &str) -> Result<()> {
        // Value elided: this line runs for every new key, including
        // client-set secrets, and would defeat the value masking applied
        // in every other log site.
        debug!("Adding property: {name:?} (<{} bytes>)", value.len());

        // An interior NUL would desync `namelen` from the NUL-scanned
        // trailing name: the entry becomes unreachable under its real name
        // while every retry re-allocates fresh trie nodes (leaking area
        // space until AreaFull) and enumeration shows a truncated ghost
        // name. Reject here — the single choke point every caller
        // (`SystemProperties::add`, the build.prop load path) goes through.
        crate::wire::validate_no_interior_nul("property name", name)?;
        // Only `ro.` names may get a LONG entry: a reader takes the LONG
        // layout only for `ro.` names [R-UNION], so a long non-`ro.` value
        // would read back as the legacy error string.
        crate::wire::validate_value_len(name, value)?;
        // Empty segments ("a.", "a..b", "") are validated BEFORE the trie
        // walk for the same allocation-hygiene reason as the NUL check: a
        // mid-walk rejection would leave freshly-allocated nodes for the
        // leading segments behind on every retry. (`find` keeps its
        // in-loop check instead — it allocates nothing.)
        if name.is_empty() || name.split('.').any(str::is_empty) {
            error!("Invalid property name (empty segment): {name:?}");
            return Err(Error::Parse(format!("Invalid property name: {name:?}")));
        }

        let mut remaining_name = name;
        let mut current = ROOT;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            let subname = &remaining_name[0..substr_size];

            let children_offset = self
                .mmap
                .to_object(current, self.data_offset)?
                .children_offset();
            let root_offset = match children_offset {
                Some(children_offset) => children_offset,
                None => {
                    let node = self.new_prop_trie_node(subname)?;
                    node.link(&self.mmap.to_object(current, self.data_offset)?.children)
                }
            };

            current = self.add_prop_trie_node(root_offset, subname)?;

            if sep.is_none() {
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
        }

        let prop_offset = self
            .mmap
            .to_object(current, self.data_offset)?
            .prop_offset();

        if prop_offset.is_none() {
            let entry = self.new_prop_info(name, value)?;
            // [R-WRITE]: `allocate_obj` checked PROT_WRITE.
            entry.link(&self.mmap.to_object(current, self.data_offset)?.prop);
        }

        Ok(())
    }

    // Snapshot the dirty backup slot into `dst`, byte-wise atomic.
    //
    // The slot is shared per-area and may be concurrently rewritten by
    // another process's writer starting its next update, so it follows the
    // same byte-wise atomic discipline as the value slots (see the
    // concurrency notes in `property_info.rs`). The caller (seqlock read
    // loop) must copy *before* its fence/serial re-check and use only the
    // snapshot afterwards.
    pub(crate) fn read_dirty_backup(&self, dst: &mut [u8]) -> Result<()> {
        // Mirror the write side's bound: reads past the reserved slot
        // would return bytes of the first allocated object as "backup".
        let reserved = crate::bionic_align(crate::wire::PROP_VALUE_MAX, mem::size_of::<u32>());
        if dst.len() >= reserved {
            return Err(Error::InvalidArgument(format!(
                "Backup read too long: {} (max: {})",
                dst.len(),
                reserved - 1
            )));
        }
        let src = self
            .mmap
            .atomic_data(BACKUP_SLOT, self.data_offset, dst.len())?;
        for (d, s) in dst.iter_mut().zip(src) {
            *d = s.load(Ordering::Relaxed);
        }
        Ok(())
    }

    /// Backs up the entry's current value into the area's dirty backup
    /// slot, then runs the entry-side seqlock write (`apply_write`) — the
    /// only way to reach a `PropertyInfoWriter`.
    ///
    /// The two steps are deliberately fused: readers that observe the dirty
    /// serial read the *backup slot*, not the entry, so publishing the
    /// dirty bit without a fresh backup would serve a previous update's
    /// backup bytes as this property's value. `&mut self` keeps one writer
    /// per mapping inside the process \[SM4\]; the `AreaWriteAccess` made
    /// here is the only one, so no other code reaches the backup slot or an
    /// entry writer.
    ///
    /// `backup` must hold the entry's current value bytes (snapshotted by
    /// the caller before taking `&mut self`).
    ///
    /// An entry named `ro.…` is refused with `Error::ReadOnlyProperty`
    /// before any store: only `ro.` entries can be long, and `apply_write`
    /// writes the short slot \[R-UNION\].
    #[cfg(feature = "builder")]
    pub(crate) fn backup_and_apply_write(
        &mut self,
        pi_offset: EntryOffset,
        backup: &[u8],
        value: &str,
    ) -> Result<()> {
        // [R-UNION]: decided by the entry's own name, not by the caller's
        // lookup name or by bit 16 (on a mutable name that bit is counter).
        let name = self.property_info_name(pi_offset)?;
        if name.to_bytes().starts_with(b"ro.") {
            return Err(Error::ReadOnlyProperty(name.to_string_lossy().into_owned()));
        }
        let access = AreaWriteAccess::new(&self.mmap)?;
        self.set_dirty_backup_area(&access, backup)?;
        // The published serial is deliberately not returned — the sole
        // caller has no use for it, and an unused `u32` invites callers to
        // treat it as something it isn't (futex waits need the serial
        // *address*, not its value).
        self.property_info(pi_offset)?
            .writer(access)
            .apply_write(value)?;
        Ok(())
    }

    // Set the dirty backup area.
    // It is used to store the backup of the property area.
    //
    // Accepts raw bytes (not `&str`) so the caller can stream the current
    // property value directly from the byte-atomic mmap slot into the
    // backup area without first materialising a `String`. The reader side
    // already validates UTF-8 after the seqlock re-check, so the backup
    // area itself stores raw bytes verbatim.
    //
    // `&self`: every store below is atomic; `_access` proves PROT_WRITE
    // [R-WRITE], and single-writer rests on `backup_and_apply_write(&mut
    // self)` [SM4].
    #[cfg(feature = "builder")]
    fn set_dirty_backup_area(&self, _access: &AreaWriteAccess<'_>, value: &[u8]) -> Result<()> {
        // Checked arithmetic so a wrapping `usize` doesn't bypass the size
        // gate. Realistically `value.len()` is < PROP_VALUE_MAX (92), but
        // this function is `pub(crate)` and the rest of the module uses
        // `checked_*` throughout — keep the discipline.
        let total_len = value.len().checked_add(1).ok_or_else(|| {
            Error::InvalidArgument(format!("Backup value too long: {}", value.len()))
        })?;
        // Bound against the *reserved* backup slot (see `PropertyArea::init`),
        // not the whole data region — a longer value would silently overwrite
        // the first allocated object right after the slot.
        let reserved = crate::bionic_align(crate::wire::PROP_VALUE_MAX, mem::size_of::<u32>());
        if total_len > reserved {
            error!("Backup value overflows the reserved slot: {total_len} > {reserved}");
            return Err(Error::InvalidArgument(format!(
                "Backup value too long: {} (max: {})",
                value.len(),
                reserved - 1
            )));
        }

        // Seqlock writer fence: the backup slot is shared per-area and this
        // rewrite happens *after* the previous update's clean serial store.
        // Without a fence the relaxed byte stores below could be observed
        // before that clean serial — a reader still parked on the previous
        // update's dirty serial could snapshot *this* update's backup bytes
        // and yet pass its serial re-check. Pairs with the reader's
        // `fence(Acquire)`: observing any backup byte written after this
        // fence implies observing the newer serial at the re-check → retry.
        std::sync::atomic::fence(Ordering::Release);
        // Byte-wise atomic stores — concurrent readers in other processes
        // may snapshot the slot while we rewrite it (their seqlock re-check
        // discards the torn copy, but the accesses themselves must be
        // atomic to be well-defined).
        let dst = self
            .mmap
            .atomic_data(BACKUP_SLOT, self.data_offset, total_len)?;
        for (slot, &b) in dst.iter().zip(value) {
            slot.store(b, Ordering::Relaxed);
        }
        dst[value.len()].store(0, Ordering::Relaxed);
        Ok(())
    }

    // Add a new property trie node with the given name to the given trie node.
    // It uses trie offset to avoid the life time issue of the current trie node.
    #[cfg(feature = "builder")]
    fn add_prop_trie_node(&mut self, trie_offset: NodeOffset, name: &str) -> Result<NodeOffset> {
        let name_bytes = name.as_bytes();
        let mut current_offset = trie_offset;
        // Same cycle bound as `find_prop_trie_node`: the builder normally
        // walks only its own freshly-built file, but a corrupt link chain
        // must fail instead of hanging the writer.
        let max_steps = self.pa_data_size / mem::size_of::<PropertyTrieNode>();
        for _ in 0..=max_steps {
            let current_node = self.mmap.to_object(current_offset, self.data_offset)?;
            let node_name = self.trie_node_name(current_offset, current_node.namelen as usize)?;
            let ordering = cmp_prop_name(name_bytes, node_name.to_bytes());
            let child_offset = match ordering {
                std::cmp::Ordering::Less => current_node.left_offset(),
                std::cmp::Ordering::Greater => current_node.right_offset(),
                std::cmp::Ordering::Equal => return Ok(current_offset),
            };
            if let Some(child_offset) = child_offset {
                current_offset = child_offset;
                continue;
            }
            // Empty slot — allocate the new node, then re-borrow to store the
            // link (Release) before returning.
            let node = self.new_prop_trie_node(name)?;
            let current_node = self.mmap.to_object(current_offset, self.data_offset)?;
            let link = match ordering {
                std::cmp::Ordering::Less => &current_node.left,
                std::cmp::Ordering::Greater => &current_node.right,
                std::cmp::Ordering::Equal => unreachable!(),
            };
            return Ok(node.link(link));
        }
        Err(Error::FileValidation(
            "Trie node cycle detected while adding (corrupt property area)".into(),
        ))
    }

    /// Reads the NUL-terminated name that trails the trie node at
    /// `node_offset`. Both `namelen` (read from the already-validated node
    /// header by the caller) and the node offset come from the mmap and
    /// are untrusted: the trailing bytes are accessed through the mmap
    /// base pointer (whole-mapping provenance, unlike a scan derived from
    /// a `&PropertyTrieNode`), and `data()` rejects any length that would
    /// leave the mapping, so a corrupt `namelen` fails with a typed error
    /// instead of an out-of-bounds read.
    fn trie_node_name(&self, node_offset: NodeOffset, namelen: usize) -> Result<&CStr> {
        let name_offset = node_offset.name()?;
        let len_with_nul = namelen.checked_add(1).ok_or_else(|| {
            Error::FileValidation(format!("Trie node namelen overflow: {namelen}"))
        })?;
        // SAFETY: [R-OFF] — `name_offset` is the `WriteOnceStr` trailing a
        // node reached through an acquire link load (in `add`, possibly a
        // node this writer just linked); its bytes were written once while
        // fresh [SM2](b). A corrupt in-bounds `namelen` that stretches into
        // a mutable slot is [SM5].
        let bytes = unsafe {
            self.mmap
                .data(name_offset.get(), self.data_offset, len_with_nul)
        }?;
        CStr::from_bytes_until_nul(bytes).map_err(|e| {
            Error::FileValidation(format!(
                "Trie node name at offset {name_offset} missing NUL terminator: {e}"
            ))
        })
    }

    /// Searches the sibling BST at `trie_offset` for the segment `name`.
    /// `Ok(None)`: no such segment; the caller reports the full name.
    fn find_prop_trie_node(
        &self,
        trie_offset: NodeOffset,
        name: &str,
    ) -> Result<Option<NodeOffset>> {
        let name_bytes = name.as_bytes();
        let mut current_offset = trie_offset;
        // A corrupt file can link BST nodes into a cycle; a distinct-node
        // walk can visit at most as many nodes as fit in the data region,
        // so exceeding that proves a loop — fail instead of spinning.
        let max_steps = self.pa_data_size / mem::size_of::<PropertyTrieNode>();
        for _ in 0..=max_steps {
            let current = self.mmap.to_object(current_offset, self.data_offset)?;
            let node_name = self.trie_node_name(current_offset, current.namelen as usize)?;
            let next_offset = match cmp_prop_name(name_bytes, node_name.to_bytes()) {
                std::cmp::Ordering::Less => current.left_offset(),
                std::cmp::Ordering::Greater => current.right_offset(),
                std::cmp::Ordering::Equal => return Ok(Some(current_offset)),
            };
            match next_offset {
                Some(next_offset) => current_offset = next_offset,
                None => return Ok(None),
            }
        }
        Err(Error::FileValidation(
            "Trie node cycle detected (corrupt property area)".into(),
        ))
    }

    #[cfg(feature = "builder")]
    fn allocate_obj<K>(&mut self, size: usize) -> Result<Fresh<K>> {
        // [R-WRITE]: the `bytes_used` store below and every store into the
        // returned allocation need a writable mapping.
        self.mmap.require_writable()?;
        let aligned = crate::bionic_align(size, mem::size_of::<u32>());
        // Relaxed: the single writer [SM4] is the only reader of
        // `bytes_used`; publication of the allocated object goes through
        // the Release store in `Fresh::link`, not this field.
        let offset = self.property_area().bytes_used.load(Ordering::Relaxed);

        // Convert aligned to u32 with overflow check
        let aligned_u32 = u32::try_from(aligned).map_err(|_| {
            Error::FileSize(format!("Aligned size too large to fit in u32: {}", aligned))
        })?;

        // checked_add to prevent overflow
        let new_offset = offset.checked_add(aligned_u32).ok_or_else(|| {
            Error::FileSize(format!(
                "Offset overflow: {} + {} would exceed u32::MAX",
                offset, aligned_u32
            ))
        })?;

        // Bounds check. Widen to u64 instead of truncating `pa_data_size`
        // with `as u32` — the module's checked-arithmetic discipline.
        // `AreaFull`, not `FileSize`: exhausting the fixed 128 KiB area is a
        // reachable operational condition (bionic returns false), not a
        // corrupt-file diagnosis — callers must be able to tell them apart.
        if u64::from(new_offset) > self.pa_data_size as u64 {
            error!(
                "Property area full: new_offset={} > pa_data_size={}",
                new_offset, self.pa_data_size
            );
            return Err(Error::AreaFull(format!(
                "property area full: {} + {} = {} > {}",
                offset, aligned_u32, new_offset, self.pa_data_size
            )));
        }

        self.property_area()
            .bytes_used
            .store(new_offset, Ordering::Relaxed);
        Ok(Fresh::alloc(offset, aligned, self.mmap.base_addr()))
    }

    /// `&mut T` over an unlinked allocation of this mapping \[SM3\].
    #[cfg(feature = "builder")]
    fn fresh_object_mut<T: MmapObject>(&mut self, fresh: &Fresh<T>) -> Result<&mut T> {
        let (offset, len) = fresh.range_in(self.mmap.base_addr()).ok_or_else(|| {
            Error::InvalidArgument("allocation token belongs to another mapping".into())
        })?;
        if mem::size_of::<T>() > len {
            return Err(Error::InvalidArgument(format!(
                "allocation of {len} bytes too small for {} bytes",
                mem::size_of::<T>()
            )));
        }
        // SAFETY: [SM3] — `fresh` is an unlinked allocation of this mapping
        // (`range_in` checked the mapping) and `T` fits in it; bounds,
        // alignment and PROT_WRITE are checked by `to_object_mut`.
        unsafe { self.mmap.to_object_mut::<T>(offset, self.data_offset) }
    }

    /// `len` bytes at `at` inside an unlinked allocation of this mapping
    /// \[SM3\].
    #[cfg(feature = "builder")]
    fn fresh_bytes_mut<K>(&mut self, fresh: &Fresh<K>, at: usize, len: usize) -> Result<&mut [u8]> {
        let (offset, fresh_len) = fresh.range_in(self.mmap.base_addr()).ok_or_else(|| {
            Error::InvalidArgument("allocation token belongs to another mapping".into())
        })?;
        match at.checked_add(len) {
            Some(end) if end <= fresh_len => {}
            _ => {
                return Err(Error::InvalidArgument(format!(
                    "range {at}+{len} exceeds allocation of {fresh_len} bytes"
                )))
            }
        }
        let start = offset
            .checked_add(at)
            .ok_or_else(|| Error::FileValidation(format!("Offset overflow: {offset} + {at}")))?;
        // SAFETY: [SM3] — the range lies inside `fresh` (checked above), an
        // unlinked allocation of this mapping; bounds and PROT_WRITE are
        // checked by `data_mut`.
        unsafe { self.mmap.data_mut(start, self.data_offset, len) }
    }

    /// Writes the NUL-terminated `name` into the `len + 1` bytes trailing
    /// the `header_size`-byte object of the fresh allocation `obj`. Goes
    /// through the mmap base pointer so the write carries whole-mapping
    /// provenance instead of escaping an object reference.
    #[cfg(feature = "builder")]
    fn write_trailing_name<K>(
        &mut self,
        obj: &Fresh<K>,
        header_size: usize,
        name: &str,
    ) -> Result<()> {
        let dst = self.fresh_bytes_mut(obj, header_size, name.len() + 1)?;
        dst[..name.len()].copy_from_slice(name.as_bytes());
        dst[name.len()] = 0;
        Ok(())
    }

    /// Allocates an *unlinked* trie node. Private: the only legitimate
    /// consumer is `add`, whose publish protocol (write name bytes, then
    /// `Fresh::link`) must not be bypassable from elsewhere in the crate.
    #[cfg(feature = "builder")]
    fn new_prop_trie_node(&mut self, name: &str) -> Result<Fresh<PropertyTrieNode>> {
        let node = self.allocate_obj::<PropertyTrieNode>(
            mem::size_of::<PropertyTrieNode>() + name.len() + 1,
        )?;
        self.fresh_object_mut(&node)?.init_header(name.len() as u32);
        self.write_trailing_name(&node, mem::size_of::<PropertyTrieNode>(), name)?;
        Ok(node)
    }

    /// Allocates an *unlinked* property entry — private for the same
    /// publish-protocol reason as [`Self::new_prop_trie_node`].
    #[cfg(feature = "builder")]
    fn new_prop_info(&mut self, name: &str, value: &str) -> Result<Fresh<PropertyInfo>> {
        let entry =
            self.allocate_obj::<PropertyInfo>(mem::size_of::<PropertyInfo>() + name.len() + 1)?;

        // `>=`, not `>`: the short slot needs room for the NUL terminator,
        // so a value of exactly PROP_VALUE_MAX bytes must go out-of-line.
        // bionic draws the same boundary (`valuelen >= PROP_VALUE_MAX` is
        // long); with `>` the 92-byte case was silently truncated to 91
        // bytes while the serial recorded a length of 92.
        if value.len() >= crate::wire::PROP_VALUE_MAX {
            let long = self.allocate_obj::<WriteOnceStr>(value.len() + 1)?;

            let target = self.fresh_bytes_mut(&long, 0, value.len() + 1)?;
            target[0..value.len()].copy_from_slice(value.as_bytes());
            target[value.len()] = 0; // Add null terminator

            // `allocate_obj` offsets grow monotonically, so this cannot
            // fail — but the invariant lives in another function, so keep
            // the module's checked-arithmetic discipline.
            let relative_offset = long.offset_from(&entry).ok_or_else(|| {
                Error::FileValidation("Long allocation not after its entry".into())
            })?;

            self.fresh_object_mut(&entry)?
                .init_with_long_offset(relative_offset);
        } else {
            self.fresh_object_mut(&entry)?.init_with_value(value);
        };
        self.write_trailing_name(&entry, mem::size_of::<PropertyInfo>(), name)?;

        Ok(entry)
    }

    pub(crate) fn property_info(&self, offset: EntryOffset) -> Result<&PropertyInfo> {
        self.mmap.to_object(offset, self.data_offset)
    }

    /// Reads the NUL-terminated name trailing the `PropertyInfo` at
    /// `offset`. The entry does not store its name length, so the scan is
    /// bounded by the end of the mapping. Accessed through the mmap base
    /// pointer, not the entry reference — see `trie_node_name`.
    pub(crate) fn property_info_name(&self, offset: EntryOffset) -> Result<&CStr> {
        // Validate the header first so a bogus offset fails with a typed
        // error instead of an arbitrary scan.
        let _ = self.property_info(offset)?;
        self.mmap.cstr_at(offset.name()?, self.data_offset)
    }

    /// Reads the value of the entry at `pi_offset`, named `name`, as raw
    /// bytes: the short variant is snapshotted into `buf` (byte-wise
    /// atomic), the long variant (\[R-UNION\]: `ro.` names only) is borrowed
    /// from the mmap (long entries are write-once) via
    /// [`Self::long_property_value`]. Builder-gated: the only caller is the
    /// update path's backup snapshot (readers use the seqlock loop's own
    /// accessors instead).
    #[cfg(feature = "builder")]
    pub(crate) fn property_value_bytes<'a>(
        &'a self,
        pi_offset: EntryOffset,
        name: &[u8],
        buf: &'a mut [u8; crate::wire::PROP_VALUE_MAX],
    ) -> Result<&'a [u8]> {
        let pi = self.property_info(pi_offset)?;
        if pi.is_long_for(name) {
            self.long_property_value(pi_offset)
        } else {
            Ok(pi.short_value_bytes(buf))
        }
    }

    /// Resolves the out-of-line bytes of a long entry. Long entries are
    /// write-once, so the returned slice is stable for the mapping's
    /// lifetime — callers may hoist it out of seqlock retry loops. The
    /// offset is resolved through the mmap base pointer (whole-mapping
    /// provenance) and validated against corrupt values.
    ///
    /// An entry whose own trailing name does not start with `ro.` is
    /// `Error::FileValidation`, whatever name the caller looked it up by
    /// \[R-UNION\].
    pub(crate) fn long_property_value(&self, pi_offset: EntryOffset) -> Result<&[u8]> {
        let pi = self.property_info(pi_offset)?;
        // [R-UNION]: `offset` is loaded only if the entry's own name starts
        // with `ro.` — the name `backup_and_apply_write` refuses by, so the
        // writer never rewrites an entry read here (a non-`ro.` entry may
        // be rewritten in place, and loading its `offset` would race those
        // byte stores and steer `cstr_at`'s non-atomic read). Hence checked
        // before `long_offset`. The caller decided by its lookup name,
        // which differs from this one only in a corrupt file. Differs from
        // bionic, whose `ReadCallback` decides by `pi->name` alone: an
        // entry whose own name is not `ro.` is read through bionic's
        // mutable path but is `FileValidation` here; a LONG `ro.` entry
        // reached by a non-`ro.` lookup is read long by bionic but as the
        // short slot (the legacy error string) by `read_with_callback`.
        // Both need a corrupt trie.
        let name = self.property_info_name(pi_offset)?;
        if !name.to_bytes().starts_with(b"ro.") {
            return Err(Error::FileValidation(format!(
                "long lookup of entry {name:?}, which is not a `ro.` entry (corrupt property area)"
            )));
        }
        let rel = pi.long_offset()?;
        // The out-of-line value must start past the entry header AND its
        // trailing name — an offset into either would silently return the
        // entry's own bytes as the value. The minimum legal offset is the
        // next u32-aligned position after the name's NUL, exactly how
        // `new_prop_info` lays the entry out.
        let name_len = name.to_bytes().len();
        let min_rel = crate::bionic_align(
            mem::size_of::<PropertyInfo>() + name_len + 1,
            mem::size_of::<u32>(),
        );
        if (rel as usize) < min_rel {
            return Err(Error::FileValidation(format!(
                "Long property offset {rel} points inside the entry (min {min_rel})"
            )));
        }
        // [R-OFF](3): the only `long_value` call, after the name check.
        let value_offset = pi_offset.long_value(rel)?;
        Ok(self
            .mmap
            .cstr_at(value_offset, self.data_offset)?
            .to_bytes())
    }
}

/// Read-only shared mapping of `property_info` \[IMM\]. The only place the
/// crate reads a whole mapped file as plain `&[u8]`.
pub(crate) struct ImmutableFileMap(MemoryMap);

impl ImmutableFileMap {
    pub(crate) fn open(file: File, size: usize) -> Result<Self> {
        Ok(Self(MemoryMap::new(file, size, false)?))
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: [IMM] — the file is replaced only by rename, never
        // written in place while mapped ([R-SIZE] for its length). Offset 0
        // and the mapping's own size are in bounds.
        unsafe { self.0.data(0, 0, self.0.size) }
            .expect("offset 0 and the mapping's own size are in bounds")
    }
}

// MemoryMap is a wrapper for the memory-mapped file.
// It provides the safe access to the memory-mapped file.
pub(crate) struct MemoryMap {
    data: *mut u8,
    size: usize,
    /// Whether the mapping was created PROT_READ|PROT_WRITE. Mutable
    /// accessors check this so a mut reference over a PROT_READ mapping
    /// (which would SIGSEGV on first write) is a typed error instead.
    writable: bool,
}

// Manual impl so `data` is never printed: an ASLR base address in logs has
// no diagnostic value (same policy as `check_size`'s error message), and a
// derived impl would leak it through every `{:?}` of the containing
// structs (`PropertyAreaMap` derives Debug).
impl Debug for MemoryMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryMap")
            .field("size", &self.size)
            .field("writable", &self.writable)
            .finish_non_exhaustive()
    }
}

// SAFETY: `data` is owned by this MemoryMap and valid for `size` bytes
// until `Drop` calls `munmap`; the pointer is never changed after
// construction. Accesses to the mapped bytes from any thread follow
// [SM1]–[SM5].
unsafe impl Send for MemoryMap {}

// SAFETY: `&self` methods reach mapped bytes only through atomics
// ([SM2](a)), write-once bytes (`data`, an `unsafe fn` whose callers cite
// [SM2](b) or [IMM]) and `MmapObject` types; non-atomic writes need
// `&mut self` and a fresh range [SM3]. A borrow excludes nothing across
// mappings [SM1].
unsafe impl Sync for MemoryMap {}

impl MemoryMap {
    fn new(file: File, size: usize, writable: bool) -> Result<Self> {
        debug!("Creating memory map: size={size}, writable={writable}");

        if size == 0 {
            return Err(Error::FileValidation(
                "Cannot mmap zero-sized region".into(),
            ));
        }

        let flags = if writable {
            mm::ProtFlags::READ.union(mm::ProtFlags::WRITE)
        } else {
            mm::ProtFlags::READ
        };

        // SAFETY: `file` is a valid owned `File`, `size > 0` is checked above,
        // and `mm::mmap` reports failure via `Result` rather than `MAP_FAILED`.
        let memory_area = unsafe {
            mm::mmap(
                std::ptr::null_mut(),
                size,
                flags,
                mm::MapFlags::SHARED,
                file,
                0,
            )
        }
        .map_err(IntoError::into_error)? as *mut u8;

        Ok(Self {
            data: memory_area,
            size,
            writable,
        })
    }

    /// Rejects mutable access to read-only mappings \[R-WRITE\]. Writing
    /// through a PROT_READ mapping kills the process with SIGSEGV — fail
    /// with a typed error at the accessor instead.
    fn require_writable(&self) -> Result<()> {
        if !self.writable {
            return Err(Error::PermissionDenied(
                "attempted mutable access to a read-only property mapping".into(),
            ));
        }
        Ok(())
    }

    /// Base address, used only to tie a `Fresh` token to its mapping.
    #[cfg(feature = "builder")]
    fn base_addr(&self) -> usize {
        self.data as usize
    }

    /// Non-atomic view of `size` bytes at `offset + base`.
    ///
    /// # Safety
    ///
    /// The range must be \[SM2\](b) write-once bytes, or lie in an
    /// `ImmutableFileMap` under \[IMM\]; a corrupt in-bounds range that
    /// overlaps a mutable slot is the \[SM5\] residual. This function checks
    /// only bounds.
    unsafe fn data(&self, offset: usize, base: usize, size: usize) -> Result<&[u8]> {
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, size)?;
        // SAFETY: `offset + size <= self.size` (checked above), so the slice
        // lies within the mapping; `u8` has no alignment requirement; the
        // lifetime is tied to `&self`. Race-freedom is the caller's
        // obligation (# Safety).
        Ok(unsafe { std::slice::from_raw_parts(self.data.add(offset) as *const u8, size) })
    }

    /// Mutable view of `size` bytes at `offset + base`.
    ///
    /// # Safety
    ///
    /// The range must be fresh per \[SM3\]. This function checks bounds and
    /// PROT_WRITE.
    #[cfg(feature = "builder")]
    unsafe fn data_mut(&mut self, offset: usize, base: usize, size: usize) -> Result<&mut [u8]> {
        self.require_writable()?;
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, size)?;
        // SAFETY: `offset + size <= self.size` and the mapping is PROT_WRITE
        // (checked above); `u8` has no alignment requirement. Exclusive
        // access to the bytes is the caller's obligation (# Safety).
        Ok(unsafe { std::slice::from_raw_parts_mut(self.data.add(offset), size) })
    }

    /// Returns `offset + base` after verifying neither the addition nor the
    /// final value overflow `usize`. Wrapping behavior in release builds would
    /// otherwise let later bounds checks be silently bypassed.
    fn checked_offset(&self, offset: usize, base: usize) -> Result<usize> {
        offset
            .checked_add(base)
            .ok_or_else(|| Error::FileValidation(format!("Offset overflow: {offset} + {base}")))
    }

    fn check_size(&self, offset: usize, size: usize) -> Result<()> {
        let end = offset
            .checked_add(size)
            .ok_or_else(|| Error::FileValidation(format!("Size overflow: {offset} + {size}")))?;
        if end > self.size {
            // Deliberately no base-pointer in the message: an ASLR address
            // in logs has no diagnostic value here.
            error!(
                "Memory access out of bounds: {} + {} > {}",
                offset, size, self.size
            );
            return Err(Error::FileValidation(format!(
                "Invalid offset: {end} > {}",
                self.size
            )));
        }
        Ok(())
    }

    /// Verifies that `self.data.add(offset)` produces a pointer with the
    /// required alignment for `T`. The mmap base is page-aligned, so this
    /// reduces to a check on `offset % align_of::<T>()`.
    fn check_alignment<T>(&self, offset: usize) -> Result<()> {
        let align = mem::align_of::<T>();
        // Plain address arithmetic — no pointer is formed or dereferenced,
        // so no `unsafe` surface. (`wrapping_add`: the caller has already
        // bounds-checked `offset`, and an alignment check doesn't care
        // about wraparound.)
        let ptr_addr = (self.data as usize).wrapping_add(offset);
        if ptr_addr % align != 0 {
            return Err(Error::FileValidation(format!(
                "Misaligned object at offset {offset}: required align={align}, addr={ptr_addr:#x}"
            )));
        }
        Ok(())
    }

    /// `&T` at `offset + base` (`base`: the area's data offset, or 0 for
    /// `HEADER`).
    fn to_object<T: MmapObject>(&self, offset: AreaOffset<T>, base: usize) -> Result<&T> {
        let offset = self.checked_offset(offset.get(), base)?;
        self.check_size(offset, mem::size_of::<T>())?;
        self.check_alignment::<T>(offset)?;
        // SAFETY: bounds and alignment are checked above; `T: MmapObject`
        // makes every bit pattern valid and restricts its non-atomic
        // fields to [SM2](b) reads. [R-OFF]: `AreaOffset<T>` addresses a
        // published `T` or one this writer has just linked. Lifetime tied
        // to `&self`.
        Ok(unsafe { &*(self.data.add(offset) as *const T) })
    }

    /// `&mut T` at `offset + base`.
    ///
    /// # Safety
    ///
    /// The range must be fresh per \[SM3\]. This function checks bounds,
    /// alignment and PROT_WRITE.
    unsafe fn to_object_mut<T: MmapObject>(
        &mut self,
        offset: usize,
        base: usize,
    ) -> Result<&mut T> {
        self.require_writable()?;
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, mem::size_of::<T>())?;
        self.check_alignment::<T>(offset)?;
        // SAFETY: bounds, alignment and PROT_WRITE are checked above;
        // `T: MmapObject` makes every bit pattern valid. Exclusive access
        // to the bytes is the caller's obligation (# Safety).
        Ok(unsafe { &mut *(self.data.add(offset) as *mut T) })
    }

    /// NUL-terminated string at `offset`; the scan is bounded by the end of
    /// the mapping. The slice is derived from the mmap base pointer, so it
    /// carries whole-mapping provenance.
    ///
    /// The NUL search runs over the byte-wise-atomic view and stops AT the
    /// first NUL — it never reads past it. A plain `&[u8]` scan
    /// (`CStr::from_bytes_until_nul` → word-sized memchr) would over-read
    /// up to 7 bytes past the NUL, and with 4-byte allocation alignment
    /// those bytes can belong to the next allocated object — e.g. a
    /// `PropertyInfo` serial word being atomically rewritten by another
    /// process's writer. The returned `&CStr` (and the non-atomic re-read
    /// constructing it) covers only `[offset, nul]`.
    ///
    /// Residual gap \[SM5\]: if a *corrupt* file makes this range alias a
    /// mutable slot (e.g. a bogus long-property offset pointing into another
    /// entry's value), the non-atomic re-read can still race a concurrent
    /// writer. Sealing that would require returning an owned copy assembled
    /// from atomic loads — an allocation on the lookup hot path.
    fn cstr_at(&self, offset: AreaOffset<WriteOnceStr>, base: usize) -> Result<&CStr> {
        let offset = self.checked_offset(offset.get(), base)?;
        let remaining = self.size.checked_sub(offset).ok_or_else(|| {
            Error::FileValidation(format!("Offset past mmap: {offset} > {}", self.size))
        })?;
        // SAFETY: `offset + remaining == self.size`, so the slice lies
        // within the mapping; `AtomicU8` has the size and alignment of `u8`
        // (asserted in property_info.rs). [R-OFF]: `offset` addresses a
        // `WriteOnceStr`. The slice spans to the end of the mapping, and
        // the bytes after the string's NUL may belong to later allocations;
        // only loads are made through it, and only up to and including the
        // first NUL — bytes of the write-once string [SM2](b), whose other
        // accesses are reads. A corrupt offset is [SM5]. Lifetime tied to
        // `&self`.
        let cells = unsafe {
            std::slice::from_raw_parts(
                self.data.add(offset) as *const std::sync::atomic::AtomicU8,
                remaining,
            )
        };
        let nul = cells
            .iter()
            .position(|c| c.load(Ordering::Relaxed) == 0)
            .ok_or_else(|| {
                Error::FileValidation(format!("No NUL terminator at offset {offset}"))
            })?;
        // SAFETY: [R-OFF], [SM2](b) — `[offset, nul]` is the write-once
        // string `offset` addresses, up to its NUL; a corrupt offset is
        // [SM5].
        let bytes = unsafe { self.data(offset, 0, nul + 1) }?;
        // Checked (not `_unchecked`) construction: a corrupt file breaking
        // the write-once invariant should surface as a typed error, not UB.
        CStr::from_bytes_with_nul(bytes)
            .map_err(|e| Error::FileValidation(format!("Invalid C string at offset {offset}: {e}")))
    }

    /// Byte-wise-atomic view of the first `size` bytes of the dirty backup
    /// slot, which — like the value slots — may be rewritten by another
    /// process's writer while a reader snapshots it.
    ///
    /// `AtomicU8::store` is a safe call but SIGSEGVs on a PROT_READ
    /// mapping — callers that *store* through the returned slice hold an
    /// `AreaWriteAccess` \[R-WRITE\] (this accessor takes `&self` because
    /// readers share it).
    fn atomic_data(
        &self,
        offset: AreaOffset<BackupSlot>,
        base: usize,
        size: usize,
    ) -> Result<&[std::sync::atomic::AtomicU8]> {
        // The slot's length (`PropertyArea::init` reserves it): a longer
        // view would reach the first allocated object, whose write-once
        // bytes are read non-atomically.
        let slot_len = crate::bionic_align(crate::wire::PROP_VALUE_MAX, mem::size_of::<u32>());
        if size > slot_len {
            return Err(Error::InvalidArgument(format!(
                "Backup slot access too long: {size} (max: {slot_len})"
            )));
        }
        let offset = self.checked_offset(offset.get(), base)?;
        self.check_size(offset, size)?;
        // SAFETY: `offset + size <= self.size` (checked above); `AtomicU8`
        // has the size and alignment of `u8` (asserted in
        // property_info.rs). [R-OFF]: `offset` is the backup slot and
        // `size` is at most its length, so every byte of the view belongs
        // to the slot, whose loads and stores are all byte-wise atomic
        // [SM2](a). Foreign writers: [R-INIT]. Lifetime tied to `&self`.
        Ok(unsafe {
            std::slice::from_raw_parts(
                self.data.add(offset) as *const std::sync::atomic::AtomicU8,
                size,
            )
        })
    }
}

impl std::ops::Drop for MemoryMap {
    fn drop(&mut self) {
        // SAFETY: `self.data` was returned by `mm::mmap` with `self.size`
        // bytes in `MemoryMap::new` and has not been unmapped since.
        let result = unsafe { mm::munmap(self.data as _, self.size) };
        if let Err(e) = result {
            error!("Failed to unmap memory: {e:?}");
        }
    }
}

#[cfg(all(test, feature = "builder", not(target_os = "android")))]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Unique scratch dir under `temp_dir()`, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rsprops_area_{tag}_{}_{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_new_rw_publishes_by_rename() {
        let tmp = TempDir::new("rename");
        let path = tmp.0.join("ctx");

        let mut first = PropertyAreaMap::new_rw(&path, None).unwrap();
        first.add("a.b", "1").unwrap();
        let first_ino = std::fs::metadata(&path).unwrap().ino();

        // Mid-creation state of a restarting writer: the new area exists
        // only under its temp name, so a reader opening the final name
        // still gets the previous, complete area.
        let (second, temp) = PropertyAreaMap::create_unpublished(&path, None).unwrap();
        assert!(temp.exists(), "temp area must exist before publish");
        let reader = PropertyAreaMap::new_ro(&path).expect("final name keeps the old area");
        assert!(reader.find("a.b").is_ok());
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), first_ino);

        PropertyAreaMap::publish(&temp, &path).unwrap();
        assert!(!temp.exists(), "temp name must be gone after publish");
        let reader = PropertyAreaMap::new_ro(&path).expect("published area opens");
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), first_ino);
        assert!(matches!(reader.find("a.b"), Err(Error::NotFound(_))));
        drop(second);
    }

    #[test]
    fn find_empty_segment_is_not_found() {
        let tmp = TempDir::new("empty_seg");
        let mut area = PropertyAreaMap::new_rw(&tmp.0.join("ctx"), None).unwrap();
        area.add("a.b", "1").unwrap();
        for name in ["", "a.", ".a", "a..b"] {
            assert!(
                matches!(area.find(name), Err(Error::NotFound(ref n)) if n == name),
                "find({name:?}) must be NotFound"
            );
        }
    }

    #[test]
    fn fresh_token_from_other_map_is_rejected() {
        let tmp = TempDir::new("fresh");
        let mut a = PropertyAreaMap::new_rw(&tmp.0.join("a"), None).unwrap();
        let mut b = PropertyAreaMap::new_rw(&tmp.0.join("b"), None).unwrap();
        let token = a.allocate_obj(mem::size_of::<PropertyTrieNode>()).unwrap();
        assert!(b.fresh_object_mut::<PropertyTrieNode>(&token).is_err());
        assert!(b.fresh_bytes_mut(&token, 0, 1).is_err());
        assert!(a.fresh_object_mut::<PropertyTrieNode>(&token).is_ok());
        // Leak the allocation into a dead slot: tokens must be consumed.
        let _ = token.link(&AtomicU32::new(0));
    }

    /// `atomic_data` refuses a view longer than the backup slot, whatever
    /// its caller checked \[R-OFF\]: the next bytes belong to the first
    /// allocated object.
    #[test]
    fn atomic_data_stays_in_backup_slot() {
        let tmp = TempDir::new("slot");
        let area = PropertyAreaMap::new_rw(&tmp.0.join("ctx"), None).unwrap();
        let slot = crate::bionic_align(crate::wire::PROP_VALUE_MAX, mem::size_of::<u32>());
        assert!(area
            .mmap
            .atomic_data(BACKUP_SLOT, area.data_offset, slot)
            .is_ok());
        assert!(matches!(
            area.mmap
                .atomic_data(BACKUP_SLOT, area.data_offset, slot + 1),
            Err(Error::InvalidArgument(_))
        ));
    }

    /// `ROOT` has no trailing name \[R-OFF\]: the bytes after the root
    /// node are the mutable backup slot, not a write-once string.
    #[test]
    fn root_node_has_no_name() {
        assert!(matches!(ROOT.name(), Err(Error::FileValidation(_))));
    }

    /// Area files (context areas and `properties_serial`) get mode 0444
    /// whatever the umask, so readers of other uids can open them. Runs
    /// again in a child process: the umask is process-wide and would
    /// change files that other tests create in parallel.
    #[test]
    fn new_rw_mode_ignores_umask() {
        const CHILD_ENV: &str = "RSPROPERTIES_AREA_MODE_TEST_CHILD";
        const TEST: &str = "property_area::tests::new_rw_mode_ignores_umask";
        if std::env::var_os(CHILD_ENV).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([TEST, "--exact", "--test-threads=1"])
                .env(CHILD_ENV, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "child: {:?}: {stdout}{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        rustix::process::umask(fs::Mode::from_raw_mode(0o077));
        let tmp = TempDir::new("mode");
        let path = tmp.0.join("ctx");
        let _area = PropertyAreaMap::new_rw(&path, None).unwrap();
        let mode = std::fs::metadata(&path).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o444, "mode {mode:o}");
    }

    /// `add` itself refuses a long non-`ro.` value, whatever its caller
    /// checked; `ro.` names still get a LONG entry.
    #[test]
    fn add_rejects_long_non_ro_value() {
        let tmp = TempDir::new("longval");
        let mut area = PropertyAreaMap::new_rw(&tmp.0.join("ctx"), None).unwrap();
        let long = "v".repeat(crate::wire::PROP_VALUE_MAX);
        assert!(matches!(
            area.add("test.long", &long),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(area.find("test.long"), Err(Error::NotFound(_))));
        area.add("ro.long", &long).unwrap();
    }

    /// `backup_and_apply_write` refuses a `ro.` entry by its own name, so
    /// a LONG entry's `offset` is never overwritten by short-slot stores
    /// \[R-UNION\], whatever its caller checked.
    #[test]
    fn apply_write_refuses_ro_entry() {
        let tmp = TempDir::new("ro_write");
        let mut area = PropertyAreaMap::new_rw(&tmp.0.join("ctx"), None).unwrap();
        let long = "v".repeat(crate::wire::PROP_VALUE_MAX);
        area.add("ro.long", &long).unwrap();
        let (_, off) = area.find("ro.long").unwrap();
        let serial_of = |area: &PropertyAreaMap| {
            area.property_info(off)
                .unwrap()
                .serial
                .load(Ordering::Relaxed)
        };
        let serial = serial_of(&area);

        assert!(matches!(
            area.backup_and_apply_write(off, b"", "x"),
            Err(Error::ReadOnlyProperty(name)) if name == "ro.long"
        ));
        assert_eq!(area.long_property_value(off).unwrap(), long.as_bytes());
        assert_eq!(
            serial_of(&area),
            serial,
            "a refused write must not touch the serial"
        );

        area.add("test.short", "1").unwrap();
        let (_, off) = area.find("test.short").unwrap();
        area.backup_and_apply_write(off, b"1", "2").unwrap();
    }

    /// `long_property_value` decides by the entry's own name: a non-`ro.`
    /// entry with bit 16 set, reached by a lookup name starting with `ro.`
    /// in a corrupt file, is refused before its value bytes are read as an
    /// offset \[R-UNION\].
    #[test]
    fn long_lookup_refuses_non_ro_entry() {
        let tmp = TempDir::new("long_non_ro");
        let mut area = PropertyAreaMap::new_rw(&tmp.0.join("ctx"), None).unwrap();
        area.add("debug.x", &"v".repeat(64)).unwrap();
        let (pi, off) = area.find("debug.x").unwrap();
        pi.serial
            .fetch_or(crate::property_info::LONG_FLAG, Ordering::Relaxed);
        match area.long_property_value(off) {
            Err(Error::FileValidation(msg)) => {
                assert!(msg.contains("not a `ro.` entry"), "{msg}")
            }
            other => panic!("expected the name refusal, got {other:?}"),
        }
    }
}
