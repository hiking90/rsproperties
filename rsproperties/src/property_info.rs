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
use std::ffi::CStr;
use std::mem;
#[cfg(feature = "builder")]
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use crate::errors::{Error, Result};
use crate::system_properties::PROP_VALUE_MAX;

#[cfg(feature = "builder")]
const LONG_LEGACY_ERROR: &str = "Must use __system_property_read_callback() to read";

const LONG_FLAG: u32 = 1 << 16;
const LONG_LEGACY_ERROR_BUFFER_SIZE: usize = 56;

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
    #[cfg(feature = "builder")]
    pub(crate) fn init_with_long_offset(&mut self, name: &str, offset: u32) {
        init_name_with_trailing_data(self, name);
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

    #[cfg(feature = "builder")]
    pub(crate) fn init_with_value(&mut self, name: &str, value: &str) {
        init_name_with_trailing_data(self, name);
        let serial_value = (value.len() as u32) << 24;
        self.serial.store(serial_value, Ordering::Relaxed);

        // SAFETY: `&mut self` grants exclusive access; we initialize the
        // `value` variant of the union to match the non-LONG `serial` state.
        unsafe {
            let slot = &mut *(*self.data.get()).value;
            init_value_bytes(slot, value.as_bytes());
        }
    }

    /// Reads the trailing name field.
    ///
    /// `bound` caps the NUL scan at the remaining mmap extent past `self`,
    /// computed by the enclosing `PropertyAreaMap` via `max_value_bound`.
    ///
    /// Gated on `builder` because the only caller is `update`'s `ro.`
    /// validation, which itself is builder-only. Without the gate this
    /// would be dead code in default Android builds.
    #[cfg(feature = "builder")]
    pub(crate) fn name(&self, bound: usize) -> Result<&CStr> {
        let header = mem::size_of::<PropertyInfo>();
        // `name_from_trailing_data` reads `len + 1` bytes; bail out when the
        // bound is too small to even hold the NUL terminator. Paired with
        // `PropertyAreaMap::max_value_bound`, which already rejects entries
        // whose trailing region is < 1 byte by returning 0.
        if bound <= header {
            return Err(Error::Encoding(
                "PropertyInfo name has no room for terminator".into(),
            ));
        }
        let len = bound - header - 1;
        name_from_trailing_data(self, Some(len))
    }

    /// Reads the property value as raw bytes (no UTF-8 validation).
    ///
    /// Returning bytes lets the seqlock read loop validate the serial
    /// *before* attempting UTF-8 decoding. With byte-wise atomic stores a
    /// reader may observe partially-written multi-byte sequences that
    /// would otherwise spuriously fail `String::from_utf8` and abort the
    /// retry loop. Callers that need a `String` should validate the
    /// serial first, then convert via `String::from_utf8`.
    ///
    /// `long_value_bound` is an upper bound on how far past `self` the
    /// long variant may extend. For the short variant this argument is
    /// ignored.
    /// Reads short-variant bytes into `buf`; returns the populated prefix.
    /// The long variant borrows bytes directly from the mmap — long
    /// properties are write-once (`apply_write` rejects LONG entries), so
    /// the out-of-line bytes are stable for the lifetime of the mapping.
    ///
    /// Lifetime invariant: both `self` and `buf` are bound to `'a`, so the
    /// returned `Cow` is valid for whichever of the two ends first (the
    /// shorter, in practice usually `buf`'s scope). Tying both to a single
    /// lifetime is what lets the long path return `Cow::Borrowed` from the
    /// mmap without leaking past `self`'s borrow.
    pub(crate) fn value_bytes<'a>(
        &'a self,
        long_value_bound: usize,
        buf: &'a mut [u8; PROP_VALUE_MAX],
    ) -> Result<std::borrow::Cow<'a, [u8]>> {
        if self.is_long() {
            // SAFETY: when the LONG flag is set the union variant is
            // `long_property`. The offset is read atomically; the
            // out-of-line value is bounded by `long_value_bound`.
            let bytes = unsafe {
                let long_property = &*(*self.data.get()).long_property;
                let offset = long_property.offset.load(Ordering::Relaxed) as usize;
                long_value_bytes(self, offset, long_value_bound)?
            };
            Ok(std::borrow::Cow::Borrowed(bytes))
        } else {
            // SAFETY: when the LONG flag is clear the union variant is the
            // byte-wise atomic `value` array of size `PROP_VALUE_MAX`.
            unsafe {
                let slot = &*(*self.data.get()).value;
                Ok(std::borrow::Cow::Borrowed(read_value_atomic(slot, buf)))
            }
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

/// Bounds-checked NUL scan for the long-property variant, returning the raw
/// bytes (no UTF-8 validation) borrowed from the mmap.
///
/// Long properties are write-once: `apply_write` rejects entries with the
/// LONG flag set, so once the entry is initialised the out-of-line bytes
/// never change. That immutability is what lets the reader borrow the
/// bytes directly instead of copying them into a `Vec<u8>`.
///
/// # Safety
/// Caller must guarantee that `info + bound_from_info` does not exceed the
/// mmap allocation containing `info`. This function further reduces the read
/// span by `offset` so the scan stays within the allocation.
unsafe fn long_value_bytes(
    info: &PropertyInfo,
    offset: usize,
    bound_from_info: usize,
) -> Result<&[u8]> {
    let scan_len = bound_from_info.checked_sub(offset).ok_or_else(|| {
        Error::Encoding(format!(
            "Long property offset {offset} exceeds mmap bound {bound_from_info}"
        ))
    })?;
    if scan_len == 0 {
        return Err(Error::Encoding("Long property scan length is zero".into()));
    }
    let self_ptr = info as *const _ as *const u8;
    // SAFETY: `value_ptr` stays within the mmap because `scan_len ==
    // bound_from_info - offset` and the caller proved `info +
    // bound_from_info` is in-bounds.
    let value_ptr = unsafe { self_ptr.add(offset) };
    // SAFETY: `value_ptr` is in-bounds for `scan_len` bytes by the same
    // argument. `u8` has no alignment requirement. The borrow is tied to
    // `'a` (the `&'a PropertyInfo`), so it can't outlive the mmap.
    let bytes = unsafe { std::slice::from_raw_parts(value_ptr, scan_len) };
    let cstr = CStr::from_bytes_until_nul(bytes).map_err(|e| {
        Error::Encoding(format!("Long property value missing NUL within bound: {e}"))
    })?;
    Ok(cstr.to_bytes())
}

#[inline(always)]
pub(crate) fn name_from_trailing_data<I: Sized>(thiz: &I, len: Option<usize>) -> Result<&CStr> {
    // The unbounded variant (`len == None`) is intentionally disallowed: a
    // missing terminator on a corrupted file would otherwise let the scan
    // run past the mmap. Callers must supply an upper bound.
    let len = len.ok_or_else(|| {
        Error::Encoding("name_from_trailing_data requires a bounded length".into())
    })?;
    // SAFETY: `thiz` points into a memory-mapped property file laid out as a
    // header (`I`) immediately followed by `len + 1` bytes of name data. The
    // caller (a property area / trie helper) guarantees that the bytes from
    // `thiz + size_of::<I>()` up to `+ len + 1` are within the mapping.
    unsafe {
        let thiz_ptr = thiz as *const _ as *const u8;
        let name_ptr = thiz_ptr.add(mem::size_of::<I>());
        CStr::from_bytes_until_nul(std::slice::from_raw_parts(name_ptr, len + 1))
            .map_err(|e| Error::Encoding(format!("Failed to convert name to CStr: {e}")))
    }
}

#[cfg(feature = "builder")]
#[inline(always)]
pub(crate) fn init_name_with_trailing_data<I: Sized>(thiz: &mut I, name: &str) {
    // SAFETY: callers (the property area allocator) reserve
    // `size_of::<I>() + name.len() + 1` bytes when constructing the trailing-
    // name layout, so writing `name.len() + 1` bytes after `thiz` is in-bounds.
    // `&mut thiz` guarantees exclusive access.
    unsafe {
        let thiz_ptr = thiz as *mut _ as *mut u8;
        let name_ptr = thiz_ptr.add(mem::size_of::<I>());

        ptr::copy_nonoverlapping(name.as_ptr(), name_ptr, name.len());
        *name_ptr.add(name.len()) = 0; // Add null terminator
    }
}
