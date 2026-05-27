// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::mem;
use std::str;
use std::vec::Vec;

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

    #[inline(always)]
    pub(crate) fn get_object<T>(&mut self, offset: usize) -> Result<&mut T> {
        let size = mem::size_of::<T>();

        // Bounds checking - always executed
        if offset.saturating_add(size) > self.data.len() {
            return Err(Error::FileValidation(format!(
                "Object access out of bounds: offset={}, size={}, data_len={}",
                offset,
                size,
                self.data.len()
            )));
        }

        // Alignment checking - always executed
        let align = mem::align_of::<T>();
        if offset % align != 0 {
            return Err(Error::FileValidation(format!(
                "Object at offset {} is not properly aligned for type {} (alignment={})",
                offset,
                std::any::type_name::<T>(),
                align
            )));
        }

        // SAFETY:
        // - Bounds checked: offset + size <= data.len()
        // - Alignment checked: offset is aligned to T's requirement
        // - data is a valid Vec<u8> buffer
        unsafe { Ok(&mut *(self.data.as_mut_ptr().add(offset) as *mut T)) }
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
    /// not over-extend into adjacent allocations. The previous form derived
    /// the length from `data.len() - offset`, which after `allocate_data`'s
    /// `resize(new_size, 0)` could span far beyond the actual allocation
    /// and allow an out-of-bounds write to silently corrupt other nodes.
    pub(crate) fn uint32_array(&mut self, offset: usize, len: usize) -> Result<&mut [u32]> {
        let byte_len = len
            .checked_mul(mem::size_of::<u32>())
            .ok_or_else(|| Error::FileValidation(format!("Array len overflow: {len}")))?;
        let end = offset.checked_add(byte_len).ok_or_else(|| {
            Error::FileValidation(format!("Array end overflow: offset={offset}, len={len}"))
        })?;

        if end > self.data.len() {
            return Err(Error::FileValidation(format!(
                "Array access out of bounds: offset={offset}, len={len}, byte_end={end}, data_len={}",
                self.data.len()
            )));
        }

        if offset % mem::align_of::<u32>() != 0 {
            return Err(Error::FileValidation(format!(
                "Array at offset {offset} is not properly aligned for u32 (alignment={})",
                mem::align_of::<u32>()
            )));
        }

        // The full alignment requirement for `*mut u32` is
        // `(base_ptr + offset) % 4 == 0`. Since `offset % 4 == 0` is
        // checked above, the residual requirement is `base_ptr % 4 == 0`.
        // `Vec<u8>`'s allocator only guarantees `align_of::<u8>() == 1`,
        // but every supported global allocator returns ≥ 8-byte alignment.
        // Mirror the `debug_assert!` from `allocate_and_write_uint32` here
        // so the invariant is documented and checked uniformly.
        debug_assert_eq!(
            (self.data.as_mut_ptr() as usize) % mem::align_of::<u32>(),
            0,
            "Vec<u8> base pointer must be u32-aligned"
        );

        // SAFETY:
        // - Bounds: `offset + len * size_of::<u32>() <= data.len()` (checked above).
        // - Alignment: `offset % 4 == 0` (checked above) AND `base_ptr % 4 == 0`
        //   (debug-asserted above; guaranteed by the global allocator's ≥ 8
        //   alignment on every supported platform).
        // - The caller-supplied `len` matches the allocation size; the slice
        //   exposes exactly the intended array and nothing more.
        unsafe {
            Ok(std::slice::from_raw_parts_mut(
                self.data.as_mut_ptr().add(offset) as *mut u32,
                len,
            ))
        }
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

        // `allocate_data` keeps `current_data_pointer` aligned to `size_of::<u32>()`
        // (every allocation is rounded up via `bionic_align(..., 4)` and the
        // pointer starts at 0), so `offset` is u32-aligned. `Vec<u8>`'s buffer
        // is allocated by the global allocator, which is ≥ 8-byte aligned on
        // every supported platform, so the resulting `*mut u32` is aligned.
        debug_assert_eq!(
            offset % mem::align_of::<u32>(),
            0,
            "allocate_data invariant: current_data_pointer must stay u32-aligned"
        );
        debug_assert_eq!(
            (self.data.as_mut_ptr() as usize) % mem::align_of::<u32>(),
            0,
            "Vec<u8> base pointer must be u32-aligned"
        );
        // SAFETY: bounds (offset + 4 ≤ data.len()) and alignment are both
        // ensured by the debug_assert!s above; in release mode the same
        // invariants hold from `allocate_data` + global allocator alignment.
        unsafe {
            let ptr = self.data.as_mut_ptr().add(offset) as *mut u32;
            ptr.write(value);
        }
    }

    /// Reserves `size` bytes, rounded up to a multiple of `size_of::<u32>()`,
    /// from the arena's growing buffer. Returns the byte offset of the
    /// allocation, which is guaranteed to be `u32`-aligned (the pointer
    /// starts at 0 and every allocation is u32-aligned in length, so the
    /// invariant is preserved across calls).
    fn allocate_data(&mut self, size: usize) -> usize {
        let aligned_size = crate::bionic_align(size, mem::size_of::<u32>());

        if self.current_data_pointer + aligned_size > self.data.len() {
            let new_size = (self.current_data_pointer + aligned_size + self.data.len()) * 2;
            self.data.resize(new_size, 0);
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

    #[test]
    fn test_get_object_out_of_bounds() {
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

        // offset + size > data.len()
        let result = arena.get_object::<u64>(96);
        assert!(result.is_err());

        if let Err(e) = result {
            assert!(e.to_string().contains("out of bounds"));
        }
    }

    #[test]
    fn test_get_object_misaligned() {
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

        // u32 requires 4-byte alignment
        let result = arena.get_object::<u32>(3);
        assert!(result.is_err());

        if let Err(e) = result {
            assert!(e.to_string().contains("not properly aligned"));
        }
    }

    #[test]
    fn test_get_object_valid() {
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

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
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

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
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

        let result = arena.uint32_array(3, 1);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not properly aligned"));
    }

    #[test]
    fn test_uint32_array_valid() {
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

        let array = arena.uint32_array(0, 25).unwrap();
        assert_eq!(array.len(), 25);
    }

    #[test]
    fn test_uint32_array_with_offset() {
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

        let array = arena.uint32_array(20, 20).unwrap();
        assert_eq!(array.len(), 20); // (100 - 20) / 4 = 20
    }

    #[test]
    fn test_uint32_array_len_zero() {
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

        let array = arena.uint32_array(0, 0).unwrap();
        assert_eq!(array.len(), 0);
    }
}
