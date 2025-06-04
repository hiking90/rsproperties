// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

#[cfg(feature = "builder")]
use log::warn;
use std::ffi::CStr;
use std::mem;
#[cfg(feature = "builder")]
use std::ptr;
use std::sync::atomic::AtomicU32;

use crate::system_properties::PROP_VALUE_MAX;

#[cfg(feature = "builder")]
const LONG_LEGACY_ERROR: &str = "Must use __system_property_read_callback() to read";

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

#[repr(C, align(4))]
pub struct PropertyInfo {
    pub(crate) serial: AtomicU32,
    data: Union,
}

impl PropertyInfo {
    #[cfg(feature = "builder")]
    pub(crate) fn init_with_long_offset(&mut self, name: &str, offset: u32) {
        init_name_with_trailing_data(self, name);
        let error_value_len = LONG_LEGACY_ERROR.len();
        let serial_value = (error_value_len << 24 | LONG_FLAG) as u32;

        self.serial
            .store(serial_value, std::sync::atomic::Ordering::Relaxed);

        // Safe memory copy with proper bounds checking
        unsafe {
            let long_property = &mut self.data.long_property;
            let error_bytes = LONG_LEGACY_ERROR.as_bytes();
            let copy_len = error_value_len
                .min(error_bytes.len())
                .min(long_property.error_message.len());

            // Always use safe bounds-checked copy
            let dest_slice = &mut long_property.error_message[..copy_len];
            dest_slice.copy_from_slice(&error_bytes[..copy_len]);

            // Clear remaining bytes if needed
            if copy_len < long_property.error_message.len() {
                long_property.error_message[copy_len..].fill(0);
            }

            long_property.offset = offset;
        }
    }

    #[cfg(feature = "builder")]
    pub(crate) fn init_with_value(&mut self, name: &str, value: &str) {
        init_name_with_trailing_data(self, name);
        let serial_value = (value.len() << 24) as u32;

        self.serial
            .store(serial_value, std::sync::atomic::Ordering::Relaxed);

        // Safe memory copy with bounds checking
        unsafe {
            let value_bytes = value.as_bytes();
            let max_len = PROP_VALUE_MAX.saturating_sub(1); // Reserve space for null terminator
            let copy_len = value_bytes.len().min(max_len);

            let dest_slice = &mut self.data.value[..copy_len];
            dest_slice.copy_from_slice(&value_bytes[..copy_len]);

            // Add null terminator
            if copy_len < PROP_VALUE_MAX {
                self.data.value[copy_len] = 0;
            }
        }
    }
    /*
        pub(crate) fn set_name(&mut self, name: &str) {
            unsafe {
                let self_ptr = self as *mut _ as *mut u8;
                let name_ptr = self_ptr.add(mem::size_of::<Self>()) as *mut u8;
                ptr::copy_nonoverlapping(name.as_ptr(), name_ptr, name.len());
                *name_ptr.add(name.len()) = 0; // Add null terminator
            }
        }
    */
    pub(crate) fn name(&self) -> &CStr {
        name_from_trailing_data(self, None)
    }

    pub(crate) fn value(&self) -> &CStr {
        if self.is_long() {
            unsafe {
                let long_property = &self.data.long_property;
                let self_ptr = self as *const _ as *const u8;
                let value_ptr = self_ptr.add(long_property.offset as usize) as *const i8;

                // Don't know the length of the long property value, so it depends on the null terminator.
                CStr::from_ptr(value_ptr)
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

    // TODO: self must be mutable. The current implementation is a workaround.
    #[cfg(feature = "builder")]
    pub(crate) fn set_value(&self, value: &str) {
        if self.is_long() {
            warn!("Attempting to set value on long property - this may not work correctly");
        }

        // Safe memory copy with bounds checking
        unsafe {
            let value_bytes = value.as_bytes();
            let max_len = PROP_VALUE_MAX.saturating_sub(1); // Reserve space for null terminator
            let copy_len = value_bytes.len().min(max_len);

            let dest_ptr = self.data.value.as_ptr() as *mut u8;
            let dest_slice = std::slice::from_raw_parts_mut(dest_ptr, copy_len);
            dest_slice.copy_from_slice(&value_bytes[..copy_len]);

            // Add null terminator
            if copy_len < PROP_VALUE_MAX {
                *dest_ptr.add(copy_len) = 0;
            }
        }
    }

    pub(crate) fn is_long(&self) -> bool {
        let serial = self.serial.load(std::sync::atomic::Ordering::Relaxed);
        serial & (LONG_FLAG as u32) != 0
    }
}

#[inline(always)]
pub(crate) fn name_from_trailing_data<I: Sized>(thiz: &I, len: Option<usize>) -> &CStr {
    unsafe {
        let thiz_ptr = thiz as *const _ as *const u8;
        let name_ptr = thiz_ptr.add(mem::size_of::<I>()) as _;
        match len {
            Some(len) => CStr::from_bytes_until_nul(std::slice::from_raw_parts(name_ptr, len + 1))
                .expect("Failed to convert name to CStr"),
            None => CStr::from_ptr(name_ptr as *const i8),
        }
    }
}

#[cfg(feature = "builder")]
#[inline(always)]
pub(crate) fn init_name_with_trailing_data<I: Sized>(thiz: &mut I, name: &str) {
    unsafe {
        let thiz_ptr = thiz as *mut _ as *mut u8;
        let name_ptr = thiz_ptr.add(mem::size_of::<I>()) as _;

        ptr::copy_nonoverlapping(name.as_ptr(), name_ptr, name.len());
        *name_ptr.add(name.len()) = 0; // Add null terminator
    }
}
