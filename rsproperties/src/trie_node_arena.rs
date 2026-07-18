// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::mem;

use zerocopy::{FromBytes, IntoBytes};

use crate::errors::{Error, Result};
use crate::property_info_parser::*;

#[derive(Debug)]
pub(crate) struct TrieNodeArena {
    /// `Vec<u32>`, not `Vec<u8>`: the parser side (`PropertyInfoArea`)
    /// reinterprets the buffer as 4-byte-aligned structs, and a `Vec<u8>`
    /// base address is only guaranteed 1-aligned by the language — every
    /// real allocator hands back more, but that is an observation, not a
    /// contract. A `u32` backing makes the 4-alignment a language-level
    /// guarantee, so `PropertyInfoArea::new`'s alignment assert can never
    /// fire on the builder path. Offsets and `current_data_pointer` remain
    /// in **bytes**; byte views are derived via zerocopy (`as_bytes`).
    data: Vec<u32>,
    current_data_pointer: usize,
}

/// Compile-time guard for arena-stored types: alignment must not exceed
/// the 4 bytes the `Vec<u32>` backing guarantees. The runtime check in
/// `get_object` validates the *runtime pointer*, which for `align > 4`
/// would pass or fail depending on where the allocator happened to place
/// the buffer — while the parser's mmap side re-checks the *file offset*
/// and would reject the file. A stricter-aligned type must fail the
/// build, not produce files the builder blesses and the parser rejects.
struct AlignFitsArena<T>(std::marker::PhantomData<T>);
impl<T> AlignFitsArena<T> {
    const OK: () = assert!(
        mem::align_of::<T>() <= mem::size_of::<u32>(),
        "arena-stored types must have alignment <= 4 (the Vec<u32> backing guarantee)"
    );
}

impl TrieNodeArena {
    pub(crate) fn new() -> Self {
        Self {
            data: Vec::with_capacity(16 * 1024 / mem::size_of::<u32>()),
            current_data_pointer: 0,
        }
    }

    /// Whole-buffer byte view (including growth slack). Callers slice it
    /// with byte offsets; bounds against the allocated extent are the
    /// responsibility of the checked accessors below.
    #[inline(always)]
    fn bytes(&self) -> &[u8] {
        self.data.as_bytes()
    }

    #[inline(always)]
    fn bytes_mut(&mut self) -> &mut [u8] {
        self.data.as_mut_bytes()
    }

    /// Reinterprets `size_of::<T>()` bytes at `offset` as `&mut T`.
    ///
    /// No `unsafe`: `mut_from_bytes` validates the size and the *actual
    /// pointer* alignment at runtime (the `u32` backing guarantees 4-byte
    /// base alignment; types with a larger alignment still get caught
    /// here), and the `FromBytes + IntoBytes` bounds guarantee every bit
    /// pattern of the zero-filled arena is a valid `T` — instantiating
    /// with e.g. `bool` or an enum fails to compile instead of being
    /// invalid-value UB.
    ///
    /// Bounds are checked against `current_data_pointer` (the allocated
    /// extent), not the buffer length — `allocate_data`'s growth `resize`
    /// leaves slack that would otherwise let a miscomputed offset silently
    /// read or write unallocated zero bytes.
    #[inline(always)]
    pub(crate) fn get_object<T>(&mut self, offset: usize) -> Result<&mut T>
    where
        T: FromBytes + zerocopy::IntoBytes + zerocopy::KnownLayout,
    {
        let () = AlignFitsArena::<T>::OK;
        let size = mem::size_of::<T>();
        let end = offset.saturating_add(size);
        if end > self.current_data_pointer {
            return Err(Error::FileValidation(format!(
                "Object access out of bounds: offset={}, size={}, allocated={}",
                offset, size, self.current_data_pointer
            )));
        }

        T::mut_from_bytes(&mut self.bytes_mut()[offset..end]).map_err(|e| {
            Error::FileValidation(format!(
                "Object at offset {} is not properly aligned for type {}: {e}",
                offset,
                std::any::type_name::<T>(),
            ))
        })
    }

    #[inline(always)]
    pub(crate) fn allocate_object<T>(&mut self) -> Result<u32> {
        // Guarded at allocation too — this is where a stricter-aligned
        // type would first break the offset discipline.
        let () = AlignFitsArena::<T>::OK;
        self.allocate_data(mem::size_of::<T>())
    }

    #[inline(always)]
    pub(crate) fn allocate_uint32_array(&mut self, length: usize) -> Result<u32> {
        let size = length
            .checked_mul(mem::size_of::<u32>())
            .ok_or_else(|| Error::FileValidation(format!("Array len overflow: {length}")))?;
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

        <[u32]>::mut_from_bytes(&mut self.bytes_mut()[offset..end]).map_err(|e| {
            Error::FileValidation(format!(
                "Array at offset {offset} is not properly aligned for u32: {e}"
            ))
        })
    }

    pub(crate) fn allocate_and_write_string(&mut self, string: &str) -> Result<u32> {
        let len = string.len();
        let offset = self.allocate_data(len + 1)?;
        let start = offset as usize;

        let dst = self.bytes_mut();
        dst[start..start + len].copy_from_slice(string.as_bytes());
        dst[start + len] = 0; // null terminator
        Ok(offset)
    }

    pub(crate) fn allocate_and_write_uint32(&mut self, value: u32) -> Result<()> {
        let offset = self.allocate_data(mem::size_of::<u32>())? as usize;
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
        self.bytes_mut()[offset..offset + mem::size_of::<u32>()]
            .copy_from_slice(&value.to_ne_bytes());
        Ok(())
    }

    /// Reserves `size` bytes, rounded up to a multiple of `size_of::<u32>()`,
    /// from the arena's growing buffer. Returns the byte offset of the
    /// allocation, which is guaranteed to be `u32`-aligned (the pointer
    /// starts at 0 and every allocation is u32-aligned in length, so the
    /// invariant is preserved across calls).
    ///
    /// The on-disk format stores offsets as `u32`, and the bound is
    /// enforced HERE, at allocation time: every offset this arena ever
    /// returns fits `u32` by construction, so the serializer needs no
    /// retroactive "if the final size fits, all earlier casts did"
    /// argument — over-large input fails with a typed error at the
    /// allocation that crossed the line.
    fn allocate_data(&mut self, size: usize) -> Result<u32> {
        let aligned_size = crate::bionic_align(size, mem::size_of::<u32>());

        let needed = self
            .current_data_pointer
            .checked_add(aligned_size)
            .filter(|&n| u32::try_from(n).is_ok())
            .ok_or_else(|| {
                Error::FileValidation(format!(
                    "Serialized property info exceeds the u32 offset space: {} + {aligned_size} bytes",
                    self.current_data_pointer
                ))
            })?;
        let byte_len = self.data.len() * mem::size_of::<u32>();
        if needed > byte_len {
            // Standard doubling growth, but never less than what this
            // allocation needs. `aligned_size` is a multiple of 4, so
            // `needed` always divides into whole u32 words.
            let target_bytes = needed.max(byte_len.saturating_mul(2));
            self.data
                .resize(target_bytes.div_ceil(mem::size_of::<u32>()), 0);
        }

        // Fits by the `needed` gate above (`offset <= needed <= u32::MAX`).
        let offset = self.current_data_pointer as u32;
        self.current_data_pointer += aligned_size;

        Ok(offset)
    }

    pub(crate) fn size(&self) -> usize {
        self.current_data_pointer
    }

    /// Parser view of the arena. Sliced to the allocated extent — handing
    /// out the growth slack too would bypass the `current_data_pointer`
    /// bounds discipline the checked accessors above enforce: the parser's
    /// own bounds checks run against `data_base.len()`, so a miscomputed
    /// offset into zero-filled slack would read 0 instead of erroring.
    pub(crate) fn info(&self) -> PropertyInfoArea<'_> {
        PropertyInfoArea::new(&self.bytes()[..self.current_data_pointer])
    }

    /// Consumes the arena into the serialized byte image. One copy — the
    /// price of the `u32` backing that guarantees alignment during the
    /// build; callers receive a plain `Vec<u8>` as before.
    pub(crate) fn into_data(self) -> Vec<u8> {
        self.bytes()[..self.current_data_pointer].to_vec()
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
        arena.allocate_data(size).unwrap();
        arena
    }

    #[test]
    fn test_get_object_out_of_bounds() {
        let mut arena = arena_with(100);

        // offset + size > allocated extent. `[u32; 2]` (size 8, align 4) —
        // an align-8 type like u64 no longer compiles against the arena.
        let result = arena.get_object::<[u32; 2]>(96);
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
        arena.allocate_data(4).unwrap();

        assert!(arena.data.len() * mem::size_of::<u32>() > 108);
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
