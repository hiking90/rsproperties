// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Linux emulation of the Android system property service.
//!
//! Serves the `property_service` / `property_service_for_system` socket protocol
//! (`PROP_MSG_SETPROP2`) and maintains the property area files on a Linux
//! host, so `rsproperties` clients can `set`, `get` and `wait` without an Android
//! device. It is not usable on Android, where init owns the property service; the
//! crate compiles only for `target_os = "linux"` (on other targets the first error
//! names the reason).
//!
//! The V1 command (`PROP_MSG_SETPROP`), which init still handles, is not
//! supported: such a request sets nothing and gets
//! `PROP_ERROR_INVALID_CMD`. A V1 client does not read that reply and
//! reports success, so clients must send `PROP_MSG_SETPROP2`. The service
//! sets `ro.property_service.version` to `2`, which is what bionic and
//! `rsproperties` clients read to choose the command, and [`run`] fails
//! when a build.prop file sets that property to a value that selects V1.
//!
//! [`run`] starts the service and returns a [`ServiceHandle`]; see there
//! for how the service is stopped.

#![warn(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!(
    "rsproperties-service emulates the Android property service on Linux and builds only for \
     target_os = \"linux\". It is not usable on Android, where init owns the property service; \
     use the `rsproperties` client crate there."
);

use std::path::PathBuf;
use std::sync::Arc;

use rsactor::{Actor, ActorRef, ActorResult};

mod control_reply;
mod persistent_properties;
mod properties_service;
mod property_type;
mod socket_service;

// Compiles the Rust blocks of this crate's README and of the workspace
// README as doctests. The workspace README is attached here and not to
// `rsproperties` because its service example needs this crate and tokio,
// and because a path outside the package would break the doctests of the
// published `rsproperties` crate (this crate is `publish = false`).
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
#[cfg(doctest)]
#[doc = include_str!("../../README.md")]
struct WorkspaceReadmeDoctests;

// The two actors are private: their messages are crate-private, so code
// outside this crate could spawn them but not wait for readiness or send
// a set, and spawning the socket service without the writer lock lets a
// second instance replace a running instance's sockets. [`run`] is the
// only way to start them, and [`ServiceHandle`] the only way to stop them.
pub use control_reply::{ControlReply, ControlReplyProbe};
use properties_service::{PropertiesService, PropertiesServiceArgs};
use socket_service::{
    HandlerCallsDrain, HandlerCallsStop, Handlers, SocketService, SocketServiceArgs,
};

/// The property whose sets are power requests ([`PowerctlMessage`]).
pub(crate) const POWERCTL_PROPERTY: &str = "sys.powerctl";

/// A `ctl.*` set, which AOSP init handles as a control message instead of
/// storing it: `ctl.<action>` = `<target>`, e.g. `ctl.start` =
/// `my_service` (init `HandleControlMessage`). The service passes it to the
/// handler given in [`ServiceOptions::control_handler`], together with the
/// [`ControlReply`] that answers it, and never writes it to a property
/// area.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ControlMessage {
    /// The name after `ctl.`: `start`, `stop`, `restart`,
    /// `interface_start`, `sigstop_on`, ... (not checked against a list).
    pub action: String,
    /// The value: the service (or interface) name to act on.
    pub target: String,
    /// The sender's pid from `SO_PEERCRED`, when available.
    pub pid: Option<i32>,
    /// The sender's uid from `SO_PEERCRED`, when available. The service
    /// checks no sender (any process that can connect may send any
    /// `ctl.*` set), so a handler that must restrict who may control what,
    /// as init does with SELinux in `CheckControlPropertyPerms`, decides
    /// by this and [`gid`](Self::gid).
    pub uid: Option<u32>,
    /// The sender's gid from `SO_PEERCRED`, when available.
    pub gid: Option<u32>,
}

impl ControlMessage {
    /// A message without the sender's uid and gid, e.g. to test a handler;
    /// [`with_uid_gid`](Self::with_uid_gid) adds them.
    ///
    /// ```
    /// use rsproperties_service::{ControlMessage, ControlReply};
    ///
    /// let handler = |msg: ControlMessage, reply: ControlReply| reply.send(msg.action == "start");
    ///
    /// let (reply, probe) = ControlReply::for_test();
    /// handler(ControlMessage::new("start", "my_service", None), reply);
    /// assert_eq!(probe.blocking_answer(), Some(true));
    /// ```
    pub fn new(action: impl Into<String>, target: impl Into<String>, pid: Option<i32>) -> Self {
        Self {
            action: action.into(),
            target: target.into(),
            pid,
            uid: None,
            gid: None,
        }
    }

    /// Sets the sender's uid and gid, which the service reads together
    /// from `SO_PEERCRED`.
    ///
    /// ```
    /// use rsproperties_service::ControlMessage;
    ///
    /// let root_only = |msg: &ControlMessage| msg.uid == Some(0);
    /// let message = ControlMessage::new("stop", "my_service", None);
    /// assert!(!root_only(&message));
    /// assert!(root_only(&message.with_uid_gid(0, 0)));
    /// ```
    #[must_use]
    pub fn with_uid_gid(mut self, uid: u32, gid: u32) -> Self {
        self.uid = Some(uid);
        self.gid = Some(gid);
        self
    }
}

/// The stored form of a `control_handler` (see
/// [`ServiceOptions::control_handler`]).
pub(crate) type ControlHandler = Arc<dyn Fn(ControlMessage, ControlReply) + Send + Sync + 'static>;

/// A `sys.powerctl` set, which AOSP init takes as a shutdown or reboot
/// request instead of storing it: `sys.powerctl` = `<command>`, e.g.
/// `reboot,recovery` or `shutdown,userrequested` (init `PropertySet` →
/// `trigger_shutdown`). The service passes it to the handler given in
/// [`ServiceOptions::powerctl_handler`] and never writes it to a property
/// area.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PowerctlMessage {
    /// The value as it was set. init acts on `shutdown[,<reason>]` and
    /// `reboot[,<target>[,...]]` and logs any other command as
    /// unrecognized; the service passes every value on except
    /// `reboot,userspace`, which it refuses as init does.
    pub command: String,
    /// The sender's pid from `SO_PEERCRED`, when available.
    pub pid: Option<i32>,
    /// The sender's uid from `SO_PEERCRED`, when available. The service
    /// checks no sender, so a handler that must restrict who may shut the
    /// host down decides by this and [`gid`](Self::gid).
    pub uid: Option<u32>,
    /// The sender's gid from `SO_PEERCRED`, when available.
    pub gid: Option<u32>,
}

impl PowerctlMessage {
    /// A message without the sender's uid and gid, e.g. to test a handler;
    /// [`with_uid_gid`](Self::with_uid_gid) adds them.
    ///
    /// ```
    /// use rsproperties_service::PowerctlMessage;
    ///
    /// let is_reboot = |msg: &PowerctlMessage| msg.command.split(',').next() == Some("reboot");
    /// assert!(is_reboot(&PowerctlMessage::new("reboot,recovery", None)));
    /// ```
    pub fn new(command: impl Into<String>, pid: Option<i32>) -> Self {
        Self {
            command: command.into(),
            pid,
            uid: None,
            gid: None,
        }
    }

    /// Sets the sender's uid and gid, which the service reads together
    /// from `SO_PEERCRED`.
    #[must_use]
    pub fn with_uid_gid(mut self, uid: u32, gid: u32) -> Self {
        self.uid = Some(uid);
        self.gid = Some(gid);
        self
    }
}

/// What a [powerctl handler](ServiceOptions::powerctl_handler) did with a
/// `sys.powerctl` command: the two ways init's `HandlePowerctlMessage`
/// ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PowerctlOutcome {
    /// The host is going down on this command. The service stops the
    /// control and powerctl handler calls before it takes the next
    /// request for a handler, exactly as
    /// [`ServiceHandle::stop_handler_calls`] does; init calls
    /// `StopSendingMessages` at this point, for a `shutdown` or `reboot`
    /// command it acts on.
    ShuttingDown,
    /// Nothing is going down: the command is not one the host acts on
    /// (init logs `unrecognized command` and goes on). Later `ctl.*` and
    /// `sys.powerctl` requests reach the handlers as before.
    Ignored,
}

/// The stored form of a `powerctl_handler` (see
/// [`ServiceOptions::powerctl_handler`]).
pub(crate) type PowerctlHandler =
    Arc<dyn Fn(&PowerctlMessage) -> PowerctlOutcome + Send + Sync + 'static>;

/// What [`run`] serves besides the directories in its
/// `rsproperties::PropertyConfig`.
///
/// ```no_run
/// use rsproperties_service::ServiceOptions;
///
/// let options = ServiceOptions::new()
///     .property_contexts_files(["/etc/props/property_contexts"])
///     .build_prop_files(["/etc/props/build.prop"])
///     .persistent_properties_file("/var/lib/props/persistent_properties")
///     .control_handler(|msg, reply| {
///         println!("ctl.{} {}", msg.action, msg.target);
///         reply.send(true);
///     });
/// ```
#[derive(Clone, Default)]
pub struct ServiceOptions {
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
    persistent_properties_file: Option<PathBuf>,
    control_handler: Option<ControlHandler>,
    powerctl_handler: Option<PowerctlHandler>,
}

impl std::fmt::Debug for ServiceOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceOptions")
            .field("property_contexts_files", &self.property_contexts_files)
            .field("build_prop_files", &self.build_prop_files)
            .field(
                "persistent_properties_file",
                &self.persistent_properties_file,
            )
            .field("control_handler", &self.control_handler.is_some())
            .field("powerctl_handler", &self.powerctl_handler.is_some())
            .finish()
    }
}

impl ServiceOptions {
    /// No input files, no persistence, no control handler, no powerctl
    /// handler.
    pub fn new() -> Self {
        Self::default()
    }

    /// `property_contexts` files, parsed in order into `property_info`.
    pub fn property_contexts_files<I, P>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.property_contexts_files = files.into_iter().map(Into::into).collect();
        self
    }

    /// build.prop files, loaded in order (a later file overrides an
    /// earlier one's key).
    ///
    /// A file that cannot be opened or read fails startup. A file whose
    /// mode has the group- or other-write bit is not loaded, as init's
    /// `ReadFile` does: the service logs it at `error` and starts without
    /// that file's properties.
    pub fn build_prop_files<I, P>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.build_prop_files = files.into_iter().map(Into::into).collect();
        self
    }

    /// Stores `persist.*` and `next_boot.*` properties in `path`, in AOSP
    /// init's `persistent_properties` format, and loads them at startup
    /// after the build.prop files (init's `load_persist_props`).
    ///
    /// - The directory of `path` must exist; startup fails otherwise.
    /// - The service takes an exclusive `flock` on `<path>.lock` (mode
    ///   0600) for its lifetime; a second service given the same file
    ///   fails to start (`ErrorKind::WouldBlock`).
    /// - The file is locked and read before `property_info` is published,
    ///   so either failure above leaves the properties dir unchanged. The
    ///   records are applied after the build.prop entries.
    /// - After a transient read error at startup (`EIO`, `EMFILE`, ...),
    ///   the first persistent set reads the file again and keeps its
    ///   records; while it still cannot be read, sets are kept in memory
    ///   and the file is not written.
    /// - The records are kept in memory. A successful set of such a name
    ///   updates that copy and, when the value changed or the previous
    ///   write failed, rewrites the whole file from it before the client
    ///   is answered: a temp file `<path>.tmp` (mode 0600) is
    ///   fsynced and renamed over `path`, then the directory is fsynced.
    ///   A failed write is logged and the set still succeeds, as in init;
    ///   the change stays in memory and goes into the next write. Do not
    ///   edit the file while the service runs: the next write replaces it.
    /// - A file that does not parse is deleted at startup, as init does.
    ///   A group- or world-writable file is not loaded, as init's
    ///   `ReadFile` does; it is not deleted, but the next persistent set
    ///   replaces it.
    ///   Records with a repeated name are merged, keeping the last value.
    ///
    /// Without this option nothing is stored, and `persist.*` values last
    /// only as long as the property areas.
    pub fn persistent_properties_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.persistent_properties_file = Some(path.into());
        self
    }

    /// Handles `ctl.*` sets. `handler` gets each message with the
    /// [`ControlReply`] that answers it: `reply.send(true)` reports
    /// success to the client (`PROP_SUCCESS`), `reply.send(false)` reports
    /// `PROP_ERROR_HANDLE_CONTROL_MESSAGE`, as init does when the service
    /// is unknown or the action fails. Without a handler every `ctl.*` set
    /// is refused with `PROP_ERROR_HANDLE_CONTROL_MESSAGE`. Either way,
    /// `ctl.*` names are never stored.
    ///
    /// The reply may be sent in the call or after it has returned, from
    /// any thread. init answers a control message once its main thread
    /// has handled it; a host that handles the messages in a thread or
    /// task of its own passes the message and the reply on to it and
    /// answers there, when the work is done:
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// use rsproperties_service::ServiceOptions;
    ///
    /// let (requests, mut control_requests) = tokio::sync::mpsc::unbounded_channel();
    /// let options = ServiceOptions::new().control_handler(move |msg, reply| {
    ///     // A send that fails drops the reply, which answers the client
    ///     // with a failure.
    ///     let _ = requests.send((msg, reply));
    /// });
    /// let mut service =
    ///     rsproperties_service::run(rsproperties::PropertyConfig::new(), options).await?;
    ///
    /// loop {
    ///     tokio::select! {
    ///         Some((msg, reply)) = control_requests.recv() => {
    ///             // ... start or stop the process named by `msg.target` ...
    ///             reply.send(msg.action == "start" || msg.action == "stop");
    ///         }
    ///         _ = service.join() => break,
    ///     }
    /// }
    /// service.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// A reply that is dropped without an answer reports a failure, or
    /// success for `ctl.stop` once the handler calls are stopped; see
    /// [`ControlReply`]. A handler that panics drops the reply it was
    /// given, so a panic is reported to the client as a failure.
    ///
    /// # Order
    ///
    /// The calls of this handler and of the
    /// [powerctl handler](Self::powerctl_handler) are made one at a time,
    /// in the order in which init's main thread takes the same requests:
    /// `ctl.*` requests in the order they were read, and a `sys.powerctl`
    /// command before the next `ctl.*` request. Each call runs on tokio's
    /// blocking pool (`spawn_blocking`), so `handler` may block; while it
    /// does, the requests behind it wait for their call.
    ///
    /// The service fixes the order of the calls only. A host that answers
    /// in the call handles the messages one at a time, as init does. A
    /// host that passes the messages on decides the order in which they
    /// are handled and answered itself: a channel read by one task keeps
    /// the order of the calls.
    ///
    /// # Unanswered requests
    ///
    /// A request is unanswered from the time it was read until its reply
    /// has been sent or dropped. A request that finds 102 others
    /// unanswered gets `PROP_ERROR_HANDLE_CONTROL_MESSAGE` without the
    /// handler being called; init refuses a control message when 101 are
    /// queued, whether or not one is being handled, so the two limits
    /// agree while a control message is in its call. Each unanswered
    /// request holds its
    /// client's connection (one fd) and none of the service's connection
    /// slots, so other sets are served meanwhile.
    ///
    /// The service does not limit the time to the reply. A client of
    /// bionic or of `rsproperties` on Android waits until the reply is
    /// sent or dropped; an `rsproperties` client on other targets gives up
    /// after 2 s (`TimedOut`). `handler` is called for a request whose
    /// client has gone, as in init, and an answer to such a client is
    /// discarded.
    ///
    /// # Handling a message as init does
    ///
    /// init's control functions start an action and return; none waits for
    /// a process. For `ctl.stop`, `Service::Stop` records that the service
    /// must not be restarted, sends `SIGKILL` to its process group, sets
    /// `init.svc.<name>` to `stopping` and answers success; the property
    /// becomes `stopped` when the process has been reaped. A service that
    /// is not running is set to `stopped` at once, also with success. An
    /// unknown service and an unknown action are failures. A host that
    /// answers only when the process has exited keeps the client waiting
    /// for that time, which init does not. The crate's README lists the
    /// whole contract ("Answering as init does").
    ///
    /// # Shutdown
    ///
    /// [`ServiceHandle::shutdown`] and [`ServiceHandle::join`] return only
    /// after the call that was running has returned, and so does a [`run`]
    /// that fails to start, for a call made while it was starting.
    /// Requests that were still waiting for their call are refused without
    /// one. A call that never returns keeps the three pending, and a
    /// handler must not wait for any of them to return.
    ///
    /// None of the three waits for a reply that the host holds: such a
    /// reply stays connected to its client and can be answered after the
    /// service has stopped.
    ///
    /// Once one of the three has returned, the service holds no clone of
    /// `handler`: unless the caller kept a clone of the
    /// [`ServiceOptions`], `handler` and what it captured (for example a
    /// channel sender, or a clone of an `Arc`) have been dropped by then.
    pub fn control_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(ControlMessage, ControlReply) + Send + Sync + 'static,
    {
        self.control_handler = Some(Arc::new(handler));
        self
    }

    /// Handles `sys.powerctl` sets, which init takes as shutdown and
    /// reboot requests. A set that passes the checks of every set (name,
    /// `property_contexts` type, value length) is answered with
    /// `PROP_SUCCESS` and passed to `handler`; the value
    /// `reboot,userspace` is refused before that with
    /// `PROP_ERROR_INVALID_VALUE`, as in init (android-17). The value is
    /// never stored, with or without a handler, so a `get` of
    /// `sys.powerctl` finds nothing; init does not write it to its
    /// property area either.
    ///
    /// The reply does not wait for `handler` and does not depend on what
    /// it returns. init answers once it has recorded the command for its
    /// main loop, and answers `PROP_SUCCESS` also for a command it then
    /// logs as unrecognized, so the reply says that the request was
    /// accepted, not that anything was shut down. Without a handler the
    /// set is still answered with `PROP_SUCCESS` and has no effect (a
    /// warning is logged). This differs from `ctl.*`, whose reply is the
    /// result of handling the message.
    ///
    /// `handler` returns what it did with the command
    /// ([`PowerctlOutcome`]), which is init's `HandlePowerctlMessage`:
    /// init acts on `shutdown[,<reason>]` and `reboot[,<target>[,...]]`
    /// and logs any other command as unrecognized. Return
    /// [`PowerctlOutcome::ShuttingDown`] when the host goes down on the
    /// command: the service then stops the handler calls as
    /// [`ServiceHandle::stop_handler_calls`] does, before it takes the
    /// next request for a handler, where init calls
    /// `StopSendingMessages`. Return [`PowerctlOutcome::Ignored`]
    /// otherwise. A panic is logged and counts as `Ignored`.
    ///
    /// The calls of this handler and of the
    /// [control handler](Self::control_handler) are made one at a time, on
    /// tokio's blocking pool (`spawn_blocking`), so `handler` may block; a
    /// command is passed before the next `ctl.*` request. As init does,
    /// the service keeps one command for the handler: a set made before
    /// `handler` was called for the previous one replaces it, and
    /// `handler` is called for the later command only.
    ///
    /// [`ServiceHandle::shutdown`] and [`ServiceHandle::join`] return only
    /// after the call that was running has returned, and after the call
    /// for a command accepted before the service ended, so a handler must
    /// not wait for either of them. To stop the service on a power
    /// request, pass the command to the task that owns the
    /// [`ServiceHandle`] and return, as init's `HandlePowerctlMessage`
    /// only queues the shutdown for its main loop:
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// use rsproperties_service::{PowerctlOutcome, ServiceOptions};
    ///
    /// let (requests, mut power_requests) = tokio::sync::mpsc::unbounded_channel();
    /// let options = ServiceOptions::new().powerctl_handler(move |msg| {
    ///     match msg.command.split(',').next() {
    ///         Some("shutdown" | "reboot") => {
    ///             let _ = requests.send(msg.command.clone());
    ///             // From here on `ctl.stop` is answered with success and
    ///             // no handler is called; other sets are still applied.
    ///             PowerctlOutcome::ShuttingDown
    ///         }
    ///         _ => PowerctlOutcome::Ignored,
    ///     }
    /// });
    /// let mut service =
    ///     rsproperties_service::run(rsproperties::PropertyConfig::new(), options).await?;
    ///
    /// tokio::select! {
    ///     command = power_requests.recv() => {
    ///         println!("power request: {command:?}");
    ///         // ... stop the processes of the host ...
    ///     }
    ///     _ = service.join() => {}
    /// }
    /// service.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// What [`control_handler`](Self::control_handler) says about the time
    /// at which `handler` and what it captured are dropped applies to this
    /// handler too.
    pub fn powerctl_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(&PowerctlMessage) -> PowerctlOutcome + Send + Sync + 'static,
    {
        self.powerctl_handler = Some(Arc::new(handler));
        self
    }
}

pub(crate) struct ReadyMessage;

/// One set that arrived as `PROP_MSG_SETPROP2`.
#[derive(Clone)]
pub(crate) struct PropertyMessage {
    pub name: String,
    pub value: String,
}

// Mask `value` in `Debug` output so log-level captures don't spill
// property contents. Property names are public knowledge (they cross the
// AOSP wire by name and appear in `getprop` output), but values may be
// sensitive — persisted tokens, device identifiers, configuration knobs.
// Logging `value.len()` is enough for diagnostics.
impl std::fmt::Debug for PropertyMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PropertyMessage")
            .field("name", &self.name)
            .field("value", &format_args!("<{} bytes>", self.value.len()))
            .finish()
    }
}

/// One of the service's two actors: its reference, and its task until the
/// task's result has been read.
struct ActorSlot<T: Actor> {
    /// The part's name in [`ServiceError`]'s `Display`.
    name: &'static str,
    actor_ref: ActorRef<T>,
    /// `None` once the task has ended; `failure` then holds what it ended
    /// with.
    join_handle: Option<tokio::task::JoinHandle<ActorResult<T>>>,
    failure: Option<ServiceError>,
}

impl<T: Actor> ActorSlot<T>
where
    T::Error: std::error::Error + Send + Sync + 'static,
{
    fn new(
        name: &'static str,
        actor_ref: ActorRef<T>,
        join_handle: tokio::task::JoinHandle<ActorResult<T>>,
    ) -> Self {
        Self {
            name,
            actor_ref,
            join_handle: Some(join_handle),
            failure: None,
        }
    }

    /// Waits for the actor's task to end and records its failure, if any.
    /// Returns at once when that has already happened.
    ///
    /// Cancel-safe: the only await is on the join handle, and the slot is
    /// changed after it, without another await in between.
    async fn wait(&mut self) {
        let Some(join_handle) = self.join_handle.as_mut() else {
            return;
        };
        let result = join_handle.await;
        self.join_handle = None;
        let cause: Option<Arc<dyn std::error::Error + Send + Sync + 'static>> = match result {
            Ok(result) => result.into_error().map(|e| Arc::new(e) as _),
            Err(join) => Some(Arc::new(join)),
        };
        self.failure = cause.map(|source| ServiceError {
            part: self.name,
            source,
        });
    }
}

/// The running service, returned by [`run`].
///
/// The service runs on the tokio runtime that called [`run`] until one of:
///
/// - [`shutdown`](Self::shutdown) stops it and waits until it has ended;
/// - one of its two parts (the socket service, the properties service)
///   ends by itself, which [`join`](Self::join) reports;
/// - the handle is dropped. This also stops the service, but nothing
///   waits for it: the socket files are unlinked and the properties dir's
///   writer lock is released later, by tasks on the runtime. Control and
///   powerctl handler calls are not waited for either: a `ctl.*` or
///   `sys.powerctl` request is refused once the socket service has
///   ended, which is after the drop returned, and the call that is
///   running then runs to completion. Call
///   [`shutdown`](Self::shutdown) when the next step
///   depends on the files and locks being released (starting the service
///   again on the same directories, for example) or on no handler call
///   running.
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
/// use rsproperties_service::ServiceOptions;
///
/// let config = rsproperties::PropertyConfig::new()
///     .properties_dir("/run/props")
///     .socket_dir("/run/props/sockets");
/// let mut service = rsproperties_service::run(config, ServiceOptions::new()).await?;
///
/// tokio::select! {
///     _ = tokio::signal::ctrl_c() => {}
///     // The service ended by itself; `shutdown` below returns the same
///     // result.
///     _ = service.join() => {}
/// }
/// service.shutdown().await?;
/// # Ok(())
/// # }
/// ```
#[must_use = "dropping the handle stops the service"]
pub struct ServiceHandle {
    socket: ActorSlot<SocketService>,
    properties: ActorSlot<PropertiesService>,
    /// Waits for the task that makes the control and powerctl handler
    /// calls; returns at once when the service has neither handler.
    handler_calls: HandlerCallsDrain,
    /// Set by [`Self::stop_handler_calls`].
    handler_stop: HandlerCallsStop,
}

impl ServiceHandle {
    /// Ends the control and powerctl handler calls while the service keeps
    /// running. This is init's `StopSendingMessages`, which init calls
    /// once it has begun a shutdown or reboot. A
    /// [powerctl handler](ServiceOptions::powerctl_handler) that returns
    /// [`PowerctlOutcome::ShuttingDown`] has the same effect; call this
    /// when the host goes down by another way than a `sys.powerctl` set (a
    /// signal, for example) and the processes that `ctl.*` requests act on
    /// are being stopped.
    ///
    /// A request the service reads after this has returned is answered as
    /// init answers it during its shutdown, and reaches no handler:
    ///
    /// - `ctl.stop`: `PROP_SUCCESS` ("If we're already shutting down and
    ///   you're asking us to stop something, just say we did"). Also
    ///   without a control handler;
    /// - every other `ctl.*` action, `ctl.interface_stop` included:
    ///   `PROP_ERROR_HANDLE_CONTROL_MESSAGE`;
    /// - `sys.powerctl`: checked as before (`reboot,userspace` is
    ///   `PROP_ERROR_INVALID_VALUE`), then `PROP_SUCCESS`;
    /// - any other name: applied as before.
    ///
    /// The `PROP_SUCCESS` for `ctl.stop` is sent without anything having
    /// been stopped. In init it holds because init kills every service
    /// during shutdown; here the caller must be stopping those processes.
    ///
    /// This cannot be undone, and a second call changes nothing. The
    /// handler call that is running is not affected, and at most one more
    /// begins: the one the service was about to make when this was
    /// called. [`shutdown`](Self::shutdown) waits for both. A `ctl.*`
    /// request that was waiting for its call gets the reply listed above
    /// without one, and a `sys.powerctl` command that was waiting is
    /// dropped. init leaves the control messages still in its queue
    /// unanswered once it has entered shutdown, so their clients wait
    /// until the device goes down; here they are answered at once.
    ///
    /// A [`ControlReply`] that the host got from an earlier call is the
    /// host's to answer. One that the host drops from now on answers as
    /// listed above (`PROP_SUCCESS` for `ctl.stop`, a failure for every
    /// other action), so a host that is going down can drop the requests
    /// it has not handled; [`ControlReply::send`] still answers what it is
    /// given.
    ///
    /// The `ctl.*` replies stay as listed after
    /// [`shutdown`](Self::shutdown) or a drop of the handle, for the
    /// connections accepted before it; without this call those get the
    /// refusals `shutdown` documents. A set that is read once the
    /// properties service has ended, `sys.powerctl` included, is not
    /// checked and gets `PROP_ERROR_SET_FAILED` either way.
    pub fn stop_handler_calls(&self) {
        self.handler_stop.stop();
    }

    /// Waits until the service has ended.
    ///
    /// Returns when one of the two parts has ended by itself — after the
    /// other one has been stopped and has ended too, and after the control
    /// and powerctl handler calls have returned (as for
    /// [`shutdown`](Self::shutdown)),
    /// so the socket files are unlinked, the writer lock is released and
    /// no handler call is running by then. Nothing ends a part in normal
    /// operation; this returns only after a failure, or never.
    ///
    /// Cancel-safe, so it can be an arm of `tokio::select!`: a cancelled
    /// call loses nothing, and the next call (or
    /// [`shutdown`](Self::shutdown)) continues from where it stopped. Once
    /// the service has ended, every call returns the same result at once.
    ///
    /// # Errors
    ///
    /// The failure the service ended with; see [`ServiceError`].
    pub async fn join(&mut self) -> Result<(), ServiceError> {
        // A slot whose task was already joined returns at once, so this
        // also covers a call made after the service has ended.
        tokio::select! {
            () = self.socket.wait() => {}
            () = self.properties.wait() => {}
        }
        self.stop_and_wait().await
    }

    /// Stops the service and waits until it has ended: the socket service
    /// first (both socket files are unlinked), then the properties service
    /// (the writer lock and the persistent properties file's lock are
    /// released). Sets already queued for the properties service are
    /// applied before it ends; a connection whose request had not been
    /// queued by then gets an error reply or is closed. A `ctl.*` request
    /// read after the socket service has ended is refused with
    /// `PROP_ERROR_HANDLE_CONTROL_MESSAGE`, and a `sys.powerctl` request
    /// with `PROP_ERROR_SET_FAILED`: the service has ended, which says
    /// nothing about the processes a handler acts on. After
    /// [`stop_handler_calls`](Self::stop_handler_calls) the `ctl.*`
    /// replies are the ones listed there instead, and so is the
    /// `sys.powerctl` reply until the properties service has ended; a
    /// `sys.powerctl` request read after that still gets
    /// `PROP_ERROR_SET_FAILED`.
    ///
    /// Last, this waits for the handler call that is running when the
    /// socket service ends, and for the powerctl handler call of a
    /// `sys.powerctl` set that was answered with `PROP_SUCCESS` before
    /// that. `ctl.*` requests that were still waiting for their call are
    /// refused with `PROP_ERROR_HANDLE_CONTROL_MESSAGE` without one. When
    /// this returns, no handler call is running, none begins afterwards,
    /// and the service has dropped both handlers (see
    /// [`ServiceOptions::control_handler`]). A [`ControlReply`] that the
    /// host got from a call and has not answered is not waited for: it
    /// keeps its client's connection and can be answered or dropped after
    /// this has returned. There is no timeout, so a
    /// call that does not return keeps this pending (init runs its
    /// control functions on its main thread and does not time them out
    /// either). Dropping the returned future before it completes is
    /// dropping the handle at that point: a part that has not ended is
    /// stopped without being waited for, and a handler call that has not
    /// returned keeps running. During the last wait both parts have ended,
    /// so the socket files are unlinked and the locks are released.
    ///
    /// The property area files and `property_info` stay in the properties
    /// dir, so readers that mapped them keep working; they see no further
    /// changes.
    ///
    /// # Errors
    ///
    /// The failure the service ended with, if it had already failed; see
    /// [`ServiceError`]. Stopping a healthy service returns `Ok(())`.
    pub async fn shutdown(mut self) -> Result<(), ServiceError> {
        self.stop_and_wait().await
    }

    /// Stops both actors and waits for both tasks. The socket service is
    /// stopped and joined first, so no connection is accepted after the
    /// properties service got its stop. The connection tasks are detached
    /// and are not waited for: one that reads its request later has it
    /// refused — by the properties service's closed mailbox, or, for
    /// `ctl.*` and for a `sys.powerctl` set the properties service still
    /// accepted, by the handler queue that the socket service closes when
    /// it is dropped (joining its task drops it). The client then gets an
    /// error reply.
    ///
    /// The handler worker is waited for last
    /// ([`HandlerCallsDrain::wait`]); it ends after the call it was
    /// making when the queue was closed. The refusal is in effect before
    /// that wait starts, and the wait comes after both tasks so that a
    /// call that does not return leaves the socket files unlinked and the
    /// locks released. The worker does not hold the `ControlReply` of a
    /// call that has returned, so the replies the host keeps are outside
    /// this wait.
    ///
    /// Cancel-safe: `ActorRef::stop` only enqueues a signal and can be
    /// repeated, also for an actor that has ended, and
    /// [`ActorSlot::wait`] and [`HandlerCallsDrain::wait`] are cancel-safe.
    async fn stop_and_wait(&mut self) -> Result<(), ServiceError> {
        self.socket.actor_ref.stop().await;
        self.socket.wait().await;
        self.properties.actor_ref.stop().await;
        self.properties.wait().await;
        self.handler_calls.wait().await;
        // Both set: the socket service's failure is the one reported, as
        // in `run`.
        match self
            .socket
            .failure
            .as_ref()
            .or(self.properties.failure.as_ref())
        {
            Some(failure) => Err(failure.clone()),
            None => Ok(()),
        }
    }
}

impl std::fmt::Debug for ServiceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceHandle")
            .field("socket_service_alive", &self.socket.actor_ref.is_alive())
            .field(
                "properties_service_alive",
                &self.properties.actor_ref.is_alive(),
            )
            .finish_non_exhaustive()
    }
}

/// Error returned by [`ServiceHandle::join`] and
/// [`ServiceHandle::shutdown`]: one of the service's two parts ended with a
/// failure after [`run`] had returned.
///
/// Neither part returns an error once it has started, so in this version
/// the cause ([`source`](std::error::Error::source)) is a panic in that
/// part's task. `Display` prints `<part> failed: <cause>`, where `<part>`
/// is `socket service` or `properties service`.
///
/// `Clone`, because [`ServiceHandle::join`] returns the same failure on
/// every call.
#[derive(Debug, Clone)]
pub struct ServiceError {
    part: &'static str,
    source: Arc<dyn std::error::Error + Send + Sync + 'static>,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed: {}", self.part, self.source)
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// The startup step of [`run`] that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartupStage {
    /// `rsproperties::init` rejected the config: a process-global directory
    /// was already set to a different path.
    Config,
    /// Taking the properties dir's writer lock
    /// ([`rsproperties::AreaWriterLock::acquire`]) failed. A second
    /// instance on the same properties dir fails here, with
    /// `rsproperties::Error::Lock` as the source.
    WriterLock,
    /// The properties service failed to start: loading the persistent
    /// properties file (its directory is missing: `NotFound`; it is used
    /// by another service: `WouldBlock`; `<path>.lock` cannot be opened,
    /// `fchmod`ed or `flock`ed for another reason, such as `EACCES` or
    /// `ENOLCK`: that errno's kind, with the lock path in the message),
    /// reading the `property_contexts` or build.prop files, publishing
    /// `property_info`, or creating the property areas — in that order, so
    /// a persistent-file failure leaves the properties dir unchanged. The
    /// source is a `std::io::Error`.
    PropertiesService,
    /// The socket service failed to start (socket dir or socket files).
    /// The source is an `rsproperties::Error`.
    SocketService,
}

impl std::fmt::Display for StartupStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Config => "configuration (init)",
            Self::WriterLock => "properties dir writer lock",
            Self::PropertiesService => "properties service",
            Self::SocketService => "socket service",
        })
    }
}

/// Error returned by [`run`]: the step that failed ([`stage`](Self::stage))
/// and its cause ([`source`](std::error::Error::source)).
///
/// The cause is the failed actor's own `on_start` error when it has one,
/// so `source()` can be downcast to it (`std::io::Error` for
/// [`StartupStage::PropertiesService`], `rsproperties::Error` for the other
/// stages). When an actor stopped without an error, the cause is a message
/// saying so.
///
/// `Display` prints `<stage> failed to start: <cause>`.
#[derive(Debug)]
pub struct StartupError {
    stage: StartupStage,
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl StartupError {
    pub(crate) fn new(
        stage: StartupStage,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self {
            stage,
            source: source.into(),
        }
    }

    /// The startup step that failed.
    pub fn stage(&self) -> StartupStage {
        self.stage
    }

    /// The cause, by value.
    pub fn into_source(self) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self.source
    }
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed to start: {}", self.stage, self.source)
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Starts the property service with the given configuration and returns
/// once it is ready for clients.
///
/// The service keeps running on the calling tokio runtime after `run`
/// returns. The returned [`ServiceHandle`] stops it
/// ([`ServiceHandle::shutdown`]) and reports a failure after startup
/// ([`ServiceHandle::join`]); dropping the handle stops the service too.
///
/// # Startup order
///
/// 1. `rsproperties::init(config)`.
/// 2. The properties dir's writer lock
///    ([`rsproperties::AreaWriterLock::acquire`]); this creates a missing
///    properties dir (the last component only). A second instance on the
///    same properties dir fails here with [`StartupStage::WriterLock`]
///    and `rsproperties::Error::Lock` as the source, before it replaces
///    `property_info` or touches any socket.
/// 3. Both actors are spawned together; the properties service gets the
///    lock and holds it for its lifetime. It reads every
///    `property_contexts` and build.prop file before it writes
///    `property_info` or any area file.
/// 4. `run` waits for the socket service, then for the properties service,
///    and returns when both are ready. If either fails, both are stopped
///    and `run` returns only after both actors have ended (the socket
///    service's final names are unlinked by then).
///
/// One service instance per socket dir: two instances with different
/// properties dirs and the same socket dir are not supported (the writer
/// lock does not cover the socket dir).
///
/// # Requests that arrive before the property areas exist
///
/// `ctl.*` sets are not queued: they go to the control handler as soon as
/// they are read, before the areas exist, and are not undone if startup
/// fails. The rest of this section is about every other set, including
/// `sys.powerctl`: it waits like any other set, and the powerctl handler
/// is called once the properties service has checked it, so not before
/// the areas exist and not when startup fails.
///
/// The socket service can be accepting connections while the properties
/// service is still reading its input files and creating the areas, so a
/// client that does not wait for `run` to return can send a `set` in that
/// window. Such a request is not rejected: it waits (in the properties
/// service's mailbox) and is applied once the areas exist, after the
/// build.prop entries and the persistent properties (so an early
/// `persist.*` set replaces the stored value), and only then is the client
/// answered. The socket
/// service closes the connection without a reply after 10 s (up to 20 s
/// when all 64 handler slots are taken and the connection first waits for
/// one). A request already queued in the mailbox then is still applied once
/// the areas exist, like one whose client gave up on its own reply timeout;
/// one that was not yet queued (mailbox full) is dropped. If the properties
/// service fails to start,
/// the request is not applied and `run` returns
/// [`StartupStage::PropertiesService`]. A SETPROP2 client whose connection
/// was already accepted gets an error reply; one that connected after the
/// socket service's last accept sees the connection reset or refused
/// instead. A `get` in that window opens whatever is in the properties dir
/// at that moment (nothing, or a previous run's files). The first `get`
/// that opens the store fixes that process's reader for the process
/// lifetime; a `get` that finds no `property_info` or `properties_serial`
/// fails, and the next one tries again (see
/// `rsproperties::system_properties`).
///
/// # Requirements
/// The parent of the properties dir must exist: only the properties dir's
/// last component is created (mode 0711 minus the umask's bits). A missing
/// socket dir is created with any missing parent (mode 0755 minus the
/// umask's bits); see "Socket files" below.
///
/// # Socket files
///
/// Socket files are created as `socket(SOCK_CLOEXEC|SOCK_NONBLOCK)`,
/// `fchmod(fd, 0660)`, `bind` at `<socket_dir>/.<name>.tmp-<pid>`. The
/// kernel creates the file with the socket's mode minus the umask, so from
/// the moment the path exists its mode is a subset of 0660. If the umask
/// removed any of those bits (e.g. group write under umask 022), the mode
/// is raised to exactly 0660 through an
/// `O_PATH|O_NOFOLLOW` descriptor opened relative to the socket dir
/// (`chmod("/proc/thread-self/fd/N")`, or `/proc/self/fd/N` on kernels
/// before 3.17; both need `/proc`) after `fstat`
/// confirms a socket owned by the service's euid. Only then does the
/// socket `listen`; before that a `connect` fails with `ECONNREFUSED`.
/// Both temp sockets are renamed onto their final names only after both
/// are listening, and a failed start unlinks what it created, final names
/// included. Both final names are unlinked when the service ends (see
/// [`ServiceHandle`]).
///
/// The socket dir is opened once at startup; every unlink, rename and
/// chmod is made relative to that descriptor or to a descriptor whose
/// inode was checked with `fstat`, so the service changes no existing
/// inode outside that directory. It changes only entries named
/// `property_service`, `property_service_for_system`, or
/// `.<either name>.tmp-<pid>` there (the startup sweep removes only
/// sockets with those prefixes), and never follows a symlink at those
/// names. `bind` itself takes a path: if the socket dir's path is swapped
/// after startup opened it, the temp socket is created at the new
/// location, startup fails, and that socket is left there.
///
/// Out of scope: any uid other than the service's euid and root that can
/// write to the socket dir or any of its ancestors can replace the sockets
/// with its own and so impersonate the service; nothing in the service can
/// prevent that. Keep the socket dir and its ancestors writable only by
/// the service's user and root. A world-writable socket dir without the
/// sticky bit is reported with a warning at startup; this is not an access
/// check. A default ACL on the socket dir gives its named users and groups
/// access beyond the 0660 mode bits; the service does not inspect or
/// remove ACLs.
///
/// # Failure semantics
/// `rsproperties::init` commits process-global, first-write-wins state. If
/// a later startup step fails, that state stays committed (a `OnceLock`
/// cannot be un-set): retrying `run` in the same process with the same
/// directories passes [`StartupStage::Config`], and with *different*
/// directories fails there with "already initialized".
///
/// [`StartupError`] is `Send + Sync` so the returned future can be
/// `tokio::spawn`ed and the error converted into `anyhow::Error` or
/// `Box<dyn Error + Send + Sync>`.
///
/// The socket service accepts requests while the properties service is
/// still starting, so a [control handler](ServiceOptions::control_handler)
/// can be called before `run` returns. A failed `run` returns its error
/// after those calls have returned, as [`ServiceHandle::shutdown`] does,
/// and without a timeout.
///
/// # Properties set at startup
///
/// After the build.prop entries, as AOSP init does: `ro.property_service.version`
/// = `2` (unless a build.prop file already set it; `run` fails, before it
/// writes to the properties dir, when that value is not a number of 2 or
/// more, because clients would then send the V1 command, which the
/// service refuses), then the records of
/// [`ServiceOptions::persistent_properties_file`] (each checked like a
/// client set), then `ro.persistent_properties.ready` = `true` — also
/// without a persistent properties file, since init always sets it.
///
/// A staged `next_boot.<name>` record leaves the persistent properties
/// file when the file is loaded: the load rewrites the file without it,
/// before the property areas are created and before any record is set
/// (init writes the file first too). A `<name>` that starts with
/// `persist.` or `next_boot.` is written back under that name, so a later
/// start still finds it. Any other `<name>` is not written back, and its
/// value exists only as the set made later in the same startup. That
/// value is lost when `run` fails after the rewrite, at whatever step
/// (for example a `property_contexts` or build.prop file that cannot be
/// read), or when the set is rejected by the checks of a client set.
pub async fn run(
    config: rsproperties::PropertyConfig,
    options: ServiceOptions,
) -> Result<ServiceHandle, StartupError> {
    // `init` fails when a global directory already holds a *different*
    // path (earlier service instance, a property read that latched the
    // default first): starting anyway would bind the service to the
    // previous directories while the caller believes the config took
    // effect.
    rsproperties::init(config).map_err(|e| StartupError::new(StartupStage::Config, e))?;

    // The writer lock is taken before either actor is spawned: a second
    // instance on the same properties dir fails here, before it can rename
    // its sockets over the running instance's sockets or replace
    // `property_info`. (Spawning the socket service only after the
    // properties service is ready would also protect the sockets, but
    // would change the 0.6.0 startup order; the lock is enough.)
    let dir = rsproperties::properties_dir();
    let lock = tokio::task::spawn_blocking(move || rsproperties::AreaWriterLock::acquire(dir))
        .await
        .map_err(|e| StartupError::new(StartupStage::WriterLock, e))?
        .map_err(|e| StartupError::new(StartupStage::WriterLock, e))?;

    let ServiceOptions {
        property_contexts_files,
        build_prop_files,
        persistent_properties_file,
        control_handler,
        powerctl_handler,
    } = options;
    let (properties_ref, properties_join) = properties_service::spawn(PropertiesServiceArgs {
        property_contexts_files,
        build_prop_files,
        writer_lock: lock,
        persistent_properties_file,
    });
    let handlers = Handlers::new(control_handler, powerctl_handler);
    let handler_calls = handlers.drain();
    let handler_stop = handlers.stop_switch();
    let (socket_ref, socket_join) = socket_service::spawn(SocketServiceArgs {
        socket_dir: rsproperties::socket_dir().to_path_buf(),
        properties_service: properties_ref.clone(),
        handlers,
    });

    // Sequential readiness checks: the socket service is checked first, so
    // when both fail the socket failure is the one reported. Either failure
    // branch waits for both actors to end, so a socket failure is reported
    // only after the properties service's `on_start` has returned.
    // On failure, stop both actors explicitly instead of leaving them to
    // the implicit drop of their references.
    //
    // A failed `ask` only carries rsactor's mailbox error; the `on_start`
    // error that actually stopped the actor is delivered solely through its
    // join handle (rsactor's own log line for it goes to `tracing`, which
    // is not bridged to `log` here). Await the handle to return that cause.
    if let Err(e) = socket_ref.ask(ReadyMessage).await {
        socket_ref.stop().await;
        properties_ref.stop().await;
        let err = startup_error(StartupStage::SocketService, e, socket_join).await;
        // `stop` only enqueues the signal; the properties service sees it
        // after its `on_start` (input files, `property_info`, areas) ends.
        // Wait for that, so nothing writes the properties dir after `run`
        // returned.
        let _ = properties_join.await;
        // The socket service may have been listening before the failure,
        // so a `ctl.*` request may have reached the control handler. Its
        // task was joined above, which closed the handler queue (the
        // actor's `Drop`, or the drop of its unused `args`), so the worker
        // ends after the call it is making: no handler call is running or
        // begins once `run` has returned its error. The same holds for
        // the other failure branch below.
        handler_calls.wait().await;
        return Err(err);
    }
    if let Err(e) = properties_ref.ask(ReadyMessage).await {
        socket_ref.stop().await;
        properties_ref.stop().await;
        let err = startup_error(StartupStage::PropertiesService, e, properties_join).await;
        // The socket service passed `on_start` and is listening on both
        // final names; it handles the stop on its next loop iteration, and
        // dropping its join result drops the actor, which unlinks both
        // names. Wait for that, so a retry on the same socket dir does not
        // race this instance's unlink.
        let _ = socket_join.await;
        handler_calls.wait().await;
        return Err(err);
    }

    Ok(ServiceHandle {
        socket: ActorSlot::new("socket service", socket_ref, socket_join),
        properties: ActorSlot::new("properties service", properties_ref, properties_join),
        handler_calls,
        handler_stop,
    })
}

/// Builds the [`StartupError`] for a failed actor: the cause is the
/// actor's own failure (normally its `on_start` error) when the join
/// handle has one, the `JoinError` when the task panicked or was
/// cancelled, else a message built from the `ask` error.
async fn startup_error<T: Actor>(
    stage: StartupStage,
    ask_error: rsactor::Error,
    join_handle: tokio::task::JoinHandle<ActorResult<T>>,
) -> StartupError
where
    T::Error: std::error::Error + Send + Sync + 'static,
{
    match join_handle.await {
        Ok(result) => match result.into_error() {
            Some(cause) => StartupError::new(stage, cause),
            None => StartupError::new(
                stage,
                format!("actor stopped before replying Ready: {ask_error}"),
            ),
        },
        Err(join) => StartupError::new(stage, join),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::time::{Duration, Instant};

    use rsproperties::wire::{
        PROP_ERROR_HANDLE_CONTROL_MESSAGE, PROP_ERROR_INVALID_VALUE, PROP_ERROR_SET_FAILED,
        PROP_MSG_SETPROP2, PROP_SUCCESS,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    use crate::socket_service::tests::{socket_suffix_len, socket_test_dir};

    /// `Debug` must not print the value (value-masking policy).
    #[test]
    fn test_property_message() {
        let msg = PropertyMessage {
            name: "test.key".to_string(),
            value: "test.value".to_string(),
        };
        let debug = format!("{msg:?}");
        assert!(!debug.contains("test.value"), "value leaked: {debug}");
        assert!(debug.contains("<10 bytes>"), "length missing: {debug}");
        assert!(debug.contains("test.key"), "name missing: {debug}");
    }

    /// `run` must return the actor's own `on_start` error, not rsactor's
    /// generic mailbox error, tagged with the stage that failed. The only
    /// test that calls `run` in this process (`init` is
    /// process-global); the early-request tests below call it in a child
    /// process.
    #[tokio::test]
    async fn run_reports_on_start_cause() {
        let dir = socket_test_dir("rsprops_svc_run_err", socket_suffix_len("sockets"));
        let _ = std::fs::remove_dir_all(&dir);
        let socket_dir = dir.join("sockets");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let missing = dir.join("missing_property_contexts");

        let config = rsproperties::PropertyConfig::new()
            .properties_dir(dir.clone())
            .socket_dir(socket_dir);
        let result = run(
            config,
            ServiceOptions::new().property_contexts_files([missing]),
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);

        let err = match result {
            Ok(_) => panic!("run must fail for a missing property_contexts file"),
            Err(e) => e,
        };
        assert_eq!(err.stage(), StartupStage::PropertiesService, "{err}");
        let source = std::error::Error::source(&err).expect("StartupError has a source");
        assert!(
            source.downcast_ref::<std::io::Error>().is_some(),
            "expected the on_start io::Error, got: {source}"
        );
        let shown = err.to_string();
        assert!(
            shown.starts_with("properties service failed to start: "),
            "stage missing: {shown}"
        );
        assert!(
            shown.contains("missing_property_contexts"),
            "cause must name the missing file: {shown}"
        );
    }

    /// Env var that makes a re-executed test binary run the child half of
    /// the test it names.
    const CHILD_ENV: &str = "RSPROPS_SVC_LIB_CHILD";

    /// Whether this process is the child started by [`run_in_child`] for
    /// `test`.
    fn is_child(test: &str) -> bool {
        std::env::var(CHILD_ENV).is_ok_and(|v| v == test)
    }

    /// Runs `test` (its full path in this binary) in a new process of this
    /// test binary, so it can call `run`, and thus `init`, with its own
    /// dirs. Panics unless the child ran exactly that test and it passed.
    fn run_in_child(test: &str) {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env(CHILD_ENV, test)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child {test} failed ({}):\n{stdout}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Runs `body` on a fresh runtime and shuts the runtime down without
    /// waiting for blocking tasks: a failed assertion can leave the
    /// properties service blocked in `open` on the FIFO, and dropping the
    /// runtime would wait for that task forever.
    fn block_on_child(body: impl std::future::Future<Output = ()>) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(body)));
        rt.shutdown_background();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    /// Temp dir removed on drop.
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    type RunResult = Result<ServiceHandle, StartupError>;

    /// A `run` whose properties service is blocked before it creates the
    /// areas, plus a client connection that already sent a SETPROP2
    /// request.
    struct EarlyRequest {
        dir: TempDir,
        fifo: PathBuf,
        run: tokio::task::JoinHandle<RunResult>,
        stream: UnixStream,
    }

    /// Starts `run` with a FIFO as its only `property_contexts` file: the
    /// properties service blocks in `open` until [`release_fifo`] opens
    /// the write end, so it cannot create the areas before that. Connects
    /// to `property_service` once the socket service has bound it, sends
    /// `debug.early.set=1`, and checks that no reply has arrived and that
    /// `property_info` does not exist yet.
    async fn send_request_while_init_blocked(
        label: &str,
        build_prop_files: Vec<PathBuf>,
    ) -> EarlyRequest {
        let dir = TempDir(socket_test_dir(
            &format!("rsprops_svc_{label}"),
            socket_suffix_len("sockets"),
        ));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        let fifo = dir.0.join("property_contexts.fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let socket_dir = dir.0.join("sockets");
        let config = rsproperties::PropertyConfig::new()
            .properties_dir(dir.0.clone())
            .socket_dir(socket_dir.clone());
        let run_task = tokio::spawn(run(
            config,
            ServiceOptions::new()
                .property_contexts_files([fifo.clone()])
                .build_prop_files(build_prop_files),
        ));

        let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match UnixStream::connect(&socket).await {
                Ok(stream) => break stream,
                Err(e) => {
                    assert!(
                        Instant::now() < deadline,
                        "the socket service must accept connections while the properties \
                         service is still starting: {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        };

        let (name, value) = ("debug.early.set", "1");
        let mut frame = Vec::new();
        frame.extend_from_slice(&PROP_MSG_SETPROP2.to_ne_bytes());
        frame.extend_from_slice(&(name.len() as u32).to_ne_bytes());
        frame.extend_from_slice(name.as_bytes());
        frame.extend_from_slice(&(value.len() as u32).to_ne_bytes());
        frame.extend_from_slice(value.as_bytes());
        stream.write_all(&frame).await.unwrap();

        // The properties service is still blocked on the FIFO, so nothing
        // may answer yet. (Not a timing assumption: no reply can exist
        // before `release_fifo`; the 200 ms only bounds how long we look.)
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stream.readable())
                .await
                .is_err(),
            "a request sent before the areas exist must not be answered before they do"
        );
        assert!(
            !dir.0.join("property_info").exists(),
            "the properties service must still be blocked before it publishes property_info"
        );

        EarlyRequest {
            dir,
            fifo,
            run: run_task,
            stream,
        }
    }

    /// Opens and closes the FIFO's write end: the blocked reader then reads
    /// EOF, i.e. an empty `property_contexts` file. A non-blocking write
    /// open fails with `ENXIO` until the reader has the FIFO open, so this
    /// retries until it does.
    async fn release_fifo(fifo: &Path) {
        use std::os::unix::fs::OpenOptionsExt;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as _)
                .open(fifo)
            {
                Ok(_writer) => return,
                Err(e)
                    if e.raw_os_error() == Some(rustix::io::Errno::NXIO.raw_os_error())
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("cannot open the write end of {fifo:?}: {e}"),
            }
        }
    }

    /// Reads the 4-byte status reply; `None` when the connection closed
    /// without one.
    async fn read_reply(stream: &mut UnixStream) -> Option<i32> {
        let mut buf = [0u8; 4];
        match tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf)).await {
            Ok(Ok(_)) => Some(i32::from_ne_bytes(buf)),
            Ok(Err(_)) => None,
            Err(_) => panic!("no reply within 10 s after the properties service was released"),
        }
    }

    /// Pins the documented behavior for a `set` that arrives while the
    /// properties service is still creating the areas: it waits, is applied
    /// once the areas exist, and is answered with success. Whether the
    /// request waited in the properties service's mailbox or was still
    /// unread in the socket when init finished is not controlled; both give
    /// this client-visible result.
    #[test]
    fn set_before_areas_exist_is_applied_after_init() {
        const TEST: &str = "tests::set_before_areas_exist_is_applied_after_init";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let mut early = send_request_while_init_blocked("early_set_ok", vec![]).await;
            release_fifo(&early.fifo).await;
            assert_eq!(read_reply(&mut early.stream).await, Some(PROP_SUCCESS));

            let service = match early.run.await.unwrap() {
                Ok(service) => service,
                Err(e) => panic!("run failed: {e}"),
            };
            assert_eq!(
                rsproperties::get::<String>("debug.early.set").unwrap(),
                "1",
                "the early set must be applied to the areas"
            );
            service.shutdown().await.unwrap();
            drop(early.dir);
        });
    }

    /// Pins the behavior when the properties service fails after a `set`
    /// arrived: the set is not applied (the client gets `PROP_ERROR`, or a
    /// closed connection if the socket service is stopped before it
    /// accepts the connection) and `run` reports
    /// [`StartupStage::PropertiesService`].
    #[test]
    fn set_before_failed_init_is_not_applied() {
        const TEST: &str = "tests::set_before_failed_init_is_not_applied";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let missing = std::env::temp_dir().join(format!(
                "rsprops_svc_early_set_missing_{}.prop",
                std::process::id()
            ));
            let mut early = send_request_while_init_blocked("early_set_fail", vec![missing]).await;
            release_fifo(&early.fifo).await;
            let reply = read_reply(&mut early.stream).await;
            assert_ne!(
                reply,
                Some(PROP_SUCCESS),
                "a set must not succeed when init fails"
            );

            let err = match early.run.await.unwrap() {
                Ok(_) => panic!("run must fail for a missing build.prop file"),
                Err(e) => e,
            };
            assert_eq!(err.stage(), StartupStage::PropertiesService, "{err}");
            assert!(
                err.to_string().contains("rsprops_svc_early_set_missing_"),
                "cause must name the missing file: {err}"
            );
            drop(early.dir);
        });
    }

    /// When the properties service fails after the socket service is
    /// listening, `run` must not return before the socket service has
    /// stopped and unlinked its final names; otherwise a retry on the same
    /// socket dir races that unlink. `run` is awaited directly on a
    /// current-thread runtime, so no other task runs between its return
    /// and the checks.
    #[test]
    fn properties_failure_returns_after_sockets_are_unlinked() {
        const TEST: &str = "tests::properties_failure_returns_after_sockets_are_unlinked";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let dir = TempDir(socket_test_dir(
                "rsprops_svc_prop_fail",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let contexts = dir.0.join("property_contexts");
            std::fs::write(&contexts, "").unwrap();
            let socket_dir = dir.0.join("sockets");
            let config = rsproperties::PropertyConfig::new()
                .properties_dir(dir.0.clone())
                .socket_dir(socket_dir.clone());

            let result = run(
                config,
                ServiceOptions::new()
                    .property_contexts_files([contexts])
                    .build_prop_files([dir.0.join("missing.prop")]),
            )
            .await;
            let err = match result {
                Ok(_) => panic!("run must fail for a missing build.prop file"),
                Err(e) => e,
            };
            assert_eq!(err.stage(), StartupStage::PropertiesService, "{err}");
            for name in [
                rsproperties::PROPERTY_SERVICE_SOCKET_NAME,
                rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME,
            ] {
                assert!(
                    !socket_dir.join(name).exists(),
                    "{name} must be unlinked before run returns"
                );
            }
        });
    }

    /// A persistent properties file whose directory is missing fails
    /// startup before `property_info` or any area is written.
    #[test]
    fn missing_persistent_dir_fails_before_properties_dir_changes() {
        const TEST: &str = "tests::missing_persistent_dir_fails_before_properties_dir_changes";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let dir = TempDir(socket_test_dir(
                "rsprops_svc_persist_nodir",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let config = rsproperties::PropertyConfig::new()
                .properties_dir(dir.0.clone())
                .socket_dir(socket_dir.clone());

            let result = run(
                config,
                ServiceOptions::new().persistent_properties_file(
                    dir.0.join("missing").join("persistent_properties"),
                ),
            )
            .await;
            let err = match result {
                Ok(_) => panic!("run must fail for a missing persistent properties directory"),
                Err(e) => e,
            };
            assert_eq!(err.stage(), StartupStage::PropertiesService, "{err}");
            assert!(
                !dir.0.join("property_info").exists(),
                "property_info must not be published before the persistent file is loaded"
            );
            assert!(!dir.0.join("properties_serial").exists());
        });
    }

    /// A set sent as a V1 frame (`PROP_MSG_SETPROP`) is refused with
    /// `PROP_ERROR_INVALID_CMD` and reaches neither the property area nor
    /// the persistent properties file; the same kind of set as a V2 frame
    /// reaches both. init applies a V1 set to the area.
    #[test]
    fn v1_set_is_refused_and_not_applied() {
        use rsproperties::wire::{
            PROP_ERROR_INVALID_CMD, PROP_MSG_SETPROP, PROP_NAME_MAX, PROP_VALUE_MAX,
        };

        const TEST: &str = "tests::v1_set_is_refused_and_not_applied";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let dir = TempDir(socket_test_dir(
                "rsprops_svc_persist_v1",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let file = dir.0.join("persistent_properties");
            let config = rsproperties::PropertyConfig::new()
                .properties_dir(dir.0.join("props"))
                .socket_dir(socket_dir.clone());
            let service = run(
                config,
                ServiceOptions::new().persistent_properties_file(file.clone()),
            )
            .await
            .unwrap();
            let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);

            for name in ["persist.v1.name", "debug.v1.name"] {
                let mut v1 = PROP_MSG_SETPROP.to_ne_bytes().to_vec();
                for (field, len) in [(name, PROP_NAME_MAX), ("a", PROP_VALUE_MAX)] {
                    let mut buf = vec![0u8; len];
                    buf[..field.len()].copy_from_slice(field.as_bytes());
                    v1.extend_from_slice(&buf);
                }
                let mut stream = UnixStream::connect(&socket).await.unwrap();
                stream.write_all(&v1).await.unwrap();
                assert_eq!(
                    read_reply(&mut stream).await,
                    Some(PROP_ERROR_INVALID_CMD),
                    "{name}"
                );
            }

            let (name, value) = ("persist.v2.name", "b");
            let mut v2 = PROP_MSG_SETPROP2.to_ne_bytes().to_vec();
            v2.extend_from_slice(&(name.len() as u32).to_ne_bytes());
            v2.extend_from_slice(name.as_bytes());
            v2.extend_from_slice(&(value.len() as u32).to_ne_bytes());
            v2.extend_from_slice(value.as_bytes());
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            stream.write_all(&v2).await.unwrap();
            assert_eq!(read_reply(&mut stream).await, Some(PROP_SUCCESS));

            // The V2 set above was handled after both V1 requests had
            // been answered, so a V1 set that had been applied would be
            // visible here.
            for name in ["persist.v1.name", "debug.v1.name"] {
                assert!(
                    rsproperties::get::<String>(name).is_err(),
                    "{name} must not be set by a V1 frame"
                );
            }
            assert_eq!(rsproperties::get::<String>("persist.v2.name").unwrap(), "b");
            assert_eq!(
                crate::persistent_properties::decode(&std::fs::read(&file).unwrap()).unwrap(),
                vec![(b"persist.v2.name".to_vec(), b"b".to_vec())],
                "only the V2 set may be in the file"
            );
            service.shutdown().await.unwrap();
        });
    }

    /// Whether both final socket names exist in `socket_dir`.
    fn sockets_exist(socket_dir: &Path) -> bool {
        [
            rsproperties::PROPERTY_SERVICE_SOCKET_NAME,
            rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME,
        ]
        .iter()
        .all(|name| socket_dir.join(name).exists())
    }

    /// Whether neither final socket name exists in `socket_dir` and the
    /// writer lock of `properties_dir` can be taken (it is released again).
    fn service_is_gone(properties_dir: &Path, socket_dir: &Path) -> bool {
        let unlinked = [
            rsproperties::PROPERTY_SERVICE_SOCKET_NAME,
            rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME,
        ]
        .iter()
        .all(|name| !socket_dir.join(name).exists());
        unlinked && rsproperties::AreaWriterLock::acquire(properties_dir).is_ok()
    }

    /// The three ways a running service ends, each followed by a new `run`
    /// on the same directories:
    /// - `shutdown` returns after the sockets are unlinked and the writer
    ///   lock is released;
    /// - `join` returns once one actor has ended, after stopping the other
    ///   one, and returns the same result when called again;
    /// - dropping the handle stops both actors without waiting.
    #[test]
    fn service_handle_shutdown_join_and_drop() {
        const TEST: &str = "tests::service_handle_shutdown_join_and_drop";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let dir = TempDir(socket_test_dir(
                "rsprops_svc_handle",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let start = || {
                run(
                    rsproperties::PropertyConfig::new()
                        .properties_dir(dir.0.clone())
                        .socket_dir(socket_dir.clone()),
                    ServiceOptions::new(),
                )
            };

            // shutdown
            let service = start().await.expect("first run");
            assert!(sockets_exist(&socket_dir));
            assert!(
                rsproperties::AreaWriterLock::acquire(&dir.0).is_err(),
                "the running service holds the writer lock"
            );
            let shown = format!("{service:?}");
            assert!(shown.contains("socket_service_alive: true"), "{shown}");
            service
                .shutdown()
                .await
                .expect("shutdown of a healthy service");
            assert!(
                service_is_gone(&dir.0, &socket_dir),
                "shutdown must return after the sockets are unlinked and the lock is released"
            );

            // join: nothing has ended, so it must not return.
            let mut service = start().await.expect("run after shutdown");
            assert!(
                tokio::time::timeout(Duration::from_millis(200), service.join())
                    .await
                    .is_err(),
                "join must wait while both actors run"
            );
            assert!(sockets_exist(&socket_dir), "a cancelled join stops nothing");
            // One actor ends by itself; join stops the other one.
            service.properties.actor_ref.stop().await;
            tokio::time::timeout(Duration::from_secs(10), service.join())
                .await
                .expect("join must return once the properties service has ended")
                .expect("a stopped actor is not a failure");
            assert!(
                service_is_gone(&dir.0, &socket_dir),
                "join must return after both actors have ended"
            );
            tokio::time::timeout(Duration::from_secs(1), service.join())
                .await
                .expect("join after the end returns at once")
                .unwrap();
            service.shutdown().await.unwrap();

            // drop
            let service = start().await.expect("run after join");
            assert!(sockets_exist(&socket_dir));
            drop(service);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !service_is_gone(&dir.0, &socket_dir) {
                assert!(
                    Instant::now() < deadline,
                    "dropping the handle must stop the service"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
    }

    /// A `ctl.*` request that a connection accepted before `shutdown`
    /// sends after `shutdown` has returned must not reach the control
    /// handler: the connection's task is detached and outlives the socket
    /// service, and `ctl.*` does not pass through the properties service,
    /// whose closed mailbox refuses every other set.
    #[test]
    fn ctl_request_read_after_shutdown_is_refused() {
        const TEST: &str = "tests::ctl_request_read_after_shutdown_is_refused";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let dir = TempDir(socket_test_dir(
                "rsprops_svc_ctl_stop",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let service = run(
                rsproperties::PropertyConfig::new()
                    .properties_dir(dir.0.clone())
                    .socket_dir(socket_dir.clone()),
                ServiceOptions::new().control_handler(move |_, reply| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    reply.send(true);
                }),
            )
            .await
            .expect("run");
            let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);

            let (name, value) = ("ctl.start", "some_service");
            let mut frame = PROP_MSG_SETPROP2.to_ne_bytes().to_vec();
            frame.extend_from_slice(&(name.len() as u32).to_ne_bytes());
            frame.extend_from_slice(name.as_bytes());
            frame.extend_from_slice(&(value.len() as u32).to_ne_bytes());
            frame.extend_from_slice(value.as_bytes());

            // Connects first and sends nothing yet.
            let mut late = UnixStream::connect(&socket).await.unwrap();
            // Connects second, on the same listener, and gets its reply:
            // the listener hands out connections in order, so `late` has
            // been accepted and its task spawned by now.
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            stream.write_all(&frame).await.unwrap();
            assert_eq!(read_reply(&mut stream).await, Some(PROP_SUCCESS));
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            service.shutdown().await.unwrap();

            // A write to a connection the service already closed fails
            // (`EPIPE`), which counts as no reply.
            let reply = match late.write_all(&frame).await {
                Ok(()) => read_reply(&mut late).await,
                Err(_) => None,
            };
            assert!(
                matches!(reply, None | Some(PROP_ERROR_HANDLE_CONTROL_MESSAGE)),
                "a ctl.* request read after shutdown must be refused or dropped, got {reply:?}"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the control handler must not be called after shutdown returned"
            );
        });
    }

    /// The test's side of [`blocking_handler`].
    struct BlockedCalls {
        /// One value per handler call, sent when the call begins.
        entered: tokio::sync::mpsc::UnboundedReceiver<()>,
        /// Each value sent lets one call return; dropping it lets all
        /// return.
        release: std::sync::mpsc::Sender<()>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        returned: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// A control handler whose calls block until the test releases them,
    /// and then answer `true` before they return.
    fn blocking_handler() -> (
        impl Fn(ControlMessage, ControlReply) + Send + Sync + 'static,
        BlockedCalls,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (entered_tx, entered) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let calls = Arc::new(AtomicUsize::new(0));
        let returned = Arc::new(AtomicUsize::new(0));
        let (calls_in, returned_in) = (calls.clone(), returned.clone());
        let handler = move |_: ControlMessage, reply: ControlReply| {
            calls_in.fetch_add(1, Ordering::SeqCst);
            let _ = entered_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
            returned_in.fetch_add(1, Ordering::SeqCst);
            reply.send(true);
        };
        (
            handler,
            BlockedCalls {
                entered,
                release,
                calls,
                returned,
            },
        )
    }

    /// A SETPROP2 frame for `name` = `value`.
    fn set_frame(name: &str, value: &str) -> Vec<u8> {
        let mut frame = PROP_MSG_SETPROP2.to_ne_bytes().to_vec();
        frame.extend_from_slice(&(name.len() as u32).to_ne_bytes());
        frame.extend_from_slice(name.as_bytes());
        frame.extend_from_slice(&(value.len() as u32).to_ne_bytes());
        frame.extend_from_slice(value.as_bytes());
        frame
    }

    /// A SETPROP2 frame for `ctl.start` = `some_service`.
    fn ctl_start_frame() -> Vec<u8> {
        set_frame("ctl.start", "some_service")
    }

    /// A `sys.powerctl` set sent over the socket reaches the powerctl
    /// handler with its value and the sender's pid, uid and gid, and is
    /// not stored.
    /// `reboot,userspace` is refused without a handler call, and so is a
    /// request that a connection accepted before `shutdown` sends after
    /// `shutdown` has returned.
    #[test]
    fn powerctl_set_reaches_the_handler_and_is_not_stored() {
        const TEST: &str = "tests::powerctl_set_reaches_the_handler_and_is_not_stored";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let dir = TempDir(socket_test_dir(
                "rsprops_svc_powerctl",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let (requests, mut power_requests) = tokio::sync::mpsc::unbounded_channel();
            let service = run(
                rsproperties::PropertyConfig::new()
                    .properties_dir(dir.0.clone())
                    .socket_dir(socket_dir.clone()),
                ServiceOptions::new().powerctl_handler(move |msg| {
                    let _ = requests.send(msg.clone());
                    PowerctlOutcome::Ignored
                }),
            )
            .await
            .expect("run");
            let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
            let set = |value: &'static str| {
                let socket = socket.clone();
                async move {
                    let mut stream = UnixStream::connect(&socket).await.unwrap();
                    let frame = set_frame(POWERCTL_PROPERTY, value);
                    stream.write_all(&frame).await.unwrap();
                    read_reply(&mut stream).await
                }
            };

            // Connects first and sends nothing yet; accepted before the
            // connections of the sets below.
            let mut late = UnixStream::connect(&socket).await.unwrap();

            assert_eq!(set("reboot,recovery").await, Some(PROP_SUCCESS));
            let message = tokio::time::timeout(Duration::from_secs(10), power_requests.recv())
                .await
                .expect("the handler call must begin")
                .unwrap();
            // The sender is this process.
            assert_eq!(
                message,
                PowerctlMessage::new("reboot,recovery", Some(std::process::id() as i32))
                    .with_uid_gid(
                        rustix::process::getuid().as_raw(),
                        rustix::process::getgid().as_raw()
                    )
            );
            assert_eq!(
                set("reboot,userspace").await,
                Some(PROP_ERROR_INVALID_VALUE)
            );
            assert!(
                rsproperties::get::<String>(POWERCTL_PROPERTY).is_err(),
                "sys.powerctl must not be stored"
            );

            service.shutdown().await.unwrap();

            // A write to a connection the service already closed fails
            // (`EPIPE`), which counts as no reply.
            let reply = match late
                .write_all(&set_frame(POWERCTL_PROPERTY, "reboot"))
                .await
            {
                Ok(()) => read_reply(&mut late).await,
                Err(_) => None,
            };
            assert!(
                matches!(reply, None | Some(PROP_ERROR_SET_FAILED)),
                "a sys.powerctl request read after shutdown must be refused or dropped, got \
                 {reply:?}"
            );
            assert!(
                power_requests.try_recv().is_err(),
                "only the accepted set made before shutdown may reach the handler"
            );
        });
    }

    /// After `stop_handler_calls` the service answers as init does during
    /// its shutdown: `ctl.stop` succeeds, other `ctl.*` actions are
    /// refused, `sys.powerctl` is accepted, and no handler is called;
    /// other sets are still applied. The replies stay the same for a
    /// request that a connection accepted before `shutdown` sends after
    /// `shutdown` has returned.
    #[test]
    fn stop_handler_calls_answers_as_init_during_shutdown() {
        const TEST: &str = "tests::stop_handler_calls_answers_as_init_during_shutdown";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let dir = TempDir(socket_test_dir(
                "rsprops_svc_stop_calls",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let control_calls = Arc::new(AtomicUsize::new(0));
            let powerctl_calls = Arc::new(AtomicUsize::new(0));
            let (control_counter, powerctl_counter) =
                (control_calls.clone(), powerctl_calls.clone());
            let service = run(
                rsproperties::PropertyConfig::new()
                    .properties_dir(dir.0.clone())
                    .socket_dir(socket_dir.clone()),
                ServiceOptions::new()
                    .control_handler(move |_, reply| {
                        control_counter.fetch_add(1, Ordering::SeqCst);
                        reply.send(true);
                    })
                    .powerctl_handler(move |_| {
                        powerctl_counter.fetch_add(1, Ordering::SeqCst);
                        PowerctlOutcome::Ignored
                    }),
            )
            .await
            .expect("run");
            let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
            let set = |name: &'static str, value: &'static str| {
                let socket = socket.clone();
                async move {
                    let mut stream = UnixStream::connect(&socket).await.unwrap();
                    stream.write_all(&set_frame(name, value)).await.unwrap();
                    read_reply(&mut stream).await
                }
            };

            // Connects first and sends nothing yet; accepted before the
            // connections of the sets below.
            let mut late = UnixStream::connect(&socket).await.unwrap();

            assert_eq!(set("ctl.start", "svc").await, Some(PROP_SUCCESS));
            assert_eq!(control_calls.load(Ordering::SeqCst), 1);

            service.stop_handler_calls();
            service.stop_handler_calls();

            assert_eq!(set("ctl.stop", "svc").await, Some(PROP_SUCCESS));
            assert_eq!(
                set("ctl.start", "svc").await,
                Some(PROP_ERROR_HANDLE_CONTROL_MESSAGE)
            );
            assert_eq!(set(POWERCTL_PROPERTY, "reboot").await, Some(PROP_SUCCESS));
            assert_eq!(
                set(POWERCTL_PROPERTY, "reboot,userspace").await,
                Some(PROP_ERROR_INVALID_VALUE)
            );
            assert_eq!(set("debug.after.stop", "1").await, Some(PROP_SUCCESS));
            assert_eq!(
                rsproperties::get::<String>("debug.after.stop").unwrap(),
                "1",
                "other sets must still be applied"
            );

            service.shutdown().await.unwrap();

            // A write to a connection the service already closed fails
            // (`EPIPE`), which counts as no reply.
            let reply = match late.write_all(&set_frame("ctl.stop", "svc")).await {
                Ok(()) => read_reply(&mut late).await,
                Err(_) => None,
            };
            assert!(
                matches!(reply, None | Some(PROP_SUCCESS)),
                "after stop_handler_calls, a ctl.stop read after shutdown must be answered \
                 with success or dropped, got {reply:?}"
            );
            assert_eq!(
                control_calls.load(Ordering::SeqCst),
                1,
                "no control handler call may begin after stop_handler_calls"
            );
            assert_eq!(
                powerctl_calls.load(Ordering::SeqCst),
                0,
                "no powerctl handler call may begin after stop_handler_calls"
            );
        });
    }

    /// A powerctl handler that returns `ShuttingDown` stops the handler
    /// calls as `stop_handler_calls` does, and one that returns `Ignored`
    /// does not: the control handler is still called after an ignored
    /// command, and after the command the handler acted on `ctl.stop`
    /// succeeds without a call, `ctl.start` is refused, and a later
    /// `sys.powerctl` set is accepted without a call.
    #[test]
    fn powerctl_shutting_down_outcome_stops_handler_calls() {
        const TEST: &str = "tests::powerctl_shutting_down_outcome_stops_handler_calls";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let dir = TempDir(socket_test_dir(
                "rsprops_svc_powerctl_stop",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let control_calls = Arc::new(AtomicUsize::new(0));
            let control_counter = control_calls.clone();
            let (requests, mut power_requests) = tokio::sync::mpsc::unbounded_channel();
            let service = run(
                rsproperties::PropertyConfig::new()
                    .properties_dir(dir.0.clone())
                    .socket_dir(socket_dir.clone()),
                ServiceOptions::new()
                    .control_handler(move |_, reply| {
                        control_counter.fetch_add(1, Ordering::SeqCst);
                        reply.send(true);
                    })
                    .powerctl_handler(move |msg| {
                        let _ = requests.send(msg.command.clone());
                        match msg.command.split(',').next() {
                            Some("shutdown" | "reboot") => PowerctlOutcome::ShuttingDown,
                            _ => PowerctlOutcome::Ignored,
                        }
                    }),
            )
            .await
            .expect("run");
            let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
            let set = |name: &'static str, value: &'static str| {
                let socket = socket.clone();
                async move {
                    let mut stream = UnixStream::connect(&socket).await.unwrap();
                    stream.write_all(&set_frame(name, value)).await.unwrap();
                    read_reply(&mut stream).await
                }
            };
            async fn next_command(
                requests: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
            ) -> String {
                tokio::time::timeout(Duration::from_secs(10), requests.recv())
                    .await
                    .expect("the handler call must begin")
                    .unwrap()
            }

            assert_eq!(set(POWERCTL_PROPERTY, "bogus").await, Some(PROP_SUCCESS));
            assert_eq!(next_command(&mut power_requests).await, "bogus");
            assert_eq!(set("ctl.start", "svc").await, Some(PROP_SUCCESS));
            assert_eq!(
                control_calls.load(Ordering::SeqCst),
                1,
                "an ignored command must not stop the handler calls"
            );

            assert_eq!(
                set(POWERCTL_PROPERTY, "reboot,recovery").await,
                Some(PROP_SUCCESS)
            );
            assert_eq!(next_command(&mut power_requests).await, "reboot,recovery");
            // The `ctl.stop` request may be read before the handler call
            // has returned. It is then queued behind the call and answered
            // once the call has stopped the handler calls, with the same
            // reply; so the requests after it find the calls stopped.
            assert_eq!(set("ctl.stop", "svc").await, Some(PROP_SUCCESS));
            assert_eq!(
                set("ctl.start", "svc").await,
                Some(PROP_ERROR_HANDLE_CONTROL_MESSAGE)
            );
            assert_eq!(set(POWERCTL_PROPERTY, "shutdown").await, Some(PROP_SUCCESS));
            assert_eq!(set("debug.after.reboot", "1").await, Some(PROP_SUCCESS));

            service.shutdown().await.unwrap();
            assert_eq!(
                control_calls.load(Ordering::SeqCst),
                1,
                "no control handler call may begin after ShuttingDown"
            );
            assert!(
                power_requests.try_recv().is_err(),
                "no powerctl handler call may begin after ShuttingDown"
            );
        });
    }

    /// `shutdown` returns only after a control handler call that was
    /// running when it was called has returned, and no call begins after
    /// it returned.
    ///
    /// The call is released by a task that first sees the sockets unlinked
    /// and the writer lock released, which is the state in which a
    /// `shutdown` that did not wait for the call would return. So the call
    /// is still blocked at that point, and `shutdown` returning with the
    /// call already counted as returned shows that it waited. No step
    /// depends on a delay: the 10 s deadlines only end a failing run.
    #[test]
    fn shutdown_waits_for_running_control_handler_call() {
        const TEST: &str = "tests::shutdown_waits_for_running_control_handler_call";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            use std::sync::atomic::Ordering;

            let dir = TempDir(socket_test_dir(
                "rsprops_svc_ctl_wait",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let (handler, mut blocked) = blocking_handler();
            let service = run(
                rsproperties::PropertyConfig::new()
                    .properties_dir(dir.0.clone())
                    .socket_dir(socket_dir.clone()),
                ServiceOptions::new().control_handler(handler),
            )
            .await
            .expect("run");
            let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
            let frame = ctl_start_frame();

            // Connects first and sends nothing yet; accepted before
            // `stream`, whose request reaches the handler below.
            let mut late = UnixStream::connect(&socket).await.unwrap();
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            stream.write_all(&frame).await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), blocked.entered.recv())
                .await
                .expect("the handler call must begin")
                .unwrap();

            let (properties_dir, sockets, release) =
                (dir.0.clone(), socket_dir.clone(), blocked.release);
            let releaser = tokio::spawn(async move {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !service_is_gone(&properties_dir, &sockets) {
                    assert!(
                        Instant::now() < deadline,
                        "shutdown must unlink the sockets and release the writer lock while \
                         the handler call is still running"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                release.send(()).unwrap();
            });

            service.shutdown().await.unwrap();
            assert_eq!(
                blocked.returned.load(Ordering::SeqCst),
                1,
                "shutdown must not return before the running handler call has returned"
            );
            releaser.await.unwrap();
            assert_eq!(read_reply(&mut stream).await, Some(PROP_SUCCESS));

            // A write to a connection the service already closed fails
            // (`EPIPE`), which counts as no reply.
            let reply = match late.write_all(&frame).await {
                Ok(()) => read_reply(&mut late).await,
                Err(_) => None,
            };
            assert!(
                matches!(reply, None | Some(PROP_ERROR_HANDLE_CONTROL_MESSAGE)),
                "a ctl.* request read after shutdown must be refused or dropped, got {reply:?}"
            );
            assert_eq!(
                blocked.calls.load(Ordering::SeqCst),
                1,
                "no handler call may begin after shutdown returned"
            );
        });
    }

    /// A `run` that fails in the properties service returns only after a
    /// control handler call that began while it was starting has returned.
    /// The properties service is held before it creates the areas as in
    /// [`send_request_while_init_blocked`], and the call is released by a
    /// task that first sees the sockets unlinked, which is the last step
    /// of a failed `run` that does not wait for the call.
    #[test]
    fn failed_run_waits_for_running_control_handler_call() {
        const TEST: &str = "tests::failed_run_waits_for_running_control_handler_call";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            use std::sync::atomic::Ordering;

            let dir = TempDir(socket_test_dir(
                "rsprops_svc_ctl_fail",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let fifo = dir.0.join("property_contexts.fifo");
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                &fifo,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .unwrap();
            let socket_dir = dir.0.join("sockets");
            let (handler, mut blocked) = blocking_handler();
            let run_task = tokio::spawn(run(
                rsproperties::PropertyConfig::new()
                    .properties_dir(dir.0.clone())
                    .socket_dir(socket_dir.clone()),
                ServiceOptions::new()
                    .property_contexts_files([fifo.clone()])
                    .build_prop_files([dir.0.join("missing.prop")])
                    .control_handler(handler),
            ));

            let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match UnixStream::connect(&socket).await {
                    Ok(stream) => break stream,
                    Err(e) => {
                        assert!(
                            Instant::now() < deadline,
                            "the socket service must accept connections while the properties \
                             service is still starting: {e}"
                        );
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            };
            stream.write_all(&ctl_start_frame()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), blocked.entered.recv())
                .await
                .expect("the handler call must begin")
                .unwrap();

            let (sockets, release) = (socket_dir.clone(), blocked.release);
            let releaser = tokio::spawn(async move {
                let deadline = Instant::now() + Duration::from_secs(10);
                while [
                    rsproperties::PROPERTY_SERVICE_SOCKET_NAME,
                    rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME,
                ]
                .iter()
                .any(|name| sockets.join(name).exists())
                {
                    assert!(
                        Instant::now() < deadline,
                        "a failed run must unlink the sockets while the handler call is \
                         still running"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                release.send(()).unwrap();
            });

            release_fifo(&fifo).await;
            let err = match run_task.await.unwrap() {
                Ok(_) => panic!("run must fail for a missing build.prop file"),
                Err(e) => e,
            };
            assert_eq!(
                blocked.returned.load(Ordering::SeqCst),
                1,
                "a failed run must not return before the running handler call has returned"
            );
            assert_eq!(err.stage(), StartupStage::PropertiesService, "{err}");
            releaser.await.unwrap();
        });
    }

    /// A control handler that passes each message and its reply on to the
    /// test, as a host does that handles the messages in a task of its
    /// own.
    fn deferring_handler() -> (
        impl Fn(ControlMessage, ControlReply) + Send + Sync + 'static,
        tokio::sync::mpsc::UnboundedReceiver<(ControlMessage, ControlReply)>,
    ) {
        let (pass, passed) = tokio::sync::mpsc::unbounded_channel();
        let handler = move |msg: ControlMessage, reply: ControlReply| {
            let _ = pass.send((msg, reply));
        };
        (handler, passed)
    }

    /// Sends `ctl.start` = `target` on a new connection and returns the
    /// connection with the reply the [`deferring_handler`] passed on for
    /// it.
    async fn send_held_ctl_start(
        socket: &Path,
        passed: &mut tokio::sync::mpsc::UnboundedReceiver<(ControlMessage, ControlReply)>,
        target: &str,
    ) -> (UnixStream, ControlReply) {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        stream
            .write_all(&set_frame("ctl.start", target))
            .await
            .unwrap();
        let (message, reply) = tokio::time::timeout(Duration::from_secs(10), passed.recv())
            .await
            .expect("the handler call must begin")
            .unwrap();
        assert_eq!(message.target, target);
        (stream, reply)
    }

    /// A `ctl.*` request the host has not answered holds no connection
    /// slot: with more of them unanswered than the service handles
    /// connections at once, another set is still applied and answered, and
    /// each request then gets the reply the host sends for it.
    #[test]
    fn unanswered_control_requests_hold_no_connection_slot() {
        const TEST: &str = "tests::unanswered_control_requests_hold_no_connection_slot";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            const HELD: usize = crate::socket_service::MAX_CONCURRENT_CLIENTS + 1;

            let dir = TempDir(socket_test_dir(
                "rsprops_svc_ctl_held",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let (handler, mut passed) = deferring_handler();
            let service = run(
                rsproperties::PropertyConfig::new()
                    .properties_dir(dir.0.clone())
                    .socket_dir(socket_dir.clone()),
                ServiceOptions::new().control_handler(handler),
            )
            .await
            .expect("run");
            let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);

            let mut held = Vec::new();
            for i in 0..HELD {
                held.push(send_held_ctl_start(&socket, &mut passed, &format!("svc{i}")).await);
            }

            let mut stream = UnixStream::connect(&socket).await.unwrap();
            stream
                .write_all(&set_frame("debug.while.held", "1"))
                .await
                .unwrap();
            assert_eq!(read_reply(&mut stream).await, Some(PROP_SUCCESS));
            assert_eq!(
                rsproperties::get::<String>("debug.while.held").unwrap(),
                "1"
            );

            for (i, (mut stream, reply)) in held.into_iter().enumerate() {
                let handled = i % 2 == 0;
                reply.send(handled);
                let expected = if handled {
                    PROP_SUCCESS
                } else {
                    PROP_ERROR_HANDLE_CONTROL_MESSAGE
                };
                assert_eq!(read_reply(&mut stream).await, Some(expected), "svc{i}");
            }

            service.shutdown().await.unwrap();
        });
    }

    /// `shutdown` does not wait for a reply the host holds, and the reply
    /// outlives the service: sent after `shutdown` has returned it still
    /// reaches its client, and dropped it answers with a failure.
    #[test]
    fn reply_held_by_the_host_outlives_shutdown() {
        const TEST: &str = "tests::reply_held_by_the_host_outlives_shutdown";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let dir = TempDir(socket_test_dir(
                "rsprops_svc_ctl_late",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let socket_dir = dir.0.join("sockets");
            let (handler, mut passed) = deferring_handler();
            let service = run(
                rsproperties::PropertyConfig::new()
                    .properties_dir(dir.0.clone())
                    .socket_dir(socket_dir.clone()),
                ServiceOptions::new().control_handler(handler),
            )
            .await
            .expect("run");
            let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);

            let (mut answered, answered_reply) =
                send_held_ctl_start(&socket, &mut passed, "answered").await;
            let (mut dropped, dropped_reply) =
                send_held_ctl_start(&socket, &mut passed, "dropped").await;

            tokio::time::timeout(Duration::from_secs(10), service.shutdown())
                .await
                .expect("shutdown must not wait for the replies the host holds")
                .unwrap();
            assert!(service_is_gone(&dir.0, &socket_dir));

            answered_reply.send(true);
            assert_eq!(read_reply(&mut answered).await, Some(PROP_SUCCESS));
            drop(dropped_reply);
            assert_eq!(
                read_reply(&mut dropped).await,
                Some(PROP_ERROR_HANDLE_CONTROL_MESSAGE)
            );
        });
    }

    /// Fails in `on_start`: with an error for `false`, with a panic for
    /// `true`.
    struct FailingActor;

    impl Actor for FailingActor {
        type Args = bool;
        type Error = std::io::Error;
        type IdleEvent = ();

        async fn on_start(
            panic: Self::Args,
            _actor_ref: &ActorRef<Self>,
        ) -> std::result::Result<Self, Self::Error> {
            if panic {
                panic!("on_start panicked");
            }
            Err(std::io::Error::other("on_start failed"))
        }
    }

    /// What `join` / `shutdown` report: the actor's own error, or the
    /// task's `JoinError` after a panic, under the part's name.
    #[tokio::test]
    async fn actor_slot_records_error_and_panic() {
        let (actor_ref, join_handle) = rsactor::spawn::<FailingActor>(false);
        let mut slot = ActorSlot::new("test part", actor_ref, join_handle);
        slot.wait().await;
        // A second call finds the task joined and keeps the failure.
        slot.wait().await;
        let failure = slot.failure.clone().expect("the on_start error");
        assert_eq!(failure.to_string(), "test part failed: on_start failed");
        let source = std::error::Error::source(&failure).unwrap();
        assert!(source.downcast_ref::<std::io::Error>().is_some());

        let (actor_ref, join_handle) = rsactor::spawn::<FailingActor>(true);
        let mut slot = ActorSlot::new("test part", actor_ref, join_handle);
        slot.wait().await;
        let failure = slot.failure.clone().expect("the panic");
        let source = std::error::Error::source(&failure).unwrap();
        let join_error = source
            .downcast_ref::<tokio::task::JoinError>()
            .expect("a panic is reported as the task's JoinError");
        assert!(join_error.is_panic());
    }
}
