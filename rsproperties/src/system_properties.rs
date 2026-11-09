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
// Every serial load below can run on a `PROT_READ` mapping: Acquire goes
// through `load_acquire` (`Relaxed` load + `fence(Acquire)`),
// property_area.rs [R-RO].
use crate::property_area::{load_acquire, EntryOffset};

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
/// Callers compare the serial before calling: once the deadline has passed
/// (or for an invalid timeout) this returns `TimedOut` without the
/// syscall's value comparison.
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
                    let new_serial = load_acquire(_serial);
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
                    let new_serial = load_acquire(_serial);
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

/// Source of [`SystemProperties`] instance ids. Starts at 1 so a zeroed
/// value never matches. A `Mutex`, not `AtomicU64`: Linux targets without
/// 64-bit atomics (armv5te, powerpc) have no `AtomicU64`, and a 32-bit
/// counter would let an index from an instance 2^32 creations earlier pass.
/// Locked once per instance creation.
static NEXT_INSTANCE: std::sync::Mutex<u64> = std::sync::Mutex::new(1);

/// Handle to one property, returned by [`SystemProperties::find`].
///
/// An index is valid only on the instance whose `find` returned it — the
/// global reader and a writer on the same directory are different
/// instances, and so are two instances created one after the other on the
/// same directory. [`SystemProperties::update`], [`SystemProperties::serial`]
/// and [`SystemProperties::wait`] reject an index from another instance.
// To avoid lifetime issues, the property index is used to access the property value.
#[derive(Clone, Copy, Debug)]
pub struct PropertyIndex {
    pub(crate) context_index: u32,
    /// Entry offset from `find` on context `context_index`'s area
    /// (property_area.rs \[R-OFF\]).
    pub(crate) property_index: EntryOffset,
    /// Id of the `SystemProperties` whose `find` returned this index.
    pub(crate) instance: u64,
}

/// System properties
/// It can't be created directly. Use `system_properties()` or `system_properties_area()` instead.
pub struct SystemProperties {
    contexts: ContextsSerialized,
    /// Unique per instance; recorded in every `PropertyIndex` it returns.
    instance: u64,
}

impl SystemProperties {
    fn with_contexts(contexts: ContextsSerialized) -> Self {
        Self {
            contexts,
            instance: {
                // Nothing panics while the lock is held; a poisoned lock
                // still holds a valid counter.
                let mut next = NEXT_INSTANCE.lock().unwrap_or_else(|e| e.into_inner());
                let id = *next;
                *next = id.wrapping_add(1);
                id
            },
        }
    }

    // Create a new system properties to read system properties from a file or a directory.
    pub(crate) fn new(filename: &Path) -> Result<Self> {
        let contexts = match ContextsSerialized::new_ro(filename, false) {
            Ok(contexts) => contexts,
            Err(e) => {
                log::error!("Failed to load contexts from {filename:?}: {e}");
                return Err(e);
            }
        };

        Ok(Self::with_contexts(contexts))
    }

    /// Creates the property areas of `dirname` for writing (the property
    /// service side). Takes the directory's
    /// [`AreaWriterLock`](crate::AreaWriterLock), reads
    /// `<dirname>/property_info`, and creates a fresh, empty area per
    /// context plus `properties_serial`. If `property_info` does not exist,
    /// this fails with the loader's error before `dirname` or its lock file
    /// is created.
    ///
    /// `new_area` does not write `property_info`. If you write it yourself
    /// and then call `new_area`, another writer can replace it between your
    /// write and `new_area`'s lock: take the lock first
    /// (`AreaWriterLock::acquire`, `publish_property_info`,
    /// [`Self::new_area_locked`]).
    #[cfg(feature = "builder")]
    pub fn new_area(dirname: &Path) -> Result<Self> {
        let contexts = match ContextsSerialized::new_rw(dirname, None) {
            Ok(contexts) => contexts,
            Err(e) => {
                log::error!("Failed to create area from {dirname:?}: {e}");
                return Err(e);
            }
        };

        Ok(Self::with_contexts(contexts))
    }

    /// Like [`Self::new_area`] on `lock.dir()`, under a lock the caller
    /// already holds (typically after
    /// [`AreaWriterLock::publish_property_info`](crate::AreaWriterLock::publish_property_info)).
    /// The instance keeps the lock until it is dropped.
    #[cfg(feature = "builder")]
    pub fn new_area_locked(lock: crate::AreaWriterLock) -> Result<Self> {
        let dir = lock.dir().to_path_buf();
        let contexts = match ContextsSerialized::new_rw(&dir, Some(lock)) {
            Ok(contexts) => contexts,
            Err(e) => {
                log::error!("Failed to create area from {dir:?}: {e}");
                return Err(e);
            }
        };

        Ok(Self::with_contexts(contexts))
    }

    /// Whether `index` was returned by this instance's `find`; logs when
    /// not.
    fn owns_index(&self, index: &PropertyIndex) -> bool {
        if index.instance == self.instance {
            return true;
        }
        log::error!(
            "PropertyIndex was returned by a different SystemProperties instance \
             (index from instance {}, used on instance {})",
            index.instance,
            self.instance
        );
        false
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
        pi_offset: EntryOffset,
        name: &str,
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
        // LONG counts only on `ro.` names (bionic `ReadCallback`,
        // property_area.rs [R-UNION]): on a mutable property bit 16 is the
        // update counter, which init sets on the 32768th update. `name` is
        // the name `find` matched, which is the entry's name unless the
        // file is corrupt; `long_property_value` re-checks the entry's own
        // name before it loads the offset.
        let long_bytes: Option<&[u8]> = if prop_info.is_long_for(name.as_bytes()) {
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
            let serial = load_acquire(&prop_info.serial);

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
    /// The callback also runs while this instance is borrowed and while the
    /// context node's read lock is held, so it should not block: a
    /// `set`/`update` on this instance needs `&mut self` and cannot run
    /// until this call returns, and re-entering this crate's read API from
    /// the callback for a property of the same context takes the same
    /// `std::sync::RwLock` read lock a second time on this thread, which
    /// `RwLock::read` documents as possibly panicking (see [`Self::wait`]
    /// # Locking).
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
                log::error!("Failed to find property area for {name:?}: {e}");
                return Err(e);
            }
        };
        let pa = res.0.property_area();

        match pa.find(name) {
            Ok((_, pi_offset)) => match self.read_with_callback(pa, pi_offset, name, f) {
                Ok(r) => Ok(r),
                Err(e) => {
                    log::error!("Failed to read property {name:?}: {e}");
                    Err(e)
                }
            },
            // Absence is the caller's normal fallback flow — no log. Every
            // other failure (corrupt trie, bad name) is logged with the
            // property name, same policy as the arms above.
            Err(e @ Error::NotFound(_)) => Err(e),
            Err(e) => {
                log::error!("Failed to find {name:?} in property area: {e}");
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
    /// If the property is not found, it returns Ok(None); so does a name
    /// with an empty segment (`""`, `"a."`, `".a"`, `"a..b"`), like bionic
    /// `__system_property_find`.
    ///
    /// An index is valid only on the instance whose `find` returned it —
    /// the global reader and a writer on the same directory are different
    /// instances.
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
                log::error!("Failed to find property area for {name:?}: {e}");
                return Err(e);
            }
        };
        let pa = res.0.property_area();
        match pa.find(name) {
            Ok(pi) => {
                let index = PropertyIndex {
                    context_index: res.1,
                    property_index: pi.1,
                    instance: self.instance,
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

    /// Updates the property at `index` to `value`.
    ///
    /// `index` must come from this instance's [`Self::find`]: an index is
    /// valid only on the instance whose `find` returned it — the global
    /// reader and a writer on the same directory are different instances.
    /// An index from another instance fails with `Error::InvalidArgument`
    /// before any area is touched.
    #[cfg(feature = "builder")]
    pub fn update(&mut self, index: &PropertyIndex, value: &str) -> Result<()> {
        // Before any lock or mmap access: another instance's offsets
        // address an unrelated layout (property_area.rs [SM5]).
        if !self.owns_index(index) {
            return Err(Error::InvalidArgument(
                "PropertyIndex was returned by a different SystemProperties instance".into(),
            ));
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
                    "Try to update the read-only property: {:?}",
                    String::from_utf8_lossy(name)
                );
                log::error!("{error_msg}");
                return Err(Error::PermissionDenied(error_msg));
            }
            // Value-length check — `update` writes the short slot only:
            // only `ro.` entries are long and they were rejected above (and
            // again by `backup_and_apply_write`, property_area.rs
            // [R-UNION]), so use the short-value variant, which has no
            // `ro.` exemption.
            // Deliberately *after* the `ro.` check above: for a read-only
            // property the dominant refusal reason is read-only-ness, and
            // reporting "value too long" instead would misdirect the
            // caller. Still before the backup snapshot, preserving "every
            // failure path occurs before set_dirty".
            crate::wire::validate_short_value_len(value).inspect_err(|e| log::error!("{e}"))?;
            // No LONG pre-flight: only `ro.` entries are long
            // (property_area.rs [R-UNION]) and they were rejected above, so
            // this entry's value is the short slot even when bit 16 of its
            // serial is set (bionic's update counter reaches that bit).
            // `property_value_bytes` therefore takes the short path — a
            // snapshot into `backup_buf`. Capturing only the length lets us
            // drop the borrow without losing the bytes that already live in
            // `backup_buf`. This NUL-scanned length equals the length
            // recorded in the entry's serial word because values are
            // validated NUL-free on every write path
            // (`wire::validate_value_len` / `validate_short_value_len`) —
            // dirty-path readers size their backup copy from that serial
            // length.
            pa.property_value_bytes(index.property_index, name, &mut backup_buf)?
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
        // RMW: never on a PROT_READ mapping (property_area.rs [R-WRITE]).
        debug_assert!(self.contexts.serial_area_writable());
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
                log::error!("Failed to get mutable property area for {name:?}: {e}");
                return Err(e);
            }
        };
        let pa = res.0.property_area_mut();

        match pa.add(name, value) {
            Ok(_) => {}
            Err(e) => {
                log::error!("Failed to add property {name:?} to area: {e}");
                return Err(e);
            }
        }

        let serial_pa = self.contexts.serial_prop_area();
        // Atomic RMW: see note in `update`.
        debug_assert!(self.contexts.serial_area_writable());
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
        load_acquire(serial_pa.serial())
    }

    /// Reads the per-property serial counter, or `None` if the context/property
    /// lookup fails. `0` is a valid initial serial, so callers cannot use a
    /// numeric sentinel — use the `Option` to distinguish absence.
    ///
    /// If the entry is mid-update (dirty bit set), waits for the clean
    /// serial before returning, so a caller comparing serials sees one
    /// update as one change. bionic `__system_property_serial` returns the
    /// raw Acquire load instead (its comment notes that an earlier version
    /// was heavier). The wait is bounded to 200ms total: dirty windows last
    /// microseconds, and the bound only triggers if a writer stalls or
    /// crashed mid-update; on expiry the dirty serial is returned with a
    /// warning. The wait is split into 20ms slices and the node's read lock
    /// is released between them (see [`Self::wait`] # Locking). If the
    /// futex wait itself fails (an errno other than `EAGAIN`, `EINTR` or
    /// `ETIMEDOUT`, e.g. a seccomp filter denying `futex`), the serial is
    /// re-read and returned at once, without waiting out the bound — dirty
    /// or not, with a warning if dirty. On macOS (no futex) the raw —
    /// possibly dirty — serial is returned immediately. These three cases
    /// are the only ones that return a dirty serial.
    ///
    /// An index is valid only on the instance whose `find` returned it —
    /// the global reader and a writer on the same directory are different
    /// instances. An index from another instance returns `None` (with an
    /// `error` log line).
    pub fn serial(&self, idx: &PropertyIndex) -> Option<u32> {
        if !self.owns_index(idx) {
            return None;
        }
        #[cfg(any(target_os = "android", target_os = "linux"))]
        {
            // A dirty serial comes from a writer on another instance —
            // another process, or another `SystemProperties` in this
            // process: this instance's own `update` needs `&mut self`, and
            // other instances do not take this node's lock. Waiting under
            // the read guard therefore never blocks that writer.
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
                let serial = load_acquire(&pi.serial);
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
                        let current = load_acquire(&pi.serial);
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
            Some(load_acquire(&pi.serial))
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
    /// With an index, the returned serial can have the dirty bit (bit 0)
    /// set: a writer on another instance stores the dirty serial, writes
    /// the value, then stores the clean one, and this call returns
    /// whichever differs from `old_serial` first, as bionic
    /// `__system_property_wait` does. Passing a dirty serial as the next
    /// `old_serial` reports that update's clean store as a second change.
    /// [`Self::serial`] waits (bounded) for the clean serial instead.
    ///
    /// The `old_serial` comparison happens before the timeout is applied,
    /// so with a zero timeout this call is a non-blocking check. bionic
    /// gets the same result from the kernel: `FUTEX_WAIT` returns `EAGAIN`
    /// for a changed word with a zero timeout and `EINVAL` for an
    /// out-of-range one, and bionic reloads the serial after both. A
    /// negative or out-of-range timeout (`tv_nsec` ≥ 10^9) is compared
    /// once and then returns `None` without waiting; bionic retries the
    /// `EINVAL` until the serial changes. With an index the comparison
    /// needs the context node's read lock, which even a zero-timeout call
    /// can wait for: only when another thread of this instance started
    /// opening that area before it was open, and then until the call that
    /// holds the node's read lock releases it (see # Locking). `None`
    /// still does not distinguish an unchanged serial from a lookup or
    /// futex failure (see above).
    ///
    /// On macOS there is no futex, so this cannot *block*: the
    /// `old_serial` fast path still works (a serial that already differs
    /// returns `Some` immediately — that part is a plain atomic load),
    /// but once the serial has to be waited on, `None` is returned
    /// immediately. Do not call it in a polling loop there.
    ///
    /// An index is valid only on the instance whose `find` returned it —
    /// the global reader and a writer on the same directory are different
    /// instances. An index from another instance returns `None`
    /// immediately (with an `error` log line).
    ///
    /// # Locking
    ///
    /// A per-property wait holds the context node's read lock to reach the
    /// serial word in the mapping. The lock belongs to this instance: a
    /// writer on another instance (another process, or a `new_area`
    /// instance in this process) does not take it, and this instance's own
    /// `set`/`update` need `&mut self`, so they cannot run during a `wait`.
    /// The wait is still sliced — the lock is released and re-taken about
    /// every 100ms, and the serial is compared again each time. A thread of
    /// this instance that started opening the same context area before it
    /// was open waits for the node's write lock, and while that request is
    /// pending, new readers of the node wait too; a slice bounds the part
    /// of that delay a `wait` causes (a [`Self::read_with`] callback holds
    /// the read lock for as long as it runs). The comparison on each
    /// re-take also observes a change whose writer did not wake the futex.
    /// Global-serial waits (`index == None`) take no lock and are not
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
            let current = load_acquire(serial_pa);
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
        // Covers both platform branches below.
        if !self.owns_index(idx) {
            return None;
        }

        #[cfg(any(target_os = "android", target_os = "linux"))]
        {
            /// Upper bound on how long one slice holds the node's read
            /// lock: the part of a first-time opener's delay (another
            /// thread of this instance, see # Locking) that a `wait` can
            /// cause.
            const LOCK_SLICE: Duration = Duration::from_millis(100);

            /// What the caller's timeout allows after the first comparison.
            enum Budget {
                Forever,
                Until(Instant),
                /// Compare once, then return `None`.
                CheckOnly,
            }

            // Convert the caller timeout once, with `futex_wait`'s
            // validation: negative/invalid → compare once (bionic's futex
            // returns EINVAL for it and bionic retries until the serial
            // changes, see the rustdoc); unrepresentably huge → wait
            // forever (like bionic, which passes it to the kernel).
            let budget = match timeout {
                None => Budget::Forever,
                Some(t) if t.tv_sec < 0 || t.tv_nsec < 0 || t.tv_nsec >= 1_000_000_000 => {
                    Budget::CheckOnly
                }
                Some(t) => match Instant::now()
                    .checked_add(Duration::new(t.tv_sec as u64, t.tv_nsec as u32))
                {
                    Some(d) => Budget::Until(d),
                    None => Budget::Forever,
                },
            };

            let mut old = old_serial;
            let mut first = true;
            loop {
                // The slice is computed BEFORE the lock is (re-)taken. The
                // first iteration always takes the lock and compares
                // `old_serial` before the timeout applies, like bionic,
                // whose FUTEX_WAIT reports a changed word for a zero
                // (EAGAIN) or out-of-range (EINVAL) timeout and is followed
                // by a reload. That lock wait is bounded only as the
                // rustdoc says (another thread's first open of this area).
                // Later iterations return without the lock once the
                // deadline has passed: the previous comparison was the last
                // one, as after bionic's ETIMEDOUT, and blocking on the
                // lock after expiry would overshoot the caller's timeout.
                let slice = match budget {
                    Budget::Forever => Some(LOCK_SLICE),
                    Budget::CheckOnly => None,
                    Budget::Until(d) => match d.saturating_duration_since(Instant::now()) {
                        r if r.is_zero() && !first => return None,
                        r if r.is_zero() => None,
                        r => Some(r.min(LOCK_SLICE)),
                    },
                };
                first = false;
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
                let old_val = *old.get_or_insert_with(|| load_acquire(&pi.serial));
                // The serial may have changed while the lock was released
                // between slices — the futex wake fired with no waiter, so
                // this re-check is what closes that window.
                let current = load_acquire(&pi.serial);
                if current != old_val {
                    return Some(current);
                }
                // Expired or invalid timeout: the comparison above was the
                // only one.
                let slice = slice?;
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

/// A builder writer over a fresh properties dir with `debug.` and `ro.`
/// contexts.
#[cfg(all(test, feature = "builder", not(target_os = "android")))]
mod test_fixture {
    use super::*;
    use crate::property_info::LONG_FLAG;
    use crate::{build_trie, AreaWriterLock, PropertyInfoEntry};
    use std::path::PathBuf;

    /// Removes the dir on drop. Made before anything is created in it, so
    /// a panic while the fixture is being built still removes it.
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(super) struct Fixture {
        pub(super) sp: SystemProperties,
        // Declared after `sp`, so it is dropped after the writer.
        dir: TempDir,
    }

    impl Fixture {
        /// `tag` must differ between the tests of this binary: they run in
        /// parallel in one process and share the pid in the dir name.
        pub(super) fn new(tag: &str) -> Self {
            let guard = TempDir(
                std::env::temp_dir().join(format!("rsprops_sp_{tag}_{}", std::process::id())),
            );
            let dir = &guard.0;
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir_all(dir).unwrap();
            let contexts = dir.join("property_contexts");
            std::fs::write(
                &contexts,
                "debug. u:object_r:debug_prop:s0 prefix string\n\
                 ro. u:object_r:ro_prop:s0 prefix string\n",
            )
            .unwrap();
            let (entries, errors) = PropertyInfoEntry::parse_from_file(&contexts, false).unwrap();
            assert!(errors.is_empty(), "{errors:?}");
            let trie = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();
            let lock = AreaWriterLock::acquire(dir).unwrap();
            lock.publish_property_info(&trie).unwrap();
            let sp = SystemProperties::new_area_locked(lock).unwrap();
            Self { sp, dir: guard }
        }

        pub(super) fn dir(&self) -> &Path {
            &self.dir.0
        }

        /// What init's counter does on the 32768th update.
        pub(super) fn set_bit16(&self, name: &str) -> PropertyIndex {
            let idx = self.sp.find(name).unwrap().expect("property exists");
            let guard = self
                .sp
                .contexts
                .prop_area_with_index(idx.context_index)
                .unwrap();
            let pi = guard
                .property_area()
                .property_info(idx.property_index)
                .unwrap();
            pi.serial.fetch_or(LONG_FLAG, Ordering::SeqCst);
            idx
        }

        pub(super) fn serial_word(&self, idx: &PropertyIndex) -> u32 {
            let guard = self
                .sp
                .contexts
                .prop_area_with_index(idx.context_index)
                .unwrap();
            let pi = guard
                .property_area()
                .property_info(idx.property_index)
                .unwrap();
            pi.serial.load(Ordering::SeqCst)
        }
    }
}

/// LONG flag on mutable properties (property_area.rs [R-UNION]). init's
/// update counter sets bit 16 of a mutable property's serial on its
/// 32768th update; these tests set the bit directly instead of running
/// 32768 updates.
#[cfg(all(test, feature = "builder", not(target_os = "android")))]
mod long_flag_tests {
    use super::test_fixture::Fixture;
    use super::*;
    use crate::property_info::LONG_FLAG;

    #[test]
    fn non_ro_entry_with_bit16_reads_as_short_value() {
        let mut fx = Fixture::new("read");
        fx.sp.add("debug.rsp.bit16", "v32767").unwrap();
        fx.set_bit16("debug.rsp.bit16");

        assert_eq!(fx.sp.get_with_result("debug.rsp.bit16").unwrap(), "v32767");
        // A read-only instance, as the global reader opens it.
        let reader = SystemProperties::new(fx.dir()).unwrap();
        assert_eq!(reader.get_with_result("debug.rsp.bit16").unwrap(), "v32767");
    }

    #[test]
    fn non_ro_entry_with_bit16_updates_in_place() {
        let mut fx = Fixture::new("update");
        fx.sp.add("debug.rsp.bit16", "v32767").unwrap();
        let idx = fx.set_bit16("debug.rsp.bit16");

        fx.sp.update(&idx, "v32768").unwrap();
        assert_eq!(fx.sp.get_with_result("debug.rsp.bit16").unwrap(), "v32768");
        assert_eq!(
            fx.serial_word(&idx) & LONG_FLAG,
            0,
            "this crate's counter keeps bit 16 clear"
        );
    }

    /// 32768 same-length updates move the serial past bit 16 instead of
    /// back to the starting value, so a waiter holding the old serial
    /// still sees a change.
    #[test]
    fn counter_carries_past_bit16() {
        let mut fx = Fixture::new("carry");
        fx.sp.add("debug.rsp.carry", "a").unwrap();
        let idx = fx.sp.find("debug.rsp.carry").unwrap().unwrap();
        let start = fx.serial_word(&idx);
        for i in 0..32768u32 {
            let v = if i % 2 == 0 { "b" } else { "a" };
            fx.sp.update(&idx, v).unwrap();
        }
        let end = fx.serial_word(&idx);
        assert_ne!(end, start, "serial repeated after 32768 updates");
        assert_eq!(end & LONG_FLAG, 0);
    }

    /// The `ro.` refusal of `update` (also reached by `set` on an existing
    /// `ro.` name) prints the name `Debug`-escaped, so a name carrying a
    /// newline cannot forge a log line.
    #[test]
    fn ro_update_refusal_escapes_name() {
        let mut fx = Fixture::new("ro_escape");
        let name = "ro.rsp.x\nFAKE: forged";
        fx.sp.add(name, "1").unwrap();
        match fx.sp.set(name, "2") {
            Err(Error::PermissionDenied(msg)) => {
                assert!(!msg.contains('\n'), "raw newline in {msg:?}");
                assert!(msg.contains(r#""ro.rsp.x\nFAKE: forged""#), "{msg:?}");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    #[test]
    fn ro_long_value_still_reads_out_of_line() {
        let mut fx = Fixture::new("ro_long");
        let long = "x".repeat(200);
        fx.sp.add("ro.rsp.long", &long).unwrap();
        assert_eq!(fx.sp.get_with_result("ro.rsp.long").unwrap(), long);
        let reader = SystemProperties::new(fx.dir()).unwrap();
        assert_eq!(reader.get_with_result("ro.rsp.long").unwrap(), long);
    }
}

/// `wait` and the futex wake of `update`, with the writer and the reader on
/// different instances (the service arrangement).
#[cfg(all(test, feature = "builder", target_os = "linux"))]
mod wait_tests {
    use super::test_fixture::Fixture;
    use super::*;

    fn ts(tv_sec: i64, tv_nsec: i64) -> Timespec {
        Timespec {
            tv_sec: tv_sec as _,
            tv_nsec: tv_nsec as _,
        }
    }

    /// With an index, a zero, negative or out-of-range timeout still
    /// compares `old_serial` first, like the global-serial path and bionic.
    #[test]
    fn index_wait_compares_before_timeout() {
        let mut fx = Fixture::new("wait_zero");
        fx.sp.add("debug.rsp.zero", "0").unwrap();
        let reader = SystemProperties::new(fx.dir()).unwrap();
        let idx = reader.find("debug.rsp.zero").unwrap().unwrap();
        let old = reader.serial(&idx).unwrap();
        fx.sp.set("debug.rsp.zero", "1").unwrap();

        let timeouts = [ts(0, 0), ts(-1, 0), ts(0, 1_000_000_000)];
        for t in &timeouts {
            let got = reader.wait(Some(&idx), Some(old), Some(t));
            assert!(
                got.is_some_and(|s| s != old),
                "{:?}: {got:?}",
                (t.tv_sec, t.tv_nsec)
            );
        }
        let cur = reader.serial(&idx).unwrap();
        for t in &timeouts {
            assert_eq!(reader.wait(Some(&idx), Some(cur), Some(t)), None);
        }
        // Regression only (passes before the fix): the global path gives
        // the same answer for the same inputs.
        let global = reader.context_serial();
        fx.sp.set("debug.rsp.zero", "2").unwrap();
        assert!(reader.wait(None, Some(global), Some(&ts(0, 0))).is_some());
    }

    /// `update` wakes waiters on the entry's own serial word. The waiter
    /// makes one raw `FUTEX_WAIT` (no slicing, no re-check), so only that
    /// wake ends it before its 5 s timeout. A waiter that had not entered
    /// the kernel when `update` ran gets `EAGAIN`, which decides nothing:
    /// the trial is repeated with the new serial and a longer head start,
    /// and the test fails if every trial is undecided.
    #[test]
    fn update_wakes_property_futex() {
        use rustix::io::Errno;
        const TRIALS: u32 = 5;
        let mut fx = Fixture::new("wake");
        fx.sp.add("debug.rsp.wake", "0").unwrap();
        let widx = fx.sp.find("debug.rsp.wake").unwrap().unwrap();
        // Another instance, as the global reader is: it maps the file
        // separately and takes none of the writer's locks.
        let reader = SystemProperties::new(fx.dir()).unwrap();
        let idx = reader.find("debug.rsp.wake").unwrap().unwrap();
        let guard = reader
            .contexts
            .prop_area_with_index(idx.context_index)
            .unwrap();
        let pi = guard
            .property_area()
            .property_info(idx.property_index)
            .unwrap();
        let timeout = ts(5, 0);
        for trial in 0..TRIALS {
            // `reader`'s mapping is PROT_READ: `Relaxed` only [R-RO].
            let old = pi.serial.load(Ordering::Relaxed);
            let outcome = std::thread::scope(|s| {
                let waiter =
                    s.spawn(|| futex::wait(&pi.serial, futex::Flags::empty(), old, Some(&timeout)));
                std::thread::sleep(Duration::from_millis(100 << trial));
                fx.sp.update(&widx, &format!("v{trial}")).expect("update");
                waiter.join().expect("waiter thread")
            });
            match outcome {
                Ok(()) => {
                    assert_ne!(pi.serial.load(Ordering::Relaxed), old);
                    return;
                }
                // Undecided: the waiter was not in the kernel yet.
                Err(Errno::AGAIN) | Err(Errno::INTR) => continue,
                Err(e) => panic!(
                    "trial {trial}: FUTEX_WAIT ended with {e:?} instead of a wake \
                     (TIMEDOUT: `update` did not wake the entry's serial word)"
                ),
            }
        }
        panic!("all {TRIALS} trials undecided: the waiter never entered FUTEX_WAIT in time");
    }
}
