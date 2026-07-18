// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{
    ffi::CStr,
    fmt::Debug,
    fs::{File, OpenOptions},
    mem,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::atomic::AtomicU32,
};

use crate::errors::*;
use log::{debug, error, info, warn};
use rustix::{fs, mm};

use crate::property_info::PropertyInfo;

const PA_SIZE: u64 = 128 * 1024;
const PROP_AREA_MAGIC: u32 = 0x504f5250;
const PROP_AREA_VERSION: u32 = 0xfc6ed0ab;

/// Marker for types that may be materialized in-place from property-area
/// mmap bytes via [`MemoryMap::to_object`] / [`MemoryMap::to_object_mut`].
///
/// # Safety
///
/// Implementors must be valid for **every** bit pattern (no niches — no
/// `bool`/enum/reference fields; `repr(C)` with primitive/atomic fields,
/// or unions/`UnsafeCell` thereof), tolerate arbitrary padding-byte
/// values, and follow this module's shared-mmap concurrency protocol when
/// accessed through `&T` (fields that can be rewritten after publication
/// are accessed exclusively through atomics + the seqlock serial). Without
/// this bound `to_object` would let any safe caller conjure UB by picking
/// a type with invalid-bit-pattern niches.
pub(crate) unsafe trait MmapObject {}

// SAFETY: u32/AtomicU32 fields only, `repr(C, align(4))`, every bit pattern
// valid; mutable-after-publication fields (serial, links, prop) are atomics
// driven by the module's seqlock/publish protocol.
unsafe impl MmapObject for PropertyArea {}
unsafe impl MmapObject for PropertyTrieNode {}
// SAFETY: AtomicU32 serial + byte array value/long-offset union, every bit
// pattern valid; value bytes are accessed byte-wise atomically under the
// seqlock protocol (see property_info.rs).
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

#[repr(C, align(4))]
pub(crate) struct PropertyTrieNode {
    pub(crate) namelen: u32,
    pub(crate) prop: AtomicU32,
    pub(crate) left: AtomicU32,
    pub(crate) right: AtomicU32,
    pub(crate) children: AtomicU32,
}

impl PropertyTrieNode {
    /// Initializes the fixed-size header only. The trailing name bytes are
    /// written by `PropertyAreaMap::new_prop_trie_node` through the mmap
    /// base pointer — writing them through `&mut self` would step outside
    /// this reference's provenance (it covers exactly
    /// `size_of::<PropertyTrieNode>()` bytes).
    #[cfg(feature = "builder")]
    fn init_header(&mut self, namelen: u32) {
        self.prop.store(0, std::sync::atomic::Ordering::Relaxed);
        self.left.store(0, std::sync::atomic::Ordering::Relaxed);
        self.right.store(0, std::sync::atomic::Ordering::Relaxed);
        self.children.store(0, std::sync::atomic::Ordering::Relaxed);
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
    bytes_used: u32,
    serial: AtomicU32,
    magic: u32,
    version: u32,
    reserved: [u32; 28],
}

impl PropertyArea {
    fn init(&mut self, magic: u32, version: u32) {
        self.serial.store(0, std::sync::atomic::Ordering::Relaxed);
        self.magic = magic;
        self.version = version;
        self.reserved = [0; 28];
        self.bytes_used = mem::size_of::<PropertyTrieNode>() as _;
        self.bytes_used += crate::bionic_align(crate::PROP_VALUE_MAX, mem::size_of::<u32>()) as u32;
    }

    pub(crate) fn serial(&self) -> &AtomicU32 {
        &self.serial
    }
}

#[derive(Debug)]
pub(crate) struct PropertyAreaMap {
    mmap: MemoryMap,
    data_offset: usize,
    pa_data_size: usize,
}

impl PropertyAreaMap {
    // Initialize the property area map with the given file to create a new property area map.
    pub(crate) fn new_rw(filename: &Path, context: Option<&CStr>) -> Result<Self> {
        debug!("Creating new read-write property area map: {filename:?}");

        // A leftover area file from a previous writer instance would make
        // the O_EXCL create below fail — and the 0444 mode means it could
        // not be reopened read-write either. AOSP avoids this via the fresh
        // tmpfs mounted at /dev on every boot; that assumption doesn't hold
        // for an arbitrary properties dir, so treat `new_rw` as "build a
        // fresh area" and remove any stale file first. O_EXCL still guards
        // the create itself (no symlink / pre-created-file substitution
        // between the unlink and the open). Note this is not mutual
        // exclusion between two concurrent `new_rw` callers — each can
        // unlink the other's file and succeed on a different inode, leaving
        // earlier readers attached to the orphaned one; the system-level
        // single-writer policy is a precondition, not something enforced
        // here.
        match std::fs::remove_file(filename) {
            Ok(()) => debug!("Removed stale property area file: {filename:?}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to remove stale property area file {filename:?}: {e}"),
        }

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
            .open(filename)
            .context_with_location(format!("Failed to create property area {filename:?}"))?;

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
            // reader-side denials.
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

        fs::ftruncate(&file, PA_SIZE)
            .map_err(Error::from)
            .context_with_location(format!("Failed to size property area {filename:?}"))?;

        let pa_size = PA_SIZE as usize;
        let pa_data_size = pa_size - std::mem::size_of::<PropertyArea>();

        let mut thiz = Self {
            mmap: MemoryMap::new(file, pa_size, true)?,
            data_offset: std::mem::size_of::<PropertyArea>(),
            pa_data_size,
        };

        thiz.property_area_mut()?
            .init(PROP_AREA_MAGIC, PROP_AREA_VERSION);

        info!("Successfully created read-write property area map: {filename:?}");
        Ok(thiz)
    }

    // Initialize the property area map with the given file to read-only property area map.
    //
    // Precondition (inherent to mmap-based IPC, same as bionic): the file
    // must not be truncated below the validated size while this mapping is
    // alive — an access past the shrunken EOF raises SIGBUS, which no
    // bounds check here can intercept. The system-level single-writer
    // policy (see `new_rw`) is what rules this out in practice.
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
            .to_object::<PropertyArea>(0, 0)
            .expect("PropertyArea's offset is zero. So, it must be valid.")
    }

    /// Whether the underlying mapping was created read-write.
    pub(crate) fn is_writable(&self) -> bool {
        self.mmap.writable
    }

    // `Result`, not `expect`: offset 0 is always in-bounds/aligned, but
    // `to_object_mut` also fails (by design) on a read-only mapping — that
    // must surface as a typed error, not a panic.
    fn property_area_mut(&mut self) -> Result<&mut PropertyArea> {
        self.mmap.to_object_mut::<PropertyArea>(0, 0)
    }

    // Find the property information with the given name.
    pub(crate) fn find(&self, name: &str) -> Result<(&PropertyInfo, u32)> {
        let mut remaining_name = name;
        let mut current_offset = 0usize;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                error!("Invalid property name (empty segment): '{name}'");
                return Err(Error::Parse(format!("Invalid property name: {name}")));
            }

            let subname = &remaining_name[0..substr_size];

            let children_offset = self
                .mmap
                .to_object::<PropertyTrieNode>(current_offset, self.data_offset)?
                .children
                .load(std::sync::atomic::Ordering::Acquire);
            if children_offset == 0 {
                return Err(Error::NotFound(name.to_owned()));
            }

            current_offset = self.find_prop_trie_node(children_offset, subname)? as usize;

            if sep.is_none() {
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
        }

        let prop_offset = self
            .mmap
            .to_object::<PropertyTrieNode>(current_offset, self.data_offset)?
            .prop
            .load(std::sync::atomic::Ordering::Acquire);
        if prop_offset != 0 {
            Ok((
                self.mmap
                    .to_object(prop_offset as usize, self.data_offset)?,
                prop_offset,
            ))
        } else {
            Err(Error::NotFound(name.to_owned()))
        }
    }

    // Add the property information with the given name and value.
    #[cfg(feature = "builder")]
    pub(crate) fn add(&mut self, name: &str, value: &str) -> Result<()> {
        debug!("Adding property: '{name}' = '{value}'");

        // An interior NUL would desync `namelen` from the NUL-scanned
        // trailing name: the entry becomes unreachable under its real name
        // while every retry re-allocates fresh trie nodes (leaking area
        // space until AreaFull) and enumeration shows a truncated ghost
        // name. Reject here — the single choke point every caller
        // (`SystemProperties::add`, the build.prop load path) goes through.
        crate::wire::validate_no_interior_nul("property name", name)?;

        let mut remaining_name = name;
        let mut current = 0;
        loop {
            let sep = remaining_name.find('.');
            let substr_size = match sep {
                Some(pos) => pos,
                None => remaining_name.len(),
            };

            if substr_size == 0 {
                error!("Invalid property name (empty segment): '{name}'");
                return Err(Error::Parse(format!("Invalid property name: {name}")));
            }

            let subname = &remaining_name[0..substr_size];

            let children_offset = self
                .mmap
                .to_object::<PropertyTrieNode>(current, self.data_offset)?
                .children
                .load(std::sync::atomic::Ordering::Acquire);
            let root_offset = if children_offset != 0 {
                children_offset
            } else {
                let offset = self.new_prop_trie_node(subname)?;
                self.mmap
                    .to_object::<PropertyTrieNode>(current, self.data_offset)?
                    .children
                    .store(offset, std::sync::atomic::Ordering::Release);
                offset
            };

            current = self.add_prop_trie_node(root_offset, subname)? as _;

            if sep.is_none() {
                break;
            }

            remaining_name = &remaining_name[substr_size + 1..];
        }

        let prop_offset = self
            .mmap
            .to_object::<PropertyTrieNode>(current, self.data_offset)?
            .prop
            .load(std::sync::atomic::Ordering::Acquire);

        if prop_offset == 0 {
            let offset = self.new_prop_info(name, value)?;
            // Atomic store through a shared reference — same publish pattern
            // as the `children` link above; no exclusive access needed.
            // Writability is guaranteed non-locally: this line is reachable
            // only after `new_prop_info` → `allocate_obj` →
            // `property_area_mut` succeeded, which requires a writable
            // mapping (`to_object`, unlike `to_object_mut`, does not check).
            self.mmap
                .to_object::<PropertyTrieNode>(current, self.data_offset)?
                .prop
                .store(offset, std::sync::atomic::Ordering::Release);
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
        let offset = mem::size_of::<PropertyTrieNode>();
        // Mirror the write side's bound: reads past the reserved slot
        // would return bytes of the first allocated object as "backup".
        let reserved = crate::bionic_align(crate::PROP_VALUE_MAX, mem::size_of::<u32>());
        if dst.len() >= reserved {
            return Err(Error::InvalidArgument(format!(
                "Backup read too long: {} (max: {})",
                dst.len(),
                reserved - 1
            )));
        }
        let src = self.mmap.atomic_data(offset, self.data_offset, dst.len())?;
        for (d, s) in dst.iter_mut().zip(src) {
            *d = s.load(std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    /// Backs up the entry's current value into the area's dirty backup
    /// slot, then runs the entry-side seqlock write (`apply_write`) — the
    /// only way to reach a `PropertyInfoWriter` from outside this module.
    ///
    /// The two steps are deliberately fused: readers that observe the dirty
    /// serial read the *backup slot*, not the entry, so publishing the
    /// dirty bit without a fresh backup would serve a previous update's
    /// backup bytes as this property's value. Keeping `set_dirty_backup_area`
    /// and `property_info_mut` private makes that ordering violation
    /// unrepresentable rather than merely documented.
    ///
    /// `backup` must hold the entry's current value bytes (snapshotted by
    /// the caller before taking `&mut self`).
    #[cfg(feature = "builder")]
    pub(crate) fn backup_and_apply_write(
        &mut self,
        pi_offset: u32,
        backup: &[u8],
        value: &str,
    ) -> Result<()> {
        self.set_dirty_backup_area(backup)?;
        // The published serial is deliberately not returned — the sole
        // caller has no use for it, and an unused `u32` invites callers to
        // treat it as something it isn't (futex waits need the serial
        // *address*, not its value).
        self.property_info_mut(pi_offset)?
            .writer()
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
    #[cfg(feature = "builder")]
    fn set_dirty_backup_area(&mut self, value: &[u8]) -> Result<()> {
        // The stores below go through `atomic_data` (a `&self` accessor), so
        // check writability explicitly — a PROT_READ mapping would SIGSEGV.
        self.mmap.require_writable()?;
        let offset = mem::size_of::<PropertyTrieNode>();
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
        let reserved = crate::bionic_align(crate::PROP_VALUE_MAX, mem::size_of::<u32>());
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
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        // Byte-wise atomic stores — concurrent readers in other processes
        // may snapshot the slot while we rewrite it (their seqlock re-check
        // discards the torn copy, but the accesses themselves must be
        // atomic to be well-defined).
        let dst = self.mmap.atomic_data(offset, self.data_offset, total_len)?;
        for (slot, &b) in dst.iter().zip(value) {
            slot.store(b, std::sync::atomic::Ordering::Relaxed);
        }
        dst[value.len()].store(0, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    // Add a new property trie node with the given name to the given trie node.
    // It uses trie offset to avoid the life time issue of the current trie node.
    #[cfg(feature = "builder")]
    fn add_prop_trie_node(&mut self, trie_offset: u32, name: &str) -> Result<u32> {
        let name_bytes = name.as_bytes();
        let mut current_offset = trie_offset;
        // Same cycle bound as `find_prop_trie_node`: the builder normally
        // walks only its own freshly-built file, but a corrupt link chain
        // must fail instead of hanging the writer.
        let max_steps = self.pa_data_size / mem::size_of::<PropertyTrieNode>();
        for _ in 0..=max_steps {
            let current_node = self
                .mmap
                .to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
            let node_name =
                self.trie_node_name(current_offset as usize, current_node.namelen as usize)?;
            let ordering = cmp_prop_name(name_bytes, node_name.to_bytes());
            let child_offset = match ordering {
                std::cmp::Ordering::Less => {
                    current_node.left.load(std::sync::atomic::Ordering::Acquire)
                }
                std::cmp::Ordering::Greater => current_node
                    .right
                    .load(std::sync::atomic::Ordering::Acquire),
                std::cmp::Ordering::Equal => return Ok(current_offset),
            };
            if child_offset != 0 {
                current_offset = child_offset;
                continue;
            }
            // Empty slot — allocate the new node, then re-borrow to store the
            // link (Release) before returning.
            let offset = self.new_prop_trie_node(name)?;
            let current_node = self
                .mmap
                .to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
            let link = match ordering {
                std::cmp::Ordering::Less => &current_node.left,
                std::cmp::Ordering::Greater => &current_node.right,
                std::cmp::Ordering::Equal => unreachable!(),
            };
            link.store(offset, std::sync::atomic::Ordering::Release);
            return Ok(offset);
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
    fn trie_node_name(&self, node_offset: usize, namelen: usize) -> Result<&CStr> {
        let name_offset = node_offset
            .checked_add(mem::size_of::<PropertyTrieNode>())
            .ok_or_else(|| {
                Error::FileValidation(format!("Trie node name offset overflow: {node_offset}"))
            })?;
        let len_with_nul = namelen.checked_add(1).ok_or_else(|| {
            Error::FileValidation(format!("Trie node namelen overflow: {namelen}"))
        })?;
        let bytes = self
            .mmap
            .data(name_offset, self.data_offset, len_with_nul)?;
        CStr::from_bytes_until_nul(bytes).map_err(|e| {
            Error::FileValidation(format!(
                "Trie node name at offset {name_offset} missing NUL terminator: {e}"
            ))
        })
    }

    fn find_prop_trie_node(&self, trie_offset: u32, name: &str) -> Result<u32> {
        let name_bytes = name.as_bytes();
        let mut current_offset = trie_offset;
        // A corrupt file can link BST nodes into a cycle; a distinct-node
        // walk can visit at most as many nodes as fit in the data region,
        // so exceeding that proves a loop — fail instead of spinning.
        let max_steps = self.pa_data_size / mem::size_of::<PropertyTrieNode>();
        for _ in 0..=max_steps {
            let current = self
                .mmap
                .to_object::<PropertyTrieNode>(current_offset as usize, self.data_offset)?;
            let node_name =
                self.trie_node_name(current_offset as usize, current.namelen as usize)?;
            let next_offset = match cmp_prop_name(name_bytes, node_name.to_bytes()) {
                std::cmp::Ordering::Less => current.left.load(std::sync::atomic::Ordering::Acquire),
                std::cmp::Ordering::Greater => {
                    current.right.load(std::sync::atomic::Ordering::Acquire)
                }
                std::cmp::Ordering::Equal => return Ok(current_offset),
            };
            if next_offset == 0 {
                return Err(Error::NotFound(name.to_owned()));
            }
            current_offset = next_offset;
        }
        Err(Error::FileValidation(
            "Trie node cycle detected (corrupt property area)".into(),
        ))
    }

    #[cfg(feature = "builder")]
    fn allocate_obj(&mut self, size: usize) -> Result<u32> {
        let aligned = crate::bionic_align(size, mem::size_of::<u32>());
        let offset = self.property_area().bytes_used;

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

        // Update bytes_used
        self.property_area_mut()?.bytes_used = new_offset;
        Ok(offset)
    }

    /// Writes the NUL-terminated `name` into the `len + 1` bytes trailing
    /// the object at `obj_offset` (of header size `header_size`). Goes
    /// through the mmap base pointer so the write carries whole-mapping
    /// provenance instead of escaping an object reference.
    #[cfg(feature = "builder")]
    fn write_trailing_name(
        &mut self,
        obj_offset: usize,
        header_size: usize,
        name: &str,
    ) -> Result<()> {
        let name_offset = obj_offset.checked_add(header_size).ok_or_else(|| {
            Error::FileValidation(format!("Trailing name offset overflow: {obj_offset}"))
        })?;
        let dst = self
            .mmap
            .data_mut(name_offset, self.data_offset, name.len() + 1)?;
        dst[..name.len()].copy_from_slice(name.as_bytes());
        dst[name.len()] = 0;
        Ok(())
    }

    #[cfg(feature = "builder")]
    pub(crate) fn new_prop_trie_node(&mut self, name: &str) -> Result<u32> {
        let new_offset = self.allocate_obj(mem::size_of::<PropertyTrieNode>() + name.len() + 1)?;
        let node = self
            .mmap
            .to_object_mut::<PropertyTrieNode>(new_offset as usize, self.data_offset)?;
        node.init_header(name.len() as u32);
        self.write_trailing_name(
            new_offset as usize,
            mem::size_of::<PropertyTrieNode>(),
            name,
        )?;
        Ok(new_offset)
    }

    #[cfg(feature = "builder")]
    pub(crate) fn new_prop_info(&mut self, name: &str, value: &str) -> Result<u32> {
        let new_offset = self.allocate_obj(mem::size_of::<PropertyInfo>() + name.len() + 1)?;

        // `>=`, not `>`: the short slot needs room for the NUL terminator,
        // so a value of exactly PROP_VALUE_MAX bytes must go out-of-line.
        // bionic draws the same boundary (`valuelen >= PROP_VALUE_MAX` is
        // long); with `>` the 92-byte case was silently truncated to 91
        // bytes while the serial recorded a length of 92.
        if value.len() >= crate::PROP_VALUE_MAX {
            let long_offset = self.allocate_obj(value.len() + 1)?;

            let target =
                self.mmap
                    .data_mut(long_offset as usize, self.data_offset, value.len() + 1)?;
            target[0..value.len()].copy_from_slice(value.as_bytes());
            target[value.len()] = 0; // Add null terminator

            // `allocate_obj` offsets grow monotonically, so this cannot
            // underflow — but the invariant lives in another function, so
            // keep the module's checked-arithmetic discipline.
            let relative_offset = long_offset.checked_sub(new_offset).ok_or_else(|| {
                Error::FileValidation(format!(
                    "Long allocation not after its entry: {long_offset} < {new_offset}"
                ))
            })?;

            let info = self
                .mmap
                .to_object_mut::<PropertyInfo>(new_offset as usize, self.data_offset)?;
            info.init_with_long_offset(relative_offset as _);
        } else {
            let info = self
                .mmap
                .to_object_mut::<PropertyInfo>(new_offset as usize, self.data_offset)?;
            info.init_with_value(value);
        };
        self.write_trailing_name(new_offset as usize, mem::size_of::<PropertyInfo>(), name)?;

        Ok(new_offset)
    }

    pub(crate) fn property_info(&self, offset: u32) -> Result<&PropertyInfo> {
        self.mmap.to_object(offset as usize, self.data_offset)
    }

    /// Returns a `&mut PropertyInfo` for the entry at `offset`. Private —
    /// the only legitimate route to a writer is `backup_and_apply_write`,
    /// which guarantees the dirty backup slot is written first. Together
    /// with `&mut PropertyAreaMap` it enforces single-writer inside one
    /// process via the borrow checker.
    #[cfg(feature = "builder")]
    fn property_info_mut(&mut self, offset: u32) -> Result<&mut PropertyInfo> {
        self.mmap.to_object_mut(offset as usize, self.data_offset)
    }

    /// Reads the NUL-terminated name trailing the `PropertyInfo` at
    /// `offset`. The entry does not store its name length, so the scan is
    /// bounded by the end of the mapping. Accessed through the mmap base
    /// pointer, not the entry reference — see `trie_node_name`.
    pub(crate) fn property_info_name(&self, offset: u32) -> Result<&CStr> {
        // Validate the header first so a bogus offset fails with a typed
        // error instead of an arbitrary scan.
        let _ = self.property_info(offset)?;
        let name_offset = (offset as usize)
            .checked_add(mem::size_of::<PropertyInfo>())
            .ok_or_else(|| {
                Error::FileValidation(format!("PropertyInfo name offset overflow: {offset}"))
            })?;
        self.mmap.cstr_at(name_offset, self.data_offset)
    }

    /// Reads the value of the entry at `pi_offset` as raw bytes: the short
    /// variant is snapshotted into `buf` (byte-wise atomic), the long
    /// variant is borrowed from the mmap (long entries are write-once) via
    /// [`Self::long_property_value`]. Builder-gated: the only caller is the
    /// update path's backup snapshot (readers use the seqlock loop's own
    /// accessors instead).
    #[cfg(feature = "builder")]
    pub(crate) fn property_value_bytes<'a>(
        &'a self,
        pi_offset: u32,
        buf: &'a mut [u8; crate::PROP_VALUE_MAX],
    ) -> Result<&'a [u8]> {
        let pi = self.property_info(pi_offset)?;
        if pi.is_long() {
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
    pub(crate) fn long_property_value(&self, pi_offset: u32) -> Result<&[u8]> {
        let pi = self.property_info(pi_offset)?;
        let rel = pi.long_offset()? as usize;
        // The out-of-line value must start past the entry header AND its
        // trailing name — an offset into either would silently return the
        // entry's own bytes as the value. The minimum legal offset is the
        // next u32-aligned position after the name's NUL, exactly how
        // `new_prop_info` lays the entry out.
        let name_len = self.property_info_name(pi_offset)?.to_bytes().len();
        let min_rel = crate::bionic_align(
            mem::size_of::<PropertyInfo>() + name_len + 1,
            mem::size_of::<u32>(),
        );
        if rel < min_rel {
            return Err(Error::FileValidation(format!(
                "Long property offset {rel} points inside the entry (min {min_rel})"
            )));
        }
        let value_offset = (pi_offset as usize).checked_add(rel).ok_or_else(|| {
            Error::FileValidation(format!("Long value offset overflow: {pi_offset} + {rel}"))
        })?;
        Ok(self
            .mmap
            .cstr_at(value_offset, self.data_offset)?
            .to_bytes())
    }
}

// MemoryMap is a wrapper for the memory-mapped file.
// It provides the safe access to the memory-mapped file.
#[derive(Debug)]
pub(crate) struct MemoryMap {
    data: *mut u8,
    size: usize,
    /// Whether the mapping was created PROT_READ|PROT_WRITE. Mutable
    /// accessors check this so a mut reference over a PROT_READ mapping
    /// (which would SIGSEGV on first write) is a typed error instead.
    writable: bool,
}

// SAFETY: The `data` pointer is owned by this MemoryMap and remains valid for
// `size` bytes until `Drop` calls `munmap`. The pointer itself is not mutated
// after construction. Higher-level invariants for the contents of the mapped
// region (atomic vs non-atomic writes) are the responsibility of the callers
// in this module — for shared writable mappings, the builder phase is expected
// to complete before any readers attach.
unsafe impl Send for MemoryMap {}

// SAFETY: See `Send` above. Within a process, mutation requires `&mut self`,
// which the borrow checker makes exclusive; shared `&self` readers are
// therefore never racing an in-process writer. Non-atomic fields read
// through `&self` fall into two groups: (a) trie namelen/name bytes and
// long values are written exactly once before their offset is published
// via a Release store on the owning link/serial word, and readers reach
// them only after the paired Acquire load — that happens-before edge makes
// the reads race-free; (b) the header's magic/version have no publish
// word — their safety rests on the builder-phase precondition documented
// on `Send` (the header is fully written before any reader attaches).
// Fields that *are* rewritten after publication (value slots, the dirty
// backup slot) are accessed exclusively through byte-wise atomics plus the
// seqlock serial protocol. Cross-process sharing relies on the same
// protocol being followed by every mapping of the file.
unsafe impl Sync for MemoryMap {}

impl MemoryMap {
    pub(crate) fn new(file: File, size: usize, writable: bool) -> Result<Self> {
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
        .map_err(Error::from)? as *mut u8;

        Ok(Self {
            data: memory_area,
            size,
            writable,
        })
    }

    /// Rejects mutable access to read-only mappings. Writing through a
    /// PROT_READ mapping kills the process with SIGSEGV — fail with a
    /// typed error at the accessor instead.
    fn require_writable(&self) -> Result<()> {
        if !self.writable {
            return Err(Error::PermissionDenied(
                "attempted mutable access to a read-only property mapping".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    pub(crate) fn data(&self, offset: usize, base: usize, size: usize) -> Result<&[u8]> {
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, size)?;
        // SAFETY: `offset + size <= self.size`, so the resulting slice lies
        // entirely within the mmap region. `u8` has no alignment requirement.
        // Lifetime is tied to `&self`, matching the mmap's lifetime.
        Ok(unsafe { std::slice::from_raw_parts(self.data.add(offset) as *const u8, size) })
    }

    #[cfg(feature = "builder")]
    pub(crate) fn data_mut(
        &mut self,
        offset: usize,
        base: usize,
        size: usize,
    ) -> Result<&mut [u8]> {
        self.require_writable()?;
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, size)?;
        // SAFETY: `offset + size <= self.size`. `&mut self` ensures exclusive
        // access to the mmap region. `u8` has no alignment requirement. The
        // mapping is PROT_WRITE (checked above).
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
        // SAFETY: `add(offset)` is only used to compute the address, not
        // dereferenced here. `offset <= self.size` is verified by the caller.
        let ptr_addr = unsafe { self.data.add(offset) } as usize;
        if ptr_addr % align != 0 {
            return Err(Error::FileValidation(format!(
                "Misaligned object at offset {offset}: required align={align}, addr={ptr_addr:#x}"
            )));
        }
        Ok(())
    }

    // Convert the memory-mapped file to the object with the given offset.
    // base is the base offset of the object.
    // offset is calculated by adding the base offset and the given offset.
    pub(crate) fn to_object<T: MmapObject>(&self, offset: usize, base: usize) -> Result<&T> {
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, mem::size_of::<T>())?;
        self.check_alignment::<T>(offset)?;
        // SAFETY: bounds and alignment are verified above; `T: MmapObject`
        // guarantees every bit pattern is a valid `T` and that shared access
        // follows the module's mmap concurrency protocol. The resulting
        // reference's lifetime is tied to `&self`, which owns the mmap.
        Ok(unsafe { &*(self.data.add(offset) as *const T) })
    }

    // Convert the memory-mapped file to the mutable object with the given offset.
    pub(crate) fn to_object_mut<T: MmapObject>(
        &mut self,
        offset: usize,
        base: usize,
    ) -> Result<&mut T> {
        self.require_writable()?;
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, mem::size_of::<T>())?;
        self.check_alignment::<T>(offset)?;
        // SAFETY: bounds and alignment are verified above; `T: MmapObject`
        // guarantees every bit pattern is a valid `T`. `&mut self` ensures
        // exclusive access for the lifetime of the returned reference. The
        // mapping is PROT_WRITE (checked above).
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
    /// process's writer, a formal data race under the byte-wise atomics
    /// discipline documented on `Sync` above. The returned `&CStr` (and the
    /// non-atomic re-read constructing it) covers only `[offset, nul]`,
    /// which the module protocol publishes write-once.
    ///
    /// Residual formal gap, accepted: if a *corrupt* file makes this range
    /// alias a mutable slot (e.g. a bogus long-property offset pointing
    /// into another entry's value), the non-atomic re-read can still race a
    /// concurrent writer. Sealing that would require returning an owned
    /// copy assembled from atomic loads — an allocation on the lookup hot
    /// path to defend a corruption+concurrent-writer combination that the
    /// module's threat-model notes already exclude.
    pub(crate) fn cstr_at(&self, offset: usize, base: usize) -> Result<&CStr> {
        let offset = self.checked_offset(offset, base)?;
        let remaining = self.size.checked_sub(offset).ok_or_else(|| {
            Error::FileValidation(format!("Offset past mmap: {offset} > {}", self.size))
        })?;
        let cells = self.atomic_data(offset, 0, remaining)?;
        let nul = cells
            .iter()
            .position(|c| c.load(std::sync::atomic::Ordering::Relaxed) == 0)
            .ok_or_else(|| {
                Error::FileValidation(format!("No NUL terminator at offset {offset}"))
            })?;
        let bytes = self.data(offset, 0, nul + 1)?;
        // Checked (not `_unchecked`) construction: the prefix is write-once
        // under the module protocol, but a corrupt file breaking that
        // invariant should surface as a typed error, not UB.
        CStr::from_bytes_with_nul(bytes)
            .map_err(|e| Error::FileValidation(format!("Invalid C string at offset {offset}: {e}")))
    }

    /// Byte-wise-atomic view of `size` bytes at `offset`. Used for the
    /// dirty backup slot, which — like the value slots — may be rewritten
    /// by another process's writer while a reader snapshots it.
    ///
    /// `AtomicU8::store` is a safe call but SIGSEGVs on a PROT_READ
    /// mapping — callers that intend to *store* through the returned slice
    /// must check [`Self::require_writable`] first (this accessor takes
    /// `&self` because readers share it).
    pub(crate) fn atomic_data(
        &self,
        offset: usize,
        base: usize,
        size: usize,
    ) -> Result<&[std::sync::atomic::AtomicU8]> {
        let offset = self.checked_offset(offset, base)?;
        self.check_size(offset, size)?;
        // SAFETY: `offset + size <= self.size`, so the slice lies within the
        // mmap. `AtomicU8` has the same size/alignment as `u8` (asserted in
        // property_info.rs). Callers fall into two groups, both race-free:
        // (a) ranges under the byte-wise-atomic protocol (the dirty backup
        // slot / value slots), where every concurrent access is atomic; or
        // (b) read-only atomic scans over write-once-published bytes
        // (`cstr_at`'s NUL search), where the only concurrent accesses are
        // other *reads* — mixing atomic and non-atomic reads is not a data
        // race. A corrupt file could alias such a range with a non-atomic
        // access elsewhere, but that requires corruption *and* a concurrent
        // writer — outside the supported threat model, and bounded by the
        // seqlock retry discarding torn data. Lifetime is tied to `&self`.
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
        unsafe {
            if let Err(e) = mm::munmap(self.data as _, self.size) {
                error!("Failed to unmap memory: {e:?}");
            }
        }
    }
}
