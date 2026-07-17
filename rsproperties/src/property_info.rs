// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Per-property entries in the mmap'd property area.
//!
//! # Concurrency model
//!
//! - `serial` is the seqlock that synchronizes reader/writer races. Readers
//!   load it Acquire before and after a value read; on mismatch they retry.
//! - `data` is byte-wise atomic ([`AtomicU8`] array). Writers mutate via
//!   [`PropertyInfoWriter`] which requires `&mut self` (the borrow checker
//!   enforces single-writer within a process).
//! - **Cross-process invariant.** Multi-process mmap sharing requires a
//!   system-level single-writer policy (e.g. Android `init` owns the writable
//!   mapping). This crate cannot enforce that policy from Rust; the
//!   `unsafe impl Sync` below documents the assumption.

use std::cell::UnsafeCell;
use std::mem;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use crate::errors::{Error, Result};
use crate::system_properties::PROP_VALUE_MAX;

#[cfg(feature = "builder")]
const LONG_LEGACY_ERROR: &str = "Must use __system_property_read_callback() to read";

const LONG_FLAG: u32 = 1 << 16;
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
pub struct PropertyInfo {
    pub(crate) serial: AtomicU32,
    data: UnsafeCell<Union>,
}

// SAFETY: All writes to `data` go through `PropertyInfoWriter`, which holds
// `&mut PropertyInfo` — the borrow checker enforces a single in-process
// writer. All reader-visible memory is byte-wise atomic, so concurrent
// reads against an in-flight writer are well-defined (no torn-byte UB under
// the Rust abstract machine). Cross-process mmap sharing requires a
// system-level single-writer policy; see module-level docs.
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

        // SAFETY: `&mut self` grants exclusive access; the union variant we
        // write (`long_property`) matches what `value()` reads when the LONG
        // flag is set in `serial`. `get_mut()` on the atomic is the no-fence
        // assignment appropriate for init time (no concurrent readers).
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
        let serial_value = (value.len() as u32) << 24;
        self.serial.store(serial_value, Ordering::Relaxed);

        // SAFETY: `&mut self` grants exclusive access; we initialize the
        // `value` variant of the union to match the non-LONG `serial` state.
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
    /// Contract: the caller must have checked `!self.is_long()` (the sole
    /// callers, `PropertyAreaMap::property_value_bytes` and the seqlock
    /// read loop, both do). On a LONG entry this would misread the
    /// `LongProperty` header bytes as value bytes — not UB (the union is
    /// all plain bytes, and long entries are write-once so there is no
    /// race), but garbage; the `debug_assert!` documents the contract.
    /// For long entries use `PropertyAreaMap::long_property_value`.
    pub(crate) fn short_value_bytes<'a>(&self, buf: &'a mut [u8; PROP_VALUE_MAX]) -> &'a [u8] {
        debug_assert!(!self.is_long(), "short_value_bytes on a LONG entry");
        // SAFETY: reading the `value` union variant. Both union variants
        // are plain (atomic) bytes with no validity requirement, the array
        // lies entirely within `self` (provenance-safe), and all
        // concurrent access to this range is byte-wise atomic.
        unsafe {
            let slot = &*(*self.data.get()).value;
            read_value_atomic(slot, buf)
        }
    }

    /// Reads the long-variant relative offset (from the start of this entry
    /// to the out-of-line value bytes). Errors when the LONG flag is not
    /// set — the union would be reinterpreting value bytes as an offset.
    pub(crate) fn long_offset(&self) -> Result<u32> {
        if !self.is_long() {
            return Err(Error::Encoding(
                "long_offset called on a short property entry".into(),
            ));
        }
        // SAFETY: LONG flag checked above, so the active union variant is
        // `long_property`, which lies entirely within `self`.
        unsafe {
            let long_property = &*(*self.data.get()).long_property;
            Ok(long_property.offset.load(Ordering::Relaxed))
        }
    }

    pub(crate) fn is_long(&self) -> bool {
        let serial = self.serial.load(Ordering::Relaxed);
        serial & LONG_FLAG != 0
    }

    /// Returns a writer that has exclusive update rights to this entry.
    ///
    /// `&mut self` ensures only one writer exists within a process; the
    /// returned writer enforces the seqlock + LONG-flag invariants.
    /// Cross-process invariants are documented at the module level.
    #[cfg(feature = "builder")]
    pub(crate) fn writer(&mut self) -> PropertyInfoWriter<'_> {
        PropertyInfoWriter(self)
    }
}

/// Single-writer handle. Construction requires `&mut PropertyInfo`, so the
/// borrow checker enforces one-writer-per-process. All publish operations
/// use `Ordering::Release` so paired Acquire readers see the value writes.
#[cfg(feature = "builder")]
pub(crate) struct PropertyInfoWriter<'a>(&'a mut PropertyInfo);

/// Counter bits in `serial` — bits 0..24 excluding `LONG_FLAG` (bit 16).
/// Using a wider mask would let the counter wrap into the LONG_FLAG bit and
/// silently flip the union variant under readers.
#[cfg(feature = "builder")]
const SERIAL_COUNTER_MASK: u32 = 0x00ff_ffff & !LONG_FLAG;

#[cfg(feature = "builder")]
impl PropertyInfoWriter<'_> {
    /// Atomic short-value update: validate → set dirty → write bytes →
    /// publish new serial. Returns the published serial.
    ///
    /// The entire seqlock protocol is encapsulated here so callers cannot
    /// leave the entry in a half-published state: every failure path occurs
    /// *before* `set_dirty`, and once the dirty bit is published the
    /// remaining steps are infallible (byte-wise atomic stores + atomic
    /// serial publish).
    ///
    /// Rejects when the LONG flag is set — switching the union variant
    /// in-place would leak the out-of-line long-property buffer.
    pub(crate) fn apply_write(self, value: &str) -> Result<u32> {
        let current = self.0.serial.load(Ordering::Relaxed);
        if current & LONG_FLAG != 0 {
            return Err(Error::FileValidation(format!(
                "in-place update of long property is not supported (serial={current:#x})"
            )));
        }
        let len_u32 = u32::try_from(value.len()).map_err(|_| {
            Error::FileValidation(format!("Value length exceeds u32: {}", value.len()))
        })?;
        if value.len() >= PROP_VALUE_MAX {
            return Err(Error::FileValidation(format!(
                "Value too long: {} (max: {})",
                value.len(),
                PROP_VALUE_MAX
            )));
        }

        // bionic seqlock convention: even serial = clean, odd = dirty. We
        // first publish `current | 1` (dirty), then `(dirty + 1)` carries
        // the LSB back to 0 (clean) while bumping the counter. The counter
        // mask excludes LONG_FLAG so a long-lived short property cannot
        // wrap into the LONG variant.
        let dirty_serial = current | 1;
        let counter_next = (dirty_serial.wrapping_add(1)) & SERIAL_COUNTER_MASK;
        let new_serial = (len_u32 << 24) | counter_next;

        // From here on no further early returns: each step is infallible.
        self.0.serial.store(dirty_serial, Ordering::Release);
        // SAFETY: `&mut self` on the writer guarantees no concurrent writer
        // in this process; the LONG flag check above confirms the active
        // variant is `value`. Byte-wise atomic stores keep concurrent
        // readers race-free per the Rust memory model.
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
/// readers.
#[cfg(feature = "builder")]
fn init_value_bytes(slot: &mut [AtomicU8; PROP_VALUE_MAX], bytes: &[u8]) {
    let copy_len = bytes.len().min(PROP_VALUE_MAX - 1);
    for (i, &b) in bytes[..copy_len].iter().enumerate() {
        *slot[i].get_mut() = b;
    }
    *slot[copy_len].get_mut() = 0;
}
