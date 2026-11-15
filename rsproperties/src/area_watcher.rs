// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! [`AreaWatcher`]: the global serial followed over time.

use std::time::Duration;

use crate::errors::*;
use crate::system_properties::{wait_forever, Deadline, SystemProperties};
use crate::wait_step::{drive, Step, Wait, WaitOn, WaitStep};

/// Follows the global serial, which every property add and update moves:
/// a wait for the next change to any property, without carrying a serial
/// from call to call. Created by [`SystemProperties::watch_area`].
///
/// The watcher holds the last global serial it reported, starting with the
/// one at its creation. [`Self::wait_for_change`] returns once the serial
/// differs from it, so a change that lands between two calls — after a
/// timeout, or while the caller handled the previous change — returns the
/// next call at once instead of being missed. A loop over
/// [`SystemProperties::wait`] does the same only when it passes back the
/// serial the previous wait returned, not one read again.
///
/// ```rust,no_run
/// use std::time::Duration;
///
/// # fn main() -> rsproperties::Result<()> {
/// let props = rsproperties::system_properties()?;
/// let mut area = props.watch_area();
/// loop {
///     if area.wait_for_change_timeout(Duration::from_secs(30))? {
///         println!("something changed; global serial {}", area.serial());
///     } else {
///         println!("no change for 30 s");
///     }
/// }
/// # }
/// ```
///
/// # What a change is
///
/// - An add or an update of any property of this instance's properties
///   directory, also one that stores the value the property already had.
///   The serial says that something changed, not what: to know which
///   property, read the ones of interest again (a [`crate::PropertyWatcher`]
///   per property reads only those whose serial moved).
/// - Changes are not queued. Changes that land between two calls are one
///   change, and [`Self::serial`] is the latest serial.
///
/// A watcher borrows its `SystemProperties`, so one on the process-wide
/// reader is `AreaWatcher<'static>` and can move to another thread.
pub struct AreaWatcher<'a> {
    props: &'a SystemProperties,
    serial: u32,
}

/// Prints the serial.
impl std::fmt::Debug for AreaWatcher<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AreaWatcher")
            .field("serial", &self.serial)
            .finish_non_exhaustive()
    }
}

impl<'a> AreaWatcher<'a> {
    pub(crate) fn new(props: &'a SystemProperties) -> Self {
        Self {
            props,
            serial: props.area_serial(),
        }
    }

    /// The global serial this watcher last reported: the one at its
    /// creation until the first change.
    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// The instance this watcher reads.
    pub(crate) fn props(&self) -> &'a SystemProperties {
        self.props
    }

    /// Blocks until the global serial differs from [`Self::serial`], then
    /// records the new one. A change made since the watcher was created or
    /// last reported a change returns at once.
    /// [`Self::wait_for_change_timeout`] bounds the wait.
    ///
    /// Failures are those of [`SystemProperties::wait`]; after one the
    /// watcher holds the serial it had. On macOS a call that would have to
    /// block fails (no futex).
    pub fn wait_for_change(&mut self) -> Result<()> {
        wait_forever(|deadline| drive(AreaChangeWait::new(self), deadline))
    }

    /// [`Self::wait_for_change`] bounded by `timeout`: `Ok(true)` on a
    /// change, `Ok(false)` when `timeout` expires first — with the serial
    /// unchanged, so the next call still reports a change that lands
    /// after the expiry. With `Duration::ZERO` the call does not block and
    /// reports whether anything changed since the last report.
    pub fn wait_for_change_timeout(&mut self, timeout: Duration) -> Result<bool> {
        let done = drive(AreaChangeWait::new(self), &Deadline::after(Some(timeout)))?;
        Ok(done.is_some())
    }
}

/// [`AreaWatcher::wait_for_change`]'s [`WaitStep`]. It writes the watcher
/// only when it reports the change, so a wait dropped before that leaves
/// the watcher as it was.
pub(crate) struct AreaChangeWait<'w, 'a> {
    watcher: &'w mut AreaWatcher<'a>,
}

impl<'w, 'a> AreaChangeWait<'w, 'a> {
    pub(crate) fn new(watcher: &'w mut AreaWatcher<'a>) -> Self {
        Self { watcher }
    }
}

impl WaitStep for AreaChangeWait<'_, '_> {
    type Output = ();

    fn step(&mut self) -> Result<Step<()>> {
        let current = self.watcher.props.area_serial();
        Ok(if current != self.watcher.serial {
            self.watcher.serial = current;
            Step::Done(())
        } else {
            Step::Wait(Wait {
                on: WaitOn::Global,
                old: current,
            })
        })
    }

    fn props(&self) -> &SystemProperties {
        self.watcher.props
    }

    fn name(&self) -> Option<&str> {
        None
    }
}

#[cfg(all(test, feature = "builder", target_os = "linux"))]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::system_properties::test_fixture::Fixture;

    const NO_WAIT: Duration = Duration::ZERO;

    /// The watcher starts at the serial of its creation; an add and an
    /// update are each a change; changes between calls are one.
    #[test]
    fn reports_each_change_once() {
        let mut fx = Fixture::new("area_watch");
        fx.sp.add("debug.rsp.area", "0").unwrap();
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let mut area = reader.watch_area();
        assert_eq!(area.serial(), reader.area_serial());
        assert!(!area.wait_for_change_timeout(NO_WAIT).unwrap());

        fx.sp.add("debug.rsp.area.new", "x").unwrap();
        assert!(area.wait_for_change_timeout(NO_WAIT).unwrap());
        assert_eq!(area.serial(), reader.area_serial());
        assert!(!area.wait_for_change_timeout(NO_WAIT).unwrap());

        for value in ["1", "2", "1"] {
            fx.sp.set("debug.rsp.area", value).unwrap();
        }
        assert!(area.wait_for_change_timeout(NO_WAIT).unwrap());
        assert_eq!(area.serial(), reader.area_serial());
        assert!(!area.wait_for_change_timeout(NO_WAIT).unwrap());
    }

    /// A change after a timeout is reported by the next call: the timeout
    /// left the serial as it was.
    #[test]
    fn change_after_a_timeout_is_not_missed() {
        let mut fx = Fixture::new("area_timeout");
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let mut area = reader.watch_area();
        let before = area.serial();

        let start = Instant::now();
        assert!(!area
            .wait_for_change_timeout(Duration::from_millis(50))
            .unwrap());
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert_eq!(area.serial(), before);

        fx.sp.add("debug.rsp.area.late", "1").unwrap();
        assert!(area.wait_for_change_timeout(NO_WAIT).unwrap());
        assert_ne!(area.serial(), before);
    }

    /// `wait_for_change` without a timeout returns on a change made by
    /// another thread while it blocks.
    #[test]
    fn blocking_wait_returns_on_a_change() {
        let mut fx = Fixture::new("area_block");
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let mut area = reader.watch_area();
        std::thread::scope(|s| {
            let sp = &mut fx.sp;
            let writer = s.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                sp.add("debug.rsp.area.block", "1").unwrap();
            });
            area.wait_for_change().unwrap();
            writer.join().unwrap();
        });
        assert_eq!(area.serial(), reader.area_serial());
    }

    /// A step that has not reported leaves the serial as it was.
    #[test]
    fn step_writes_only_on_the_change() {
        let mut fx = Fixture::new("area_step");
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let mut area = reader.watch_area();
        let before = area.serial();
        {
            let mut wait = AreaChangeWait::new(&mut area);
            assert_eq!(
                wait.step().unwrap(),
                Step::Wait(Wait {
                    on: WaitOn::Global,
                    old: before,
                })
            );
        }
        assert_eq!(area.serial(), before);
        fx.sp.add("debug.rsp.area.step", "1").unwrap();
        let mut wait = AreaChangeWait::new(&mut area);
        assert_eq!(wait.step().unwrap(), Step::Done(()));
        assert_ne!(area.serial(), before);
    }

    #[test]
    fn watcher_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AreaWatcher<'static>>();
    }
}
