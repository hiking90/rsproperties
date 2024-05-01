// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::AtomicU32;
use std::{ptr, mem};
use std::ffi::CStr;

use crate::system_properties::PROP_VALUE_MAX;

const LONG_LEGACY_ERROR: &str = "Must use __system_property_read_callback() to read";
// static_assert(sizeof(kLongLegacyError) < prop_info::kLongLegacyErrorBufferSize,
//               "Error message for long properties read by legacy libc must fit within 56 chars");

const LONG_FLAG: usize = 1 << 16;
const LONG_LEGACY_ERROR_BUFFER_SIZE: usize = 56;

#[repr(C)]
struct LongProperty {
    error_message: [u8; LONG_LEGACY_ERROR_BUFFER_SIZE],
    offset: u32,
}

#[repr(C)]
union Union {
    value: [u8; PROP_VALUE_MAX],
    long_property: std::mem::ManuallyDrop<LongProperty>,
}

pub(crate) struct PropertyInfo {
    pub(crate) serial: AtomicU32,
    data: Union,
}

impl PropertyInfo {
    pub(crate) fn init_with_long_offset(&mut self, name: &str, offset: u32) {
        init_name_with_trailing_data(self, name);
        let error_value_len = LONG_LEGACY_ERROR.len();
        self.serial.store((error_value_len << 24 | LONG_FLAG) as u32, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            let long_property = &mut self.data.long_property;
            ptr::copy_nonoverlapping(LONG_LEGACY_ERROR.as_ptr(), long_property.error_message.as_mut_ptr(), error_value_len);
            long_property.offset = offset;
        }
    }

    pub(crate) fn init_with_value(&mut self, name: &str, value: &str) {
        init_name_with_trailing_data(self, name);
        self.serial.store((value.len() << 24) as u32, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            let dest = self.data.value.as_mut_ptr();
            ptr::copy_nonoverlapping(value.as_ptr(), dest, value.len());
            *dest.add(value.len()) = 0; // Add null terminator
        }
    }

    pub(crate) fn set_name(&mut self, name: &str) {
        unsafe {
            let self_ptr = self as *mut _ as *mut u8;
            let name_ptr = self_ptr.add(mem::size_of::<Self>()) as *mut u8;
            ptr::copy_nonoverlapping(name.as_ptr(), name_ptr, name.len());
            *name_ptr.add(name.len()) = 0; // Add null terminator
        }
    }

    pub(crate) fn name(&self) -> &CStr {
        name_from_trailing_data(self, None)
    }

    pub(crate) fn value(&self) -> &CStr {
        if self.is_long() {
            unsafe {
                let long_property = &self.data.long_property;
                let self_ptr = self as *const _ as *const u8;

                // Don't know the length of the long property value, so it depends on the null terminator.
                CStr::from_ptr(self_ptr.add(long_property.offset as usize) as *const i8)
            }
        } else {
            unsafe {
                let value_ptr = self.data.value.as_ptr() as _;
                // The length of the property value is limited to PROP_VALUE_MAX, so we can safely convert it to CStr.
                CStr::from_bytes_until_nul(std::slice::from_raw_parts(value_ptr, PROP_VALUE_MAX))
                    .expect("Failed to convert value to CStr")
            }
        }
    }

    pub(crate) fn is_long(&self) -> bool {
        self.serial.load(std::sync::atomic::Ordering::Relaxed) & (LONG_FLAG as u32) != 0
    }
}

#[inline(always)]
pub(crate) fn name_from_trailing_data<'a, I: Sized>(thiz: &'a I, len: Option<usize>) -> &'a CStr {
    unsafe {
        let thiz_ptr = thiz as *const _ as *const u8;
        let name_ptr = thiz_ptr.add(mem::size_of::<I>()) as _;
        match len {
            Some(len) => {
                CStr::from_bytes_until_nul(std::slice::from_raw_parts(name_ptr, len))
                    .expect("Failed to convert name to CStr")
            }
            None => CStr::from_ptr(name_ptr as *const i8),
        }
    }
}

#[inline(always)]
pub(crate) fn init_name_with_trailing_data<I: Sized>(thiz: &mut I, name: &str) {
    unsafe {
        let thiz_ptr = thiz as *mut _ as *mut u8;
        let name_ptr = thiz_ptr.add(mem::size_of::<I>()) as _;

        ptr::copy_nonoverlapping(name.as_ptr(), name_ptr, name.len());
        *name_ptr.add(name.len()) = 0; // Add null terminator
    }
}