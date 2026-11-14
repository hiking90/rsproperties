// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The decisions of the wait methods, apart from the waiting.
//!
//! A [`WaitStep`] reads the current state and either finishes or names the
//! serial word to wait on and the value it must move from. It does not
//! wait: no `FUTEX_WAIT`. It takes the context node's read lock and can
//! open and map a context area on its first lookup, as any read does.
//! [`drive`] is the sync driver: it blocks on the named word with
//! [`SystemProperties::wait_word`] and runs the step again. Keeping the
//! decision in the step lets another driver return under the same
//! conditions.

use crate::errors::*;
use crate::system_properties::{Deadline, PropertyIndex, SystemProperties};

/// What a [`WaitStep`] decided.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step<T> {
    /// The wait is over.
    Done(T),
    /// Not yet.
    Wait(Wait),
}

/// Wait until the word `on` differs from `old`, then step again. `old`
/// is a value the step has just read from that word — except for
/// [`WaitOn::Global`], where it can be a serial sampled before a lookup —
/// so [`SystemProperties::wait_word`] does not read the word again before
/// its futex call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Wait {
    pub(crate) on: WaitOn,
    pub(crate) old: u32,
}

/// The serial word a [`Wait`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitOn {
    /// The global serial, which every add and update moves.
    Global,
    /// The serial of one property.
    Property(PropertyIndex),
}

/// One wait method's decision, run once per wake.
pub(crate) trait WaitStep {
    type Output;

    /// Reads the current state. Called again after every wake, so a step
    /// keeps what it has learnt (a found index, a dirty serial) in `self`.
    fn step(&mut self) -> Result<Step<Self::Output>>;

    /// The sync driver's wake: the word the last [`Wait`] named moved to
    /// `new`. A step whose result is that word returns it here, so the wait
    /// ends without another step — and with the serial the futex wait
    /// reloaded, as bionic `__system_property_wait` returns. `None` (the
    /// default) steps again.
    fn woke(&mut self, new: u32) -> Option<Self::Output> {
        let _ = new;
        None
    }

    /// The instance the named words belong to.
    fn props(&self) -> &SystemProperties;

    /// The property a failed wait is reported for (`Error::Context`), or
    /// `None` to return the error as it is.
    fn name(&self) -> Option<&str>;
}

/// Runs `wait` to completion, blocking between steps: `Ok(Some(out))` when
/// it is done, `Ok(None)` when `deadline` passes first. An
/// `Error::Context` it adds records the caller's location.
#[track_caller]
pub(crate) fn drive<S: WaitStep>(mut wait: S, deadline: &Deadline) -> Result<Option<S::Output>> {
    loop {
        let Wait { on, old } = match wait.step()? {
            Step::Done(out) => return Ok(Some(out)),
            Step::Wait(w) => w,
        };
        let changed = wait.props().wait_word(on, old, deadline);
        let changed = match wait.name() {
            Some(name) => {
                changed.with_context_location(|| format!("waiting for property {name:?}"))?
            }
            None => changed?,
        };
        let Some(new) = changed else {
            return Ok(None);
        };
        if let Some(out) = wait.woke(new) {
            return Ok(Some(out));
        }
    }
}

/// [`SystemProperties::wait`]: done with the current serial once it
/// differs from `old`.
pub(crate) struct SerialWait<'a> {
    props: &'a SystemProperties,
    index: Option<PropertyIndex>,
    old: u32,
}

impl<'a> SerialWait<'a> {
    pub(crate) fn new(props: &'a SystemProperties, index: Option<PropertyIndex>, old: u32) -> Self {
        Self { props, index, old }
    }
}

impl WaitStep for SerialWait<'_> {
    type Output = u32;

    fn step(&mut self) -> Result<Step<u32>> {
        let (on, current) = match self.index {
            None => (WaitOn::Global, self.props.area_serial()),
            Some(index) => (WaitOn::Property(index), self.props.serial_word(index)?),
        };
        Ok(if current != self.old {
            Step::Done(current)
        } else {
            Step::Wait(Wait { on, old: self.old })
        })
    }

    fn woke(&mut self, new: u32) -> Option<u32> {
        Some(new)
    }

    fn props(&self) -> &SystemProperties {
        self.props
    }

    fn name(&self) -> Option<&str> {
        None
    }
}

/// The async [`serial`](crate::asynchronous::SystemProperties::serial):
/// the property's serial once it is clean. The sync `serial` waits for that
/// at most 200 ms and then returns the dirty one; this has no bound (docs
/// 2.8, item 4).
pub(crate) struct CleanSerialWait<'a> {
    props: &'a SystemProperties,
    index: PropertyIndex,
}

impl<'a> CleanSerialWait<'a> {
    pub(crate) fn new(props: &'a SystemProperties, index: PropertyIndex) -> Self {
        Self { props, index }
    }
}

impl WaitStep for CleanSerialWait<'_> {
    type Output = u32;

    fn step(&mut self) -> Result<Step<u32>> {
        /// Bit 0 of a property serial: set during an update.
        const DIRTY: u32 = 1;
        let word = self.props.serial_word(self.index)?;
        Ok(if word & DIRTY == 0 {
            Step::Done(word)
        } else {
            Step::Wait(Wait {
                on: WaitOn::Property(self.index),
                old: word,
            })
        })
    }

    fn props(&self) -> &SystemProperties {
        self.props
    }

    fn name(&self) -> Option<&str> {
        None
    }
}

/// One lookup of `name`: its index, or the global serial sampled before
/// the lookup to wait from. Sampled first, so an add that lands between
/// the lookup and the wait has already moved it.
pub(crate) fn creation(props: &SystemProperties, name: &str) -> Result<Step<PropertyIndex>> {
    let serial = props.area_serial();
    Ok(match props.find(name)? {
        Some(index) => Step::Done(index),
        None => Step::Wait(Wait {
            on: WaitOn::Global,
            old: serial,
        }),
    })
}

/// [`SystemProperties::wait_for_creation`].
pub(crate) struct CreationWait<'a, 'n> {
    props: &'a SystemProperties,
    name: &'n str,
}

impl<'a, 'n> CreationWait<'a, 'n> {
    pub(crate) fn new(props: &'a SystemProperties, name: &'n str) -> Self {
        Self { props, name }
    }
}

impl WaitStep for CreationWait<'_, '_> {
    type Output = PropertyIndex;

    fn step(&mut self) -> Result<Step<PropertyIndex>> {
        creation(self.props, self.name)
    }

    fn props(&self) -> &SystemProperties {
        self.props
    }

    fn name(&self) -> Option<&str> {
        Some(self.name)
    }
}

/// [`SystemProperties::wait_for_value`]: [`creation`] until `name`
/// exists, then the value and its serial from one read.
pub(crate) struct ValueWait<'a, 'n> {
    props: &'a SystemProperties,
    name: &'n str,
    expected: &'n str,
    index: Option<PropertyIndex>,
}

impl<'a, 'n> ValueWait<'a, 'n> {
    pub(crate) fn new(props: &'a SystemProperties, name: &'n str, expected: &'n str) -> Self {
        Self {
            props,
            name,
            expected,
            index: None,
        }
    }
}

impl WaitStep for ValueWait<'_, '_> {
    type Output = ();

    fn step(&mut self) -> Result<Step<()>> {
        let index = match self.index {
            Some(index) => index,
            None => match creation(self.props, self.name)? {
                Step::Done(index) => *self.index.insert(index),
                Step::Wait(w) => return Ok(Step::Wait(w)),
            },
        };
        let expected = self.expected.as_bytes();
        let (matches, serial) = self
            .props
            .read_index_bytes(index, |_, value, serial| (value == expected, serial))?;
        Ok(if matches {
            Step::Done(())
        } else {
            Step::Wait(Wait {
                on: WaitOn::Property(index),
                old: serial,
            })
        })
    }

    fn props(&self) -> &SystemProperties {
        self.props
    }

    fn name(&self) -> Option<&str> {
        Some(self.name)
    }
}

#[cfg(all(test, feature = "builder", target_os = "linux"))]
mod tests {
    use super::*;
    use crate::system_properties::test_fixture::Fixture;

    fn wait(on: WaitOn, old: u32) -> Step<u32> {
        Step::Wait(Wait { on, old })
    }

    /// `wait`'s step names the word the index selects, from `old`, and
    /// takes the woken serial as its result.
    #[test]
    fn serial_step_names_the_word() {
        let mut fx = Fixture::new("step_serial");
        fx.sp.add("debug.rsp.step", "0").unwrap();
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let index = reader.find("debug.rsp.step").unwrap().unwrap();
        let serial = reader.serial(index).unwrap();
        let global = reader.area_serial();

        let mut by_index = SerialWait::new(&reader, Some(index), serial);
        assert_eq!(
            by_index.step().unwrap(),
            wait(WaitOn::Property(index), serial)
        );
        let mut any = SerialWait::new(&reader, None, global);
        assert_eq!(any.step().unwrap(), wait(WaitOn::Global, global));
        assert_eq!(by_index.woke(serial + 2), Some(serial + 2));

        fx.sp.set("debug.rsp.step", "1").unwrap();
        assert_eq!(
            by_index.step().unwrap(),
            Step::Done(reader.serial(index).unwrap())
        );
        assert!(matches!(any.step().unwrap(), Step::Done(g) if g != global));
    }

    /// The value step waits on the global serial until the property
    /// exists, then on the property's serial until the value matches, and
    /// keeps the index it found.
    #[test]
    fn value_step_moves_from_creation_to_value() {
        let mut fx = Fixture::new("step_value");
        let reader = SystemProperties::open(fx.dir()).unwrap();
        let mut value = ValueWait::new(&reader, "debug.rsp.state", "running");

        let global = reader.area_serial();
        assert_eq!(
            value.step().unwrap(),
            Step::Wait(Wait {
                on: WaitOn::Global,
                old: global
            })
        );
        assert_eq!(value.index, None);

        fx.sp.add("debug.rsp.state", "starting").unwrap();
        let step = value.step().unwrap();
        let index = value.index.expect("found by the step");
        assert_eq!(
            step,
            Step::Wait(Wait {
                on: WaitOn::Property(index),
                old: reader.serial(index).unwrap()
            })
        );
        assert_eq!(value.woke(0), None, "a value wait steps again");

        fx.sp.set("debug.rsp.state", "running").unwrap();
        assert_eq!(value.step().unwrap(), Step::Done(()));
    }
}
