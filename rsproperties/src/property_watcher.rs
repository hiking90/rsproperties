// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! [`PropertyWatcher`]: one property followed over time.

use std::time::Duration;

use crate::errors::*;
use crate::system_properties::{Deadline, PropertyIndex, SystemProperties};

/// Bit 0 of a property serial: set by a writer's first store of an update
/// and cleared by its last.
const DIRTY: u32 = 1;

/// The property a watcher has read.
#[derive(Clone, Copy)]
struct Seen {
    index: PropertyIndex,
    /// Serial the cached value was read under, dirty bit cleared. A read
    /// that overlaps an update returns the dirty serial with the value
    /// from before that update, which is the value of the clean serial
    /// below it.
    serial: u32,
}

/// Follows one property: its current value without a name lookup per
/// read, and a wait for its next change — libbase `CachedProperty`.
/// Created by [`SystemProperties::watch`].
///
/// The watcher keeps the property's index, the serial of the value it last
/// read and that value in one `String`. [`Self::value`] reads the store
/// again only when the serial has moved, and [`Self::wait_for_change`]
/// blocks until it moves, so a loop of the two neither misses a change
/// that lands between them nor has to carry an index and a serial.
///
/// ```rust,no_run
/// use std::time::Duration;
///
/// # fn main() -> rsproperties::Result<()> {
/// let props = rsproperties::system_properties()?;
/// let mut boot = props.watch("sys.boot_completed");
/// while boot.value()? != Some("1") {
///     if !boot.wait_for_change(Some(Duration::from_secs(30)))? {
///         println!("no change for 30 s; still {:?}", boot.value()?);
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// # What a change is
///
/// - The property's creation, then each update — also one that stores the
///   value the property already had: the store keeps a serial per
///   property, not the previous value, and every update advances it.
/// - One update is one change. A writer stores the dirty serial, the
///   value, then the clean serial, and [`SystemProperties::wait`] returns
///   for either store; the watcher waits on until the value has been
///   replaced.
/// - Changes are not queued. Updates that land between two calls are one
///   change, and the value is the latest one.
///
/// A watcher borrows its `SystemProperties`, so one on the process-wide
/// reader is `PropertyWatcher<'static>` and can move to another thread.
pub struct PropertyWatcher<'a> {
    props: &'a SystemProperties,
    name: String,
    seen: Option<Seen>,
    /// Global serial sampled before the last lookup that did not find
    /// `name`: the lookup is repeated only after it has moved (every add
    /// moves it), like `CachedProperty::Get`.
    missed_at: Option<u32>,
    /// The value read under `seen.serial`.
    value: String,
}

/// Prints the name and whether the property has been found, not the value.
impl std::fmt::Debug for PropertyWatcher<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PropertyWatcher")
            .field("name", &self.name)
            .field("found", &self.seen.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> PropertyWatcher<'a> {
    pub(crate) fn new(props: &'a SystemProperties, name: &str) -> Self {
        Self {
            props,
            name: name.to_owned(),
            seen: None,
            missed_at: None,
            value: String::new(),
        }
    }

    /// The name this watcher was created for.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The property's current value, or `None` while it does not exist.
    ///
    /// Once the property has been found, a call costs the context node's
    /// read lock and one atomic load of the serial; the value is copied
    /// out of the store again only when the serial has moved. Before that,
    /// the name is looked up again only when the global serial has moved
    /// since the last lookup.
    ///
    /// A value that is not UTF-8 is [`Error::Utf8`], on every call until
    /// the property is updated; the other failures are those of
    /// [`SystemProperties::find`] and
    /// [`SystemProperties::read_index_with`].
    pub fn value(&mut self) -> Result<Option<&str>> {
        let index = match &self.seen {
            Some(seen) => seen.index,
            None => {
                // Sampled BEFORE `find`, so an add that lands between the
                // two moves the serial past the recorded one.
                let area_serial = self.props.area_serial();
                if self.missed_at == Some(area_serial) {
                    return Ok(None);
                }
                let Some(index) = self.props.find(&self.name)? else {
                    self.missed_at = Some(area_serial);
                    return Ok(None);
                };
                index
            }
        };
        // While an update is in progress the word is `seen.serial | DIRTY`
        // and a read returns the backup of the value already cached.
        let word = self.props.serial_word(index)?;
        if self.seen.as_ref().map(|seen| seen.serial) != Some(word & !DIRTY) {
            self.read(index)?;
        }
        Ok(Some(&self.value))
    }

    /// Blocks until the property changes (see the type's "What a change
    /// is"), then reads the new value, which the next [`Self::value`]
    /// returns without reading the store again. `Ok(true)` on a change,
    /// `Ok(false)` when `timeout` expires first; `None` waits forever.
    ///
    /// The wait starts from the value this watcher last read, by
    /// [`Self::value`] or by an earlier call: a change made since then
    /// returns at once. A watcher that has not read the property yet waits
    /// for it to exist, so the first call returns `Ok(true)` at once for a
    /// property that already does. `timeout` bounds the whole call. With
    /// `Some(Duration::ZERO)` the call does not block and reports whether
    /// the property changed since the last read.
    ///
    /// Failures are those of [`SystemProperties::wait`] and
    /// [`Self::value`]. After an `Err` the watcher still holds the value
    /// it had, so the next call reports the same change again; a value
    /// that is not UTF-8 therefore fails every call until the property is
    /// updated. On macOS a call that would have to block fails (no futex).
    pub fn wait_for_change(&mut self, timeout: Option<Duration>) -> Result<bool> {
        let deadline = Deadline::after(timeout);
        let Some(Seen { index, serial }) = self.seen else {
            let Some(index) = self.props.wait_for_creation_until(&self.name, &deadline)? else {
                return Ok(false);
            };
            self.read(index)?;
            return Ok(true);
        };
        let mut old_serial = serial;
        loop {
            let changed = self
                .props
                .wait_until(Some(index), old_serial, &deadline)
                .with_context_location(|| format!("waiting for property {:?}", self.name))?;
            if changed.is_none() {
                return Ok(false);
            }
            let word = self.read(index)?;
            if word & !DIRTY != serial {
                return Ok(true);
            }
            // The dirty serial of an update in progress: the value read is
            // the backup of the one already reported. Wait from the dirty
            // serial for that update's clean store.
            old_serial = word;
        }
    }

    /// Reads value and serial in one seqlock-validated read and records
    /// both. Returns the serial as read, dirty bit included. On a failure
    /// (a non-UTF-8 value) nothing is recorded.
    fn read(&mut self, index: PropertyIndex) -> Result<u32> {
        let value = &mut self.value;
        let word = self.props.read_index_with(index, |_, text, serial| {
            value.clear();
            value.push_str(text);
            serial
        })?;
        self.seen = Some(Seen {
            index,
            serial: word & !DIRTY,
        });
        Ok(word)
    }
}

#[cfg(all(test, feature = "builder", target_os = "linux"))]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::system_properties::test_fixture::Fixture;

    const NO_WAIT: Option<Duration> = Some(Duration::ZERO);

    /// `value` is `None` until the property exists, then follows every
    /// update without a wait in between.
    #[test]
    fn value_follows_creation_and_updates() {
        let mut fx = Fixture::new("watch_value");
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let mut watcher = reader.watch("debug.rsp.watch");
        assert_eq!(watcher.name(), "debug.rsp.watch");
        assert_eq!(watcher.value().unwrap(), None);
        // An unrelated add moves the global serial: the lookup runs again
        // and still misses.
        fx.sp.add("debug.rsp.other", "x").unwrap();
        assert_eq!(watcher.value().unwrap(), None);

        fx.sp.add("debug.rsp.watch", "a").unwrap();
        assert_eq!(watcher.value().unwrap(), Some("a"));
        assert_eq!(watcher.value().unwrap(), Some("a"));
        fx.sp.set("debug.rsp.watch", "b").unwrap();
        assert_eq!(watcher.value().unwrap(), Some("b"));
        // A shorter value does not keep the tail of the longer one.
        fx.sp.set("debug.rsp.watch", "longer").unwrap();
        assert_eq!(watcher.value().unwrap(), Some("longer"));
        fx.sp.set("debug.rsp.watch", "").unwrap();
        assert_eq!(watcher.value().unwrap(), Some(""));
    }

    /// Creation and each later update end a blocked `wait_for_change`; no
    /// change within the timeout is `Ok(false)`.
    #[test]
    fn wait_reports_creation_then_each_update() {
        let mut fx = Fixture::new("watch_wait");
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let mut watcher = reader.watch("debug.rsp.state");
        assert_eq!(watcher.value().unwrap(), None);

        let timeout = Some(Duration::from_secs(5));
        std::thread::scope(|s| {
            let sp = &mut fx.sp;
            let writer = s.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                sp.add("debug.rsp.state", "starting").unwrap();
            });
            assert!(watcher.wait_for_change(timeout).unwrap());
            assert_eq!(watcher.value().unwrap(), Some("starting"));
            writer.join().unwrap();
        });
        std::thread::scope(|s| {
            let sp = &mut fx.sp;
            let writer = s.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                sp.set("debug.rsp.state", "running").unwrap();
            });
            assert!(watcher.wait_for_change(timeout).unwrap());
            assert_eq!(watcher.value().unwrap(), Some("running"));
            writer.join().unwrap();
        });

        let start = Instant::now();
        assert!(!watcher
            .wait_for_change(Some(Duration::from_millis(50)))
            .unwrap());
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert_eq!(watcher.value().unwrap(), Some("running"));
    }

    /// The first `wait_for_change` of a watcher that has read nothing
    /// returns at once for a property that exists, and times out for one
    /// that does not.
    #[test]
    fn first_wait_is_a_creation_wait() {
        let mut fx = Fixture::new("watch_first");
        fx.sp.add("debug.rsp.exists", "1").unwrap();
        let reader = SystemProperties::open(fx.dir()).unwrap();

        let mut exists = reader.watch("debug.rsp.exists");
        assert!(exists.wait_for_change(NO_WAIT).unwrap());
        assert_eq!(exists.value().unwrap(), Some("1"));
        assert!(!exists.wait_for_change(NO_WAIT).unwrap());

        let mut missing = reader.watch("debug.rsp.missing");
        assert!(!missing.wait_for_change(NO_WAIT).unwrap());
        assert_eq!(missing.value().unwrap(), None);
    }

    /// A change is reported once; updates between two calls are one
    /// change with the latest value; an update to the same value is a
    /// change.
    #[test]
    fn changes_are_merged_not_queued() {
        let mut fx = Fixture::new("watch_merge");
        fx.sp.add("debug.rsp.merge", "0").unwrap();
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let mut watcher = reader.watch("debug.rsp.merge");
        assert_eq!(watcher.value().unwrap(), Some("0"));
        assert!(!watcher.wait_for_change(NO_WAIT).unwrap());

        for value in ["1", "2", "3"] {
            fx.sp.set("debug.rsp.merge", value).unwrap();
        }
        assert!(watcher.wait_for_change(NO_WAIT).unwrap());
        assert_eq!(watcher.value().unwrap(), Some("3"));
        assert!(!watcher.wait_for_change(NO_WAIT).unwrap());

        // A change `value` has already read is not reported by the wait.
        fx.sp.set("debug.rsp.merge", "4").unwrap();
        assert_eq!(watcher.value().unwrap(), Some("4"));
        assert!(!watcher.wait_for_change(NO_WAIT).unwrap());

        fx.sp.set("debug.rsp.merge", "4").unwrap();
        assert!(watcher.wait_for_change(NO_WAIT).unwrap());
        assert_eq!(watcher.value().unwrap(), Some("4"));
    }

    /// The dirty store of an update is not a change: the watcher keeps
    /// the value it had and reports the update once, when it completes.
    #[test]
    fn dirty_serial_is_not_a_change() {
        let mut fx = Fixture::new("watch_dirty");
        fx.sp.add("debug.rsp.dirty", "v1").unwrap();
        // Twice, so the backup slot a dirty read takes holds "v1" too.
        fx.sp.set("debug.rsp.dirty", "v1").unwrap();
        fx.sp.set("debug.rsp.dirty", "v1").unwrap();
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let mut watcher = reader.watch("debug.rsp.dirty");
        assert_eq!(watcher.value().unwrap(), Some("v1"));

        let index = fx.set_dirty("debug.rsp.dirty");
        assert_eq!(fx.serial_word(index) & DIRTY, DIRTY);
        // `SystemProperties::wait` reports the dirty store.
        let seen = watcher.seen.as_ref().unwrap();
        let raw = reader.wait(Some(seen.index), seen.serial, NO_WAIT).unwrap();
        assert_eq!(raw, Some(seen.serial | DIRTY));
        // The watcher does not.
        assert!(!watcher
            .wait_for_change(Some(Duration::from_millis(50)))
            .unwrap());
        assert_eq!(watcher.value().unwrap(), Some("v1"));

        // The writer completes an update: one change.
        fx.sp.set("debug.rsp.dirty", "v2").unwrap();
        assert!(watcher.wait_for_change(NO_WAIT).unwrap());
        assert_eq!(watcher.value().unwrap(), Some("v2"));
        assert!(!watcher.wait_for_change(NO_WAIT).unwrap());
    }

    /// A watcher on a `'static` reader can be moved to another thread.
    #[test]
    fn watcher_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PropertyWatcher<'static>>();
    }
}
