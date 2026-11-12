// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use rustix::fs::Timespec;
#[cfg(any(target_os = "android", target_os = "linux"))]
use rustix::thread::futex;

use crate::errors::*;

use crate::context_node::PropertyAreaGuard;
use crate::contexts_serialized::ContextsSerialized;
// Every serial load below can run on a `PROT_READ` mapping: Acquire goes
// through `load_acquire` (`Relaxed` load + `fence(Acquire)`),
// property_area.rs [R-RO].
use crate::property_area::{load_acquire, EntryOffset};

pub(crate) use crate::wire::PROP_VALUE_MAX;
pub(crate) const PROP_TREE_FILE: &str = "/dev/__properties__/property_info";

/// Name prefix under which bionic looks up an appcompat override first
/// (`system_properties.cpp` `APPCOMPAT_PREFIX`).
const APPCOMPAT_PREFIX: &str = "ro.appcompat_override.";
/// Environment variable whose presence (any value) turns the override
/// lookup on (`APPCOMPAT_OVERRIDE_ENV_VAR`). Zygote sets it with `putenv`
/// in `__system_properties_zygote_reload`; child processes inherit it.
const APPCOMPAT_OVERRIDE_ENV_VAR: &str = "BIONIC_APPCOMPAT_OVERRIDE";

/// First API level whose bionic reads [`APPCOMPAT_OVERRIDE_ENV_VAR`]
/// (bionic 3bcd5082d, first in android-17.0.0_r1). API 35–36 bionic
/// neither reads the variable nor looks up the prefixed name: the writer
/// mirrors every property into `/dev/__properties__/appcompat_override`,
/// with the overrides stored under their plain names, and zygote bind
/// mounts that directory over `/dev/__properties__` in the app's mount
/// namespace. A reader created there opens the mirrored files, so this
/// crate needs no code for it. Honoring the variable on those levels
/// would make this crate read overrides that libc in the same process
/// does not (measured on an API 36 emulator:
/// `BIONIC_APPCOMPAT_OVERRIDE=1 getprop` returns the plain value; on an
/// API 37 emulator it returns the override).
#[cfg(target_os = "android")]
const FIRST_SDK_WITH_OVERRIDE_ENV: u32 = 37;

/// Calls `f` with `APPCOMPAT_PREFIX` + `name`, built in a stack buffer when
/// it fits, so an override probe on the read hot path does not allocate
/// for ordinary name lengths.
fn with_override_name<R>(name: &str, f: impl FnOnce(&str) -> R) -> R {
    const STACK_LEN: usize = 128;
    let prefix = APPCOMPAT_PREFIX.as_bytes();
    let total = prefix.len() + name.len();
    if total > STACK_LEN {
        return f(&format!("{APPCOMPAT_PREFIX}{name}"));
    }
    let mut buf = [0u8; STACK_LEN];
    buf[..prefix.len()].copy_from_slice(prefix);
    buf[prefix.len()..total].copy_from_slice(name.as_bytes());
    let joined =
        std::str::from_utf8(&buf[..total]).expect("two concatenated `&str`s are valid UTF-8");
    f(joined)
}

/// Absolute end of a caller-supplied `Option<Duration>` timeout. `None`
/// waits forever — also for a timeout too large for `Instant`.
struct Deadline(Option<Instant>);

impl Deadline {
    fn after(timeout: Option<Duration>) -> Self {
        Self(timeout.and_then(|t| Instant::now().checked_add(t)))
    }

    /// Time left (zero once expired), or `None` for no deadline.
    fn left(&self) -> Option<Duration> {
        self.0.map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// [`Self::left`] as the relative `Timespec` the futex takes.
    fn remaining(&self) -> Option<Timespec> {
        self.left().map(|r| Timespec {
            tv_sec: r.as_secs() as _,
            tv_nsec: r.subsec_nanos() as _,
        })
    }
}

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

/// Waits until `_serial` differs from `_value` — `Ok(Some(new))` — or the
/// timeout elapses — `Ok(None)`. A futex error other than the retried ones
/// is `Err`; the callers' wait loops stop on it instead of retrying, which
/// would turn a persistent futex error into a busy loop.
///
/// Callers compare the serial before calling: once the deadline has passed
/// this returns `Ok(None)` without the syscall's value comparison.
/// `_timeout` comes from [`Deadline::remaining`], so it is never negative.
///
/// On macOS there is no futex; fails immediately — see
/// `SystemProperties::wait`.
fn futex_wait(
    _serial: &AtomicU32,
    _value: u32,
    _timeout: Option<&Timespec>,
) -> Result<Option<u32>> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        use rustix::io::Errno;
        // Linux futex_wait takes a *relative* timeout. Spurious wakes restart
        // the syscall, so we track a deadline and shrink the remaining timeout
        // each iteration to keep the total wait bounded by the caller-supplied
        // value. An unrepresentable deadline (`Instant + Duration` overflow)
        // degrades to an infinite wait, like bionic, which passes the value
        // to the futex untouched.
        let deadline = Deadline(_timeout.and_then(|t| {
            Instant::now().checked_add(Duration::new(t.tv_sec as u64, t.tv_nsec as u32))
        }));
        loop {
            let remaining_ts = deadline.remaining();
            if remaining_ts.is_some_and(|t| t.tv_sec == 0 && t.tv_nsec == 0) {
                return Ok(None);
            }
            match futex::wait(
                _serial,
                futex::Flags::empty(),
                _value as _,
                remaining_ts.as_ref(),
            ) {
                Ok(_) => {
                    let new_serial = load_acquire(_serial);
                    if _value != new_serial {
                        return Ok(Some(new_serial));
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
                        return Ok(Some(new_serial));
                    }
                    // Serial changed and wrapped back to `_value` between the
                    // syscall and the reload — vanishingly unlikely; retry.
                }
                // Interrupted by a signal — retry with the recomputed
                // remaining timeout so the total wait stays bounded.
                Err(Errno::INTR) => {}
                // Timeout is a normal outcome, not an error worth logging.
                Err(Errno::TIMEDOUT) => return Ok(None),
                Err(e) => {
                    log::error!("Failed to wait for property change: {e}");
                    return Err(e.into_error());
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (_serial, _value, _timeout);
        Err(no_futex())
    }
}

/// The error of a wait that would have to block on macOS (no futex).
#[cfg(target_os = "macos")]
fn no_futex() -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "property waits need futex, which macOS does not have",
    ))
}

/// Source of [`SystemProperties`] instance ids. Starts at 1 so a zeroed
/// value never matches. A `Mutex`, not `AtomicU64`: Linux targets without
/// 64-bit atomics (armv5te, powerpc) have no `AtomicU64`, and a 32-bit
/// counter would let an index from an instance 2^32 creations earlier pass.
/// Locked once per instance creation.
static NEXT_INSTANCE: std::sync::Mutex<u64> = std::sync::Mutex::new(1);

/// Handle to one property, returned by [`SystemProperties::find`] and
/// [`SystemProperties::foreach`]. `Copy`: the methods that use one take it
/// by value.
///
/// An index is valid only on the instance that returned it — the global
/// reader and a writer on the same directory are different instances, and
/// so are two instances created one after the other on the same directory.
/// [`SystemProperties::read_index_with`], [`SystemProperties::serial`],
/// [`SystemProperties::wait`] and (`builder` feature)
/// `SystemProperties::update` reject an index from another instance.
///
/// Two indexes are equal when the same instance returned them for the same
/// entry, as two `const prop_info*` are in bionic: `find` on one name
/// twice, or `find` and `foreach`, give equal indexes, and an entry keeps
/// its index for the life of the instance (entries are never moved or
/// removed). Indexes from different instances are never equal. So an
/// index can be a `HashMap` key, for example to cache the serial last seen
/// per property.
// To avoid lifetime issues, the property index is used to access the property value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PropertyIndex {
    pub(crate) context_index: u32,
    /// Entry offset from `find` on context `context_index`'s area
    /// (property_area.rs \[R-OFF\]).
    pub(crate) property_index: EntryOffset,
    /// Id of the `SystemProperties` whose `find` returned this index.
    pub(crate) instance: u64,
}

/// The property store of one properties directory: the `property_info`
/// trie and the area files it names, mapped into this process.
///
/// A read-only instance comes from [`crate::system_properties()`] (the
/// process-wide one). With the `builder` feature, `SystemProperties::new_area`
/// and `SystemProperties::new_area_locked` create the areas and return the
/// writer instance.
pub struct SystemProperties {
    contexts: ContextsSerialized,
    /// Unique per instance; recorded in every `PropertyIndex` it returns.
    instance: u64,
    /// `find` and `read_with` try `ro.appcompat_override.<name>` first
    /// (bionic `SystemProperties::Find`). Set only on read-only instances,
    /// from [`APPCOMPAT_OVERRIDE_ENV_VAR`] at creation, like bionic's
    /// `Init`; writer instances look up names as given, like bionic's
    /// `AreaInit`.
    appcompat_override: bool,
}

/// Prints the instance id and the lookup mode only: no property name or
/// value, and no mapped memory.
impl std::fmt::Debug for SystemProperties {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemProperties")
            .field("instance", &self.instance)
            .field("appcompat_override", &self.appcompat_override)
            .finish_non_exhaustive()
    }
}

impl SystemProperties {
    fn with_contexts(contexts: ContextsSerialized, appcompat_override: bool) -> Self {
        Self {
            contexts,
            appcompat_override,
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
        let mut sp = Self::new_ro(filename, false)?;
        sp.appcompat_override = std::env::var_os(APPCOMPAT_OVERRIDE_ENV_VAR).is_some()
            && sp.bionic_reads_override_env();
        Ok(sp)
    }

    /// Whether this device's bionic turns the override lookup on from
    /// [`APPCOMPAT_OVERRIDE_ENV_VAR`] (see [`FIRST_SDK_WITH_OVERRIDE_ENV`]).
    /// Read before the flag is set, so `ro.build.version.sdk` is looked up
    /// by its plain name.
    #[cfg(target_os = "android")]
    fn bionic_reads_override_env(&self) -> bool {
        self.read_with("ro.build.version.sdk", crate::parse::parse_int::<u32>)
            .ok()
            .flatten()
            .is_some_and(|sdk| sdk >= FIRST_SDK_WITH_OVERRIDE_ENV)
    }

    /// No bionic on Linux: follow the current bionic, which reads the
    /// variable.
    #[cfg(not(target_os = "android"))]
    fn bionic_reads_override_env(&self) -> bool {
        true
    }

    /// [`Self::new`] with the override lookup decided by the caller
    /// instead of the environment (tests cannot set the variable without
    /// racing the other tests of the process).
    fn new_ro(filename: &Path, appcompat_override: bool) -> Result<Self> {
        let contexts = match ContextsSerialized::new_ro(filename, false) {
            Ok(contexts) => contexts,
            Err(e) => {
                log::error!("Failed to load contexts from {filename:?}: {e}");
                return Err(e);
            }
        };

        Ok(Self::with_contexts(contexts, appcompat_override))
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

        Ok(Self::with_contexts(contexts, false))
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

        Ok(Self::with_contexts(contexts, false))
    }

    /// `Error::InvalidArgument` (logged) unless `index` was returned by this
    /// instance's `find` or `foreach`. Every method taking an index calls
    /// it before any lock or mmap access (property_area.rs \[SM5\]).
    fn check_index(&self, index: PropertyIndex) -> Result<()> {
        if index.instance == self.instance {
            return Ok(());
        }
        log::error!(
            "PropertyIndex was returned by a different SystemProperties instance \
             (index from instance {}, used on instance {})",
            index.instance,
            self.instance
        );
        Err(Error::InvalidArgument(
            "PropertyIndex was returned by a different SystemProperties instance".into(),
        ))
    }

    /// [`Self::read_entry`] with the value validated as UTF-8 and the
    /// serial dropped: the `read_with` path.
    ///
    /// Returning a value through `f` instead of allocating a `String` is
    /// what makes the parse-and-discard hot path (`get<T>`/`get_or<T>`)
    /// allocation-free for short and long properties alike.
    fn read_with_callback<R, F>(
        &self,
        pa: &crate::property_area::PropertyAreaMap,
        pi_offset: EntryOffset,
        name: &[u8],
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&str) -> R,
    {
        // `Error::Utf8`, not a message string: keep every UTF-8 decode
        // failure on the same source-preserving variant.
        self.read_entry(pa, pi_offset, name, |bytes, _| {
            std::str::from_utf8(bytes).map(f)
        })?
        .map_err(Error::Utf8)
    }

    /// Reads the value of the entry at `pi_offset` under the seqlock
    /// protocol and hands the raw bytes and the serial they were read
    /// under to `f` — the pair bionic `__system_property_read_callback`
    /// passes. `name` decides whether the entry is long \[R-UNION\]: the
    /// lookup name on the `read_with` path, the entry's own name on the
    /// index path.
    ///
    /// The callback is invoked exactly once, on the iteration whose
    /// pre/post serial reads agree — earlier iterations (torn reads, dirty
    /// bit set then cleared) are absorbed by the retry loop. The serial is
    /// that iteration's, as in bionic `ReadMutablePropertyValue`: its dirty
    /// bit can be set, in which case the bytes are the backup slot's copy
    /// of the previous value.
    fn read_entry<R, F>(
        &self,
        pa: &crate::property_area::PropertyAreaMap,
        pi_offset: EntryOffset,
        name: &[u8],
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8], u32) -> R,
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
        let long_bytes: Option<&[u8]> = if prop_info.is_long_for(name) {
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
                return Ok(f.take().expect("callback consumed once on success")(
                    bytes, serial,
                ));
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
    ///
    /// # Appcompat override
    ///
    /// When the process environment had `BIONIC_APPCOMPAT_OVERRIDE` set
    /// (any value) at the time this instance was created —
    /// `__system_properties_zygote_reload` sets it — and the device is API
    /// 37 or later, `ro.appcompat_override.<name>` is looked up first and
    /// read if it exists, as bionic's `__system_property_find` does. Any
    /// failure of that lookup (no such entry, an area this process may not
    /// open) falls back to `name`. The same applies to [`Self::find`].
    /// Instances created with `new_area` / `new_area_locked` never do
    /// this. On Linux the API-level condition does not apply.
    ///
    /// On API 35–36 bionic enables overrides only in memory (zygote's
    /// children inherit the flag through `fork`), so this crate cannot see
    /// whether libc in the same process reads overrides, and reads plain
    /// names there.
    pub fn read_with<R, F>(&self, name: &str, f: F) -> Result<R>
    where
        F: FnOnce(&str) -> R,
    {
        if let Some((guard, _, pi_offset)) = self.probe_override(name) {
            let pa = guard.property_area();
            // The override entry's own name decides long vs short: the
            // lookup name lived in `with_override_name`'s buffer.
            return pa
                .property_info_name(pi_offset)
                .and_then(|entry_name| {
                    self.read_with_callback(pa, pi_offset, entry_name.to_bytes(), f)
                })
                .inspect_err(|e| log::error!("Failed to read override of {name:?}: {e}"));
        }
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
            Ok((_, pi_offset)) => {
                match self.read_with_callback(pa, pi_offset, name.as_bytes(), f) {
                    Ok(r) => Ok(r),
                    Err(e) => {
                        log::error!("Failed to read property {name:?}: {e}");
                        Err(e)
                    }
                }
            }
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

    /// The override entry of `name` — its area's read guard, context index
    /// and entry offset — when this instance has the appcompat override on
    /// and `ro.appcompat_override.<name>` exists. `None` on every failure:
    /// bionic `Find` falls back to `name` when the override name's area is
    /// missing or cannot be opened (`pa == nullptr`) or has no such entry.
    fn probe_override(&self, name: &str) -> Option<(PropertyAreaGuard<'_>, u32, EntryOffset)> {
        if !self.appcompat_override {
            return None;
        }
        with_override_name(name, |override_name| {
            let (guard, context_index) =
                self.contexts.prop_area_for_name_quiet(override_name).ok()?;
            let offset = guard.property_area().find(override_name).ok()?.1;
            Some((guard, context_index, offset))
        })
    }

    /// Reads the property at `index` and passes its name, value and serial
    /// to `f` — bionic `__system_property_read_callback`. The three come
    /// from one seqlock-validated read, so the serial is the one the value
    /// was read under: pass it as `old_serial` to [`Self::wait`] to wait
    /// for the next change without missing one that lands in between.
    ///
    /// For a mutable property the serial can have the dirty bit (bit 0)
    /// set, as in bionic: the read overlapped an update, and the value is
    /// the previous one, copied from the area's backup slot. Passing that
    /// serial to `wait` reports the same update's clean store as a change.
    /// `ro.` properties never change after they are added.
    ///
    /// No name lookup runs, unlike [`Self::read_with`]: keep an index from
    /// [`Self::find`] or [`Self::foreach`] to re-read a property without
    /// walking the trie again. The name passed to `f` is the entry's own,
    /// which on an instance with the appcompat override on can be
    /// `ro.appcompat_override.<name>`.
    ///
    /// Fails with `Error::InvalidArgument` for an index from another
    /// instance, and with `Error::Utf8` for a non-UTF-8 name or value. `f`
    /// runs under the context node's read lock, with the cautions of
    /// [`Self::read_with`].
    pub fn read_index_with<R, F>(&self, index: PropertyIndex, f: F) -> Result<R>
    where
        F: FnOnce(&str, &str, u32) -> R,
    {
        self.read_index_bytes(index, |name, value, serial| {
            let name = std::str::from_utf8(name)?;
            let value = std::str::from_utf8(value)?;
            Ok(f(name, value, serial))
        })?
        .map_err(Error::Utf8)
    }

    /// [`Self::read_index_with`] on raw bytes: entry name, value, serial.
    fn read_index_bytes<R, F>(&self, index: PropertyIndex, f: F) -> Result<R>
    where
        F: FnOnce(&[u8], &[u8], u32) -> R,
    {
        self.check_index(index)?;
        let guard = self.contexts.prop_area_with_index(index.context_index)?;
        let pa = guard.property_area();
        let name = pa
            .property_info_name(index.property_index)
            .inspect_err(|e| {
                log::error!(
                    "Failed to read property name for index {}: {e}",
                    index.property_index
                )
            })?
            .to_bytes();
        self.read_entry(pa, index.property_index, name, |value, serial| {
            f(name, value, serial)
        })
    }

    /// Calls `f` with the index of every property this process can read —
    /// bionic `__system_property_foreach`. Read each one with
    /// [`Self::read_index_with`]; `f` runs with no lock held, so it may
    /// call any read method of this instance.
    ///
    /// Areas are visited in `property_info` context order, and the
    /// properties of an area in bionic's trie order, which is not sorted
    /// by name. An area that cannot be opened — on Android typically an
    /// SELinux denial — is skipped, as in bionic; so is a context entry
    /// that was corrupt when this instance was created.
    ///
    /// The indexes of an area are collected under its node's read lock,
    /// which is released before `f` runs for them; a property added to
    /// that area after the collection is not reported. A corrupt trie in
    /// an opened area ends the walk with `Error::FileValidation` (bionic
    /// stops that area's walk without reporting it); `f` has then already
    /// run for the areas visited before.
    ///
    /// ```rust,no_run
    /// # fn main() -> rsproperties::Result<()> {
    /// let props = rsproperties::system_properties()?;
    /// props.foreach(|index| {
    ///     let _ = props.read_index_with(index, |name, value, _serial| {
    ///         println!("[{name}]: [{value}]");
    ///     });
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn foreach<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(PropertyIndex),
    {
        for context_index in 0..self.contexts.context_count() {
            let Some(offsets) = self.contexts.entry_offsets_at(context_index)? else {
                continue;
            };
            for property_index in offsets {
                f(PropertyIndex {
                    context_index,
                    property_index,
                    instance: self.instance,
                });
            }
        }
        Ok(())
    }

    /// The value of `name` — libbase `GetProperty` without its default:
    /// `Error::NotFound` for a missing property, with the other failures of
    /// [`Self::read_with`].
    ///
    /// Allocates a `String`; for the parse-and-discard hot path prefer
    /// [`Self::read_with`], which hands the value as `&str` without
    /// allocating. The free function [`crate::get`] parses the value of
    /// the global instance instead.
    pub fn get(&self, name: &str) -> Result<String> {
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
    ///
    /// On an instance with the appcompat override on, the index can
    /// address `ro.appcompat_override.<name>` (see [`Self::read_with`]
    /// # Appcompat override); [`Self::read_index_with`] reports the
    /// entry's own name.
    pub fn find(&self, name: &str) -> Result<Option<PropertyIndex>> {
        if let Some((_, context_index, property_index)) = self.probe_override(name) {
            return Ok(Some(PropertyIndex {
                context_index,
                property_index,
                instance: self.instance,
            }));
        }
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
            Some(prop_ref) => self.update(prop_ref, value),
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
    pub fn update(&mut self, index: PropertyIndex, value: &str) -> Result<()> {
        // Another instance's offsets address an unrelated layout.
        self.check_index(index)?;
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
        // and, worse, skip the global serial bump below, leaving
        // global-serial waiters (`wait(None, ..)`) permanently unaware of the
        // change. That is why the
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

    /// The type `property_info` records for `name` — the `property_contexts`
    /// type field (`"string"`, `"bool"`, `"int"`, `"uint"`, `"double"`,
    /// `"size"`, or `"enum <values...>"`), or the default type the trie was
    /// built with when no entry gives one. `None` when neither exists. A
    /// property service checks a value against it before a set, as AOSP
    /// init's `CheckType` does; `set` and `add` do not check it.
    #[cfg(feature = "builder")]
    pub fn property_type(&self, name: &str) -> Result<Option<&str>> {
        self.contexts.property_type(name)
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

    /// Reads the global serial (bionic `__system_property_area_serial`):
    /// one counter per properties directory, kept in the `properties_serial`
    /// area, that every property add and update increments. A plain atomic
    /// load; no lock is taken.
    ///
    /// Pass it as `old_serial` to [`Self::wait`] with `index == None` to
    /// wait for the next change to any property. Sample it *before* the
    /// lookup whose result decides whether to wait, so a change between the
    /// lookup and the wait is not missed.
    pub fn area_serial(&self) -> u32 {
        let serial_pa = self.contexts.serial_prop_area();
        load_acquire(serial_pa.serial())
    }

    /// Reads the per-property serial counter (bionic
    /// `__system_property_serial`). Fails when the area or entry cannot be
    /// reached; `0` is a valid serial.
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
    /// instances. An index from another instance fails with
    /// `Error::InvalidArgument`.
    pub fn serial(&self, idx: PropertyIndex) -> Result<u32> {
        self.check_index(idx)?;
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
                let guard = self.contexts.prop_area_with_index(idx.context_index)?;
                let pi = guard.property_area().property_info(idx.property_index)?;
                let serial = load_acquire(&pi.serial);
                if !serial_dirty(serial) {
                    return Ok(serial);
                }
                // Clamp the final slice so the total bound is exact, and
                // check it BEFORE waiting so expiry never adds a slice.
                let remaining = DIRTY_WAIT_TOTAL.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    log::warn!(
                        "serial: entry still dirty after {DIRTY_WAIT_TOTAL:?} \
                         (writer crashed mid-update?); returning the dirty serial"
                    );
                    return Ok(serial);
                }
                let slice = Deadline::after(Some(remaining.min(DIRTY_SLICE)));
                match futex_wait(&pi.serial, serial, slice.remaining().as_ref()) {
                    Ok(Some(s)) if !serial_dirty(s) => return Ok(s),
                    // Still dirty (writer burst) or slice expired: drop the
                    // guard at the end of this iteration and re-acquire.
                    Ok(_) => {}
                    Err(e) => {
                        let current = load_acquire(&pi.serial);
                        if serial_dirty(current) {
                            log::warn!(
                                "serial: futex wait failed ({e}); returning the dirty serial"
                            );
                        }
                        return Ok(current);
                    }
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            let guard = self.contexts.prop_area_with_index(idx.context_index)?;
            let pi = guard.property_area().property_info(idx.property_index)?;
            Ok(load_acquire(&pi.serial))
        }
    }

    /// Waits until the property at `index` (or, with `index == None`, the
    /// global serial — i.e. any property) changes — bionic
    /// `__system_property_wait`. Returns `Ok(Some(new_serial))` on a
    /// change and `Ok(None)` when `timeout` expires first; `None` waits
    /// forever (also for a timeout too large to represent). A lookup or
    /// futex failure is `Err`, unlike bionic, whose `bool` reports it as a
    /// timeout. `wait(None, old_serial, None)` is bionic's deprecated
    /// `__system_property_wait_any(old_serial)`.
    ///
    /// `old_serial` is the serial observed when the caller last read the
    /// value: from [`Self::serial`] or [`Self::read_index_with`] for an
    /// index, from [`Self::area_serial`] for the global serial. If the
    /// serial already differs from `old_serial`, this returns immediately
    /// with the current one, so a change between the read and the wait is
    /// not missed — the contract of bionic
    /// `__system_property_wait(pi, old_serial, …)`, where the argument is
    /// required as well. `0` reports the current serial at once unless it
    /// is `0` itself (bionic: "If you don't know the current serial, use
    /// 0").
    ///
    /// ```rust,no_run
    /// # fn main() -> rsproperties::Result<()> {
    /// let props = rsproperties::system_properties()?;
    /// if let Some(index) = props.find("sys.boot_completed")? {
    ///     loop {
    ///         // Value and serial from one read; wait from that serial.
    ///         let (booted, serial) =
    ///             props.read_index_with(index, |_, value, serial| (value == "1", serial))?;
    ///         if booted {
    ///             break;
    ///         }
    ///         props.wait(Some(index), serial, None)?;
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
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
    /// so with a zero timeout this call is a non-blocking check
    /// (`Ok(None)` for an unchanged serial). bionic gets the same result
    /// from the kernel: `FUTEX_WAIT` returns `EAGAIN` for a changed word
    /// with a zero timeout, and bionic reloads the serial after it. With an
    /// index the comparison needs the context node's read lock, which even
    /// a zero-timeout call can wait for: only when another thread of this
    /// instance started opening that area before it was open, and then
    /// until the call that holds the node's read lock releases it (see
    /// # Locking).
    ///
    /// On macOS there is no futex, so this cannot *block*: the
    /// `old_serial` fast path still works (a serial that already differs
    /// returns `Ok(Some)` immediately — that part is a plain atomic load),
    /// but once the serial has to be waited on, the call fails.
    ///
    /// An index is valid only on the instance whose `find` returned it —
    /// the global reader and a writer on the same directory are different
    /// instances. An index from another instance fails with
    /// `Error::InvalidArgument` before any lock is taken.
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
        index: Option<PropertyIndex>,
        old_serial: u32,
        timeout: Option<Duration>,
    ) -> Result<Option<u32>> {
        self.wait_until(index, old_serial, &Deadline::after(timeout))
    }

    /// [`Self::wait`] against an absolute deadline, so `wait_for_creation`
    /// / `wait_for_value` can bound a whole loop of waits with one.
    fn wait_until(
        &self,
        index: Option<PropertyIndex>,
        old_serial: u32,
        deadline: &Deadline,
    ) -> Result<Option<u32>> {
        // No index → wait on the global serial (lock-free, no slicing).
        let Some(idx) = index else {
            let serial_pa = self.contexts.serial_prop_area().serial();
            // Documented already-changed fast path, checked BEFORE the
            // futex: on Linux it merely pre-empts the syscall's EAGAIN,
            // but on macOS (no futex — `futex_wait` fails immediately)
            // it is the only thing keeping the `old_serial` contract.
            let current = load_acquire(serial_pa);
            if current != old_serial {
                return Ok(Some(current));
            }
            return futex_wait(serial_pa, old_serial, deadline.remaining().as_ref());
        };
        // Covers both platform branches below.
        self.check_index(idx)?;

        #[cfg(any(target_os = "android", target_os = "linux"))]
        {
            /// Upper bound on how long one slice holds the node's read
            /// lock: the part of a first-time opener's delay (another
            /// thread of this instance, see # Locking) that a `wait` can
            /// cause.
            const LOCK_SLICE: Duration = Duration::from_millis(100);

            let mut first = true;
            loop {
                // The slice is computed BEFORE the lock is (re-)taken. The
                // first iteration always takes the lock and compares
                // `old_serial` before the timeout applies, like bionic,
                // whose FUTEX_WAIT reports a changed word for a zero
                // timeout (EAGAIN) and is followed by a reload. That lock
                // wait is bounded only as the rustdoc says (another
                // thread's first open of this area). Later iterations
                // return without the lock once the deadline has passed:
                // the previous comparison was the last one, as after
                // bionic's ETIMEDOUT, and blocking on the lock after
                // expiry would overshoot the caller's timeout.
                let slice = match deadline.left() {
                    None => Some(LOCK_SLICE),
                    Some(r) if r.is_zero() && !first => return Ok(None),
                    Some(r) if r.is_zero() => None,
                    Some(r) => Some(r.min(LOCK_SLICE)),
                };
                first = false;
                // (Re-)acquire the node lock for this slice only.
                let guard = self.contexts.prop_area_with_index(idx.context_index)?;
                let pi = guard.property_area().property_info(idx.property_index)?;
                // The serial may have changed while the lock was released
                // between slices — the futex wake fired with no waiter, so
                // this re-check is what closes that window.
                let current = load_acquire(&pi.serial);
                if current != old_serial {
                    return Ok(Some(current));
                }
                // Expired timeout: the comparison above was the only one.
                let Some(slice) = slice else {
                    return Ok(None);
                };
                let slice = Deadline::after(Some(slice));
                if let Some(s) = futex_wait(&pi.serial, old_serial, slice.remaining().as_ref())? {
                    return Ok(Some(s));
                }
                // Slice expired: fall through, dropping `guard` at the end of
                // the iteration so writers get a window.
            }
        }
        #[cfg(target_os = "macos")]
        {
            // No futex, so blocking is impossible — but the documented
            // `old_serial` fast path is a plain load and must still hold:
            // a serial that already moved past `old_serial` returns
            // immediately instead of being misreported as a failure.
            let current = self.serial(idx)?;
            if current != old_serial {
                return Ok(Some(current));
            }
            let _ = deadline;
            Err(no_futex())
        }
    }

    /// Waits until `name` exists and returns its index — libbase
    /// `WaitForPropertyCreation`, which returns only `bool`. Between
    /// lookups it waits on the global serial, which every property add
    /// (and update) moves. `timeout` bounds the whole call; `None` waits
    /// forever.
    ///
    /// Returns `Ok(None)` when `timeout` expires. A lookup error from
    /// [`Self::find`] and a failed futex wait are `Err`; libbase reports
    /// both as a timeout.
    ///
    /// On an instance with the appcompat override on, the override name
    /// counts as `name` existing (see [`Self::read_with`]).
    pub fn wait_for_creation(
        &self,
        name: &str,
        timeout: Option<Duration>,
    ) -> Result<Option<PropertyIndex>> {
        self.wait_for_creation_until(name, &Deadline::after(timeout))
    }

    fn wait_for_creation_until(
        &self,
        name: &str,
        deadline: &Deadline,
    ) -> Result<Option<PropertyIndex>> {
        loop {
            // Sampled BEFORE `find`: an add that lands between the lookup
            // and the wait has already moved the global serial, so the
            // wait returns at once instead of missing it.
            let serial = self.area_serial();
            if let Some(index) = self.find(name)? {
                return Ok(Some(index));
            }
            let changed = self
                .wait_until(None, serial, deadline)
                .with_context_location(|| format!("waiting for property {name:?}"))?;
            if changed.is_none() {
                return Ok(None);
            }
        }
    }

    /// Waits until `name` exists and its value equals `expected` — libbase
    /// `WaitForProperty`. Returns `Ok(true)` once the value matches, and
    /// `Ok(false)` when `timeout` (for the whole call, creation included;
    /// `None` waits forever) expires first. Failures are `Err`, as in
    /// [`Self::wait_for_creation`].
    ///
    /// Each check reads the value and its serial together
    /// ([`Self::read_index_with`]) and waits from that serial, so a change
    /// between the check and the wait is not missed. The comparison is on
    /// bytes: a non-UTF-8 value never equals `expected` and the wait goes
    /// on.
    ///
    /// ```rust,no_run
    /// # fn main() -> rsproperties::Result<()> {
    /// use std::time::Duration;
    ///
    /// let props = rsproperties::system_properties()?;
    /// if props.wait_for_value("sys.boot_completed", "1", Some(Duration::from_secs(30)))? {
    ///     println!("booted");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn wait_for_value(
        &self,
        name: &str,
        expected: &str,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        let deadline = Deadline::after(timeout);
        let Some(index) = self.wait_for_creation_until(name, &deadline)? else {
            return Ok(false);
        };
        loop {
            let (matches, serial) = self.read_index_bytes(index, |_, value, serial| {
                (value == expected.as_bytes(), serial)
            })?;
            if matches {
                return Ok(true);
            }
            let changed = self
                .wait_until(Some(index), serial, &deadline)
                .with_context_location(|| format!("waiting for property {name:?}"))?;
            if changed.is_none() {
                return Ok(false);
            }
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
            let version1 = system_properties.get(VERSION_PROPERTY).unwrap_or_default();
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
    use crate::{build_trie, AreaWriterLock, MatchKeyword, PropertyInfoEntry};
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
            let (entries, errors) =
                PropertyInfoEntry::parse_from_file(&contexts, MatchKeyword::Unchecked).unwrap();
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

        pub(super) fn serial_word(&self, idx: PropertyIndex) -> u32 {
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

        assert_eq!(fx.sp.get("debug.rsp.bit16").unwrap(), "v32767");
        // A read-only instance, as the global reader opens it.
        let reader = SystemProperties::new(fx.dir()).unwrap();
        assert_eq!(reader.get("debug.rsp.bit16").unwrap(), "v32767");
    }

    #[test]
    fn non_ro_entry_with_bit16_updates_in_place() {
        let mut fx = Fixture::new("update");
        fx.sp.add("debug.rsp.bit16", "v32767").unwrap();
        let idx = fx.set_bit16("debug.rsp.bit16");

        fx.sp.update(idx, "v32768").unwrap();
        assert_eq!(fx.sp.get("debug.rsp.bit16").unwrap(), "v32768");
        assert_eq!(
            fx.serial_word(idx) & LONG_FLAG,
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
        let start = fx.serial_word(idx);
        for i in 0..32768u32 {
            let v = if i % 2 == 0 { "b" } else { "a" };
            fx.sp.update(idx, v).unwrap();
        }
        let end = fx.serial_word(idx);
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
        assert_eq!(fx.sp.get("ro.rsp.long").unwrap(), long);
        let reader = SystemProperties::new(fx.dir()).unwrap();
        assert_eq!(reader.get("ro.rsp.long").unwrap(), long);
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

    /// With an index, a zero timeout still compares `old_serial` first,
    /// like the global-serial path and bionic.
    #[test]
    fn index_wait_compares_before_timeout() {
        let mut fx = Fixture::new("wait_zero");
        fx.sp.add("debug.rsp.zero", "0").unwrap();
        let reader = SystemProperties::new(fx.dir()).unwrap();
        let idx = reader.find("debug.rsp.zero").unwrap().unwrap();
        let old = reader.serial(idx).unwrap();
        fx.sp.set("debug.rsp.zero", "1").unwrap();

        let got = reader.wait(Some(idx), old, Some(Duration::ZERO)).unwrap();
        assert!(got.is_some_and(|s| s != old), "{got:?}");
        let cur = reader.serial(idx).unwrap();
        let unchanged = reader.wait(Some(idx), cur, Some(Duration::ZERO));
        assert!(matches!(unchanged, Ok(None)), "{unchanged:?}");
        // Regression only (passes before the fix): the global path gives
        // the same answer for the same inputs.
        let global = reader.area_serial();
        fx.sp.set("debug.rsp.zero", "2").unwrap();
        let got = reader.wait(None, global, Some(Duration::ZERO));
        assert!(matches!(got, Ok(Some(_))), "{got:?}");
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
                fx.sp.update(widx, &format!("v{trial}")).expect("update");
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

    /// libbase `WaitForPropertyCreation`: a property added after the call
    /// started is found; a missing one times out with `Ok(None)`.
    #[test]
    fn wait_for_creation_finds_later_add_and_times_out() {
        let mut fx = Fixture::new("wait_create");
        let reader = SystemProperties::new_ro(fx.dir(), false).unwrap();
        let found = std::thread::scope(|s| {
            let sp = &mut fx.sp;
            s.spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                sp.add("debug.rsp.later", "x").unwrap();
            });
            reader.wait_for_creation("debug.rsp.later", Some(Duration::from_secs(5)))
        })
        .unwrap()
        .expect("created before the timeout");
        assert_eq!(
            reader
                .read_index_with(found, |name, value, _| (name.to_owned(), value.to_owned()))
                .unwrap(),
            ("debug.rsp.later".to_owned(), "x".to_owned())
        );

        let start = Instant::now();
        let none = reader
            .wait_for_creation("debug.rsp.never", Some(Duration::from_millis(50)))
            .unwrap();
        assert!(none.is_none());
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    /// libbase `WaitForProperty`: creation and the value change both count
    /// against one timeout; a value that never arrives returns `false`.
    #[test]
    fn wait_for_value_sees_create_then_update() {
        let mut fx = Fixture::new("wait_value");
        let reader = SystemProperties::new_ro(fx.dir(), false).unwrap();
        let matched = std::thread::scope(|s| {
            let sp = &mut fx.sp;
            s.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                sp.add("debug.rsp.state", "starting").unwrap();
                std::thread::sleep(Duration::from_millis(50));
                sp.set("debug.rsp.state", "running").unwrap();
            });
            reader.wait_for_value("debug.rsp.state", "running", Some(Duration::from_secs(5)))
        })
        .unwrap();
        assert!(matched);
        // Already equal: no wait.
        assert!(reader
            .wait_for_value("debug.rsp.state", "running", Some(Duration::ZERO))
            .unwrap());

        let start = Instant::now();
        assert!(!reader
            .wait_for_value(
                "debug.rsp.state",
                "stopped",
                Some(Duration::from_millis(50))
            )
            .unwrap());
        assert!(start.elapsed() >= Duration::from_millis(50));
    }
}

/// `foreach`, `read_index_with` and the appcompat override lookup.
#[cfg(all(test, feature = "builder", not(target_os = "android")))]
mod client_parity_tests {
    use super::test_fixture::Fixture;
    use super::*;
    use std::collections::BTreeMap;

    fn dump(sp: &SystemProperties) -> BTreeMap<String, (String, u32)> {
        let mut all = BTreeMap::new();
        sp.foreach(|idx| {
            let (name, value, serial) = sp
                .read_index_with(idx, |n, v, s| (n.to_owned(), v.to_owned(), s))
                .unwrap();
            assert_eq!(sp.serial(idx).unwrap(), serial, "{name}");
            assert!(
                all.insert(name, (value, serial)).is_none(),
                "reported twice"
            );
        })
        .unwrap();
        all
    }

    #[test]
    fn foreach_reports_every_property_once() {
        let mut fx = Fixture::new("foreach");
        let long = "l".repeat(200);
        let props = [
            ("debug.a", "1"),
            ("debug.b.c", "2"),
            ("ro.x", "3"),
            ("ro.long", long.as_str()),
            ("other.default", "4"),
        ];
        for (name, value) in props {
            fx.sp.add(name, value).unwrap();
        }
        fx.sp.set("debug.a", "updated").unwrap();

        let expected: BTreeMap<String, String> = props
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .chain([("debug.a".to_string(), "updated".to_string())])
            .collect();
        let reader = SystemProperties::new_ro(fx.dir(), false).unwrap();
        for sp in [&fx.sp, &reader] {
            let got: BTreeMap<String, String> =
                dump(sp).into_iter().map(|(n, (v, _))| (n, v)).collect();
            assert_eq!(got, expected);
        }
    }

    /// Within an area the walk is bionic's: `left`, `prop`, `children`,
    /// `right`, which for siblings is (length, bytes) order whatever the
    /// insertion order was.
    #[test]
    fn foreach_follows_bionic_trie_order() {
        let mut fx = Fixture::new("foreach_order");
        for name in ["debug.bb", "debug.a", "debug.ccc", "debug.bb.x", "debug.ab"] {
            fx.sp.add(name, "v").unwrap();
        }
        let mut names = Vec::new();
        fx.sp
            .foreach(|idx| {
                names.push(fx.sp.read_index_with(idx, |n, _, _| n.to_owned()).unwrap());
            })
            .unwrap();
        assert_eq!(
            names,
            ["debug.a", "debug.ab", "debug.bb", "debug.bb.x", "debug.ccc"]
        );
    }

    #[test]
    fn read_index_with_rejects_foreign_index() {
        let mut fx = Fixture::new("foreign_read");
        fx.sp.add("debug.rsp.f", "1").unwrap();
        let reader = SystemProperties::new_ro(fx.dir(), false).unwrap();
        let writer_idx = fx.sp.find("debug.rsp.f").unwrap().unwrap();
        assert!(matches!(
            reader.read_index_with(writer_idx, |_, _, _| ()),
            Err(Error::InvalidArgument(_))
        ));
    }

    /// bionic `SystemProperties::Find` with `BIONIC_APPCOMPAT_OVERRIDE`
    /// set: the override name wins when it exists, the plain name is used
    /// otherwise, and a writer instance never looks at overrides.
    #[test]
    fn appcompat_override_is_looked_up_first() {
        let mut fx = Fixture::new("appcompat");
        let long = "o".repeat(150);
        for (name, value) in [
            ("debug.ac", "orig"),
            ("ro.appcompat_override.debug.ac", "ovr"),
            ("debug.plain", "p"),
            ("debug.longovr", "short"),
            ("ro.appcompat_override.debug.longovr", long.as_str()),
        ] {
            fx.sp.add(name, value).unwrap();
        }
        let on = SystemProperties::new_ro(fx.dir(), true).unwrap();
        let off = SystemProperties::new_ro(fx.dir(), false).unwrap();

        assert_eq!(on.get("debug.ac").unwrap(), "ovr");
        assert_eq!(off.get("debug.ac").unwrap(), "orig");
        assert_eq!(fx.sp.get("debug.ac").unwrap(), "orig");
        assert_eq!(on.get("debug.plain").unwrap(), "p");
        // The override entry is `ro.`, so it may be long although the
        // plain name is not `ro.`: the entry's own name decides.
        assert_eq!(on.get("debug.longovr").unwrap(), long);
        assert!(matches!(on.get("debug.none"), Err(Error::NotFound(_))));
        assert!(on.find("debug.none").unwrap().is_none());

        let idx = on.find("debug.ac").unwrap().unwrap();
        assert_eq!(
            on.read_index_with(idx, |n, v, _| (n.to_owned(), v.to_owned()))
                .unwrap(),
            (
                "ro.appcompat_override.debug.ac".to_owned(),
                "ovr".to_owned()
            )
        );
    }

    #[test]
    fn override_name_longer_than_stack_buffer() {
        let mut fx = Fixture::new("appcompat_long_name");
        let name = format!("debug.{}", "n".repeat(150));
        fx.sp.add(&name, "orig").unwrap();
        fx.sp
            .add(&format!("ro.appcompat_override.{name}"), "ovr")
            .unwrap();
        let on = SystemProperties::new_ro(fx.dir(), true).unwrap();
        assert_eq!(on.get(&name).unwrap(), "ovr");
    }

    fn foreach_names(sp: &SystemProperties) -> Vec<String> {
        let mut names = Vec::new();
        sp.foreach(|idx| {
            names.push(sp.read_index_with(idx, |n, _, _| n.to_owned()).unwrap());
        })
        .unwrap();
        names
    }

    /// bionic `CheckAccessAndOpen`: an area `foreach` was denied is not
    /// tried again by that instance, while name lookups still open it.
    #[test]
    fn foreach_does_not_retry_a_denied_area() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return; // root opens a 0000 file; nothing to observe
        }
        let mut fx = Fixture::new("foreach_denied");
        fx.sp.add("debug.denied.a", "1").unwrap();
        let area = fx.dir().join("u:object_r:debug_prop:s0");
        let mode = std::fs::metadata(&area).unwrap().permissions().mode();
        let reader = SystemProperties::new_ro(fx.dir(), false).unwrap();

        std::fs::set_permissions(&area, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(!foreach_names(&reader).contains(&"debug.denied.a".to_owned()));
        std::fs::set_permissions(&area, std::fs::Permissions::from_mode(mode)).unwrap();

        assert!(
            !foreach_names(&reader).contains(&"debug.denied.a".to_owned()),
            "a denied area is not tried again by foreach"
        );
        assert_eq!(reader.get("debug.denied.a").unwrap(), "1");
    }

    /// A missing area file is not recorded: `foreach` finds the area once
    /// the file exists (the service can create it after a reader started).
    #[test]
    fn foreach_retries_a_missing_area() {
        let mut fx = Fixture::new("foreach_missing");
        fx.sp.add("debug.missing.a", "1").unwrap();
        let area = fx.dir().join("u:object_r:debug_prop:s0");
        let parked = fx.dir().join("parked_area");
        let reader = SystemProperties::new_ro(fx.dir(), false).unwrap();

        std::fs::rename(&area, &parked).unwrap();
        assert!(!foreach_names(&reader).contains(&"debug.missing.a".to_owned()));
        std::fs::rename(&parked, &area).unwrap();
        assert!(foreach_names(&reader).contains(&"debug.missing.a".to_owned()));
    }
}
