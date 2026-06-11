// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::atomic::{fence, AtomicU32, Ordering};
#[cfg(any(target_os = "android", target_os = "linux"))]
use std::time::{Duration, Instant};

use rustix::fs::Timespec;
#[cfg(any(target_os = "android", target_os = "linux"))]
use rustix::thread::futex;

use crate::errors::*;

use crate::contexts_serialized::ContextsSerialized;
use crate::property_info::PropertyInfo;

pub(crate) use crate::wire::PROP_VALUE_MAX;
pub(crate) const PROP_TREE_FILE: &str = "/dev/__properties__/property_info";

#[inline(always)]
fn serial_dirty(serial: u32) -> bool {
    (serial & 1) != 0
}

#[cfg(feature = "builder")]
fn futex_wake(_addr: &AtomicU32) -> Result<usize> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        futex::wake(_addr, futex::Flags::empty(), i32::MAX as u32)
            .context_with_location("Failed to wake futex")
    }
    #[cfg(target_os = "macos")]
    Ok(0)
}

/// Waits until `_serial` differs from `_value`, returning the new serial.
///
/// Returns `None` on timeout (or on macOS, where no futex is available and
/// this is an immediate no-op — see `SystemProperties::wait`).
fn futex_wait(_serial: &AtomicU32, _value: u32, _timeout: Option<&Timespec>) -> Option<u32> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        use rustix::io::Errno;
        // Linux futex_wait takes a *relative* timeout. Spurious wakes restart
        // the syscall, so we track a deadline and shrink the remaining timeout
        // each iteration to keep the total wait bounded by the caller-supplied
        // value.
        //
        // `Timespec.tv_sec`/`tv_nsec` are signed (i64). Negative values are
        // not valid timeouts; treat them as immediate timeout to avoid
        // panicking in `Instant + Duration` from a `usize::MAX`-ish wrap.
        let deadline = match _timeout {
            None => None,
            Some(t) if t.tv_sec < 0 || t.tv_nsec < 0 || t.tv_nsec >= 1_000_000_000 => {
                return None;
            }
            Some(t) => Some(Instant::now() + Duration::new(t.tv_sec as u64, t.tv_nsec as u32)),
        };
        loop {
            let remaining_ts = match deadline {
                None => None,
                Some(d) => {
                    let r = d.saturating_duration_since(Instant::now());
                    if r.is_zero() {
                        return None;
                    }
                    Some(Timespec {
                        tv_sec: r.as_secs() as _,
                        tv_nsec: r.subsec_nanos() as _,
                    })
                }
            };
            match futex::wait(
                _serial,
                futex::Flags::empty(),
                _value as _,
                remaining_ts.as_ref(),
            ) {
                Ok(_) => {
                    let new_serial = _serial.load(Ordering::Acquire);
                    if _value != new_serial {
                        return Some(new_serial);
                    }
                    // Spurious wake — loop with the recomputed remaining timeout.
                }
                // EAGAIN: the serial no longer equals `_value` at syscall
                // time — i.e. the property changed between the caller's load
                // and the wait. This is the *common* race, not a failure;
                // bionic's wait loop falls through to the serial re-check
                // and reports success. Treating it as an error here would
                // silently swallow a real property change.
                Err(Errno::AGAIN) => {
                    let new_serial = _serial.load(Ordering::Acquire);
                    if _value != new_serial {
                        return Some(new_serial);
                    }
                    // Serial changed and wrapped back to `_value` between the
                    // syscall and the reload — vanishingly unlikely; retry.
                }
                // Interrupted by a signal — retry with the recomputed
                // remaining timeout so the total wait stays bounded.
                Err(Errno::INTR) => {}
                // Timeout is a normal outcome, not an error worth logging.
                Err(Errno::TIMEDOUT) => return None,
                Err(e) => {
                    log::error!("Failed to wait for property change: {e}");
                    return None;
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (_serial, _value, _timeout);
        None
    }
}

// To avoid lifetime issues, the property index is used to access the property value.
pub struct PropertyIndex {
    pub(crate) context_index: u32,
    pub(crate) property_index: u32,
}

/// System properties
/// It can't be created directly. Use `system_properties()` or `system_properties_area()` instead.
pub struct SystemProperties {
    contexts: ContextsSerialized,
}

impl SystemProperties {
    // Create a new system properties to read system properties from a file or a directory.
    pub(crate) fn new(filename: &Path) -> Result<Self> {
        let contexts = match ContextsSerialized::new(false, filename, &mut false, false) {
            Ok(contexts) => contexts,
            Err(e) => {
                log::error!("Failed to load contexts from {filename:?}: {e}");
                return Err(e);
            }
        };

        Ok(Self { contexts })
    }

    // Create a new area for system properties
    // The new area is used by the property service to store system properties.
    #[cfg(feature = "builder")]
    pub fn new_area(dirname: &Path) -> Result<Self> {
        let contexts = match ContextsSerialized::new(true, dirname, &mut false, false) {
            Ok(contexts) => contexts,
            Err(e) => {
                log::error!("Failed to create area from {dirname:?}: {e}");
                return Err(e);
            }
        };

        Ok(Self { contexts })
    }

    /// Reads the mutable property value under the seqlock protocol and
    /// hands the validated `&str` to `f`. The callback is invoked exactly
    /// once, on the iteration whose pre/post serial reads agree — earlier
    /// iterations (torn reads, dirty bit set then cleared) are absorbed by
    /// the retry loop.
    ///
    /// Returning a value through `f` instead of allocating a `String` is
    /// what makes the parse-and-discard hot path (`get<T>`/`get_or<T>`)
    /// allocation-free for short and long properties alike.
    fn read_with_callback<R, F>(
        &self,
        pa: &crate::property_area::PropertyAreaMap,
        prop_info: &PropertyInfo,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&str) -> R,
    {
        let bound = pa.max_value_bound(prop_info);
        // Reused across retries — short-variant reads borrow from this
        // stack buffer so the seqlock loop allocates nothing.
        let mut buf = [0u8; PROP_VALUE_MAX];
        // `FnOnce` must be consumed exactly once, but the retry loop may
        // iterate multiple times. Park it in `Option` so a successful
        // serial match can `take()` and call it.
        let mut f = Some(f);
        loop {
            // Read current serial at the beginning of each iteration
            let serial = prop_info.serial.load(Ordering::Acquire);

            // Read RAW bytes (no UTF-8 validation yet) — byte-wise atomic
            // reads can surface partially-written multi-byte sequences when
            // a writer is mid-update. Deferring UTF-8 validation until
            // *after* the serial re-check lets the retry loop absorb those
            // spurious decodes instead of bailing on `?`.
            let bytes: &[u8] = if serial_dirty(serial) {
                let backup = pa.dirty_backup_area().map_err(|e| {
                    log::error!("Failed to read dirty backup area: {e}");
                    e
                })?;
                backup.to_bytes()
            } else {
                // `value_bytes` returns Cow — short borrows `buf`, long
                // borrows directly from the mmap. Either way no heap
                // allocation. The borrow is alive across the serial
                // re-check below so we can hand it to `f` on success.
                let cow = prop_info.value_bytes(bound, &mut buf)?;
                let serial_check = {
                    fence(Ordering::Acquire);
                    prop_info.serial.load(Ordering::Acquire)
                };
                if serial_check == serial {
                    let s = std::str::from_utf8(&cow).map_err(|e| {
                        Error::Encoding(format!("property value is not valid UTF-8: {e}"))
                    })?;
                    return Ok(f.take().expect("callback consumed once on success")(s));
                }
                continue;
            };

            // Dirty path: backup is a `&CStr` that doesn't borrow from `buf`,
            // so the original fence/recheck pattern works as before.
            fence(Ordering::Acquire);
            let final_serial = prop_info.serial.load(Ordering::Acquire);
            if final_serial == serial {
                let s = std::str::from_utf8(bytes).map_err(|e| {
                    Error::Encoding(format!("property value is not valid UTF-8: {e}"))
                })?;
                return Ok(f.take().expect("callback consumed once on success")(s));
            }
            // serial changed → retry; spurious UTF-8 from a torn read is
            // naturally absorbed here.
        }
    }

    /// Reads `name`'s value and passes it to `f` as `&str` without ever
    /// materialising an owned `String`. Intended for the parse-and-discard
    /// hot path (`get<T>`, `get_or<T>`) where the caller does not need
    /// ownership of the value bytes.
    ///
    /// Mirrors bionic's `__system_property_read_callback` pattern. The
    /// callback runs while the seqlock-validated bytes are still borrowed
    /// (from `buf` for short properties, from the mmap for long ones), so
    /// it should be cheap and non-blocking.
    pub fn read_with<R, F>(&self, name: &str, f: F) -> Result<R>
    where
        F: FnOnce(&str) -> R,
    {
        let res = match self.contexts.prop_area_for_name(name) {
            Ok(res) => res,
            Err(e) => {
                log::error!("Failed to find property area for {name}: {e}");
                return Err(e);
            }
        };
        let pa = res.0.property_area();

        match pa.find(name) {
            Ok(pi) => match self.read_with_callback(pa, pi.0, f) {
                Ok(r) => Ok(r),
                Err(e) => {
                    log::error!("Failed to read property {name}: {e}");
                    Err(e)
                }
            },
            Err(e) => Err(e),
        }
    }

    /// Get property value that returns error for missing properties.
    ///
    /// Allocates a `String`; for the parse-and-discard hot path prefer
    /// [`Self::read_with`], which hands the value as `&str` without
    /// allocating.
    pub fn get_with_result(&self, name: &str) -> Result<String> {
        self.read_with(name, str::to_owned)
    }

    /// Get the property index of a system property by name.
    /// The property index is used to update the property value.
    /// If the property is not found, it returns Ok(None)
    pub fn find(&self, name: &str) -> Result<Option<PropertyIndex>> {
        let res = match self.contexts.prop_area_for_name(name) {
            Ok(res) => res,
            Err(e) => {
                log::error!("Failed to find property area for {name}: {e}");
                return Err(e);
            }
        };
        let pa = res.0.property_area();
        match pa.find(name) {
            Ok(pi) => {
                let index = PropertyIndex {
                    context_index: res.1,
                    property_index: pi.1,
                };
                Ok(Some(index))
            }
            Err(_) => Ok(None),
        }
    }

    /// Set the value of a system property
    /// If the property is not found, it creates a new property.
    /// If the property value is too long, it returns an error.
    /// If the property is read-only, it returns an error.
    /// If the property is updated successfully, it returns Ok(()).
    #[cfg(feature = "builder")]
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        match self.find(name)? {
            Some(prop_ref) => match self.update(&prop_ref, value) {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Failed to update property {name}: {e}");
                    return Err(e);
                }
            },
            None => match self.add(name, value) {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Failed to create property {name}: {e}");
                    return Err(e);
                }
            },
        }

        Ok(())
    }

    #[cfg(feature = "builder")]
    pub fn update(&mut self, index: &PropertyIndex, value: &str) -> Result<bool> {
        // Pre-flight value-length check — `update` cannot promote to a long
        // property in-place (`PropertyInfoWriter::apply_write` rejects on
        // LONG_FLAG). Pass an empty name so the `ro.` exception in
        // `validate_value_len` doesn't apply here.
        if let Err(e) = crate::wire::validate_value_len("", value) {
            log::error!("{e}");
            return Err(Error::FileValidation(e));
        }

        let mut res = match self.contexts.prop_area_mut_with_index(index.context_index) {
            Ok(res) => res,
            Err(e) => {
                log::error!(
                    "Failed to get mutable property area for context {}: {}",
                    index.context_index,
                    e
                );
                return Err(e);
            }
        };
        let pa = res.property_area_mut();

        // Inspect through `&pi` first: validate ro., snapshot backup into a
        // stack buffer. `pi` borrow is dropped at the end of this block so
        // we can take `&mut pa` for `set_dirty_backup_area` immediately
        // after. The buffer outlives the inner borrow scope, so the bytes
        // it captured remain valid after `pi`/`cow` go out of scope.
        let mut backup_buf = [0u8; crate::wire::PROP_VALUE_MAX];
        let backup_len = {
            let pi = pa.property_info(index.property_index).map_err(|e| {
                log::error!(
                    "Failed to get property info for index {}: {e}",
                    index.property_index
                );
                e
            })?;
            let bound = pa.max_value_bound(pi);
            let name = pi.name(bound)?.to_bytes();
            if name.starts_with(b"ro.") {
                let error_msg = format!("Try to update the read-only property: {name:?}");
                log::error!("{error_msg}");
                return Err(Error::PermissionDenied(error_msg));
            }
            // Pre-flight LONG check: if the entry was created long, we can't
            // overwrite it in-place. Checking *before* writing the backup
            // keeps backup_area aligned with the entry it shadows.
            if pi.is_long() {
                let error_msg =
                    format!("in-place update of long property is not supported: {name:?}");
                log::error!("{error_msg}");
                return Err(Error::FileValidation(error_msg));
            }
            // After the LONG check, `value_bytes` is guaranteed to return
            // the short variant — a `Cow::Borrowed(&backup_buf[..len])`.
            // Capturing only the length lets us drop the borrow without
            // losing the bytes that already live in `backup_buf`.
            pi.value_bytes(bound, &mut backup_buf)?.len()
        };

        // Back up the current value so concurrent readers can observe a
        // consistent snapshot via the dirty bit. No standalone fence needed:
        // the Release stores inside `apply_write` synchronize this write
        // with readers.
        pa.set_dirty_backup_area(&backup_buf[..backup_len])
            .map_err(|e| {
                log::error!("Failed to set backup area: {e}");
                e
            })?;

        // Single-transaction publish: set_dirty → write → publish_serial.
        // Encapsulated in writer so a half-published state is impossible.
        let pi = pa.property_info_mut(index.property_index).map_err(|e| {
            log::error!("Failed to get mutable property info after backup: {e}");
            e
        })?;
        pi.writer().apply_write(value)?;

        if let Err(e) = futex_wake(&pi.serial) {
            log::error!("Failed to wake property futex: {e}");
            return Err(e);
        }

        let serial_pa = self.contexts.serial_prop_area();
        // Atomic RMW: multiple service writers (or multi-process mmap sharing)
        // would otherwise lose updates with a load + store pair.
        serial_pa.serial().fetch_add(1, Ordering::Release);

        if let Err(e) = futex_wake(serial_pa.serial()) {
            log::error!("Failed to wake global serial futex: {e}");
            return Err(e);
        }

        Ok(true)
    }

    /// Adds a new property.
    ///
    /// If a property with `name` already exists this is a silent no-op that
    /// returns `Ok(())` **without** updating the value — the same contract
    /// as bionic `prop_area::add`. Use [`Self::set`] (or `find` +
    /// [`Self::update`]) for create-or-update semantics.
    #[cfg(feature = "builder")]
    pub fn add(&mut self, name: &str, value: &str) -> Result<()> {
        // Shared policy across client/server: only `ro.` names may exceed
        // PROP_VALUE_MAX (stored as long properties).
        if let Err(e) = crate::wire::validate_value_len(name, value) {
            log::error!("{e}");
            return Err(Error::FileValidation(e));
        }

        let mut res = match self.contexts.prop_area_mut_for_name(name) {
            Ok(res) => res,
            Err(e) => {
                log::error!("Failed to get mutable property area for {name}: {e}");
                return Err(e);
            }
        };
        let pa = res.0.property_area_mut();

        match pa.add(name, value) {
            Ok(_) => {}
            Err(e) => {
                log::error!("Failed to add property {name} to area: {e}");
                return Err(e);
            }
        }

        let serial_pa = self.contexts.serial_prop_area();
        // Atomic RMW: see note in `update`.
        serial_pa.serial().fetch_add(1, Ordering::Release);

        match futex_wake(serial_pa.serial()) {
            Ok(_) => {}
            Err(e) => {
                log::error!("Failed to wake global serial futex after adding property: {e}");
                return Err(e);
            }
        }

        Ok(())
    }

    pub fn context_serial(&self) -> u32 {
        let serial_pa = self.contexts.serial_prop_area();
        serial_pa.serial().load(Ordering::Acquire)
    }

    /// Reads the per-property serial counter, or `None` if the context/property
    /// lookup fails. `0` is a valid initial serial, so callers cannot use a
    /// numeric sentinel — use the `Option` to distinguish absence.
    pub fn serial(&self, idx: &PropertyIndex) -> Option<u32> {
        let guard = self
            .contexts
            .prop_area_with_index(idx.context_index)
            .map_err(|e| {
                log::error!(
                    "Failed to get PropertyArea for index {}: {e}",
                    idx.context_index
                );
                e
            })
            .ok()?;
        let pa = guard.property_area();
        let pi = pa
            .property_info(idx.property_index)
            .map_err(|e| {
                log::error!(
                    "Failed to get PropertyInfo for index {}: {e}",
                    idx.property_index
                );
                e
            })
            .ok()?;
        Some(pi.serial.load(Ordering::Acquire))
    }

    /// Waits for any property to change. Equivalent to
    /// `wait(None, None, None)`; see [`Self::wait`] for the race caveat of
    /// not passing an `old_serial`.
    pub fn wait_any(&self) -> Option<u32> {
        self.wait(None, None, None)
    }

    /// Waits until the property at `index` (or, with `index == None`, the
    /// global serial — i.e. any property) changes, returning the new serial.
    /// Returns `None` on timeout or lookup failure.
    ///
    /// `old_serial` should be the serial observed when the caller last read
    /// the value (via [`Self::serial`] / [`Self::context_serial`]). Passing
    /// `Some` closes the lost-wakeup window between reading a value and
    /// entering the wait: if the property already changed since
    /// `old_serial`, this returns immediately with the new serial — the
    /// same contract as bionic `__system_property_wait(pi, old_serial, …)`.
    /// With `None`, the current serial is sampled at entry, so a change
    /// that lands before this call is only observed at the *next* change.
    ///
    /// On macOS there is no futex; this returns `None` immediately, so do
    /// not call it in a polling loop there.
    pub fn wait(
        &self,
        index: Option<&PropertyIndex>,
        old_serial: Option<u32>,
        timeout: Option<&Timespec>,
    ) -> Option<u32> {
        match index {
            Some(idx) => match self.contexts.prop_area_with_index(idx.context_index).ok() {
                Some(guard) => {
                    let pa = guard.property_area();
                    match pa.property_info(idx.property_index).ok() {
                        Some(pi) => {
                            let old =
                                old_serial.unwrap_or_else(|| pi.serial.load(Ordering::Acquire));
                            futex_wait(&pi.serial, old, timeout)
                        }
                        None => {
                            log::error!(
                                "Failed to get PropertyInfo for index: {}",
                                idx.property_index
                            );
                            None
                        }
                    }
                }
                None => {
                    log::error!(
                        "Failed to get PropertyArea for index: {}",
                        idx.context_index
                    );
                    None
                }
            },
            None => {
                let serial_pa = self.contexts.serial_prop_area().serial();
                let old = old_serial.unwrap_or_else(|| serial_pa.load(Ordering::Acquire));
                futex_wait(serial_pa, old, timeout)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_imports)]
    use super::*;

    #[cfg(target_os = "android")]
    use android_system_properties::AndroidSystemProperties;

    #[cfg(target_os = "android")]
    const VERSION_PROPERTY: &str = "ro.build.version.release";

    #[cfg(target_os = "android")]
    #[test]
    fn test_system_properties() -> Result<()> {
        let system_properties = SystemProperties::new(&Path::new(crate::PROP_DIRNAME)).unwrap();

        let handle = std::thread::spawn(move || {
            let version1 = system_properties
                .get_with_result(VERSION_PROPERTY)
                .unwrap_or_default();
            let version2 = AndroidSystemProperties::new()
                .get(VERSION_PROPERTY)
                .unwrap_or_default();
            assert_eq!(version1, version2);
        });

        let _ = handle.join().unwrap();

        Ok(())
    }
}
