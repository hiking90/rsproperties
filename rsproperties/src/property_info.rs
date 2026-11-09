// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Per-property entries in the mmap'd property area.
//!
//! The rules cited below (\[SM1\]–\[SM5\], \[R-UNION\], \[R-WRITE\], …) are
//! defined once, in the `# Shared-mapping contract` section of
//! `property_area.rs`.
//!
//! # Concurrency model
//!
//! - `serial` is the seqlock that synchronizes reader/writer races. Readers
//!   load it before a value read (`load_acquire`: `Relaxed` load +
//!   `fence(Acquire)`, \[R-RO\]) and again after `fence(Acquire)` with a
//!   `Relaxed` load; on mismatch they retry.
//! - `data` is accessed only through atomics once published \[SM2\](a): the
//!   byte-wise `value` array ([`AtomicU8`]) and a long entry's `AtomicU32`
//!   `offset`, as listed in \[R-UNION\]; plain writes happen only while the
//!   entry is fresh \[SM3\].
//! - Writers mutate a published entry only through [`PropertyInfoWriter`],
//!   which needs an `AreaWriteAccess` (\[R-WRITE\]) and is reachable only
//!   from `PropertyAreaMap::backup_and_apply_write(&mut self)` \[SM4\].
//! - Whether an entry is LONG depends on its name as well as its serial
//!   (\[R-UNION\]): see [`PropertyInfo::is_long_for`].

#![deny(clippy::undocumented_unsafe_blocks)]

use std::cell::UnsafeCell;
use std::mem;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use crate::errors::{Error, Result};
#[cfg(feature = "builder")]
use crate::property_area::AreaWriteAccess;
use crate::system_properties::PROP_VALUE_MAX;

#[cfg(feature = "builder")]
const LONG_LEGACY_ERROR: &str = "Must use __system_property_read_callback() to read";

/// bionic `prop_info::kLongFlag`. Meaningful only on `ro.` names
/// \[R-UNION\].
pub(crate) const LONG_FLAG: u32 = 1 << 16;
const LONG_LEGACY_ERROR_BUFFER_SIZE: usize = 56;

// The legacy error message must fit the fixed buffer *with* its NUL
// terminator — a longer message would be silently truncated by
// `error_bytes_padded` and lose NUL termination, while the serial length
// byte kept the untruncated length.
#[cfg(feature = "builder")]
const _: () = assert!(LONG_LEGACY_ERROR.len() < LONG_LEGACY_ERROR_BUFFER_SIZE);

// `AtomicU8` / `AtomicU32` are guaranteed by `std::sync::atomic` to have the
// same in-memory representation and alignment as `u8` / `u32`. Asserting it
// here keeps the on-disk mmap layout — shared with native bionic property
// files — byte-compatible.
const _: () = assert!(mem::size_of::<AtomicU8>() == mem::size_of::<u8>());
const _: () = assert!(mem::align_of::<AtomicU8>() == mem::align_of::<u8>());
const _: () = assert!(mem::size_of::<AtomicU32>() == mem::size_of::<u32>());
const _: () = assert!(mem::align_of::<AtomicU32>() == mem::align_of::<u32>());
const _: () = assert!(mem::size_of::<[AtomicU8; PROP_VALUE_MAX]>() == PROP_VALUE_MAX);

// Whole-struct layout pin, like the `PropertyTrieNode`/`PropertyArea`
// asserts in property_area.rs: `PropertyInfo` overlays bionic's `prop_info`
// (serial u32 + PROP_VALUE_MAX-byte value/long union = 96 bytes).
const _: () = assert!(mem::size_of::<PropertyInfo>() == 4 + PROP_VALUE_MAX);
const _: () = assert!(mem::offset_of!(PropertyInfo, serial) == 0);
const _: () = assert!(mem::offset_of!(PropertyInfo, data) == 4);

#[repr(C)]
struct LongProperty {
    error_message: [u8; LONG_LEGACY_ERROR_BUFFER_SIZE],
    offset: AtomicU32,
}

#[repr(C)]
union Union {
    value: mem::ManuallyDrop<[AtomicU8; PROP_VALUE_MAX]>,
    long_property: mem::ManuallyDrop<LongProperty>,
}

#[repr(C, align(4))]
pub(crate) struct PropertyInfo {
    pub(crate) serial: AtomicU32,
    data: UnsafeCell<Union>,
}

// SAFETY: `&PropertyInfo` hands out only atomics: `serial`, the value slot
// and the long `offset` ([SM2](a)). The non-atomic `error_message` is
// written only through `&mut` while fresh [SM3]; after publication its
// bytes are read only as the value slot's atomics, never as `u8`. Stores to
// a published entry come from the single writer [SM4] and are byte-wise on
// entries whose `offset` is never loaded; [R-UNION] lists every access to
// the union bytes. Foreign writers: [R-INIT]; corrupt offsets: [SM5].
unsafe impl Sync for PropertyInfo {}

impl PropertyInfo {
    /// Initializes the header for a long property. The trailing name bytes
    /// are written separately by `PropertyAreaMap::new_prop_info` through
    /// the mmap base pointer — writing them through `&mut self` would step
    /// outside this reference's provenance.
    #[cfg(feature = "builder")]
    pub(crate) fn init_with_long_offset(&mut self, offset: u32) {
        let error_bytes = LONG_LEGACY_ERROR.as_bytes();
        let serial_value = ((error_bytes.len() as u32) << 24) | LONG_FLAG;

        self.serial.store(serial_value, Ordering::Relaxed);

        // SAFETY: [SM3] — `&mut self` comes from `fresh_object_mut`, so the
        // entry is unlinked and no other access to these bytes exists. The
        // variant written (`long_property`) matches the LONG flag set above,
        // which this crate's writer sets only on `ro.` names
        // (`wire::validate_value_len` in `PropertyAreaMap::add`, [R-UNION]).
        unsafe {
            let long_property = &mut *(*self.data.get()).long_property;
            long_property.error_message =
                error_bytes_padded::<LONG_LEGACY_ERROR_BUFFER_SIZE>(error_bytes);
            *long_property.offset.get_mut() = offset;
        }
    }

    /// Initializes the header and short value. See `init_with_long_offset`
    /// for why the trailing name is not written here.
    ///
    /// The caller must have routed values of `PROP_VALUE_MAX` bytes or more
    /// to the long variant — a short slot needs room for the NUL, so
    /// storing exactly `PROP_VALUE_MAX` bytes would truncate to
    /// `PROP_VALUE_MAX - 1` while the serial recorded the full length.
    #[cfg(feature = "builder")]
    pub(crate) fn init_with_value(&mut self, value: &str) {
        debug_assert!(
            value.len() < PROP_VALUE_MAX,
            "init_with_value: value of {} bytes must use the long variant",
            value.len()
        );
        // Derive the serial's length field from the length that will
        // actually be stored, so a release-build contract violation cannot
        // desync the two: `init_value_bytes` clamps to the slot, and a
        // len >= 256 would otherwise shift bits clean out of the length
        // field. A bionic-compatible reader trusts `serial >> 24` verbatim.
        let stored_len = value.len().min(PROP_VALUE_MAX - 1);
        let serial_value = (stored_len as u32) << 24;
        self.serial.store(serial_value, Ordering::Relaxed);

        // SAFETY: [SM3] — `&mut self` comes from `fresh_object_mut`, so the
        // entry is unlinked; the `value` variant matches the non-LONG
        // serial set above.
        unsafe {
            let slot = &mut *(*self.data.get()).value;
            init_value_bytes(slot, value.as_bytes());
        }
    }

    /// Snapshots the short-variant value into `buf` (byte-wise atomic, no
    /// UTF-8 validation) and returns the populated prefix. Raw bytes let
    /// the seqlock read loop validate the serial *before* UTF-8 decoding,
    /// so torn multi-byte sequences are absorbed by the retry instead of
    /// aborting on `?`. The returned slice borrows `buf`, not `self` — it
    /// is a copy, valid independent of later writes to the entry.
    ///
    /// Contract: the caller has decided the entry is short
    /// (`!self.is_long_for(name)`; the sole callers,
    /// `PropertyAreaMap::property_value_bytes` and the seqlock read loop,
    /// both do). On a LONG entry this would misread the `LongProperty`
    /// header bytes as value bytes — not UB (both variants are valid for
    /// every bit pattern, and long entries get no stores after publication
    /// \[R-UNION\]), but garbage. A corrupt trie reaches this case: a
    /// non-`ro.` lookup of a LONG `ro.` entry. For long entries use
    /// `PropertyAreaMap::long_property_value`.
    pub(crate) fn short_value_bytes<'a>(&self, buf: &'a mut [u8; PROP_VALUE_MAX]) -> &'a [u8] {
        // SAFETY: [SM2](a) — the `value` variant is an array of atomics,
        // valid for every bit pattern and lying entirely within `self`.
        // Concurrent stores to these bytes are byte-wise atomic (single
        // writer [SM4]); the only other-width access, the `u32` long
        // `offset` load, is made only on entries without stores [R-UNION].
        // Foreign writers: [R-INIT]; corrupt offsets: [SM5].
        unsafe {
            let slot = &*(*self.data.get()).value;
            read_value_atomic(slot, buf)
        }
    }

    /// Reads the long-variant relative offset (from the start of this entry
    /// to the out-of-line value bytes). Errors when the LONG flag is not
    /// set — the union would be reinterpreting value bytes as an offset.
    ///
    /// Precondition \[R-UNION\]: the entry's trailing name starts with
    /// `ro.`. The raw bit does not ensure it (on other names bit 16 is
    /// counter, and the writer stores byte-wise into those entries), so
    /// call this only through `PropertyAreaMap::long_property_value`.
    pub(crate) fn long_offset(&self) -> Result<u32> {
        if !self.is_long() {
            // API misuse by the caller, not a data-encoding failure.
            return Err(Error::InvalidArgument(
                "long_offset called on a short property entry".into(),
            ));
        }
        // SAFETY: only the atomic `offset` field is referenced. Both
        // projections are raw (`addr_of!`; `ManuallyDrop` is
        // `repr(transparent)`, so the field pointer casts to
        // `*const LongProperty`), so no `&LongProperty` covering the
        // non-atomic `error_message` is formed. The field lies within
        // `self`, and `AtomicU32` is valid for every bit pattern. The load
        // races no byte store: the sole caller,
        // `PropertyAreaMap::long_property_value`, calls this only after
        // checking that the entry's trailing name starts with `ro.`, and
        // the writer stores to no such entry [R-UNION]; corrupt offsets:
        // [SM5].
        let offset: &AtomicU32 = unsafe {
            let long_property =
                std::ptr::addr_of!((*self.data.get()).long_property).cast::<LongProperty>();
            &*std::ptr::addr_of!((*long_property).offset)
        };
        Ok(offset.load(Ordering::Relaxed))
    }

    /// The raw LONG bit. Private: whether the bit selects the long variant
    /// depends on the name ([`Self::is_long_for`]).
    fn is_long(&self) -> bool {
        let serial = self.serial.load(Ordering::Relaxed);
        serial & LONG_FLAG != 0
    }

    /// Whether the entry named `name` stores its value out of line
    /// \[R-UNION\]: the LONG flag counts only on `ro.` names, like bionic's
    /// `ReadCallback`. On any other name bit 16 is part of the update
    /// counter (bionic's writer reaches it on the 32768th update).
    pub(crate) fn is_long_for(&self, name: &[u8]) -> bool {
        name.starts_with(b"ro.") && self.is_long()
    }

    /// Returns the writer for a published entry.
    ///
    /// `access` proves the mapping is PROT_WRITE \[R-WRITE\]; it is created
    /// only in `PropertyAreaMap::backup_and_apply_write`, which holds
    /// `&mut PropertyAreaMap` (one writer per mapping in this process,
    /// \[SM4\]) and writes the dirty backup slot first. That ordering — the
    /// slot must hold this entry's current value before `apply_write`
    /// publishes the dirty bit — is why no other path reaches this writer.
    #[cfg(feature = "builder")]
    pub(crate) fn writer(&self, _access: AreaWriteAccess<'_>) -> PropertyInfoWriter<'_> {
        PropertyInfoWriter(self)
    }
}

/// Single-writer handle for a published entry. All stores are atomic
/// \[SM2\](a); publish operations use `Ordering::Release` so paired Acquire
/// readers see the value writes.
#[cfg(feature = "builder")]
pub(crate) struct PropertyInfoWriter<'a>(&'a PropertyInfo);

/// Counter bits in `serial` — bits 0..24 excluding `LONG_FLAG` (bit 16).
/// bionic's writer does not mask bit 16 (\[R-UNION\]: readers ignore it on
/// non-`ro.` names); this crate's writer keeps its counter out of it anyway
/// so its serials never carry a bit an older reader of this crate (≤ 0.6)
/// reads as LONG: `apply_write` carries a set bit 16 into bit 17, so the
/// counter (bits 1..15 and 17..23) repeats every 2^22 updates. Serial
/// values therefore differ from bionic's after 32768 updates; waiters
/// compare serials only for equality.
#[cfg(feature = "builder")]
const SERIAL_COUNTER_MASK: u32 = 0x00ff_ffff & !LONG_FLAG;

#[cfg(feature = "builder")]
impl PropertyInfoWriter<'_> {
    /// Atomic short-value update: validate → set dirty → write bytes →
    /// publish new serial. Returns the published serial.
    ///
    /// The *entry-side* seqlock steps are encapsulated here so callers
    /// cannot leave the entry in a half-published state: every failure path
    /// occurs *before* `set_dirty`, and once the dirty bit is published the
    /// remaining steps are infallible (byte-wise atomic stores + atomic
    /// serial publish).
    ///
    /// # Precondition
    ///
    /// The area's dirty backup slot must already hold this entry's current
    /// value: readers that observe the dirty serial read the backup slot,
    /// not the entry, so publishing the dirty bit without a fresh backup
    /// serves a previous update's backup bytes as this value. The ordering
    /// is enforced by `PropertyAreaMap::backup_and_apply_write` — keep it
    /// the only route to this method.
    ///
    /// The entry is short: `backup_and_apply_write` refuses entries named
    /// `ro.…`, and only `ro.` entries are long \[R-UNION\]. A set bit 16 in
    /// the serial is therefore counter, not LONG; the carry below moves it
    /// into bit 17, so the published serial never has bit 16 set.
    pub(crate) fn apply_write(self, value: &str) -> Result<u32> {
        let current = self.0.serial.load(Ordering::Relaxed);
        if value.len() >= PROP_VALUE_MAX {
            // `PROP_VALUE_MAX - 1`: the slot reserves one byte for the NUL,
            // so 92 bytes is rejected and the honest maximum is 91.
            return Err(Error::InvalidArgument(format!(
                "Value too long: {} (max: {})",
                value.len(),
                PROP_VALUE_MAX - 1
            )));
        }
        // Infallible after the check above: len < PROP_VALUE_MAX (92), far
        // inside both u32 range and the 8-bit serial length field.
        let len_u32 = value.len() as u32;

        // bionic seqlock convention: even serial = clean, odd = dirty. We
        // first publish `current | 1` (dirty), then `(dirty + 1)` carries
        // the LSB back to 0 (clean) while bumping the counter.
        let dirty_serial = current | 1;
        let mut next = dirty_serial.wrapping_add(1);
        if next & LONG_FLAG != 0 {
            // Skip bit 16: carry into bit 17 instead of masking the carry
            // away, which would reset the counter every 32768 updates.
            next = next.wrapping_add(LONG_FLAG);
        }
        let counter_next = next & SERIAL_COUNTER_MASK;
        let new_serial = (len_u32 << 24) | counter_next;

        // From here on no further early returns: each step is infallible.
        self.0.serial.store(dirty_serial, Ordering::Release);
        // Seqlock writer fence (bionic: atomic_thread_fence(release) between
        // the dirty store and strlcpy). A `Release` *store* only orders the
        // writes before it — the relaxed value-byte stores below could still
        // be observed *before* the dirty serial, letting a reader pass its
        // serial re-check while holding torn bytes. The fence pairs with the
        // reader's `fence(Acquire)`: a reader that observes any value byte
        // written after this fence is guaranteed to observe the dirty serial
        // at its re-check and retry.
        std::sync::atomic::fence(Ordering::Release);
        // SAFETY: [SM2](a) — the `value` variant is an array of atomics
        // within the entry, and every store below is byte-wise atomic; the
        // entry is short and its `offset` is never loaded [R-UNION] (the
        // sole caller `backup_and_apply_write` refused `ro.` trailing
        // names), so no access of another width overlaps these stores, and
        // the writer is single [SM4]. Corrupt offsets: [SM5].
        unsafe {
            let slot = &*(*self.0.data.get()).value;
            write_value_atomic(slot, value.as_bytes());
        }
        self.0.serial.store(new_serial, Ordering::Release);
        Ok(new_serial)
    }
}

#[cfg(feature = "builder")]
fn error_bytes_padded<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut buf = [0u8; N];
    let copy_len = src.len().min(N);
    buf[..copy_len].copy_from_slice(&src[..copy_len]);
    buf
}

/// Reads bytes from `slot` into `buf` until the first NUL or `PROP_VALUE_MAX`
/// bytes, byte-wise with `Relaxed` ordering. Returns the populated prefix
/// (excluding the NUL terminator).
///
/// Uses a caller-provided stack buffer instead of allocating a `Vec` so the
/// seqlock retry loop in `read_mutable_property_value` doesn't allocate on
/// every iteration. The caller materialises an owned value only once, after
/// the serial re-check confirms the read was consistent.
fn read_value_atomic<'a>(
    slot: &[AtomicU8; PROP_VALUE_MAX],
    buf: &'a mut [u8; PROP_VALUE_MAX],
) -> &'a [u8] {
    let mut len = 0;
    for (i, cell) in slot.iter().enumerate() {
        let b = cell.load(Ordering::Relaxed);
        if b == 0 {
            break;
        }
        buf[i] = b;
        len = i + 1;
    }
    &buf[..len]
}

/// Byte-wise atomic write. Truncates to `PROP_VALUE_MAX - 1`, then writes a
/// NUL terminator. Caller is responsible for the surrounding seqlock fences
/// (Release stores on `serial`).
#[cfg(feature = "builder")]
fn write_value_atomic(slot: &[AtomicU8; PROP_VALUE_MAX], bytes: &[u8]) {
    let copy_len = bytes.len().min(PROP_VALUE_MAX - 1);
    for (i, &b) in bytes[..copy_len].iter().enumerate() {
        slot[i].store(b, Ordering::Relaxed);
    }
    slot[copy_len].store(0, Ordering::Relaxed);
}

/// Init-only variant for `init_with_value`. Uses `get_mut()` (plain
/// non-atomic assignment) since the property has not yet been published to
/// readers \[SM3\].
#[cfg(feature = "builder")]
fn init_value_bytes(slot: &mut [AtomicU8; PROP_VALUE_MAX], bytes: &[u8]) {
    let copy_len = bytes.len().min(PROP_VALUE_MAX - 1);
    for (i, &b) in bytes[..copy_len].iter().enumerate() {
        *slot[i].get_mut() = b;
    }
    *slot[copy_len].get_mut() = 0;
}
