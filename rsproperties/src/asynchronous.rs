// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The property waits as `async fn`s.
//!
//! [`SystemProperties`] and [`PropertyWatcher`] here are the crate root's
//! types with the same names, with every wait an `async fn` and the same
//! arguments: `props.wait(index, serial)` blocks the thread,
//! `props.as_async().wait(index, serial).await` does not. An async
//! [`SystemProperties`] is a view of a sync one —
//! [`crate::SystemProperties::as_async`] and [`SystemProperties::as_sync`]
//! convert between them for free — so a [`PropertyIndex`] from either
//! works with both. The reads that do not wait are here too, as plain
//! `fn`s.
//!
//! The waits need no runtime: their futures are `Send`, and a waker is all
//! they use, so they run on any executor. `set` (`tokio` feature) talks
//! to the property service on tokio's I/O and time drivers.
//!
//! - **No timeout argument.** Wrap a future in the runtime's timeout
//!   (`tokio::time::timeout`, for one). Dropping the future cancels it.
//! - **One thread per instance.** A futex has no fd an executor could
//!   poll, so the first wait of an instance that has to block starts a
//!   thread that waits on the global serial — every add and update moves
//!   it — and wakes the waiting futures, which check their own condition
//!   again. A wait that is already satisfied returns on its first poll and
//!   starts nothing. The thread of the process-wide instance runs until
//!   the process exits; another instance's thread ends within about a
//!   second of the instance's drop.
//! - **Return conditions** are those of the sync methods, except:
//!   a per-property wait wakes on the global serial, so it returns after
//!   the writer's global `FUTEX_WAKE` rather than the property's; and
//!   [`SystemProperties::serial`] waits for the clean serial without the
//!   sync method's 200 ms bound. A writer that moves the global serial
//!   without `FUTEX_WAKE` — bionic in an appcompat override process on
//!   API 35–36 — is seen at the next change that wakes, as bionic's own
//!   waits see it (`docs/aosp-parity.md` C3).
//! - **fork.** A child forked without exec has no waiter thread, and its
//!   async waits that have to block are never woken. Not supported.
//! - **macOS** has no futex: a wait that has to block fails, as the sync
//!   one does.
//!
//! ```rust,no_run
//! # async fn run() -> rsproperties::Result<()> {
//! let props = rsproperties::asynchronous::system_properties()?;
//! props.wait_for_value("sys.boot_completed", "1").await?;
//!
//! let mut level = props.watch("debug.my_service.log_level");
//! loop {
//!     println!("log level: {:?}", level.value()?);
//!     level.wait_for_change().await?;
//! }
//! # }
//! ```

use std::future::poll_fn;
use std::task::Poll;

use crate::errors::*;
use crate::property_watcher::ChangeWait;
use crate::wait_step::{CleanSerialWait, CreationWait, SerialWait, Step, ValueWait, WaitStep};
use crate::waiter::Waiter;
use crate::PropertyIndex;

/// [`crate::set`] without blocking the thread on the property service:
/// connect, send and the reply wait run on tokio's I/O and time drivers
/// (`tokio` feature).
///
/// The name and value are checked, and the request is built, when this is
/// called; a refused one fails at the `await` with the error
/// [`crate::set`] returns, without a connect. The future owns what it
/// needs (`'static`) and is `Send`, so it can go to `tokio::spawn`.
///
/// Differences from [`crate::set`]:
///
/// - **No bound on the reply wait.** [`crate::set`] gives up after 2 s
///   outside Android (on Android it waits as long as this does). A
///   service whose handler takes longer — a `ctl.*` request, a persistent
///   write — is answered here. Wrap the future in `tokio::time::timeout`
///   to bound it. Connect and send keep their 2 s bounds.
/// - **Cancellation.** Dropping the future before the request is sent in
///   full leaves the property unchanged: the service applies only a
///   complete request. Dropped after that, the service applies it and the
///   outcome is not reported, as after [`crate::set`]'s reply timeout.
///
/// # Panics
///
/// On its first poll, when that is outside a tokio runtime or in one built
/// without its I/O or time driver (`enable_io`, `enable_time`). Calling
/// `set` itself uses no tokio, and tokio has no way to ask whether the
/// time driver is on, so this is not checked beforehand.
///
/// ```rust,no_run
/// # async fn run() -> rsproperties::Result<()> {
/// rsproperties::asynchronous::set("debug.my_service.mode", "fast").await?;
/// rsproperties::asynchronous::set("debug.my_service.workers", 4).await?;
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "tokio")]
pub fn set(
    name: &str,
    value: impl std::fmt::Display,
) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
    crate::system_property_set::tokio_transport::set(name, &value.to_string())
}

/// The process-wide reader ([`crate::system_properties()`]) with the async
/// waits.
pub fn system_properties() -> Result<&'static SystemProperties> {
    crate::system_properties().map(crate::SystemProperties::as_async)
}

/// [`crate::SystemProperties`] with the waits as `async fn`s. Obtained
/// from [`crate::SystemProperties::as_async`] or [`system_properties`];
/// it is the same instance, not a copy.
#[derive(Debug)]
#[repr(transparent)]
pub struct SystemProperties(crate::SystemProperties);

impl SystemProperties {
    /// This instance with the blocking methods.
    pub fn as_sync(&self) -> &crate::SystemProperties {
        &self.0
    }

    /// [`crate::SystemProperties::read_with`].
    pub fn read_with<R, F>(&self, name: &str, f: F) -> Result<R>
    where
        F: FnOnce(&str) -> R,
    {
        self.0.read_with(name, f)
    }

    /// [`crate::SystemProperties::read_index_with`].
    pub fn read_index_with<R, F>(&self, index: PropertyIndex, f: F) -> Result<R>
    where
        F: FnOnce(&str, &str, u32) -> R,
    {
        self.0.read_index_with(index, f)
    }

    /// [`crate::SystemProperties::foreach`].
    pub fn foreach<F>(&self, f: F) -> Result<()>
    where
        F: FnMut(PropertyIndex),
    {
        self.0.foreach(f)
    }

    /// [`crate::SystemProperties::list`].
    pub fn list<F>(&self, f: F) -> Result<()>
    where
        F: FnMut(&str, &str),
    {
        self.0.list(f)
    }

    /// [`crate::SystemProperties::get`].
    pub fn get(&self, name: &str) -> Result<String> {
        self.0.get(name)
    }

    /// [`crate::SystemProperties::find`].
    pub fn find(&self, name: &str) -> Result<Option<PropertyIndex>> {
        self.0.find(name)
    }

    /// [`crate::SystemProperties::area_serial`].
    pub fn area_serial(&self) -> u32 {
        self.0.area_serial()
    }

    /// [`crate::SystemProperties::property_type`].
    #[cfg(feature = "builder")]
    pub fn property_type(&self, name: &str) -> Result<Option<&str>> {
        self.0.property_type(name)
    }

    /// The serial of the property at `index`, once it is clean — the
    /// sync [`crate::SystemProperties::serial`] without its bound: that one
    /// returns the dirty serial after 200 ms of an update in progress,
    /// this one waits until the writer stores the clean serial. Wrap it in
    /// a timeout if the writer may have died mid-update.
    pub async fn serial(&self, index: PropertyIndex) -> Result<u32> {
        drive(self.0.waiter(), CleanSerialWait::new(&self.0, index)).await
    }

    /// [`crate::SystemProperties::wait`]: the property's serial (or, with
    /// `index == None`, the global serial) once it differs from
    /// `old_serial`. A serial that already differs is returned on the
    /// first poll.
    pub async fn wait(&self, index: Option<PropertyIndex>, old_serial: u32) -> Result<u32> {
        drive(self.0.waiter(), SerialWait::new(&self.0, index, old_serial)).await
    }

    /// [`crate::SystemProperties::wait_for_creation`].
    pub async fn wait_for_creation(&self, name: &str) -> Result<PropertyIndex> {
        drive(self.0.waiter(), CreationWait::new(&self.0, name)).await
    }

    /// [`crate::SystemProperties::wait_for_value`].
    pub async fn wait_for_value(&self, name: &str, expected: &str) -> Result<()> {
        drive(self.0.waiter(), ValueWait::new(&self.0, name, expected)).await
    }

    /// [`crate::SystemProperties::watch`], with an async
    /// [`PropertyWatcher::wait_for_change`].
    pub fn watch(&self, name: &str) -> PropertyWatcher<'_> {
        PropertyWatcher(self.0.watch(name))
    }
}

/// [`crate::PropertyWatcher`] with an async
/// [`wait_for_change`](Self::wait_for_change). Created by
/// [`SystemProperties::watch`].
#[derive(Debug)]
pub struct PropertyWatcher<'a>(crate::PropertyWatcher<'a>);

impl PropertyWatcher<'_> {
    /// [`crate::PropertyWatcher::name`].
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// [`crate::PropertyWatcher::value`].
    pub fn value(&mut self) -> Result<Option<&str>> {
        self.0.value()
    }

    /// [`crate::PropertyWatcher::wait_for_change`]. Dropping the future
    /// before it completes leaves the watcher as it was: the next call
    /// waits from the value last read.
    pub async fn wait_for_change(&mut self) -> Result<()> {
        let props = self.0.props();
        drive(props.waiter(), ChangeWait::new(&mut self.0)).await
    }
}

/// The async driver: check, register, check again (docs 2.5). A wait
/// already satisfied returns before the registry is touched; the second
/// check covers a change between the first one and the registration,
/// whose wake went to the futures registered before.
async fn drive<S: WaitStep>(waiter: &Waiter, mut wait: S) -> Result<S::Output> {
    let mut registration = waiter.registration();
    poll_fn(|cx| {
        if let Step::Done(out) = wait.step()? {
            return Poll::Ready(Ok(out));
        }
        let registered = registration.register(cx.waker());
        match wait.name() {
            Some(name) => {
                registered.with_context_location(|| format!("waiting for property {name:?}"))?
            }
            None => registered?,
        }
        match wait.step()? {
            Step::Done(out) => Poll::Ready(Ok(out)),
            Step::Wait(_) => Poll::Pending,
        }
    })
    .await
}

/// Fails the build when a wait's future stops being `Send`: a future of
/// the process-wide reader must be able to go to `tokio::spawn`.
#[allow(dead_code)]
fn futures_are_send(
    props: &'static SystemProperties,
    index: PropertyIndex,
    watcher: &'static mut PropertyWatcher<'static>,
) {
    fn send<T: Send>(_: T) {}
    send(props.serial(index));
    send(props.wait(None, 0));
    send(props.wait_for_creation(""));
    send(props.wait_for_value("", ""));
    send(watcher.wait_for_change());
}

#[cfg(all(test, feature = "builder", target_os = "linux"))]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Wake, Waker};
    use std::time::Duration;

    use super::*;
    use crate::system_properties::test_fixture::Fixture;

    #[derive(Default)]
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Polls `future` by hand with a waker that counts its wakes.
    struct Poller {
        count: Arc<Count>,
        waker: Waker,
    }

    impl Poller {
        fn new() -> Self {
            let count = Arc::new(Count::default());
            let waker = Waker::from(Arc::clone(&count));
            Self { count, waker }
        }

        fn poll<F: Future>(&self, future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
            future.poll(&mut Context::from_waker(&self.waker))
        }

        fn wakes(&self) -> usize {
            self.count.0.load(Ordering::SeqCst)
        }

        /// Waits up to 5 s for a wake.
        fn woken(&self) -> bool {
            let start = std::time::Instant::now();
            while self.wakes() == 0 {
                if start.elapsed() > Duration::from_secs(5) {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            true
        }
    }

    fn reader(fx: &Fixture) -> crate::SystemProperties {
        crate::SystemProperties::open(fx.dir()).unwrap()
    }

    /// No thread runs: no wait of this instance has blocked.
    fn no_thread(props: &crate::SystemProperties) -> bool {
        props.waiter().started_from().is_none()
    }

    /// The rows of docs 2.8 that return without waiting: the first poll
    /// returns what the sync method returns for the same input, and no
    /// thread starts.
    #[test]
    fn first_poll_matches_sync() {
        let mut fx = Fixture::new("async_first");
        fx.sp.add("debug.rsp.async", "v").unwrap();
        let sync = reader(&fx);
        let props = sync.as_async();
        let poller = Poller::new();
        let index = sync.find("debug.rsp.async").unwrap().unwrap();
        let serial = sync.serial(index).unwrap();
        let global = sync.area_serial();

        // `wait(None, old)`, global serial already moved.
        let got = poller.poll(pin!(props.wait(None, global - 1)));
        assert!(matches!(got, Poll::Ready(Ok(g)) if g == global));
        // `wait(Some(i), old)`, dirty serial included: the word as read.
        fx.set_dirty("debug.rsp.async");
        let sync_got = sync
            .wait_timeout(Some(index), serial, Duration::ZERO)
            .unwrap();
        assert_eq!(sync_got, Some(serial | 1));
        let got = poller.poll(pin!(props.wait(Some(index), serial)));
        assert!(matches!(got, Poll::Ready(Ok(s)) if Some(s) == sync_got));
        fx.sp.set("debug.rsp.async", "v").unwrap();
        // `wait_for_creation`, the property exists.
        let got = poller.poll(pin!(props.wait_for_creation("debug.rsp.async")));
        assert!(matches!(got, Poll::Ready(Ok(i)) if i == index));
        // `wait_for_value`, the value matches.
        let got = poller.poll(pin!(props.wait_for_value("debug.rsp.async", "v")));
        assert!(matches!(got, Poll::Ready(Ok(()))));
        // `serial`, the entry is clean.
        let clean = sync.serial(index).unwrap();
        let got = poller.poll(pin!(props.serial(index)));
        assert!(matches!(got, Poll::Ready(Ok(s)) if s == clean));
        // A watcher that has not read the property: its creation.
        let mut watcher = props.watch("debug.rsp.async");
        let got = poller.poll(pin!(watcher.wait_for_change()));
        assert!(matches!(got, Poll::Ready(Ok(()))));
        assert_eq!(watcher.value().unwrap(), Some("v"));

        assert!(no_thread(&sync));
        assert_eq!(poller.wakes(), 0);
    }

    /// An index from another instance fails on the first poll, as the sync
    /// call fails, without registering.
    #[test]
    fn foreign_index_fails_first_poll() {
        let mut fx = Fixture::new("async_foreign");
        fx.sp.add("debug.rsp.foreign", "v").unwrap();
        let sync = reader(&fx);
        let foreign = fx.sp.find("debug.rsp.foreign").unwrap().unwrap();
        let poller = Poller::new();
        let got = poller.poll(pin!(sync.as_async().wait(Some(foreign), 0)));
        assert!(matches!(got, Poll::Ready(Err(Error::InvalidArgument(_)))));
        let got = poller.poll(pin!(sync.as_async().serial(foreign)));
        assert!(matches!(got, Poll::Ready(Err(Error::InvalidArgument(_)))));
        assert!(no_thread(&sync));
    }

    /// Each wait: `Pending` while its condition does not hold, a wake on
    /// the change, `Ready` on the next poll.
    #[test]
    fn pending_then_change_then_ready() {
        let mut fx = Fixture::new("async_cycle");
        fx.sp.add("debug.rsp.cycle", "0").unwrap();
        let sync = reader(&fx);
        let props = sync.as_async();
        let index = sync.find("debug.rsp.cycle").unwrap().unwrap();

        let serial = sync.serial(index).unwrap();
        let poller = Poller::new();
        let mut wait = pin!(props.wait(Some(index), serial));
        assert!(poller.poll(wait.as_mut()).is_pending());
        fx.sp.set("debug.rsp.cycle", "1").unwrap();
        assert!(poller.woken());
        let new = sync.serial(index).unwrap();
        assert!(matches!(poller.poll(wait.as_mut()), Poll::Ready(Ok(s)) if s == new));

        let poller = Poller::new();
        let mut created = pin!(props.wait_for_creation("debug.rsp.later"));
        assert!(poller.poll(created.as_mut()).is_pending());
        fx.sp.add("debug.rsp.later", "x").unwrap();
        assert!(poller.woken());
        let later = sync.find("debug.rsp.later").unwrap().unwrap();
        assert!(matches!(poller.poll(created.as_mut()), Poll::Ready(Ok(i)) if i == later));

        let poller = Poller::new();
        let mut value = pin!(props.wait_for_value("debug.rsp.cycle", "2"));
        assert!(poller.poll(value.as_mut()).is_pending());
        // Another value: woken, still pending.
        fx.sp.set("debug.rsp.cycle", "3").unwrap();
        assert!(poller.woken());
        assert!(poller.poll(value.as_mut()).is_pending());
        fx.sp.set("debug.rsp.cycle", "2").unwrap();
        assert!(matches!(poller.poll(value.as_mut()), Poll::Ready(Ok(()))));

        let poller = Poller::new();
        let mut watcher = props.watch("debug.rsp.cycle");
        assert_eq!(watcher.value().unwrap(), Some("2"));
        {
            let mut change = pin!(watcher.wait_for_change());
            assert!(poller.poll(change.as_mut()).is_pending());
            fx.sp.set("debug.rsp.cycle", "4").unwrap();
            assert!(poller.woken());
            assert!(matches!(poller.poll(change.as_mut()), Poll::Ready(Ok(()))));
        }
        assert_eq!(watcher.value().unwrap(), Some("4"));

        // `serial` of an entry mid-update: the clean serial once it lands.
        let poller = Poller::new();
        fx.set_dirty("debug.rsp.cycle");
        let mut clean = pin!(props.serial(index));
        assert!(poller.poll(clean.as_mut()).is_pending());
        fx.sp.set("debug.rsp.cycle", "5").unwrap();
        assert!(poller.woken());
        let new = sync.serial(index).unwrap();
        assert!(matches!(poller.poll(clean.as_mut()), Poll::Ready(Ok(s)) if s == new));
    }

    /// A watcher whose property is mid-update stays pending — the dirty
    /// serial is not a change, as in the sync watcher — and a future
    /// dropped there leaves the watcher's value as it was.
    #[test]
    fn watcher_dirty_serial_is_pending() {
        let mut fx = Fixture::new("async_dirty");
        fx.sp.add("debug.rsp.dirty", "v1").unwrap();
        fx.sp.set("debug.rsp.dirty", "v1").unwrap();
        fx.sp.set("debug.rsp.dirty", "v1").unwrap();
        let sync = reader(&fx);
        let mut sync_watcher = sync.watch("debug.rsp.dirty");
        let mut watcher = sync.as_async().watch("debug.rsp.dirty");
        assert_eq!(sync_watcher.value().unwrap(), Some("v1"));
        assert_eq!(watcher.value().unwrap(), Some("v1"));

        fx.set_dirty("debug.rsp.dirty");
        assert!(!sync_watcher
            .wait_for_change_timeout(Duration::ZERO)
            .unwrap());
        let poller = Poller::new();
        assert!(poller.poll(pin!(watcher.wait_for_change())).is_pending());
        assert_eq!(watcher.value().unwrap(), Some("v1"));
        assert_eq!(sync.waiter().registered(), 0, "dropped future unregistered");
    }

    /// `as_async` and `as_sync` are views of one instance: same address,
    /// one index space, one waiter.
    #[test]
    fn one_instance_behind_both_types() {
        let mut fx = Fixture::new("async_views");
        fx.sp.add("debug.rsp.view", "0").unwrap();
        let sync = reader(&fx);
        let first = sync.as_async();
        let second = sync.as_async();
        assert!(std::ptr::eq(first.as_sync(), &sync));

        let index = sync.find("debug.rsp.view").unwrap().unwrap();
        let serial = sync.serial(index).unwrap();
        let poller = Poller::new();
        // An index from the sync type on the async one, and back.
        let mut a = pin!(first.wait(Some(index), serial));
        assert!(poller.poll(a.as_mut()).is_pending());
        let async_index = second.find("debug.rsp.view").unwrap().unwrap();
        assert_eq!(sync.serial(async_index).unwrap(), serial);
        let mut b = pin!(second.wait(Some(async_index), serial));
        assert!(poller.poll(b.as_mut()).is_pending());
        assert_eq!(sync.waiter().registered(), 2);
        assert!(sync.waiter().started_from().is_some());
    }
}
