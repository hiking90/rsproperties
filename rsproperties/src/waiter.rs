// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The wake source of the async waits (docs/async-wait-set.md 2.3–2.6).
//!
//! A futex has no fd, so a runtime cannot poll it. A [`Waiter`] runs one
//! thread per `SystemProperties` instance that waits on the global serial —
//! every add and update moves it — and wakes every registered [`Waker`] when
//! it changes. The thread reports only that something changed; each future
//! checks its own condition again when it is polled.
//!
//! - The thread starts at the first [`Registration::register`]. A spawn
//!   failure is that call's `Err`, and the next call tries again.
//! - The serial the thread waits from first is read by `register` under the
//!   registry mutex, before the future's re-check. A thread that read it
//!   itself could take in a change made between that re-check and its read,
//!   and never wake for it.
//! - The thread shares the instance's `properties_serial` mapping (an
//!   `Arc`). After a service restart the path names a new file, so opening
//!   it again would wait on a serial the instance does not read.
//! - A futex error other than the retried ones ends the thread: it records
//!   the errno, wakes everyone, and every later `register` returns it.
//!   Retrying the same call would turn the error into a busy loop.
//! - Dropping the [`Waiter`] sets a stop flag that the thread checks each
//!   time its timed futex wait returns. `Drop` does not join, so the thread
//!   and the mapping outlive the instance by at most one slice.
//! - The process-wide reader is never dropped, so its thread has no stop
//!   flag to check and waits without a slice ([`Waiter::never_dropped`]):
//!   it wakes only on a writer's `FUTEX_WAKE`, as bionic's waits do. A
//!   writer that moves the serial without one (bionic's appcompat override
//!   area, API 35–36) is seen at the next change that wakes.
//! - Not supported: a child forked without exec has no thread but a
//!   registry that says one is running, so its async waits are not woken.

// macOS has no futex: `register` fails before any thread code runs.
#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Waker;
use std::time::Duration;

use crate::errors::*;
#[cfg(not(target_os = "macos"))]
use crate::property_area::load_acquire;
use crate::property_area::PropertyAreaMap;
#[cfg(target_os = "macos")]
use crate::system_properties::no_futex;
use crate::system_properties::{futex_wait, Deadline};

/// How long one futex wait of the thread lasts, for a waiter that can be
/// dropped: it bounds the thread's exit after the drop. A writer that
/// wakes is seen at once; one that moves the global serial without
/// `FUTEX_WAKE` is seen when the slice ends, a side effect only.
const SLICE: Duration = Duration::from_secs(1);

pub(crate) struct Waiter {
    shared: Arc<Shared>,
    serial: Arc<PropertyAreaMap>,
    /// `None` once [`Self::never_dropped`]: one untimed futex wait.
    slice: Option<Duration>,
}

struct Shared {
    registry: Mutex<Registry>,
    stop: AtomicBool,
    /// Runs on the thread before its first futex wait.
    #[cfg(test)]
    before_first_wait: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[derive(Default)]
struct Registry {
    thread: Thread,
    /// One entry per registration that has not been woken since it last
    /// registered.
    wakers: Vec<(u64, Waker)>,
    next_id: u64,
    /// The serial the thread was started from.
    #[cfg(test)]
    started_from: Option<u32>,
}

#[derive(Default, Clone, Copy)]
enum Thread {
    #[default]
    NotStarted,
    Running,
    /// The errno of the futex error that ended it.
    Failed(i32),
}

impl Shared {
    /// The registry, also after a panic under the lock: no code holding
    /// it leaves an entry half-written (a `Vec` push or swap, an enum
    /// store).
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes every registered waker. The list is swapped with `spare`
    /// under the lock and woken after it, so a woken future that registers
    /// again does not wait for the others' `wake`, and both vectors keep
    /// their capacity: no allocation per change once they have grown.
    fn wake_all(&self, spare: &mut Vec<(u64, Waker)>) {
        std::mem::swap(&mut self.lock().wakers, spare);
        for (_, waker) in spare.drain(..) {
            waker.wake();
        }
    }

    fn fail(&self, error: &Error, spare: &mut Vec<(u64, Waker)>) {
        let errno = match error {
            Error::Io(e) => e.raw_os_error(),
            _ => None,
        }
        .unwrap_or_else(|| rustix::io::Errno::IO.raw_os_error());
        let mut registry = self.lock();
        registry.thread = Thread::Failed(errno);
        std::mem::swap(&mut registry.wakers, spare);
        drop(registry);
        for (_, waker) in spare.drain(..) {
            waker.wake();
        }
    }
}

impl Waiter {
    /// A waiter on `serial`, the instance's `properties_serial` mapping.
    /// No thread runs until the first registration.
    pub(crate) fn new(serial: Arc<PropertyAreaMap>) -> Self {
        Self::with_slice(serial, SLICE)
    }

    fn with_slice(serial: Arc<PropertyAreaMap>, slice: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                registry: Mutex::new(Registry::default()),
                stop: AtomicBool::new(false),
                #[cfg(test)]
                before_first_wait: Mutex::new(None),
            }),
            serial,
            slice: Some(slice),
        }
    }

    /// Marks the owner as never dropped (the process-wide reader, kept in
    /// a `static`). Its thread has no stop flag to check, so it waits on
    /// the futex without a slice and wakes only on a writer's
    /// `FUTEX_WAKE`: no wakeup while nothing changes. Called before the
    /// first registration, while the owner is not shared yet.
    pub(crate) fn never_dropped(&mut self) {
        self.slice = None;
    }

    /// A registration with nothing registered yet.
    pub(crate) fn registration(&self) -> Registration<'_> {
        Registration {
            waiter: self,
            id: None,
        }
    }

    /// Starts the thread, waiting first from `last`.
    fn spawn(&self, last: u32) -> Result<()> {
        let serial = Arc::clone(&self.serial);
        let shared = Arc::clone(&self.shared);
        let slice = self.slice;
        std::thread::Builder::new()
            .name("rsproperties-waiter".into())
            .spawn(move || run(&serial, &shared, last, slice))
            .map(drop)
            .context_with_location("Failed to start the property waiter thread")
    }

    #[cfg(all(test, feature = "builder", target_os = "linux"))]
    pub(crate) fn registered(&self) -> usize {
        self.shared.lock().wakers.len()
    }

    #[cfg(all(test, feature = "builder", target_os = "linux"))]
    pub(crate) fn started_from(&self) -> Option<u32> {
        self.shared.lock().started_from
    }

    #[cfg(all(test, feature = "builder", target_os = "linux"))]
    fn before_first_wait(&self, hook: impl FnOnce() + Send + 'static) {
        *self.shared.before_first_wait.lock().unwrap() = Some(Box::new(hook));
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// The thread: waits on the global serial from `last` — in timed slices,
/// or untimed when `slice` is `None` — and wakes everyone on each change.
fn run(serial: &PropertyAreaMap, shared: &Shared, mut last: u32, slice: Option<Duration>) {
    #[cfg(test)]
    if let Some(hook) = shared.before_first_wait.lock().unwrap().take() {
        hook();
    }
    let word = serial.property_area().serial();
    let mut spare = Vec::new();
    loop {
        match futex_wait(word, last, &Deadline::after(slice)) {
            // `last` is updated only together with a wake: a future that
            // registered before this change is in the list being woken.
            Ok(Some(new)) => {
                last = new;
                shared.wake_all(&mut spare);
            }
            // Slice expired. `last` is not read again: a change after the
            // expiry makes the next futex call return at once (`EAGAIN`).
            Ok(None) => {}
            Err(e) => {
                shared.fail(&e, &mut spare);
                return;
            }
        }
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
    }
}

/// One future's entry in the registry. Dropping it removes the entry.
pub(crate) struct Registration<'w> {
    waiter: &'w Waiter,
    id: Option<u64>,
}

impl Registration<'_> {
    /// Registers `waker` to be woken at the next change of the global
    /// serial, replacing the one registered before, and starts the thread
    /// if it is not running. The caller checks its condition again after
    /// this returns: a change before the registration has no wake.
    ///
    /// Fails when the thread cannot be started (tried again on the next
    /// call), after the thread ended on a futex error, and on macOS (no
    /// futex).
    pub(crate) fn register(&mut self, waker: &Waker) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let _ = waker;
            return Err(no_futex());
        }
        #[cfg(not(target_os = "macos"))]
        {
            let waiter = self.waiter;
            let mut registry = waiter.shared.lock();
            match registry.thread {
                Thread::Running => {}
                Thread::Failed(errno) => {
                    return Err(Error::Io(std::io::Error::from_raw_os_error(errno)))
                        .context_with_location("The property waiter thread stopped");
                }
                Thread::NotStarted => {
                    // Read under the lock, before the caller's re-check
                    // (module docs).
                    let last = load_acquire(waiter.serial.property_area().serial());
                    waiter.spawn(last)?;
                    registry.thread = Thread::Running;
                    #[cfg(test)]
                    {
                        registry.started_from = Some(last);
                    }
                }
            }
            let id = *self.id.get_or_insert_with(|| {
                let id = registry.next_id;
                registry.next_id += 1;
                id
            });
            match registry.wakers.iter_mut().find(|(i, _)| *i == id) {
                Some((_, registered)) => {
                    if !registered.will_wake(waker) {
                        *registered = waker.clone();
                    }
                }
                None => registry.wakers.push((id, waker.clone())),
            }
            Ok(())
        }
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let Some(id) = self.id else {
            return;
        };
        let mut registry = self.waiter.shared.lock();
        if let Some(at) = registry.wakers.iter().position(|(i, _)| *i == id) {
            registry.wakers.swap_remove(at);
        }
    }
}

#[cfg(all(test, feature = "builder", target_os = "linux"))]
mod tests {
    use std::sync::mpsc;
    use std::sync::{Barrier, Condvar};
    use std::task::Wake;
    use std::time::Instant;

    use super::*;
    use crate::system_properties::test_fixture::Fixture;
    use crate::system_properties::SystemProperties;

    /// A waker that counts its wakes.
    #[derive(Default)]
    struct Flag {
        wakes: Mutex<usize>,
        cv: Condvar,
    }

    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            *self.wakes.lock().unwrap() += 1;
            self.cv.notify_all();
        }
    }

    impl Flag {
        fn waker() -> (Arc<Self>, Waker) {
            let flag = Arc::new(Self::default());
            let waker = Waker::from(Arc::clone(&flag));
            (flag, waker)
        }

        /// Whether a wake arrives within `timeout`.
        fn woken_within(&self, timeout: Duration) -> bool {
            let wakes = self.wakes.lock().unwrap();
            let (wakes, _) = self
                .cv
                .wait_timeout_while(wakes, timeout, |n| *n == 0)
                .unwrap();
            *wakes > 0
        }

        fn wakes(&self) -> usize {
            *self.wakes.lock().unwrap()
        }
    }

    const LONG: Duration = Duration::from_secs(5);

    fn reader(fx: &Fixture) -> SystemProperties {
        SystemProperties::open(fx.dir()).unwrap()
    }

    /// A change after the registration wakes the waker, through the
    /// writer's `FUTEX_WAKE` (the slice is far longer than the wait).
    #[test]
    fn change_after_registration_wakes() {
        let mut fx = Fixture::new("waiter_wake");
        let reader = reader(&fx);
        let waiter = Waiter::with_slice(Fixture::serial_area_of(&reader), Duration::from_secs(60));
        let (flag, waker) = Flag::waker();
        let mut registration = waiter.registration();
        registration.register(&waker).unwrap();
        assert_eq!(waiter.registered(), 1);

        fx.sp.add("debug.rsp.waiter", "1").unwrap();
        assert!(flag.woken_within(LONG));
        // The wake took the entry out; the future registers again.
        assert_eq!(waiter.registered(), 0);
    }

    /// R1: the thread waits from the serial `register` read, not from one
    /// it reads itself. The thread is held before its first futex wait,
    /// the change lands, then the thread goes on: it must see the change.
    #[test]
    fn change_before_the_threads_first_wait_wakes() {
        let mut fx = Fixture::new("waiter_r1");
        let reader = reader(&fx);
        let waiter = Waiter::with_slice(Fixture::serial_area_of(&reader), Duration::from_secs(60));
        let (release, held) = mpsc::channel::<()>();
        waiter.before_first_wait(move || {
            let _ = held.recv();
        });
        let (flag, waker) = Flag::waker();
        let mut registration = waiter.registration();
        registration.register(&waker).unwrap();

        fx.sp.add("debug.rsp.r1", "1").unwrap();
        release.send(()).unwrap();
        assert!(flag.woken_within(LONG));
    }

    /// A writer that moves the global serial without `FUTEX_WAKE` is seen
    /// when a slice ends.
    #[test]
    fn change_without_futex_wake_is_seen_after_a_slice() {
        let fx = Fixture::new("waiter_slice");
        let reader = reader(&fx);
        let slice = Duration::from_millis(50);
        let waiter = Waiter::with_slice(Fixture::serial_area_of(&reader), slice);
        let (flag, waker) = Flag::waker();
        let mut registration = waiter.registration();
        registration.register(&waker).unwrap();

        let start = Instant::now();
        fx.bump_global_serial();
        assert!(flag.woken_within(LONG));
        // Bounded by a slice, with room for a loaded machine.
        assert!(start.elapsed() < slice * 20, "{:?}", start.elapsed());
    }

    /// A never-dropped waiter waits without a slice: a change without
    /// `FUTEX_WAKE` is not seen, the next change that wakes is.
    #[test]
    fn never_dropped_waits_for_a_futex_wake() {
        let mut fx = Fixture::new("waiter_static");
        let reader = reader(&fx);
        let serial = Fixture::serial_area_of(&reader);
        let before = Arc::strong_count(&serial);
        let mut waiter = Waiter::with_slice(Arc::clone(&serial), Duration::from_millis(20));
        waiter.never_dropped();
        let (tid_tx, tid) = mpsc::channel();
        waiter.before_first_wait(move || {
            let _ = tid_tx.send(rustix::thread::gettid().as_raw_nonzero().get());
        });
        let (flag, waker) = Flag::waker();
        let mut registration = waiter.registration();
        registration.register(&waker).unwrap();

        // A change before the thread is in `FUTEX_WAIT` is seen by the
        // kernel's comparison on entry (R1), so wait until it sleeps.
        // Between the hook and the futex wait nothing else blocks it.
        let stat = format!("/proc/self/task/{}/stat", tid.recv_timeout(LONG).unwrap());
        let start = Instant::now();
        loop {
            let stat = std::fs::read_to_string(&stat).unwrap();
            let state = stat.rsplit_once(") ").map(|(_, rest)| rest.as_bytes()[0]);
            if state == Some(b'S') {
                break;
            }
            assert!(start.elapsed() < LONG, "the thread never slept: {stat}");
            std::thread::sleep(Duration::from_millis(1));
        }
        fx.bump_global_serial();
        // 25 of the 20 ms slices it was built with: a timed wait sees it.
        assert!(!flag.woken_within(Duration::from_millis(500)));
        fx.sp.add("debug.rsp.static", "1").unwrap();
        assert!(flag.woken_within(LONG));

        // Not a real owner's path: dropped here so the test leaves no
        // thread. The stop flag is checked after a wake, so wake it.
        drop(registration);
        drop(waiter);
        fx.sp.add("debug.rsp.static.end", "1").unwrap();
        let start = Instant::now();
        while Arc::strong_count(&serial) != before {
            assert!(start.elapsed() < LONG, "the thread did not end");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Dropping a registration removes its entry; registering again
    /// replaces the waker instead of adding one.
    #[test]
    fn registrations_come_and_go() {
        let fx = Fixture::new("waiter_reg");
        let reader = reader(&fx);
        let waiter = Waiter::new(Fixture::serial_area_of(&reader));
        let (_, first_waker) = Flag::waker();
        let (_, second_waker) = Flag::waker();
        let mut first = waiter.registration();
        let mut second = waiter.registration();
        first.register(&first_waker).unwrap();
        first.register(&first_waker).unwrap();
        first.register(&second_waker).unwrap();
        second.register(&second_waker).unwrap();
        assert_eq!(waiter.registered(), 2);
        drop(first);
        assert_eq!(waiter.registered(), 1);
        drop(second);
        assert_eq!(waiter.registered(), 0);
        // A registration that never registered removes nothing.
        drop(waiter.registration());
    }

    /// One change wakes every registered waker, each once.
    #[test]
    fn one_change_wakes_all() {
        let mut fx = Fixture::new("waiter_many");
        let reader = reader(&fx);
        let waiter = Waiter::with_slice(Fixture::serial_area_of(&reader), Duration::from_secs(60));
        let flags: Vec<_> = (0..100).map(|_| Flag::waker()).collect();
        let mut registrations: Vec<_> = flags.iter().map(|_| waiter.registration()).collect();
        for (registration, (_, waker)) in registrations.iter_mut().zip(&flags) {
            registration.register(waker).unwrap();
        }
        fx.sp.add("debug.rsp.many", "1").unwrap();
        for (flag, _) in &flags {
            assert!(flag.woken_within(LONG));
        }
        assert!(flags.iter().all(|(flag, _)| flag.wakes() == 1));
    }

    /// The thread ends within a slice or two of the waiter's drop and
    /// releases its share of the mapping.
    #[test]
    fn thread_ends_after_the_waiter_is_dropped() {
        let fx = Fixture::new("waiter_drop");
        let reader = reader(&fx);
        let serial = Fixture::serial_area_of(&reader);
        let before = Arc::strong_count(&serial);
        let slice = Duration::from_millis(20);
        let waiter = Waiter::with_slice(Arc::clone(&serial), slice);
        let (_, waker) = Flag::waker();
        let mut registration = waiter.registration();
        registration.register(&waker).unwrap();
        // The waiter's share and the thread's.
        assert_eq!(Arc::strong_count(&serial), before + 2);
        drop(registration);
        drop(waiter);

        let start = Instant::now();
        while Arc::strong_count(&serial) != before {
            assert!(start.elapsed() < LONG, "the thread did not end");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The thread's start and a writer race, many times: whenever the
    /// change landed after the serial the thread started from, the waker
    /// is woken.
    #[test]
    fn start_and_change_race() {
        let mut fx = Fixture::new("waiter_race");
        fx.sp.add("debug.rsp.race", "0").unwrap();
        let reader = reader(&fx);
        let mut checked = 0;
        for round in 0..100 {
            let waiter =
                Waiter::with_slice(Fixture::serial_area_of(&reader), Duration::from_millis(20));
            let (flag, waker) = Flag::waker();
            let mut registration = waiter.registration();
            let start = Barrier::new(2);
            std::thread::scope(|s| {
                let sp = &mut fx.sp;
                let start = &start;
                s.spawn(move || {
                    start.wait();
                    sp.set("debug.rsp.race", &round.to_string()).unwrap();
                });
                start.wait();
                registration.register(&waker).unwrap();
            });
            if waiter.started_from() != Some(reader.area_serial()) {
                assert!(flag.woken_within(LONG), "round {round}");
                checked += 1;
            }
        }
        // Not a requirement, but a race that never lands after the start
        // tests nothing; report it instead of passing silently.
        println!("{checked} of 100 rounds changed after the thread started");
    }
}
