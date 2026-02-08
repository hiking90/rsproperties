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
            return Err(Error::new_file_validation(format!(
                "Object access out of bounds: offset={}, size={}, data_len={}",
                offset, size, self.data.len()
            )));
        }

        // Alignment checking - always executed
        let align = mem::align_of::<T>();
        if offset % align != 0 {
            return Err(Error::new_file_validation(format!(
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

    pub(crate) fn uint32_array(&mut self, offset: usize) -> Result<&mut [u32]> {
        // Bounds checking - always executed
        if offset > self.data.len() {
            return Err(Error::new_file_validation(format!(
                "Array access out of bounds: offset={}, data_len={}",
                offset, self.data.len()
            )));
        }

        // Alignment checking - always executed
        if offset % mem::align_of::<u32>() != 0 {
            return Err(Error::new_file_validation(format!(
                "Array at offset {} is not properly aligned for u32 (alignment={})",
                offset,
                mem::align_of::<u32>()
            )));
        }

        let remaining_bytes = self.data.len() - offset;
        let array_len = remaining_bytes / mem::size_of::<u32>();

        // SAFETY:
        // - Bounds checked: offset <= data.len()
        // - Alignment checked: offset is aligned to u32
        // - array_len calculated from remaining bytes
        unsafe {
            Ok(std::slice::from_raw_parts_mut(
                self.data.as_mut_ptr().add(offset) as *mut u32,
                array_len,
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

        // SAFETY: We just allocated the space and verified alignment in allocate_data
        unsafe {
            let ptr = self.data.as_mut_ptr().add(offset) as *mut u32;
            *ptr = value;
        }
    }

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

    pub(crate) fn take_data(&mut self) -> Vec<u8> {
        let mut data = std::mem::take(&mut self.data);
        data.truncate(self.current_data_pointer);

        self.current_data_pointer = 0;
        data
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

        let result = arena.uint32_array(101);
        assert!(result.is_err());

        if let Err(e) = result {
            assert!(e.to_string().contains("out of bounds"));
        }
    }

    #[test]
    fn test_uint32_array_misaligned() {
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

        let result = arena.uint32_array(3);
        assert!(result.is_err());

        if let Err(e) = result {
            assert!(e.to_string().contains("not properly aligned"));
        }
    }

    #[test]
    fn test_uint32_array_valid() {
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

        let result = arena.uint32_array(0);
        assert!(result.is_ok());

        let array = result.unwrap();
        assert_eq!(array.len(), 25); // 100 bytes / 4 bytes per u32
    }

    #[test]
    fn test_uint32_array_with_offset() {
        let mut arena = TrieNodeArena::new();
        arena.data = vec![0u8; 100];

        // Test with offset
        let result = arena.uint32_array(20);
        assert!(result.is_ok());

        let array = result.unwrap();
        assert_eq!(array.len(), 20); // (100 - 20) / 4 = 20
    }
}
