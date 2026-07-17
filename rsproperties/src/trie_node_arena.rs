// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::mem;
use std::vec::Vec;

use zerocopy::FromBytes;

use crate::errors::{Error, Result};
use crate::property_info_parser::*;

#[derive(Debug)]
pub(crate) struct TrieNodeArena {
    pub(crate) data: Vec<u8>,
    current_data_pointer: usize,
}

impl TrieNodeArena {
    pub(crate) fn new() -> Self {
        Self {
            data: Vec::with_capacity(16 * 1024),
            current_data_pointer: 0,
        }
    }

    /// Reinterprets `size_of::<T>()` bytes at `offset` as `&mut T`.
    ///
    /// No `unsafe`: `mut_from_bytes` validates the size and the *actual
    /// pointer* alignment at runtime (including the `Vec` base address,
    /// which the language only guarantees to be 1-aligned), and the
    /// `FromBytes + IntoBytes` bounds guarantee every bit pattern of the
    /// zero-filled arena is a valid `T` — instantiating with e.g. `bool`
    /// or an enum fails to compile instead of being invalid-value UB.
    ///
    /// Bounds are checked against `current_data_pointer` (the allocated
    /// extent), not `data.len()` — `allocate_data`'s growth `resize` leaves
    /// slack that would otherwise let a miscomputed offset silently read
    /// or write unallocated zero bytes.
    #[inline(always)]
    pub(crate) fn get_object<T>(&mut self, offset: usize) -> Result<&mut T>
    where
        T: FromBytes + zerocopy::IntoBytes + zerocopy::KnownLayout,
    {
        let size = mem::size_of::<T>();
        let end = offset.saturating_add(size);
        if end > self.current_data_pointer {
            return Err(Error::FileValidation(format!(
                "Object access out of bounds: offset={}, size={}, allocated={}",
                offset, size, self.current_data_pointer
            )));
        }

        T::mut_from_bytes(&mut self.data[offset..end]).map_err(|e| {
            Error::FileValidation(format!(
                "Object at offset {} is not properly aligned for type {}: {e}",
                offset,
                std::any::type_name::<T>(),
            ))
        })
    }

    #[inline(always)]
    pub(crate) fn allocate_object<T>(&mut self) -> usize {
        let size = mem::size_of::<T>();

        self.allocate_data(size)
    }

    #[inline(always)]
    pub(crate) fn allocate_uint32_array(&mut self, length: usize) -> usize {
        let size = mem::size_of::<u32>() * length;

        self.allocate_data(size)
    }

    /// Returns a mutable slice of `len` u32 elements starting at `offset`.
    ///
    /// The caller must supply the array length so the returned slice does
    /// not over-extend into adjacent allocations. Like `get_object`, this
    /// is bounds-checked against the allocated extent and delegates the
    /// (actual-pointer) alignment check to zerocopy — no `unsafe`.
    pub(crate) fn uint32_array(&mut self, offset: usize, len: usize) -> Result<&mut [u32]> {
        let byte_len = len
            .checked_mul(mem::size_of::<u32>())
            .ok_or_else(|| Error::FileValidation(format!("Array len overflow: {len}")))?;
        let end = offset.checked_add(byte_len).ok_or_else(|| {
            Error::FileValidation(format!("Array end overflow: offset={offset}, len={len}"))
        })?;

        if end > self.current_data_pointer {
            return Err(Error::FileValidation(format!(
                "Array access out of bounds: offset={offset}, len={len}, byte_end={end}, allocated={}",
                self.current_data_pointer
            )));
        }

        <[u32]>::mut_from_bytes(&mut self.data[offset..end]).map_err(|e| {
            Error::FileValidation(format!(
                "Array at offset {offset} is not properly aligned for u32: {e}"
            ))
        })
    }

    pub(crate) fn allocate_and_write_string(&mut self, string: &str) -> usize {
        let bytes = string.as_bytes();
        let offset = self.allocate_data(bytes.len() + 1);

        self.data[offset..offset + bytes.len()].copy_from_slice(bytes);
        self.data[offset + bytes.len()] = 0; // null terminator
        offset
    }

    pub(crate) fn allocate_and_write_uint32(&mut self, value: u32) {
        let offset = self.allocate_data(mem::size_of::<u32>());
        // A plain byte copy needs no alignment at all, so the previous
        // unsafe pointer write (whose safety leaned on allocator behavior
        // the language doesn't guarantee) is unnecessary.
        //
        // `to_ne_bytes`: the serialized format is host-endian throughout
        // (zerocopy struct writes here, `align_to::<u32>` reads in the
        // parser) — the same property AOSP's format has, since it mmaps
        // native structs verbatim. Files are NOT portable across
        // endianness; a big-endian host cannot produce files for a
        // little-endian device or vice versa.
        self.data[offset..offset + mem::size_of::<u32>()].copy_from_slice(&value.to_ne_bytes());
    }

    /// Reserves `size` bytes, rounded up to a multiple of `size_of::<u32>()`,
    /// from the arena's growing buffer. Returns the byte offset of the
    /// allocation, which is guaranteed to be `u32`-aligned (the pointer
    /// starts at 0 and every allocation is u32-aligned in length, so the
    /// invariant is preserved across calls).
    fn allocate_data(&mut self, size: usize) -> usize {
        let aligned_size = crate::bionic_align(size, mem::size_of::<u32>());

        let needed = self.current_data_pointer + aligned_size;
        if needed > self.data.len() {
            // Standard doubling growth, but never less than what this
            // allocation needs.
            self.data.resize(needed.max(self.data.len() * 2), 0);
        }

        let offset = self.current_data_pointer;
        self.current_data_pointer += aligned_size;

        offset
    }

    pub(crate) fn size(&self) -> usize {
        self.current_data_pointer
    }

    pub(crate) fn info(&'_ self) -> PropertyInfoArea<'_> {
        PropertyInfoArea::new(&self.data)
    }

    pub(crate) fn into_data(mut self) -> Vec<u8> {
        self.data.truncate(self.current_data_pointer);
        self.data
    }
}

#[cfg(test)]
mod arena_tests {
    use super::*;

    /// Reserves exactly `size` bytes through the real allocation path so
    /// bounds checks (which run against the allocated extent, not the
    /// resize slack) behave as in production.
    fn arena_with(size: usize) -> TrieNodeArena {
        let mut arena = TrieNodeArena::new();
        arena.allocate_data(size);
        arena
    }

    #[test]
    fn test_get_object_out_of_bounds() {
        let mut arena = arena_with(100);

        // offset + size > allocated extent
        let result = arena.get_object::<u64>(96);
        assert!(result.is_err());

        if let Err(e) = result {
            assert!(e.to_string().contains("out of bounds"));
        }
    }

    #[test]
    fn test_get_object_rejects_unallocated_slack() {
        let mut arena = arena_with(100);
        // The second allocation triggers doubling growth (needed=104 →
        // resize to 200), leaving zeroed slack past the allocated extent.
        arena.allocate_data(4);

        assert!(arena.data.len() > 108);
        // Access inside the slack must fail even though `data.len()`
        // covers it.
        let result = arena.get_object::<u32>(108);
        assert!(result.is_err());
    }

    #[test]
    fn test_get_object_misaligned() {
        let mut arena = arena_with(100);

        // u32 requires 4-byte alignment
        let result = arena.get_object::<u32>(3);
        assert!(result.is_err());

        if let Err(e) = result {
            assert!(e.to_string().contains("not properly aligned"));
        }
    }

    #[test]
    fn test_get_object_valid() {
        let mut arena = arena_with(100);

        let result = arena.get_object::<u32>(0);
        assert!(result.is_ok());

        // Test with proper alignment
        let result = arena.get_object::<u32>(4);
        assert!(result.is_ok());

        let result = arena.get_object::<u32>(8);
        assert!(result.is_ok());
    }

    #[test]
    fn test_uint32_array_out_of_bounds() {
        let mut arena = arena_with(100);

        // offset alone past end
        let result = arena.uint32_array(104, 1);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("out of bounds"));

        // offset + len * 4 past end
        let result = arena.uint32_array(96, 2);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("out of bounds"));
    }

    #[test]
    fn test_uint32_array_misaligned() {
        let mut arena = arena_with(100);

        let result = arena.uint32_array(3, 1);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not properly aligned"));
    }

    #[test]
    fn test_uint32_array_valid() {
        let mut arena = arena_with(100);

        let array = arena.uint32_array(0, 25).unwrap();
        assert_eq!(array.len(), 25);
    }

    #[test]
    fn test_uint32_array_with_offset() {
        let mut arena = arena_with(100);

        let array = arena.uint32_array(20, 20).unwrap();
        assert_eq!(array.len(), 20);
    }

    #[test]
    fn test_uint32_array_len_zero() {
        let mut arena = arena_with(100);

        let array = arena.uint32_array(0, 0).unwrap();
        assert_eq!(array.len(), 0);
    }
}
