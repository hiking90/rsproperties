// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::mem;
use std::str;
use std::vec::Vec;

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
    pub(crate) fn to_object<T>(&mut self, offset: usize) -> &mut T {
        // Bounds checking
        let size = mem::size_of::<T>();
        assert!(
            offset.checked_add(size).unwrap_or(usize::MAX) <= self.data.len(),
            "Object access out of bounds: offset={}, size={}, data_len={}",
            offset,
            size,
            self.data.len()
        );

        // Alignment checking
        let align = mem::align_of::<T>();
        assert!(
            offset % align == 0,
            "Object at offset {} is not properly aligned for type {} (alignment={})",
            offset,
            std::any::type_name::<T>(),
            align
        );

        // SAFETY: We've verified bounds and alignment above
        unsafe { &mut *(self.data.as_mut_ptr().add(offset) as *mut T) }
    }

    #[inline(always)]
    pub(crate) fn allocate_object<T>(&mut self) -> usize {
        let size = mem::size_of::<T>();
        let offset = self.allocate_data(size);
        offset
    }

    #[inline(always)]
    pub(crate) fn allocate_uint32_array(&mut self, length: usize) -> usize {
        let size = mem::size_of::<u32>() * length;
        let offset = self.allocate_data(size);
        offset
    }

    pub(crate) fn uint32_array(&mut self, offset: usize) -> &mut [u32] {
        // Bounds checking
        assert!(
            offset <= self.data.len(),
            "Array access out of bounds: offset={}, data_len={}",
            offset,
            self.data.len()
        );

        // Alignment checking
        assert!(
            offset % mem::align_of::<u32>() == 0,
            "Array at offset {} is not properly aligned for u32 (alignment={})",
            offset,
            mem::align_of::<u32>()
        );

        let remaining_bytes = self.data.len() - offset;
        let array_len = remaining_bytes / mem::size_of::<u32>();

        // SAFETY: We've verified bounds and alignment above
        unsafe {
            std::slice::from_raw_parts_mut(
                self.data.as_mut_ptr().add(offset) as *mut u32,
                array_len,
            )
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

    pub(crate) fn info(&self) -> PropertyInfoArea {
        PropertyInfoArea::new(&self.data)
    }

    pub(crate) fn take_data(&mut self) -> Vec<u8> {
        let mut data = std::mem::take(&mut self.data);
        data.truncate(self.current_data_pointer);

        self.current_data_pointer = 0;
        data
    }
}
