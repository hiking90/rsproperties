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
        self.set_name(name);
        let error_value_len = LONG_LEGACY_ERROR.len();
        self.serial.store((error_value_len << 24 | LONG_FLAG) as u32, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            let long_property = &mut self.data.long_property;
            ptr::copy_nonoverlapping(LONG_LEGACY_ERROR.as_ptr(), long_property.error_message.as_mut_ptr(), error_value_len);
            long_property.offset = offset;
        }
    }

    pub(crate) fn init_with_value(&mut self, name: &str, value: &str) {
        self.set_name(name);
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
        unsafe {
            let self_ptr = self as *const _ as *const u8;
            let name_ptr = self_ptr.add(mem::size_of::<Self>()) as *const i8;
            CStr::from_ptr(name_ptr)
        }
    }

    pub(crate) fn value(&self) -> &CStr {
        unsafe {
            let value_ptr = self.data.value.as_ptr() as _;
            CStr::from_ptr(value_ptr)
        }
    }

    pub(crate) fn is_long(&self) -> bool {
        self.serial.load(std::sync::atomic::Ordering::Relaxed) & (LONG_FLAG as u32) != 0
    }
}