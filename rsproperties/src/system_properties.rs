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

/// Outcome of one [`futex_wait`] call. A three-way result rather than
/// `Option`: the sliced wait loop in [`SystemProperties::wait`] must retry
/// on an ordinary timeout but bail out on a syscall-level failure —
/// collapsing both into `None` would turn a persistent futex error into a
/// busy loop.
#[derive(Clone, Copy, Debug)]
// On macOS only `Failed` is ever constructed (no futex support).
#[cfg_attr(target_os = "macos", allow(dead_code))]
enum FutexWaitOutcome {
    /// The serial changed; carries the freshly-loaded value.
    Changed(u32),
    /// The timeout elapsed (or the caller passed an invalid/negative one).
    TimedOut,
    /// Unexpected futex error — or no futex support on this platform.
    Failed,
}

/// Waits until `_serial` differs from `_value`, or the timeout elapses.
///
/// On macOS there is no futex; returns [`FutexWaitOutcome::Failed`]
/// immediately — see `SystemProperties::wait`.
fn futex_wait(_serial: &AtomicU32, _value: u32, _timeout: Option<&Timespec>) -> FutexWaitOutcome {
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
        // A *huge* positive `tv_sec` (e.g. i64::MAX, a reasonable "wait
        // forever") is the opposite hazard: `Instant + Duration` panics on
        // overflow, so an unrepresentable deadline degrades to an infinite
        // wait instead — matching bionic, which passes the value through to
        // the futex untouched.
        let deadline = match _timeout {
            None => None,
            Some(t) if t.tv_sec < 0 || t.tv_nsec < 0 || t.tv_nsec >= 1_000_000_000 => {
                return FutexWaitOutcome::TimedOut;
            }
            Some(t) => Instant::now().checked_add(Duration::new(t.tv_sec as u64, t.tv_nsec as u32)),
        };
        loop {
            let remaining_ts = match deadline {
                None => None,
                Some(d) => {
                    let r = d.saturating_duration_since(Instant::now());
                    if r.is_zero() {
                        return FutexWaitOutcome::TimedOut;
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
                        return FutexWaitOutcome::Changed(new_serial);
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
                        return FutexWaitOutcome::Changed(new_serial);
                    }
                    // Serial changed and wrapped back to `_value` between the
                    // syscall and the reload — vanishingly unlikely; retry.
                }
                // Interrupted by a signal — retry with the recomputed
                // remaining timeout so the total wait stays bounded.
                Err(Errno::INTR) => {}
                // Timeout is a normal outcome, not an error worth logging.
                Err(Errno::TIMEDOUT) => return FutexWaitOutcome::TimedOut,
                Err(e) => {
                    log::error!("Failed to wait for property change: {e}");
                    return FutexWaitOutcome::Failed;
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (_serial, _value, _timeout);
        FutexWaitOutcome::Failed
    }
}

// To avoid lifetime issues, the property index is used to access the property value.
#[derive(Clone, Copy, Debug)]
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
        let contexts = match ContextsSerialized::new(false, filename, false) {
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
        let contexts = match ContextsSerialized::new(true, dirname, false) {
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
        pi_offset: u32,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&str) -> R,
    {
        let prop_info = pa.property_info(pi_offset)?;
        // Long entries are write-once (their serial never changes after
        // init), so resolve the out-of-line bytes once, outside the retry
        // loop — re-validating the entry every iteration is pure overhead
        // on this hot path. Short entries snapshot per iteration below.
        let long_bytes: Option<&[u8]> = if prop_info.is_long() {
            Some(pa.long_property_value(pi_offset)?)
        } else {
            None
        };
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
                // The backup slot is shared per-area and may be rewritten by
                // the *next* update the moment the current one completes, so
                // it must be snapshotted into the stack buffer BEFORE the
                // fence/serial re-check below — nothing may re-read the slot
                // after validation (bionic memcpys before its fence for the
                // same reason). While dirty, the top 8 bits of `serial`
                // still hold the *old* value length, which is exactly the
                // backup snapshot's length; clamp defends against a corrupt
                // length field.
                let len = ((serial >> 24) as usize).min(PROP_VALUE_MAX - 1);
                pa.read_dirty_backup(&mut buf[..len])?;
                &buf[..len]
            } else if let Some(bytes) = long_bytes {
                // Write-once long bytes, resolved before the loop —
                // re-reading them after the re-check below is stable.
                bytes
            } else {
                // Short: byte-wise atomic snapshot into the stack buffer.
                prop_info.short_value_bytes(&mut buf)
            };

            fence(Ordering::Acquire);
            // `Relaxed` is sufficient for the re-check: the fence above
            // pairs with the writer's release fence and provides all the
            // ordering the protocol needs (bionic's post-fence re-load is
            // likewise relaxed). An Acquire here costs an `ldar` per retry
            // on aarch64 for nothing.
            let final_serial = prop_info.serial.load(Ordering::Relaxed);
            if final_serial == serial {
                // `Error::Utf8`, not `Encoding(String)`: keep every UTF-8
                // decode failure on the same source-preserving variant.
                let s = std::str::from_utf8(bytes).map_err(Error::Utf8)?;
                return Ok(f.take().expect("callback consumed once on success")(s));
            }
            // serial changed → retry; spurious UTF-8 from a torn read is
            // naturally absorbed here. The loop is unbounded like bionic's,
            // so at least tell the CPU it's a spin-wait (SMT/power hint).
            std::hint::spin_loop();
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
    ///
    /// The callback also runs under the context node's read lock: a
    /// callback that blocks until a *same-process* builder writer makes
    /// progress deadlocks (the writer needs that node's write lock) —
    /// same caution as [`Self::wait`].
    pub fn read_with<R, F>(&self, name: &str, f: F) -> Result<R>
    where
        F: FnOnce(&str) -> R,
    {
        let res = match self.contexts.prop_area_for_name(name) {
            Ok(res) => res,
            // Don't add a second log line for NotFound: the layer below
            // already reports it at the appropriate level (debug for an
            // unknown context, error for a corrupt-at-init slot). Other
            // errors (corrupt mapping, poisoned lock) get logged here with
            // the property name for context.
            Err(e @ Error::NotFound(_)) => return Err(e),
            Err(e) => {
                log::error!("Failed to find property area for {name}: {e}");
                return Err(e);
            }
        };
        let pa = res.0.property_area();

        match pa.find(name) {
            Ok((_, pi_offset)) => match self.read_with_callback(pa, pi_offset, f) {
                Ok(r) => Ok(r),
                Err(e) => {
                    log::error!("Failed to read property {name}: {e}");
                    Err(e)
                }
            },
            // Absence is the caller's normal fallback flow — no log. Every
            // other failure (corrupt trie, bad name) is logged with the
            // property name, same policy as the arms above.
            Err(e @ Error::NotFound(_)) => Err(e),
            Err(e) => {
                log::error!("Failed to find {name} in property area: {e}");
                Err(e)
            }
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
            // A name that maps to no context cannot have a property — the
            // same "genuine absence" contract as the in-area miss below.
            // (Unlike a flattened in-area lookup *failure*, sending `set`
            // down the `add` path here is harmless: `add` hits the same
            // context miss and fails loudly.) The lower layer already
            // logged the miss at the appropriate level. A corrupt-at-init
            // context slot is NOT folded in here: `context_node_at` reports
            // it as `FileValidation`, which propagates through the arm
            // below — corruption must stay distinguishable from absence.
            Err(Error::NotFound(_)) => return Ok(None),
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
            // Only genuine absence maps to `None`. Lookup *failures* (bad
            // name, corrupt mmap) must propagate — flattening them would
            // send `set` down the `add` path, which is a silent no-op for
            // existing names, turning a real error into a successful-looking
            // write that never happened.
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Set the value of a system property
    /// If the property is not found, it creates a new property.
    /// If the property value is too long, it returns an error.
    /// If the property is read-only, it returns an error.
    /// If the property is updated successfully, it returns Ok(()).
    #[cfg(feature = "builder")]
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        // No extra logging here: every failure path inside `update`/`add`
        // already logs with full context — a second line per failure only
        // duplicated the noise.
        match self.find(name)? {
            Some(prop_ref) => self.update(&prop_ref, value),
            None => self.add(name, value),
        }
    }

    #[cfg(feature = "builder")]
    pub fn update(&mut self, index: &PropertyIndex, value: &str) -> Result<()> {
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
        // we can take `&mut pa` for `backup_and_apply_write` immediately
        // after. The buffer outlives the inner borrow scope, so the bytes
        // it captured remain valid after `pi`/`cow` go out of scope.
        let mut backup_buf = [0u8; crate::wire::PROP_VALUE_MAX];
        let backup_len = {
            let name = pa
                .property_info_name(index.property_index)
                .map_err(|e| {
                    log::error!(
                        "Failed to read property name for index {}: {e}",
                        index.property_index
                    );
                    e
                })?
                .to_bytes();
            if name.starts_with(b"ro.") {
                let error_msg = format!(
                    "Try to update the read-only property: {}",
                    String::from_utf8_lossy(name)
                );
                log::error!("{error_msg}");
                return Err(Error::PermissionDenied(error_msg));
            }
            // Value-length check — `update` cannot promote to a long
            // property in-place (`apply_write` rejects on LONG_FLAG), so
            // use the short-value variant, which has no `ro.` exemption.
            // Deliberately *after* the `ro.` check above: for a read-only
            // property the dominant refusal reason is read-only-ness, and
            // reporting "value too long" instead would misdirect the
            // caller. Still before the backup snapshot, preserving "every
            // failure path occurs before set_dirty".
            crate::wire::validate_short_value_len(value).inspect_err(|e| log::error!("{e}"))?;
            // Pre-flight LONG check: if the entry was created long, we can't
            // overwrite it in-place. Checking *before* writing the backup
            // keeps backup_area aligned with the entry it shadows.
            let pi = pa.property_info(index.property_index).map_err(|e| {
                log::error!(
                    "Failed to get property info for index {}: {e}",
                    index.property_index
                );
                e
            })?;
            if pi.is_long() {
                // `InvalidArgument` like `apply_write`'s own LONG check —
                // an unsupported operation on this entry kind, not file
                // corruption.
                let error_msg = format!(
                    "in-place update of long property is not supported: {}",
                    String::from_utf8_lossy(name)
                );
                log::error!("{error_msg}");
                return Err(Error::InvalidArgument(error_msg));
            }
            // After the LONG check, `property_value_bytes` is guaranteed to
            // take the short path — a snapshot into `backup_buf`. Capturing
            // only the length lets us drop the borrow without losing the
            // bytes that already live in `backup_buf`. This NUL-scanned
            // length equals the length recorded in the entry's serial word
            // because values are validated NUL-free on every write path
            // (`wire::validate_value_len`) — dirty-path readers size their
            // backup copy from that serial length.
            pa.property_value_bytes(index.property_index, &mut backup_buf)?
                .len()
        };

        // Backup-then-publish as a single fused operation: readers that
        // observe the dirty serial read the backup slot, so the backup must
        // land before the dirty bit — `backup_and_apply_write` makes that
        // ordering structural (the entry writer is unreachable otherwise).
        pa.backup_and_apply_write(index.property_index, &backup_buf[..backup_len], value)
            .map_err(|e| {
                log::error!("Failed to update property value: {e}");
                e
            })?;

        // The value is fully published at this point — from here on NOTHING
        // may early-return. A failure would misreport a completed update
        // and, worse, skip the global serial bump below, leaving `wait_any`
        // observers permanently unaware of the change. That is why the
        // re-fetch for the futex wake is non-fatal too (it re-runs checks
        // the offset just passed inside `backup_and_apply_write`, so a
        // failure here is theoretical): a missed FUTEX_WAKE only delays
        // waiters — they re-check the serial on their own; bionic ignores
        // the wake result entirely. Log and continue.
        match pa.property_info(index.property_index) {
            Ok(pi) => {
                if let Err(e) = futex_wake(&pi.serial) {
                    log::warn!("Failed to wake property futex: {e}");
                }
            }
            Err(e) => log::warn!("Failed to re-fetch property info for futex wake: {e}"),
        }

        let serial_pa = self.contexts.serial_prop_area();
        // Atomic RMW: multiple service writers (or multi-process mmap sharing)
        // would otherwise lose updates with a load + store pair.
        serial_pa.serial().fetch_add(1, Ordering::Release);

        if let Err(e) = futex_wake(serial_pa.serial()) {
            log::warn!("Failed to wake global serial futex: {e}");
        }

        Ok(())
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
        crate::wire::validate_value_len(name, value).inspect_err(|e| log::error!("{e}"))?;

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

        // See the wake-failure note in `update`: the property is already
        // added and the serial bumped — report success.
        if let Err(e) = futex_wake(serial_pa.serial()) {
            log::warn!("Failed to wake global serial futex after adding property: {e}");
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
    ///
    /// bionic parity (`__system_property_serial`): if a writer is
    /// mid-update (dirty bit set), waits for the clean serial before
    /// returning — otherwise a caller comparing serials would observe one
    /// logical update as two transitions. Unlike bionic the wait is
    /// *bounded* to 200ms total (dirty windows are microseconds; the bound
    /// only triggers if a writer crashed mid-update, where bionic would
    /// hang): on expiry the dirty serial is returned as-is with a warning.
    /// The wait is sliced like [`Self::wait`] — the node's read lock is
    /// released between 20ms slices so a queued same-process builder
    /// writer (and, behind it on a writer-preferring `RwLock`, new
    /// readers) is never blocked for the whole bound. On macOS (no futex)
    /// the raw — possibly dirty — serial is returned immediately.
    pub fn serial(&self, idx: &PropertyIndex) -> Option<u32> {
        #[cfg(any(target_os = "android", target_os = "linux"))]
        {
            // A same-process builder writer cannot be mid-update while we
            // hold the read guard — it takes the node's write lock for the
            // whole update — so a dirty serial implies a *cross-process*
            // writer and each bounded slice below cannot deadlock.
            const DIRTY_SLICE: Duration = Duration::from_millis(20);
            const DIRTY_WAIT_TOTAL: Duration = Duration::from_millis(200);
            let start = Instant::now();
            loop {
                // (Re-)acquire the node lock for this slice only.
                let guard = self
                    .contexts
                    .prop_area_with_index(idx.context_index)
                    .inspect_err(|e| {
                        log::error!(
                            "Failed to get PropertyArea for index {}: {e}",
                            idx.context_index
                        )
                    })
                    .ok()?;
                let pi = guard
                    .property_area()
                    .property_info(idx.property_index)
                    .inspect_err(|e| {
                        log::error!(
                            "Failed to get PropertyInfo for index {}: {e}",
                            idx.property_index
                        )
                    })
                    .ok()?;
                let serial = pi.serial.load(Ordering::Acquire);
                if !serial_dirty(serial) {
                    return Some(serial);
                }
                // Clamp the final slice so the total bound is exact, and
                // check it BEFORE waiting so expiry never adds a slice.
                let remaining = DIRTY_WAIT_TOTAL.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    log::warn!(
                        "serial: entry still dirty after {DIRTY_WAIT_TOTAL:?} \
                         (writer crashed mid-update?); returning the dirty serial"
                    );
                    return Some(serial);
                }
                let slice = remaining.min(DIRTY_SLICE);
                // Derive both fields from the Duration (like `wait`) —
                // hardcoding `tv_sec: 0` would silently truncate whole
                // seconds if `DIRTY_SLICE` ever grew past 1s.
                let slice_ts = Timespec {
                    tv_sec: slice.as_secs() as _,
                    tv_nsec: slice.subsec_nanos() as _,
                };
                match futex_wait(&pi.serial, serial, Some(&slice_ts)) {
                    FutexWaitOutcome::Changed(s) if !serial_dirty(s) => return Some(s),
                    // Still dirty (writer burst) or slice expired: drop the
                    // guard at the end of this iteration and re-acquire.
                    FutexWaitOutcome::Changed(_) | FutexWaitOutcome::TimedOut => {}
                    FutexWaitOutcome::Failed => {
                        let current = pi.serial.load(Ordering::Acquire);
                        if serial_dirty(current) {
                            log::warn!("serial: futex wait failed; returning the dirty serial");
                        }
                        return Some(current);
                    }
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            let guard = self
                .contexts
                .prop_area_with_index(idx.context_index)
                .inspect_err(|e| {
                    log::error!(
                        "Failed to get PropertyArea for index {}: {e}",
                        idx.context_index
                    )
                })
                .ok()?;
            let pi = guard
                .property_area()
                .property_info(idx.property_index)
                .inspect_err(|e| {
                    log::error!(
                        "Failed to get PropertyInfo for index {}: {e}",
                        idx.property_index
                    )
                })
                .ok()?;
            Some(pi.serial.load(Ordering::Acquire))
        }
    }

    /// Waits for any property to change. Equivalent to
    /// `wait(None, None, None)`; see [`Self::wait`] for the race caveat of
    /// not passing an `old_serial`.
    pub fn wait_any(&self) -> Option<u32> {
        self.wait(None, None, None)
    }

    /// Waits until the property at `index` (or, with `index == None`, the
    /// global serial — i.e. any property) changes, returning the new serial.
    /// Returns `None` on timeout, lookup failure, **or a futex syscall
    /// failure** — the three are indistinguishable through this signature.
    /// Consequence: do not tight-poll this method with a zero/short timeout
    /// and treat every `None` as "timed out, retry" — a persistent futex
    /// error would turn that loop into a busy-spin. Without a timeout, a
    /// `None` always means an error.
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
    /// On macOS there is no futex, so this cannot *block*: the
    /// `old_serial` fast path still works (a serial that already differs
    /// returns `Some` immediately — that part is a plain atomic load),
    /// but once the serial has to be waited on, `None` is returned
    /// immediately. Do not call it in a polling loop there.
    ///
    /// # Same-process builder writers
    ///
    /// Per-property waits need the context node's read lock to reach the
    /// serial word inside the mmap, and a *same-process* builder writer
    /// (`set`/`update`) needs that node's write lock. To keep the two from
    /// deadlocking, the wait is **sliced**: the lock is released and
    /// re-acquired roughly every 100ms, with a serial re-check on each
    /// re-acquisition closing the missed-wakeup window. A same-process
    /// writer is therefore delayed by at most one slice instead of
    /// blocking forever; cross-process waiters (the normal Android
    /// arrangement, and this crate's service) never contend on the lock at
    /// all. Global-serial waits (`index == None`) take no lock and are not
    /// sliced.
    pub fn wait(
        &self,
        index: Option<&PropertyIndex>,
        old_serial: Option<u32>,
        timeout: Option<&Timespec>,
    ) -> Option<u32> {
        // No index → wait on the global serial (lock-free, no slicing).
        let Some(idx) = index else {
            let serial_pa = self.contexts.serial_prop_area().serial();
            // Documented already-changed fast path, checked BEFORE the
            // futex: on Linux it merely pre-empts the syscall's EAGAIN,
            // but on macOS (no futex — `futex_wait` fails immediately)
            // it is the only thing keeping the `old_serial` contract.
            let current = serial_pa.load(Ordering::Acquire);
            let old = match old_serial {
                Some(old) if old != current => return Some(current),
                Some(old) => old,
                None => current,
            };
            return match futex_wait(serial_pa, old, timeout) {
                FutexWaitOutcome::Changed(s) => Some(s),
                FutexWaitOutcome::TimedOut | FutexWaitOutcome::Failed => None,
            };
        };

        #[cfg(any(target_os = "android", target_os = "linux"))]
        {
            /// Upper bound on how long one slice may hold the node's read
            /// lock — i.e. the worst-case delay imposed on a same-process
            /// builder writer.
            const LOCK_SLICE: Duration = Duration::from_millis(100);

            // Convert the caller timeout to a deadline once, mirroring
            // `futex_wait`'s own validation: negative/invalid → immediate
            // timeout; unrepresentably-huge → wait forever (like bionic).
            let deadline = match timeout {
                None => None,
                Some(t) if t.tv_sec < 0 || t.tv_nsec < 0 || t.tv_nsec >= 1_000_000_000 => {
                    return None;
                }
                Some(t) => {
                    Instant::now().checked_add(Duration::new(t.tv_sec as u64, t.tv_nsec as u32))
                }
            };

            let mut old = old_serial;
            loop {
                // Compute the slice BEFORE (re-)acquiring the lock: an
                // already-expired deadline must return without another
                // acquisition, and blocking on a writer-held lock right
                // after expiry would overshoot the caller's timeout.
                let slice = match deadline {
                    None => LOCK_SLICE,
                    Some(d) => {
                        let remaining = d.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return None;
                        }
                        remaining.min(LOCK_SLICE)
                    }
                };
                // (Re-)acquire the node lock for this slice only.
                let guard = self
                    .contexts
                    .prop_area_with_index(idx.context_index)
                    .inspect_err(|e| {
                        log::error!(
                            "Failed to get PropertyArea for index {}: {e}",
                            idx.context_index
                        )
                    })
                    .ok()?;
                let pi = guard
                    .property_area()
                    .property_info(idx.property_index)
                    .inspect_err(|e| {
                        log::error!(
                            "Failed to get PropertyInfo for index {}: {e}",
                            idx.property_index
                        )
                    })
                    .ok()?;
                let old_val = *old.get_or_insert_with(|| pi.serial.load(Ordering::Acquire));
                // The serial may have changed while the lock was released
                // between slices — the futex wake fired with no waiter, so
                // this re-check is what closes that window.
                let current = pi.serial.load(Ordering::Acquire);
                if current != old_val {
                    return Some(current);
                }
                let slice_ts = Timespec {
                    tv_sec: slice.as_secs() as _,
                    tv_nsec: slice.subsec_nanos() as _,
                };
                match futex_wait(&pi.serial, old_val, Some(&slice_ts)) {
                    FutexWaitOutcome::Changed(s) => return Some(s),
                    // Slice expired: fall through, dropping `guard` at the
                    // end of the iteration so writers get a window.
                    FutexWaitOutcome::TimedOut => {}
                    FutexWaitOutcome::Failed => return None,
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            // No futex, so blocking is impossible — but the documented
            // `old_serial` fast path is a plain load and must still hold:
            // a serial that already moved past `old_serial` returns
            // immediately instead of being misreported as a failure.
            let _ = timeout;
            let current = self.serial(idx)?;
            if old_serial.is_some_and(|old| old != current) {
                return Some(current);
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    // Everything in this module is android-only; scope the imports the same
    // way instead of blanket-allowing unused_imports.
    #[cfg(target_os = "android")]
    use super::*;

    #[cfg(target_os = "android")]
    use android_system_properties::AndroidSystemProperties;

    #[cfg(target_os = "android")]
    const VERSION_PROPERTY: &str = "ro.build.version.release";

    #[cfg(target_os = "android")]
    #[test]
    fn test_system_properties() -> Result<()> {
        let system_properties = SystemProperties::new(Path::new(crate::PROP_DIRNAME))?;

        let handle = std::thread::spawn(move || {
            let version1 = system_properties
                .get_with_result(VERSION_PROPERTY)
                .unwrap_or_default();
            let version2 = AndroidSystemProperties::new()
                .get(VERSION_PROPERTY)
                .unwrap_or_default();
            assert_eq!(version1, version2);
        });

        handle.join().unwrap();

        Ok(())
    }
}
