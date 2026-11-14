// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, trace, warn};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SendFlags, SocketAddrUnix, SocketFlags, SocketType};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt;

use rsactor::{Actor, ActorRef, ActorWeak};

use rsproperties::wire::{
    validate_property_name, MAX_WIRE_NAME_LEN, MAX_WIRE_VALUE_LEN,
    PROP_ERROR_HANDLE_CONTROL_MESSAGE, PROP_ERROR_INVALID_CMD, PROP_ERROR_INVALID_NAME,
    PROP_ERROR_INVALID_VALUE, PROP_ERROR_READ_CMD, PROP_ERROR_READ_DATA, PROP_ERROR_SET_FAILED,
    PROP_MSG_SETPROP, PROP_MSG_SETPROP2, PROP_SUCCESS,
};
use rsproperties::{
    Error, Result, PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME, PROPERTY_SERVICE_SOCKET_NAME,
};

use crate::control_reply::{ControlReply, ReplyFd, ReplyState};
use crate::properties_service::PropertiesService;

/// The error types [`ContextWithLocation`] wraps: `rustix`'s `Errno`, a
/// `std::io::Error` (both become `Error::Io`) or an `Error`.
trait IntoError {
    fn into_error(self) -> Error;
}

impl IntoError for Error {
    fn into_error(self) -> Error {
        self
    }
}

impl IntoError for std::io::Error {
    fn into_error(self) -> Error {
        Error::Io(self)
    }
}

impl IntoError for Errno {
    fn into_error(self) -> Error {
        Error::Io(self.into())
    }
}

/// Wraps the error of a `Result` in `Error::Context`, with the caller's
/// source location.
trait ContextWithLocation<T> {
    #[track_caller]
    fn context_with_location(self, msg: &str) -> Result<T>;

    /// Like [`Self::context_with_location`], with the message built only on
    /// the error path.
    #[track_caller]
    fn with_context_location(self, f: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E> ContextWithLocation<T> for std::result::Result<T, E>
where
    E: IntoError,
{
    #[track_caller]
    fn context_with_location(self, msg: &str) -> Result<T> {
        self.with_context_location(|| msg.to_owned())
    }

    #[track_caller]
    fn with_context_location(self, f: impl FnOnce() -> String) -> Result<T> {
        let location = std::panic::Location::caller();
        self.map_err(|e| Error::Context {
            msg: f(),
            location,
            source: Box::new(e.into_error()),
        })
    }
}

/// Upper bound on simultaneously *serviced* client connections. Each
/// handler task holds one permit for the duration of the exchange.
pub(crate) const MAX_CONCURRENT_CLIENTS: usize = 64;

/// Upper bound on accepted connections *waiting* for a handler permit.
/// Every waiting task holds an accepted `UnixStream` (one fd), so without
/// this cap a connect flood while all handler permits are taken would
/// accumulate fds until EMFILE and take the whole process down. Beyond
/// `MAX_CONCURRENT_CLIENTS + MAX_WAITING_CLIENTS`, new connections are
/// dropped immediately; well-behaved clients see ECONNRESET and retry.
/// The connection of a `ctl.*` request that is waiting for its reply is
/// held outside both; `MAX_UNANSWERED_CONTROL_MESSAGES` bounds those.
const MAX_WAITING_CLIENTS: usize = 256;

/// Wall-clock timeout for an entire `handle_client` exchange. Trusted
/// clients (init, system services) complete well under this; untrusted or
/// stuck clients are torn down rather than tying up a task indefinitely.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most `ctl.*` requests that may be unanswered at once: those queued
/// for the handler worker and those whose [`ControlReply`] the host holds.
/// A request that finds this many is refused. init's `QueueControlMessage`
/// drops a message when `pending_control_messages.size() > 100`, so its
/// queue holds at most 101, and with the message its main thread is
/// handling at most 102 are unanswered.
///
/// The bound is on the unanswered requests and not on the queue because
/// the handler call returns before the host has answered: the worker
/// empties the queue however many replies the host still holds. Each
/// unanswered request holds its client's connection (one fd) and no
/// connection permit, so this also bounds the fds that
/// `MAX_CONCURRENT_CLIENTS + MAX_WAITING_CLIENTS` does not cover. The
/// worker makes one handler call at a time ([`handler_worker`]), so the
/// handlers use at most one blocking-pool thread however many requests
/// are queued.
const MAX_UNANSWERED_CONTROL_MESSAGES: usize = 102;

/// Mode of both socket files: `0o660` (rw-rw----) — owner and group may
/// connect, others may not. Stricter than AOSP init, which creates
/// `property_service` 0666 (relying on SELinux) and only
/// `property_service_for_system` 0660. The mode is set on the socket fd
/// before `bind` and, if the umask cleared bits, raised to exactly this
/// value before `listen` (see `bind_service_sockets`), so it does not
/// depend on the process umask.
///
/// **Access model.** This file mode is the service's *entire* access
/// control: any peer that can `connect()` (i.e. has write permission on
/// the socket file — owner or group) may set any non-`ro.` property, and
/// any `ro.` property that has no value yet (the first writer fixes it),
/// and send any `ctl.*` control message to the configured handler. The
/// `property_contexts` type check applies to values, not to senders.
/// Unlike AOSP init, no per-property SO_PEERCRED / property_contexts
/// authorization is performed, and the "property" / "system" sockets
/// share one handler; peer credentials are logged for auditability only.
/// This is an intentional simplification for host-side deployments —
/// restrict group membership and the socket dir as described on
/// [`SocketService`].
const SOCKET_FILE_MODE: u32 = 0o660;

/// Backoff applied when `accept()` returns an error. Without it, a
/// permanent failure (EMFILE, ENFILE, listener torn down) would re-enter
/// `on_idle` immediately and spin the worker producing a high-rate log
/// flood. 100ms is short enough to recover quickly when the condition
/// clears, long enough to dampen the loop.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Creates `dir` and any missing parent with mode 0755 minus the umask's
/// bits. `create_dir_all` would use 0777 minus the umask's bits, which is
/// world-writable under umask 0000. An existing directory keeps its mode;
/// under a narrow umask (e.g. 077 → 0700) the group cannot reach the
/// sockets, so a deployment that needs group access creates the socket
/// dir beforehand with the mode it wants.
fn ensure_socket_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .with_context_location(|| format!("create socket dir {dir:?}"))
}

/// Opens the socket dir once. Every later unlink, rename and chmod is
/// made relative to this descriptor, so a symlink at `dir` itself is
/// followed here and nowhere else.
fn open_socket_dir(dir: &Path) -> Result<OwnedFd> {
    rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context_location(|| format!("open socket dir {dir:?}"))
}

/// Startup warning for a socket dir that any local user can write to and
/// that has no sticky bit: such a user can rename or unlink the service's
/// sockets and put its own in their place. Only that case is checked —
/// ancestors, group write and ACLs are not inspected — so `None` is not an
/// access check. A sticky world-writable dir (`/tmp`) is not reported:
/// other uids cannot rename or unlink entries the service owns there.
fn socket_dir_warning(mode: Mode) -> Option<&'static str> {
    (mode.contains(Mode::WOTH) && !mode.contains(Mode::SVTX)).then_some(
        "socket dir is world-writable without the sticky bit; any local user can replace the sockets",
    )
}

/// Temp name of a socket while it is set up: `.<name>.tmp-<pid>`, the
/// name 0.6.0 used, so the startup sweep also removes temps a crashed
/// 0.6.0 left behind.
fn temp_name(name: &str) -> String {
    format!(".{name}.tmp-{}", std::process::id())
}

/// Removes temp sockets of crashed earlier runs (their names embed another
/// pid). Only entries named `.property_service.tmp-*` or
/// `.property_service_for_system.tmp-*` that are sockets are removed: the
/// socket dir may be shared with other programs (e.g. `/dev/socket`).
/// Errors are ignored; a temp that stays only takes up a name.
fn sweep_stale_temps(dirfd: &OwnedFd) {
    let prefixes = [
        format!(".{PROPERTY_SERVICE_SOCKET_NAME}.tmp-"),
        format!(".{PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME}.tmp-"),
    ];
    // `Dir::read_from` opens "." relative to `dirfd`: it lists the opened
    // directory, not whatever the socket dir's path names now.
    let Ok(entries) = rustix::fs::Dir::read_from(dirfd) else {
        return;
    };
    for entry in entries.map_while(std::result::Result::ok) {
        let name = entry.file_name();
        if !prefixes
            .iter()
            .any(|p| name.to_bytes().starts_with(p.as_bytes()))
        {
            continue;
        }
        let file_type = match entry.file_type() {
            // `d_type` is not filled in by every filesystem.
            FileType::Unknown => match rustix::fs::statat(dirfd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => FileType::from_raw_mode(st.st_mode),
                Err(_) => continue,
            },
            file_type => file_type,
        };
        if file_type == FileType::Socket {
            debug!("Removing stale temp socket: {name:?}");
            let _ = rustix::fs::unlinkat(dirfd, name, AtFlags::empty());
        }
    }
}

/// Entries this instance created directly in the socket dir, unlinked
/// relative to `dirfd` on drop.
///
/// During startup it holds each temp name once its `bind` succeeded and,
/// after each rename, the final name in place of the temp, so a failure, a
/// panic, or a cancelled start (the `spawn_blocking` result is dropped)
/// removes exactly what startup created. After a successful start it holds
/// the two final names and lives in [`SocketService`], so dropping the
/// service unlinks them. Unlinking is by name: an instance that bound the
/// same names in between loses its sockets — hence one instance per
/// socket dir.
struct SocketDirEntries {
    dirfd: OwnedFd,
    names: Vec<String>,
}

impl SocketDirEntries {
    fn rename(&mut self, dir: &Path, from: &str, to: &str) -> Result<()> {
        rustix::fs::renameat(&self.dirfd, from, &self.dirfd, to).with_context_location(|| {
            format!("rename {:?} to {:?}", dir.join(from), dir.join(to))
        })?;
        for name in &mut self.names {
            if name == from {
                *name = to.to_owned();
            }
        }
        Ok(())
    }
}

impl Drop for SocketDirEntries {
    fn drop(&mut self) {
        for name in self.names.drain(..) {
            match rustix::fs::unlinkat(&self.dirfd, name.as_str(), AtFlags::empty()) {
                Ok(()) => debug!("Socket file removed: {name}"),
                Err(e) if e == Errno::NOENT => {}
                Err(e) => warn!("Failed to remove socket file {name}: {e}"),
            }
        }
    }
}

/// Creates the socket for `tmp_name` and binds it at `dir/tmp_name`
/// without listening.
fn bind_temp(dir: &Path, dirfd: &OwnedFd, tmp_name: &str) -> Result<OwnedFd> {
    let path = dir.join(tmp_name);
    // Our own leftover (pid reuse, or a repeated start in this process).
    match rustix::fs::unlinkat(dirfd, tmp_name, AtFlags::empty()) {
        Ok(()) => {}
        Err(e) if e == Errno::NOENT => {}
        Err(e) => return Err(e).with_context_location(|| format!("unlink {path:?}")),
    }
    // Same flags as tokio's `UnixListener::bind` (mio 1.2.2
    // `sys/unix/uds/listener.rs:35`): `from_std` requires a non-blocking
    // socket, and CLOEXEC keeps the listener out of exec'd children.
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )
    .context_with_location("create unix socket")?;
    // Linux creates the socket file with the socket inode's mode minus the
    // umask (`unix_bind_bsd`; implementation behavior, not documented in
    // unix(7)), so from the moment the path exists its mode is a subset of
    // 0660. Without this the file starts at 0777 minus the umask.
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(SOCKET_FILE_MODE))
        .context_with_location("fchmod unix socket")?;
    // `sun_path` limit: rustix `SocketAddrUnix::new` accepts 108 bytes
    // (0.6.0's std path: 107).
    let addr = SocketAddrUnix::new(path.as_path())
        .with_context_location(|| format!("socket address {path:?}"))?;
    rustix::net::bind(&fd, &addr).with_context_location(|| format!("bind {path:?}"))?;
    Ok(fd)
}

/// Raises the mode of the socket at `dir/tmp_name` to exactly
/// `SOCKET_FILE_MODE` when the umask cleared bits at `bind`.
///
/// The entry is opened `O_PATH|O_NOFOLLOW` relative to the socket dir and
/// checked with `fstat` to be a socket owned by this process's euid; the
/// chmod then goes through `/proc/thread-self/fd/N`, which resolves to the
/// opened inode without walking the path again. This is how bionic's
/// `fchmodat(AT_SYMLINK_NOFOLLOW)` works: `fchmod` on an `O_PATH` fd fails
/// with EBADF, and `fchmodat2` needs Linux 6.6. `thread-self` rather than
/// `self`: `/proc/self` names the thread-group leader, and in a host
/// process whose leader thread has exited its `fd/` may be gone
/// (unverified); the calling thread is alive. `/proc/thread-self` exists
/// since Linux 3.17; on older kernels the chmod falls back to
/// `/proc/self/fd/N` (see [`chmod_opened`]). Needs `/proc` — only when
/// the umask cleared bits of 0660.
///
/// If the socket dir's path was swapped after it was opened, `bind`
/// created the socket elsewhere and this `openat` fails with ENOENT.
fn widen_to_socket_mode(dir: &Path, dirfd: &OwnedFd, tmp_name: &str) -> Result<()> {
    let path = dir.join(tmp_name);
    let pfd = rustix::fs::openat(
        dirfd,
        tmp_name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context_location(|| format!("open {path:?}"))?;
    let st = rustix::fs::fstat(&pfd).with_context_location(|| format!("fstat {path:?}"))?;
    let euid = rustix::process::geteuid().as_raw();
    if FileType::from_raw_mode(st.st_mode) != FileType::Socket || st.st_uid != euid {
        return Err(Error::FileValidation(format!(
            "not a socket owned by euid {euid}; mode left unchanged"
        )))
        .with_context_location(|| format!("raise mode of {path:?}"));
    }
    if Mode::from_raw_mode(st.st_mode).bits() == SOCKET_FILE_MODE {
        return Ok(());
    }
    chmod_opened(
        &pfd,
        &st,
        Mode::from_raw_mode(SOCKET_FILE_MODE),
        &PROC_FD_DIRS,
    )
    .with_context_location(|| {
        format!(
            "raising {path:?} to 0660 via /proc/thread-self/fd or /proc/self/fd failed \
                 (is /proc mounted? or run with a umask that keeps group rw, e.g. 007)"
        )
    })
}

/// `fd/` directories tried by [`chmod_opened`], in order.
const PROC_FD_DIRS: [&str; 2] = ["/proc/thread-self/fd", "/proc/self/fd"];

/// `chmod`s the inode `pfd` refers to (`st` is its `fstat`) through the
/// first `<fd_dir>/N` that exists; ENOENT moves to the next directory
/// (`/proc/thread-self` is absent before Linux 3.17, everything is absent
/// without `/proc`).
///
/// `/proc/self/fd/N` is entry N of the thread-group leader's fd table. That
/// is the calling thread's table for every thread created with
/// `CLONE_FILES` (std, pthread and tokio threads); a thread that ran
/// `unshare(CLONE_FILES)` has its own table, and the leader's entry N can
/// be an unrelated file. So the link is `stat`ed first and used only when
/// it resolves to `st`'s device and inode; otherwise nothing is changed.
/// In the shared-table case entry N is `pfd`, which this call holds open,
/// so it cannot change between the `stat` and the `chmod`.
fn chmod_opened(pfd: &OwnedFd, st: &rustix::fs::Stat, mode: Mode, fd_dirs: &[&str]) -> Result<()> {
    for fd_dir in fd_dirs {
        let link = format!("{fd_dir}/{}", pfd.as_raw_fd());
        match rustix::fs::stat(link.as_str()) {
            Ok(target) if target.st_dev == st.st_dev && target.st_ino == st.st_ino => {
                return rustix::fs::chmod(link.as_str(), mode)
                    .with_context_location(|| format!("chmod {link}"));
            }
            Ok(_) => {
                return Err(Error::FileValidation(format!(
                    "{link} is another file (separate fd table); mode left unchanged"
                )))
            }
            Err(Errno::NOENT) => continue,
            Err(e) => return Err(e).with_context_location(|| format!("stat {link}")),
        }
    }
    Err(Errno::NOENT).with_context_location(|| format!("none of {fd_dirs:?} exists"))
}

/// One socket up to the point where only the rename is left: bind at its
/// temp name, mode raised, listening. The temp name is registered in
/// `entries` as soon as `bind` created it.
fn prepare_temp_socket(
    dir: &Path,
    entries: &mut SocketDirEntries,
    tmp_name: &str,
) -> Result<OwnedFd> {
    let fd = bind_temp(dir, &entries.dirfd, tmp_name)?;
    entries.names.push(tmp_name.to_owned());
    widen_to_socket_mode(dir, &entries.dirfd, tmp_name)?;
    // `listen` only after the mode is final (AOSP `CreateSocket` order:
    // bind → fchmodat → listen): until then a `connect` fails with
    // ECONNREFUSED, so no connection is made under an intermediate mode.
    // Backlog -1 (SOMAXCONN) as tokio/mio on Linux.
    let path = dir.join(tmp_name);
    rustix::net::listen(&fd, -1).with_context_location(|| format!("listen {path:?}"))?;
    Ok(fd)
}

/// Both listeners, already at their final names.
struct BoundSockets {
    property: std::os::unix::net::UnixListener,
    system: std::os::unix::net::UnixListener,
    /// Holds the two final names; dropping it unlinks them.
    entries: SocketDirEntries,
}

/// Creates the socket dir if missing, then [`bind_service_sockets_at`].
/// The sequence and its limits are documented on [`SocketService`].
fn bind_service_sockets(dir: &Path) -> Result<BoundSockets> {
    ensure_socket_dir(dir)?;
    let dirfd = open_socket_dir(dir)?;
    bind_service_sockets_at(dir, dirfd)
}

/// Binds both sockets in the directory `dirfd` refers to. `dir` is used
/// for `bind` and for messages; every other operation goes through
/// `dirfd`. Both temp sockets are set up before either is renamed, so a
/// failure on the second leaves the existing final names untouched.
fn bind_service_sockets_at(dir: &Path, dirfd: OwnedFd) -> Result<BoundSockets> {
    let mut entries = SocketDirEntries {
        dirfd,
        names: Vec::with_capacity(2),
    };
    let st = rustix::fs::fstat(&entries.dirfd)
        .with_context_location(|| format!("fstat socket dir {dir:?}"))?;
    if let Some(warning) = socket_dir_warning(Mode::from_raw_mode(st.st_mode)) {
        warn!("{warning}: {dir:?}");
    }
    sweep_stale_temps(&entries.dirfd);

    let property_tmp = temp_name(PROPERTY_SERVICE_SOCKET_NAME);
    let system_tmp = temp_name(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME);
    let property = prepare_temp_socket(dir, &mut entries, &property_tmp)?;
    let system = prepare_temp_socket(dir, &mut entries, &system_tmp)?;
    // `renameat` replaces a socket left by a crashed earlier run without a
    // window in which the name is missing. A failure on the second rename
    // unlinks the first final name (via `entries`): the name belonged to
    // no live service, since the caller holds the properties dir's writer
    // lock and one instance serves one socket dir.
    entries.rename(dir, &property_tmp, PROPERTY_SERVICE_SOCKET_NAME)?;
    entries.rename(dir, &system_tmp, PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME)?;
    Ok(BoundSockets {
        property: property.into(),
        system: system.into(),
        entries,
    })
}

/// What [`spawn`] serves: the socket dir and the properties service the
/// sets are forwarded to.
pub(crate) struct SocketServiceArgs {
    pub(crate) socket_dir: PathBuf,
    pub(crate) properties_service: ActorRef<PropertiesService>,
    /// Handle the `ctl.*` and `sys.powerctl` sets. Built by the caller,
    /// which keeps its [`Handlers::drain`] and [`Handlers::stop_switch`].
    pub(crate) handlers: Handlers,
}

/// Spawns the socket service actor with `args`.
///
/// Spawn this only while the properties dir's writer lock
/// (`rsproperties::AreaWriterLock`) is held — `crate::run` takes it before
/// spawning either actor. Spawned without it, a second instance can
/// replace and then unlink the running instance's sockets.
pub(crate) fn spawn(
    args: SocketServiceArgs,
) -> (
    ActorRef<SocketService>,
    tokio::task::JoinHandle<rsactor::ActorResult<SocketService>>,
) {
    // `with_idle()` is required: rsactor's idle-event channel is opt-in, and
    // this actor drives its accept loop through `subscribe_idle` / `on_idle`.
    // Without it `subscribe_idle` would return `IdleChannelNotEnabled` and the
    // listeners would never be polled.
    rsactor::spawn_with_options(args, rsactor::SpawnOptions::new().with_idle())
}

/// The actor that binds `property_service` and
/// `property_service_for_system` in the socket dir, reads the requests and
/// forwards the sets to the properties service. How the socket files are
/// created and what the socket dir must look like is documented on
/// [`crate::run`] ("Socket files"). Dropping the actor unlinks both final
/// names.
pub(crate) struct SocketService {
    properties_service: ActorRef<PropertiesService>,
    /// Limits concurrently in-flight client tasks.
    connection_sem: Arc<Semaphore>,
    /// Limits accepted-but-not-yet-serviced connections (fd backpressure);
    /// see `MAX_WAITING_CLIENTS`.
    waiting_sem: Arc<Semaphore>,
    /// The two final socket names; unlinked when the service is dropped.
    _socket_files: SocketDirEntries,
    handlers: Handlers,
}

impl Actor for SocketService {
    type Args = SocketServiceArgs;
    type Error = Error;
    /// Each idle event is one accepted connection (or an `accept()` error)
    /// tagged with the listener it came from, so logging can name the source.
    /// The accept loop is modelled as `Stream`s of connections subscribed
    /// via `subscribe_idle` (rsactor has no `on_run` hook).
    type IdleEvent = (std::io::Result<UnixStream>, &'static str);

    async fn on_start(
        args: Self::Args,
        actor_ref: &ActorRef<Self>,
    ) -> std::result::Result<Self, Self::Error> {
        info!(
            "Property socket services will be created at: {} and {}",
            args.socket_dir.join(PROPERTY_SERVICE_SOCKET_NAME).display(),
            args.socket_dir
                .join(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME)
                .display()
        );
        // All filesystem work is blocking syscalls; one `spawn_blocking`
        // runs it off the runtime threads.
        let dir = args.socket_dir;
        let bound = tokio::task::spawn_blocking(move || bind_service_sockets(&dir))
            .await
            .map_err(|e| std::io::Error::other(format!("socket bind task failed: {e}")))??;
        // From here to `Ok(Self)`, any early return drops `entries`, which
        // unlinks both final names, so a failed start does not leave them
        // pointing at closed listeners.
        let BoundSockets {
            property,
            system,
            entries,
        } = bound;
        let property_listener = UnixListener::from_std(property)
            .context_with_location("register property_service listener")?;
        let system_listener = UnixListener::from_std(system)
            .context_with_location("register property_service_for_system listener")?;
        info!("AsyncPropertySocketService started successfully");

        // Model each listener as a `Stream` of accepted connections and hand
        // it to the actor's idle loop. The runtime owns the stream state, so
        // — unlike the old `on_run` — accept progress is never lost to
        // `select!` cancellation. Subscribing from `on_start` is safe:
        // `subscribe_idle` is synchronous (`try_send`) and the runtime drains
        // queued subscriptions before the first loop iteration.
        actor_ref
            .subscribe_idle(UnixListenerStream::new(property_listener).map(|r| (r, "property")))
            .map_err(|e| std::io::Error::other(format!("subscribe property listener: {e}")))?;
        actor_ref
            .subscribe_idle(UnixListenerStream::new(system_listener).map(|r| (r, "system")))
            .map_err(|e| std::io::Error::other(format!("subscribe system listener: {e}")))?;

        Ok(Self {
            properties_service: args.properties_service,
            connection_sem: Arc::new(Semaphore::new(MAX_CONCURRENT_CLIENTS)),
            waiting_sem: Arc::new(Semaphore::new(MAX_WAITING_CLIENTS)),
            _socket_files: entries,
            handlers: args.handlers,
        })
    }

    async fn on_idle(
        &mut self,
        event: Self::IdleEvent,
        _actor_weak: &ActorWeak<Self>,
    ) -> std::result::Result<(), Self::Error> {
        // IMPORTANT: the actor runtime awaits `on_idle` inline in its
        // single `select!` loop — any await that parks here stalls the
        // mailbox (stop/ask) AND the other listener's accepts. Keep this
        // body non-blocking except for the short, bounded error backoff.
        let (accepted, source) = event;

        let stream = match accepted {
            Ok(stream) => stream,
            Err(e) => {
                // Permanent conditions (EMFILE, ENFILE, broken listener)
                // would otherwise let the idle loop spin at full speed and
                // saturate the log with the same error every microsecond. A
                // small sleep dampens the loop and gives the kernel time to
                // recover; it does block the actor loop, but only for the
                // fixed 100ms — unlike an unbounded permit wait.
                // `UnixListenerStream` keeps yielding after an error, so the
                // listener is not lost.
                error!("Error accepting connection on {source} listener: {e}");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                return Ok(());
            }
        };

        // Peer credentials: logged for auditability — see the access-model
        // note on `SOCKET_FILE_MODE` (no per-property authorization).
        if let Ok(cred) = stream.peer_cred() {
            debug!(
                "Client connected on {source} listener (uid={}, gid={})",
                cred.uid(),
                cred.gid()
            );
        }

        // Bound the number of concurrently in-flight client handlers
        // WITHOUT awaiting in the actor loop — the previous inline
        // `acquire_owned().await` parked the whole loop (mailbox + the
        // other listener) for up to CLIENT_TIMEOUT per saturated
        // connection. Two-level backpressure, both non-blocking here:
        // 1. a *waiting-room* permit is taken via try_acquire; if the room
        //    is full the connection is dropped immediately, which caps the
        //    total accepted fds at MAX_CONCURRENT_CLIENTS +
        //    MAX_WAITING_CLIENTS (and MAX_UNANSWERED_CONTROL_MESSAGES for
        //    the `ctl.*` requests whose replies are pending, which hold
        //    neither permit) instead of growing until EMFILE;
        // 2. the spawned task then waits (deadline-bounded) for a handler
        //    permit, holding its waiting-room slot until it gets one.
        let waiting = match self.waiting_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!("Waiting room full; dropping {source} connection");
                return Ok(()); // `stream` dropped → connection closed
            }
        };
        let sem = self.connection_sem.clone();
        let connection_sender = self.properties_service.clone();
        let handlers = self.handlers.clone();
        tokio::spawn(async move {
            let permit = {
                let _waiting = waiting; // released once a handler slot is ours

                // Bounded by `CLIENT_TIMEOUT`, not by the rsproperties
                // client's 2 s reply budget. The timer only limits how long
                // a waiting-room slot is held; it is not meant to drop
                // requests the client may have given up on. A queued
                // connection's request is usually already in the socket
                // buffer, and AOSP init keeps pending connections in its
                // listen backlog without a timer and applies every request
                // it receives in full.
                match tokio::time::timeout(CLIENT_TIMEOUT, sem.acquire_owned()).await {
                    Ok(Ok(p)) => p,
                    Ok(Err(_)) => {
                        error!("Connection semaphore closed");
                        return;
                    }
                    Err(_elapsed) => {
                        warn!(
                            "No handler slot available within {CLIENT_TIMEOUT:?}, dropping {source} connection"
                        );
                        return;
                    }
                }
            };
            let _permit = permit; // dropped when the task ends
            match tokio::time::timeout(
                CLIENT_TIMEOUT,
                Self::handle_client(stream, connection_sender, handlers),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => error!("Error handling client: {e}"),
                Err(_elapsed) => {
                    warn!("Client exchange timed out after {CLIENT_TIMEOUT:?}, dropping connection")
                }
            }
        });
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_weak: &ActorWeak<Self>,
        killed: bool,
    ) -> std::result::Result<(), Self::Error> {
        // A graceful stop is normal operation — keep it at `info!` so log
        // monitors don't alarm on routine shutdowns; only a kill warrants
        // `warn!`.
        if killed {
            warn!("SocketService killed — cleaning up resources");
        } else {
            info!("SocketService stopping gracefully");
        }
        Ok(())
    }
}

impl rsactor::Message<crate::ReadyMessage> for SocketService {
    type Reply = ();

    async fn handle(
        &mut self,
        _message: crate::ReadyMessage,
        _actor_ref: &ActorRef<Self>,
    ) -> Self::Reply {
    }
}

/// The sender of a request, from `SO_PEERCRED`. Every field is `None`
/// when the credentials could not be read.
#[derive(Clone, Copy, Default)]
struct Peer {
    pid: Option<i32>,
    uid: Option<u32>,
    gid: Option<u32>,
}

impl Peer {
    fn of(stream: &UnixStream) -> Self {
        match stream.peer_cred() {
            Ok(cred) => Self {
                pid: cred.pid(),
                uid: Some(cred.uid()),
                gid: Some(cred.gid()),
            },
            Err(_) => Self::default(),
        }
    }
}

/// The form the log lines use: `pid 123`, or `an unknown pid`.
impl std::fmt::Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pid {
            Some(pid) => write!(f, "pid {pid}"),
            None => f.write_str("an unknown pid"),
        }
    }
}

/// One `ctl.*` request waiting for the handler worker.
struct ControlJob {
    message: crate::ControlMessage,
    /// Owns the client's connection. The client may have closed its end
    /// by the time the worker takes the job; the handler is called all
    /// the same, as init handles a queued control message whose client
    /// has closed its socket. Dropping the job answers the client
    /// ([`ControlReply`], "Dropping a reply").
    reply: ControlReply,
}

/// What the connection tasks have queued for the handler worker.
#[derive(Default)]
struct PendingCalls {
    /// init `pending_control_messages`.
    control: VecDeque<ControlJob>,
    /// init `shutdown_command_`: one slot, which a later set overwrites.
    powerctl: Option<crate::PowerctlMessage>,
    /// Set by [`HandlerShared::close`]; nothing is queued afterwards.
    closed: bool,
}

/// Why [`HandlerShared::queue_control`] did not queue a request.
enum QueueRefusal {
    /// The socket service has been dropped.
    Closed,
    /// `MAX_UNANSWERED_CONTROL_MESSAGES` requests are unanswered.
    Full,
}

/// What the next step of [`handler_worker`] is.
enum HandlerWork {
    Powerctl(crate::PowerctlMessage),
    Control(ControlJob),
    /// The queue is closed; these were still waiting.
    Closed(VecDeque<ControlJob>),
    Idle,
}

/// The state that the connection tasks, the handler worker and
/// `crate::ServiceHandle` share.
struct HandlerShared {
    /// `false` refuses every `ctl.*` set.
    has_control: bool,
    /// `false` accepts a `sys.powerctl` set and does nothing with it.
    has_powerctl: bool,
    /// Never held across an await or a handler call.
    pending: std::sync::Mutex<PendingCalls>,
    /// `notify_one` after each change of `pending` that the worker has to
    /// see. A permit given while the worker is not waiting is kept for its
    /// next wait, so no change is missed.
    wake: tokio::sync::Notify,
    /// Set by `crate::ServiceHandle::stop_handler_calls` and by a powerctl
    /// handler call that returned `PowerctlOutcome::ShuttingDown`.
    stop: HandlerCallsStop,
    /// What every [`ControlReply`] of this service has a clone of: the
    /// same stop flag and the number of unanswered requests. Separate
    /// from this struct because a reply may outlive it in the host, and
    /// because a reply that pointed back here from inside `pending` would
    /// keep both alive.
    replies: Arc<ReplyState>,
}

impl HandlerShared {
    fn pending(&self) -> std::sync::MutexGuard<'_, PendingCalls> {
        // Nothing can panic while the lock is held, so a poisoned lock
        // would still hold consistent data.
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// init `QueueControlMessage`: queues `message` with the connection
    /// its reply goes to. A refusal gives the connection back for the
    /// refusal's reply.
    ///
    /// The count of unanswered requests grows only here, under the lock,
    /// so it never exceeds `MAX_UNANSWERED_CONTROL_MESSAGES`; a reply
    /// that is answered meanwhile only lowers it.
    fn queue_control(
        &self,
        message: crate::ControlMessage,
        fd: ReplyFd,
    ) -> std::result::Result<(), (QueueRefusal, ReplyFd)> {
        {
            let mut pending = self.pending();
            if pending.closed {
                return Err((QueueRefusal::Closed, fd));
            }
            if self.replies.unanswered() >= MAX_UNANSWERED_CONTROL_MESSAGES {
                return Err((QueueRefusal::Full, fd));
            }
            let reply =
                ControlReply::to_client(fd, message.action.clone(), Arc::clone(&self.replies));
            pending.control.push_back(ControlJob { message, reply });
        }
        self.wake.notify_one();
        Ok(())
    }

    /// init `TriggerShutdown`: records `message` for the worker in place
    /// of a command the worker has not taken yet. Returns `false`, with
    /// nothing recorded, once the queue is closed.
    fn queue_powerctl(&self, message: crate::PowerctlMessage) -> bool {
        {
            let mut pending = self.pending();
            if pending.closed {
                return false;
            }
            if pending.powerctl.replace(message).is_some() {
                info!(
                    "A later {} set replaced one the powerctl handler had not been called for",
                    crate::POWERCTL_PROPERTY
                );
            }
        }
        self.wake.notify_one();
        true
    }

    /// The worker's next step. A recorded `sys.powerctl` command comes
    /// before the control messages, as init's main loop calls
    /// `HandlePowerctlMessage` at the top of an iteration and
    /// `HandleControlMessages` at its end, and before the end of the
    /// worker, since its client was answered with `PROP_SUCCESS`.
    fn next_work(&self) -> HandlerWork {
        let mut pending = self.pending();
        if let Some(message) = pending.powerctl.take() {
            HandlerWork::Powerctl(message)
        } else if pending.closed {
            HandlerWork::Closed(std::mem::take(&mut pending.control))
        } else if let Some(job) = pending.control.pop_front() {
            HandlerWork::Control(job)
        } else {
            HandlerWork::Idle
        }
    }

    /// Nothing is queued from now on, and the worker ends once the call it
    /// is making has returned. Can be repeated.
    fn close(&self) {
        self.pending().closed = true;
        self.wake.notify_one();
    }

    /// The reply to a `ctl.<action>` request whose handler call does not
    /// begin because the calls are stopped, the queue is closed or too
    /// many requests are unanswered. The stop decides first: see
    /// [`HandlerCallsStop`]. A [`ControlReply`] that is dropped answers
    /// by the same rule.
    fn reply_without_call(&self, action: &str) -> i32 {
        self.replies.code_without_answer(action)
    }
}

/// Closes the queue when the last clone of a [`Handlers`] is dropped, so
/// the worker also ends when no [`SocketService`] was built from it (a
/// failed `on_start`).
struct CloseOnDrop(Arc<HandlerShared>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The handlers of the sets that are not stored: `ctl.*`
/// ([`SocketService::handle_control`]) and `sys.powerctl`
/// ([`SocketService::notify_powerctl`]). Each connection task has a clone.
///
/// The handlers themselves are owned by one task, [`handler_worker`],
/// which calls them one at a time in the order init's main thread takes
/// the same requests. The connection tasks only queue requests for it, so
/// they hold no clone of a handler. A `ctl.*` request is queued with its
/// connection, and its connection task ends there: the reply is sent by
/// the [`ControlReply`] the handler gets.
#[derive(Clone)]
pub(crate) struct Handlers {
    shared: Arc<HandlerShared>,
    _close: Arc<CloseOnDrop>,
    /// Its sender is owned by the worker and never sends.
    worker_ended: tokio::sync::watch::Receiver<()>,
}

/// init's `accept_messages`, inverted: set once and never cleared, by
/// `crate::ServiceHandle::stop_handler_calls` or by a powerctl handler
/// call that returned `PowerctlOutcome::ShuttingDown` (init:
/// `StopSendingMessages`). While it is set no handler call begins, and
/// [`SocketService::handle_control`] and
/// [`SocketService::notify_powerctl`] answer as init does during its
/// shutdown; a [`ControlReply`] that is dropped reads it too, also after
/// the service has ended. It differs from the closed queue of [`SocketService`]'s
/// `Drop` in what the client is told: this one says that the host is
/// going down, so `ctl.stop` and `sys.powerctl` are answered with
/// success; the closed queue says only that this service has ended, so
/// both are refused. This one is checked first.
#[derive(Clone, Default)]
pub(crate) struct HandlerCallsStop(Arc<std::sync::atomic::AtomicBool>);

impl HandlerCallsStop {
    /// A connection task or the worker that reads the flag after this
    /// returned begins no handler call. The worker reads it just before
    /// each call, so at most the call it was about to make begins later.
    pub(crate) fn stop(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Handlers {
    /// Spawns the worker when there is a handler, so with one this must
    /// be called inside a tokio runtime.
    pub(crate) fn new(
        control: Option<crate::ControlHandler>,
        powerctl: Option<crate::PowerctlHandler>,
    ) -> Self {
        let stop = HandlerCallsStop::default();
        let shared = Arc::new(HandlerShared {
            has_control: control.is_some(),
            has_powerctl: powerctl.is_some(),
            pending: std::sync::Mutex::default(),
            wake: tokio::sync::Notify::new(),
            replies: Arc::new(ReplyState::new(stop.clone())),
            stop,
        });
        let (ended, worker_ended) = tokio::sync::watch::channel(());
        // Without a handler nothing is ever queued, and `ended` is dropped
        // here: `HandlerCallsDrain::wait` returns at once.
        if control.is_some() || powerctl.is_some() {
            tokio::spawn(handler_worker(
                Arc::clone(&shared),
                control,
                powerctl,
                ended,
            ));
        }
        Self {
            _close: Arc::new(CloseOnDrop(Arc::clone(&shared))),
            shared,
            worker_ended,
        }
    }

    /// The handle that stops the calls of both handlers.
    pub(crate) fn stop_switch(&self) -> HandlerCallsStop {
        self.shared.stop.clone()
    }

    /// The handle that waits for the worker to end.
    pub(crate) fn drain(&self) -> HandlerCallsDrain {
        HandlerCallsDrain(self.worker_ended.clone())
    }

    /// Closes the queue: see [`HandlerShared::close`].
    fn close(&self) {
        self.shared.close();
    }
}

/// The one task that calls the control handler and the powerctl handler,
/// in place of init's main thread: one call at a time, each on the
/// blocking pool (`spawn_blocking`) so that a handler may block. A
/// `sys.powerctl` command is taken before the next control message
/// ([`HandlerShared::next_work`]).
///
/// A control handler call ends when the handler returns, which may be
/// before the host has answered: the handler gets the [`ControlReply`]
/// and the worker goes on to the next message. So the worker fixes the
/// order in which the host gets the messages, and the host the order in
/// which it handles and answers them.
///
/// The stop flag is read before each call. Once it is set, a recorded
/// `sys.powerctl` command is dropped (init `CheckShutdown` returns nothing
/// while `IsShuttingDown()`), and a queued control message is answered
/// without a call by dropping its reply, with the reply a request read
/// after the stop gets ([`HandlerShared::reply_without_call`]). init
/// leaves such a message in its queue and never answers it (`if
/// (!IsShuttingDown()) HandleControlMessages()`); here the client is told
/// at once.
///
/// The task ends when the queue is closed: after the call it is making,
/// and after a `sys.powerctl` command recorded before the close. The
/// control messages still queued then are refused, again by dropping
/// their replies; a runtime that drops this task drops the queue with it,
/// to the same effect. `ended` is dropped last, after both handlers, so
/// the service holds no clone of a handler once
/// [`HandlerCallsDrain::wait`] has returned. The replies the host holds
/// are not waited for.
async fn handler_worker(
    shared: Arc<HandlerShared>,
    control: Option<crate::ControlHandler>,
    powerctl: Option<crate::PowerctlHandler>,
    ended: tokio::sync::watch::Sender<()>,
) {
    loop {
        match shared.next_work() {
            HandlerWork::Powerctl(message) => {
                // Recorded only with a powerctl handler.
                let Some(handler) = powerctl.clone() else {
                    continue;
                };
                if shared.stop.is_stopped() {
                    info!(
                        "Handler calls are stopped; dropping a recorded {} command",
                        crate::POWERCTL_PROPERTY
                    );
                    continue;
                }
                // init logs the value here ("Received sys.powerctl='...'
                // from pid: ..."); values are not logged by this service,
                // and the handler gets the command.
                match tokio::task::spawn_blocking(move || handler(&message)).await {
                    // init `HandlePowerctlMessage`: `StopSendingMessages`
                    // once the command is one it acts on.
                    Ok(crate::PowerctlOutcome::ShuttingDown) => {
                        shared.stop.stop();
                        info!("The powerctl handler began a shutdown; handler calls are stopped");
                    }
                    Ok(crate::PowerctlOutcome::Ignored) => {
                        info!("The powerctl handler ignored the command");
                    }
                    Err(e) => error!("The powerctl handler call did not return: {e}"),
                }
            }
            HandlerWork::Control(ControlJob { message, reply }) => {
                // Queued only with a control handler.
                let Some(handler) = control.clone() else {
                    continue;
                };
                let action = message.action.clone();
                if shared.stop.is_stopped() {
                    info!("Handler calls are stopped; the queued ctl.{action} gets no call");
                    // Answers by the stop flag.
                    drop(reply);
                    continue;
                }
                // A handler that panics drops the reply it was given while
                // unwinding, which answers the client with a failure.
                if let Err(e) = tokio::task::spawn_blocking(move || handler(message, reply)).await {
                    error!("The control handler call for ctl.{action} did not return: {e}");
                }
            }
            HandlerWork::Closed(queued) => {
                for ControlJob { message, reply } in queued {
                    warn!(
                        "The service has stopped; the queued ctl.{} gets no call",
                        message.action
                    );
                    drop(reply);
                }
                break;
            }
            HandlerWork::Idle => shared.wake.notified().await,
        }
    }
    drop(control);
    drop(powerctl);
    drop(ended);
}

/// Waits for the [`handler_worker`] of one [`Handlers`]; kept by
/// `crate::ServiceHandle`.
pub(crate) struct HandlerCallsDrain(tokio::sync::watch::Receiver<()>);

impl HandlerCallsDrain {
    /// Returns once the worker has ended (at once when the [`Handlers`]
    /// has no handler and so no worker): no handler call is running, none
    /// begins later, and the worker has dropped both handlers.
    ///
    /// The worker ends after the queue is closed, which the
    /// [`SocketService`]'s `Drop` does, so this is called after the socket
    /// service's task has been joined. Called before that, it waits for
    /// the close as well. A worker that panicked or was dropped by its
    /// runtime has ended too.
    ///
    /// Cancel-safe: it changes nothing, so a later call waits again.
    pub(crate) async fn wait(&self) {
        let mut ended = self.0.clone();
        // The sender never sends: the only result is the error for a
        // dropped sender.
        while ended.changed().await.is_ok() {}
    }
}

impl Drop for SocketService {
    /// The connection tasks spawned by `on_idle` are detached, so they
    /// outlive the actor, each with a clone of `handlers`. A `ctl.*`
    /// request does not pass through the properties service, whose closed
    /// mailbox refuses every other set once the service has stopped;
    /// closing the queue makes [`SocketService::handle_control`] refuse a
    /// request that one of those tasks reads from now on. A
    /// `sys.powerctl` request does pass through the properties service,
    /// but that one is stopped after this actor has been dropped, so a
    /// request it checks in between is refused by
    /// [`SocketService::notify_powerctl`] in the same way. The handler
    /// call the worker is making is not affected and runs to completion,
    /// and so does the call for a `sys.powerctl` command recorded before
    /// this; [`HandlerCallsDrain::wait`] waits for both. Control messages
    /// still queued are refused without a call. A [`ControlReply`] that
    /// the host got from an earlier call is not affected either: it holds
    /// its client's connection itself and can be answered later.
    fn drop(&mut self) {
        self.handlers.close();
    }
}

impl SocketService {
    /// Handles a client connection. Generic over the receiving actor so
    /// tests can record forwarded requests; production passes
    /// `PropertiesService`.
    async fn handle_client<A>(
        mut stream: UnixStream,
        service: ActorRef<A>,
        handlers: Handlers,
    ) -> Result<()>
    where
        A: rsactor::Message<crate::PropertyMessage, Reply = i32>,
    {
        trace!("Handling new client connection");
        let peer = Peer::of(&stream);

        // Read the command (u32)
        let mut cmd_buf = [0u8; 4];
        if let Err(e) = stream.read_exact(&mut cmd_buf).await {
            // init answers `PROP_ERROR_READ_CMD` whenever the command word
            // cannot be read (`property_service.cpp` `RecvUint32` failure).
            // Best-effort: a peer that already closed gets nothing (EPIPE).
            let _ = Self::send_response(&mut stream, PROP_ERROR_READ_CMD).await;
            // Connect-then-close without writing (port probes, health
            // checks) is routine — not worth an `error!` in the caller.
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                debug!("Client closed the connection before sending a command");
                return Ok(());
            }
            return Err(e.into());
        }
        let cmd = u32::from_ne_bytes(cmd_buf);

        debug!("Received command: 0x{cmd:08X}");

        match cmd {
            // The V1 command is not supported, unlike in init, which still
            // handles it. It gets the reply of an unknown command, and the
            // rest of its frame (name and value) is not read. A V1 client
            // does not read a reply (bionic waits for the socket to close
            // and then reports success), so this log line is the only
            // place the refusal shows.
            PROP_MSG_SETPROP => {
                warn!(
                    "Refused a V1 set request (PROP_MSG_SETPROP) from {peer}: this service \
                     handles PROP_MSG_SETPROP2 only, and nothing was set"
                );
                Self::send_response(&mut stream, PROP_ERROR_INVALID_CMD).await?;
            }
            PROP_MSG_SETPROP2 => {
                trace!("Processing SETPROP2 command");
                // By value: a `ctl.*` request hands the connection over to
                // its `ControlReply`.
                Self::handle_setprop2(stream, service, handlers, peer).await?;
            }
            _ => {
                warn!("Unknown command received: 0x{cmd:08X}");
                Self::send_response(&mut stream, PROP_ERROR_INVALID_CMD).await?;
            }
        }

        trace!("Client connection handled successfully");
        Ok(())
    }

    /// The response code for one decoded set whose name is not `ctl.*`
    /// (those go to [`Self::handle_control`]). The set goes to the
    /// properties service, whose reply is the code
    /// ([`crate::PropertiesService`]). `sys.powerctl` is one of these: the
    /// properties service checks it and does not store it, and a set that
    /// passed the checks then goes to the powerctl handler
    /// ([`Self::notify_powerctl`]).
    ///
    /// The `sys.powerctl` path runs in a task of its own. `CLIENT_TIMEOUT`
    /// drops the connection task's future while `ask` waits, but the set
    /// is already in the properties service's mailbox and is checked
    /// later; the task still records an accepted command, as init records
    /// every set it has read.
    async fn dispatch<A>(
        name: String,
        value: String,
        peer: Peer,
        service: ActorRef<A>,
        handlers: Handlers,
    ) -> i32
    where
        A: rsactor::Message<crate::PropertyMessage, Reply = i32>,
    {
        if name != crate::POWERCTL_PROPERTY {
            return Self::ask_service(&service, crate::PropertyMessage { name, value }).await;
        }
        let task = tokio::spawn(async move {
            let command = value.clone();
            let code = Self::ask_service(&service, crate::PropertyMessage { name, value }).await;
            if code == PROP_SUCCESS {
                Self::notify_powerctl(command, peer, &handlers)
            } else {
                code
            }
        });
        task.await.unwrap_or_else(|e| {
            error!("The {} task failed: {e}", crate::POWERCTL_PROPERTY);
            PROP_ERROR_SET_FAILED
        })
    }

    /// The properties service's reply to `message`, or
    /// `PROP_ERROR_SET_FAILED` when it cannot be asked.
    async fn ask_service<A>(service: &ActorRef<A>, message: crate::PropertyMessage) -> i32
    where
        A: rsactor::Message<crate::PropertyMessage, Reply = i32>,
    {
        match service.ask(message).await {
            Ok(code) => code,
            Err(e) => {
                error!("Failed to send property message through channel: {e}");
                PROP_ERROR_SET_FAILED
            }
        }
    }

    /// init `PropertySet` → `PropertyChanged` → `trigger_shutdown` for a
    /// `sys.powerctl` set that the properties service accepted: records
    /// the command for the handler worker and returns `PROP_SUCCESS`
    /// without waiting for the handler call, as init answers once
    /// `TriggerShutdown` has recorded the command for its main loop. Like
    /// init's, the record is one slot: a set made before the worker took
    /// the previous command replaces it, and the handler is called for the
    /// later one only.
    ///
    /// Without a handler the reply is still `PROP_SUCCESS`: init's reply
    /// says that the set was accepted, not that anything acted on it (it
    /// answers `PROP_SUCCESS` for a command it then logs as unrecognized).
    /// A request checked after the socket service was dropped gets
    /// `PROP_ERROR_SET_FAILED`; init has no such case.
    ///
    /// Once the stop flag is set the reply is `PROP_SUCCESS` and nothing
    /// is recorded, with or without a handler: init `NotifyPropertyChange`
    /// skips `PropertyChanged` while `accept_messages` is false, and
    /// `PropertySet` still returns `PROP_SUCCESS`.
    fn notify_powerctl(command: String, peer: Peer, handlers: &Handlers) -> i32 {
        let shared = &handlers.shared;
        if shared.stop.is_stopped() {
            info!(
                "Handler calls are stopped; the {} set from {peer} was accepted and has no \
                 effect",
                crate::POWERCTL_PROPERTY
            );
            return PROP_SUCCESS;
        }
        if !shared.has_powerctl {
            warn!(
                "No powerctl handler; the {} set from {peer} was accepted and has no effect",
                crate::POWERCTL_PROPERTY
            );
            return PROP_SUCCESS;
        }
        let message = crate::PowerctlMessage {
            command,
            pid: peer.pid,
            uid: peer.uid,
            gid: peer.gid,
        };
        if !shared.queue_powerctl(message) {
            // Closed when the socket service is dropped (`Drop`).
            warn!(
                "The service has stopped; refusing {} from {peer}",
                crate::POWERCTL_PROPERTY
            );
            return PROP_ERROR_SET_FAILED;
        }
        info!(
            "Recorded {} from {peer} for the powerctl handler",
            crate::POWERCTL_PROPERTY
        );
        PROP_SUCCESS
    }

    /// init `CheckPermissions` + `SendControlMessage` for `ctl.<action>` =
    /// `<target>`, which came on `stream`: an illegal name is
    /// `PROP_ERROR_INVALID_NAME`, and a request without a handler gets
    /// `PROP_ERROR_HANDLE_CONTROL_MESSAGE`. Any other request is queued
    /// for the handler worker together with its connection, and this
    /// returns without a reply: the [`ControlReply`] that the handler gets
    /// sends it, when the host answers or drops it. init's
    /// `SendControlMessage` releases the client's socket to the queued
    /// message in the same way. The connection task ends with this call,
    /// so a request that waits for its reply holds no connection permit
    /// and `CLIENT_TIMEOUT` does not apply to the wait.
    ///
    /// A request that finds `MAX_UNANSWERED_CONTROL_MESSAGES` unanswered
    /// gets `PROP_ERROR_HANDLE_CONTROL_MESSAGE`, as init's
    /// `SendControlMessage` answers when `QueueControlMessage` fails, and
    /// so does a request read after the socket service was dropped.
    ///
    /// Once the stop flag is set no call begins, with or without a
    /// handler, and the reply is init's for `accept_messages` false
    /// (`SendControlMessage`): `PROP_SUCCESS` for `ctl.stop` ("If we're
    /// already shutting down and you're asking us to stop something, just
    /// say we did"), `PROP_ERROR_HANDLE_CONTROL_MESSAGE` for every other
    /// action. init compares the whole action with `stop`, so
    /// `ctl.interface_stop` and `ctl.sigstop_off` are refused. The name
    /// check comes first, as init's `CheckPermissions` does.
    ///
    /// An error is a reply that could not be sent from here, or a
    /// connection that could not be handed over.
    async fn handle_control(
        mut stream: UnixStream,
        name: String,
        value: String,
        peer: Peer,
        handlers: &Handlers,
    ) -> Result<()> {
        if let Err(e) = validate_property_name(&name) {
            error!("Rejected control message: {e}");
            return Self::send_response(&mut stream, PROP_ERROR_INVALID_NAME).await;
        }
        let shared = &handlers.shared;
        let action = &name["ctl.".len()..];
        if shared.stop.is_stopped() {
            let code = shared.reply_without_call(action);
            info!("Handler calls are stopped; answering {name:?} with {code:#x}");
            return Self::send_response(&mut stream, code).await;
        }
        if !shared.has_control {
            warn!("No control handler; refusing {name:?}");
            return Self::send_response(&mut stream, PROP_ERROR_HANDLE_CONTROL_MESSAGE).await;
        }
        let message = crate::ControlMessage {
            action: action.to_owned(),
            target: value,
            pid: peer.pid,
            uid: peer.uid,
            gid: peer.gid,
        };
        // Dropping the stream on an error closes the connection without a
        // reply; the client reads the end of the stream.
        let fd = ReplyFd::new(stream)?;
        let (refusal, fd) = match shared.queue_control(message, fd) {
            Ok(()) => return Ok(()),
            Err(refused) => refused,
        };
        match refusal {
            // Closed when the socket service is dropped (`Drop`).
            QueueRefusal::Closed => warn!("The service has stopped; refusing {name:?}"),
            QueueRefusal::Full => warn!(
                "{MAX_UNANSWERED_CONTROL_MESSAGES} control messages are unanswered; refusing \
                 {name:?}"
            ),
        }
        fd.send(shared.reply_without_call(action))?;
        Ok(())
    }

    /// Handles SETPROP2 command: reads the request and answers it, or
    /// hands a `ctl.*` request and its connection over to
    /// [`Self::handle_control`].
    async fn handle_setprop2<A>(
        mut stream: UnixStream,
        service: ActorRef<A>,
        handlers: Handlers,
        peer: Peer,
    ) -> Result<()>
    where
        A: rsactor::Message<crate::PropertyMessage, Reply = i32>,
    {
        trace!("Handling SETPROP2 request");
        let (name, value) = Self::read_setprop2(&mut stream).await?;

        // Do NOT log the value; it may carry sensitive payloads. Names
        // are public on the wire (and surface in getprop output), but
        // values are not.
        debug!("Property value length: {} bytes", value.len());

        info!("Forwarding property: {name:?} ({} bytes)", value.len());

        // Forwarded whether or not the client is still connected, as AOSP
        // init applies every request it received in full and ignores the
        // result of sending the reply. A client that gave up waiting
        // (`TimedOut`) may therefore find its set applied; checking the
        // peer here would narrow that window, not close it.
        //
        // A `ctl.*` name goes to the control handler and never reaches the
        // properties service, so it is never stored (init:
        // `HandlePropertySet` sends it to `SendControlMessage`).
        if name.starts_with("ctl.") {
            return Self::handle_control(stream, name, value, peer, &handlers).await;
        }
        let code = Self::dispatch(name, value, peer, service, handlers).await;
        if code != PROP_SUCCESS {
            warn!("Property set was rejected: error code {code:#x}");
        }
        Self::send_response(&mut stream, code).await?;

        Ok(())
    }

    /// Reads the name and the value of a SETPROP2 request. A request that
    /// cannot be read or decoded is answered here, and the error returned.
    async fn read_setprop2(stream: &mut UnixStream) -> Result<(String, String)> {
        // Protocol consistency: every V2 failure this function returns
        // answers with a status code before closing — a bare connection
        // close left the client's `recv_i32` to diagnose an EOF instead of
        // a definite error. Not covered: `CLIENT_TIMEOUT` in `on_idle`
        // drops this future and closes the connection without a reply.
        // The codes are init's: a field that cannot be
        // read (including a length over the limit, which init's
        // `RecvString` also refuses) is `PROP_ERROR_READ_DATA`.
        // (`send_response` results are ignored: the peer may already be
        // gone, which is fine — the response is best-effort, and `?` would
        // replace the real error with a write failure.)
        let name_len = match Self::read_u32(stream).await {
            Ok(len) => len,
            Err(e) => {
                let _ = Self::send_response(stream, PROP_ERROR_READ_DATA).await;
                return Err(e);
            }
        };
        trace!("Name length: {name_len}");

        if name_len as usize > MAX_WIRE_NAME_LEN {
            error!("Name length too large: {name_len} (max {MAX_WIRE_NAME_LEN})");
            let _ = Self::send_response(stream, PROP_ERROR_READ_DATA).await;
            return Err(Error::FileValidation(format!(
                "Name length too large: {name_len}"
            )));
        }

        // init reads both fields (`RecvString` twice) before it checks
        // either, so a field that cannot be read (`PROP_ERROR_READ_DATA`)
        // is answered at once, while bytes that are not a valid string
        // (interior NUL, not UTF-8) are kept until both fields are in and
        // then checked in init's order: name first.
        let name = match Self::read_string(stream, name_len as usize).await {
            Err(e @ Error::Io(_)) => {
                let _ = Self::send_response(stream, PROP_ERROR_READ_DATA).await;
                return Err(e);
            }
            name => name,
        };

        // Read value length and value
        let value_len = match Self::read_u32(stream).await {
            Ok(len) => len,
            Err(e) => {
                let _ = Self::send_response(stream, PROP_ERROR_READ_DATA).await;
                return Err(e);
            }
        };
        trace!("Value length: {value_len}");

        if value_len as usize > MAX_WIRE_VALUE_LEN {
            error!("Value length too large: {value_len} (max {MAX_WIRE_VALUE_LEN})");
            // Best-effort — see the name-length branch above.
            let _ = Self::send_response(stream, PROP_ERROR_READ_DATA).await;
            return Err(Error::FileValidation(format!(
                "Value length too large: {value_len}"
            )));
        }

        let value = match Self::read_string(stream, value_len as usize).await {
            Err(e @ Error::Io(_)) => {
                let _ = Self::send_response(stream, PROP_ERROR_READ_DATA).await;
                return Err(e);
            }
            value => value,
        };
        let name = match name {
            Ok(name) => name,
            Err(e) => {
                let _ = Self::send_response(stream, PROP_ERROR_INVALID_NAME).await;
                return Err(e);
            }
        };
        debug!("Property name: {name:?}");
        let value = match value {
            Ok(value) => value,
            Err(e) => {
                // init's `IsLegalPropertyName` runs before any value check.
                let code = if validate_property_name(&name).is_err() {
                    PROP_ERROR_INVALID_NAME
                } else {
                    PROP_ERROR_INVALID_VALUE
                };
                let _ = Self::send_response(stream, code).await;
                return Err(e);
            }
        };
        Ok((name, value))
    }

    /// Reads a u32 value from the stream
    async fn read_u32(stream: &mut UnixStream) -> Result<u32> {
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await?;
        Ok(u32::from_ne_bytes(buf))
    }

    /// Reads a string of specified length from the stream
    async fn read_string(stream: &mut UnixStream, len: usize) -> Result<String> {
        if len == 0 {
            return Ok(String::new());
        }

        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;

        // Reject NUL bytes instead of truncating at the first one: V2
        // strings are length-prefixed and sent without a terminator
        // (bionic does the same), so a NUL inside the declared length is a
        // malformed frame. Truncating would silently retarget the write —
        // a name "a\0b" would become property "a" and *pass* the
        // downstream validators, which never see the NUL. AOSP init
        // likewise rejects such names (IsLegalPropertyName).
        if buf.contains(&0) {
            return Err(Error::InvalidArgument(
                "wire string contains an interior NUL byte".into(),
            ));
        }

        // `Utf8(e.utf8_error())`, not a `FromUtf8Error` passthrough: the
        // owned-error form retains the failed bytes — which here may be a
        // property *value* — and would leak them into any `{e:?}` log,
        // against this file's don't-log-values policy. `Utf8Error` keeps
        // the position diagnostics and the source chain without the bytes.
        String::from_utf8(buf).map_err(|e| Error::Utf8(e.utf8_error()))
    }

    /// Sends a status reply with `send(MSG_NOSIGNAL)`, so a peer that
    /// already closed yields `EPIPE` instead of `SIGPIPE`.
    ///
    /// tokio's `write_all` ends in std's `write`, which on Rust 1.77–1.86
    /// is a plain `write(2)`: a host process that leaves `SIGPIPE` at its
    /// default (a C or JNI host embedding this service) was killed by the
    /// signal. Same rule as the client's `ServiceWriter::send` in
    /// `rsproperties/src/system_property_set.rs`; here it is a readiness
    /// loop, and `CLIENT_TIMEOUT` around the handler bounds the wait. This
    /// crate is Linux-only (see the `compile_error!` in lib.rs), so
    /// `MSG_NOSIGNAL` needs no fallback.
    async fn send_response(stream: &mut UnixStream, response: i32) -> Result<()> {
        trace!("Sending response: {response}");
        let bytes = response.to_ne_bytes();
        let mut written = 0;
        while written < bytes.len() {
            stream.writable().await?;
            match stream.try_io(tokio::io::Interest::WRITABLE, || {
                rustix::net::send(stream.as_fd(), &bytes[written..], SendFlags::NOSIGNAL)
                    .map_err(std::io::Error::from)
            }) {
                Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into()),
                Ok(n) => written += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        trace!("Response sent successfully");
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncWriteExt;

    const PROPERTY: &str = PROPERTY_SERVICE_SOCKET_NAME;
    const SYSTEM: &str = PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME;

    /// Bytes a fixture adds below its dir for its longest socket path:
    /// `/<subdir>/` and the system temp name (the longer of the two).
    pub(crate) fn socket_suffix_len(subdir: &str) -> usize {
        subdir.len() + 2 + temp_name(SYSTEM).len()
    }

    /// Path `<base>/<label>_<pid>` for a socket test fixture whose longest
    /// path adds `suffix_len` bytes. std's `UnixStream::connect` and
    /// `UnixListener::bind` need room for the NUL in the 108-byte
    /// `sun_path`, so the full path must stay under 108 bytes. `base` is
    /// `temp_dir()` when that fits; otherwise `/tmp`, used only if the dir
    /// can be created there (it is removed again; the caller creates it).
    /// Panics with the reason when neither works.
    pub(crate) fn socket_test_dir(label: &str, suffix_len: usize) -> PathBuf {
        const SUN_PATH_LEN: usize = 108;
        let name = format!("{label}_{}", std::process::id());
        let fits = |dir: &Path| dir.as_os_str().len() + suffix_len < SUN_PATH_LEN;
        let preferred = std::env::temp_dir().join(&name);
        if fits(&preferred) {
            return preferred;
        }
        let fallback = Path::new("/tmp").join(&name);
        let reason = if fits(&fallback) {
            let _ = std::fs::remove_dir_all(&fallback);
            match std::fs::create_dir(&fallback) {
                Ok(()) => {
                    std::fs::remove_dir(&fallback).unwrap();
                    return fallback;
                }
                Err(e) => format!("creating {} failed: {e}", fallback.display()),
            }
        } else {
            format!("{} is too long as well", fallback.display())
        };
        panic!(
            "socket fixture {} plus {suffix_len} bytes reaches the {SUN_PATH_LEN}-byte \
             sun_path limit (shorten TMPDIR); /tmp fallback: {reason}",
            preferred.display()
        );
    }

    /// Per-test socket dir with mode 0755, removed on drop.
    struct SocketDir(PathBuf);

    impl SocketDir {
        fn new(label: &str) -> Self {
            Self::at(socket_test_dir(label, socket_suffix_len("sockets")))
        }

        fn at(dir: PathBuf) -> Self {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self(dir)
        }
    }

    impl Drop for SocketDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Leaves a socket file at `path` with no listener behind it.
    fn plant_socket(path: &Path) {
        drop(std::os::unix::net::UnixListener::bind(path).unwrap());
    }

    const CHILD_ENV: &str = "RSPROPERTIES_SERVICE_TEST_CHILD";

    pub(crate) fn in_child() -> bool {
        std::env::var_os(CHILD_ENV).is_some()
    }

    /// Re-runs `test_path` alone in a child process, for tests that change
    /// process-global state (umask, SIGPIPE disposition); a regression
    /// fails or kills only the child.
    pub(crate) fn run_in_child(test_path: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test_path, "--exact", "--test-threads=1", "--nocapture"])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "child failed ({:?}): {stdout}{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        // Guard against a filter that matched nothing (vacuous pass).
        assert!(
            stdout.contains("1 passed"),
            "child did not run the test: {stdout}"
        );
    }

    /// Socket and directory modes come from the code, not the umask:
    /// under umask 000 created dirs are 0755 and the temp socket is 0660
    /// at `bind`; under umask 077 the socket is 0600 at `bind` and 0660 at
    /// its final name.
    #[test]
    fn modes_do_not_depend_on_umask() {
        if !in_child() {
            run_in_child("socket_service::tests::modes_do_not_depend_on_umask");
            return;
        }
        let dir = SocketDir::new("rsprops_svc_umask");

        rustix::process::umask(Mode::empty());
        let nested = dir.0.join("a").join("b");
        ensure_socket_dir(&nested).unwrap();
        assert_eq!(mode_of(&dir.0.join("a")), 0o755);
        assert_eq!(mode_of(&nested), 0o755);
        let dirfd = open_socket_dir(&dir.0).unwrap();
        let _open = bind_temp(&dir.0, &dirfd, "umask000").unwrap();
        assert_eq!(mode_of(&dir.0.join("umask000")), SOCKET_FILE_MODE);

        rustix::process::umask(Mode::from_raw_mode(0o077));
        let _narrow = bind_temp(&dir.0, &dirfd, "umask077").unwrap();
        assert_eq!(mode_of(&dir.0.join("umask077")), 0o600);
        let sockets = SocketDir::new("rsprops_svc_umask_sockets");
        let _bound = bind_service_sockets(&sockets.0).unwrap();
        assert_eq!(mode_of(&sockets.0.join(PROPERTY)), SOCKET_FILE_MODE);
        assert_eq!(mode_of(&sockets.0.join(SYSTEM)), SOCKET_FILE_MODE);
    }

    /// The mode is raised only on a socket reached without following a
    /// symlink. The euid half of the check cannot be tested: removing it
    /// changes nothing unless the socket belongs to another uid, which a
    /// test without a second uid cannot create.
    #[test]
    fn widen_refuses_symlink_and_non_socket() {
        let dir = SocketDir::new("rsprops_svc_widen");
        let victim = dir.0.join("victim");
        std::fs::write(&victim, b"").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&victim, dir.0.join("link")).unwrap();
        let dirfd = open_socket_dir(&dir.0).unwrap();

        assert!(widen_to_socket_mode(&dir.0, &dirfd, "link").is_err());
        assert!(widen_to_socket_mode(&dir.0, &dirfd, "victim").is_err());
        assert_eq!(mode_of(&victim), 0o600);
    }

    /// Kernels before 3.17 have no `/proc/thread-self`: an absent first
    /// `fd/` directory falls through to the next one (`/proc/self/fd`).
    #[test]
    fn chmod_opened_falls_back_when_thread_self_is_absent() {
        let dir = SocketDir::new("rsprops_svc_procfb");
        let sock = dir.0.join("s");
        plant_socket(&sock);
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
        let pfd = rustix::fs::open(
            &sock,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let st = rustix::fs::fstat(&pfd).unwrap();

        let fd_dirs = ["/proc/rsprops-absent-thread-self/fd", "/proc/self/fd"];
        chmod_opened(&pfd, &st, Mode::from_raw_mode(SOCKET_FILE_MODE), &fd_dirs).unwrap();
        assert_eq!(mode_of(&sock), SOCKET_FILE_MODE);

        // No `fd/` directory at all (no `/proc`): an error, mode unchanged.
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(chmod_opened(
            &pfd,
            &st,
            Mode::from_raw_mode(SOCKET_FILE_MODE),
            &fd_dirs[..1]
        )
        .is_err());
        assert_eq!(mode_of(&sock), 0o600);
    }

    /// A temp socket refuses connections until it listens, so nobody
    /// connects while its mode may still be raised.
    #[test]
    fn temp_socket_refuses_connections_until_listen() {
        let dir = SocketDir::new("rsprops_svc_listen");
        let dirfd = open_socket_dir(&dir.0).unwrap();
        let tmp = temp_name(PROPERTY);
        let fd = bind_temp(&dir.0, &dirfd, &tmp).unwrap();

        let err = std::os::unix::net::UnixStream::connect(dir.0.join(&tmp)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
        rustix::net::listen(&fd, -1).unwrap();
        std::os::unix::net::UnixStream::connect(dir.0.join(&tmp)).unwrap();
    }

    /// When the second socket cannot be bound (its temp path exceeds
    /// `sun_path`), the first is not renamed over an existing socket and
    /// no temp is left behind.
    #[test]
    fn second_bind_failure_leaves_no_partial_state() {
        // rustix `SocketAddrUnix::new` accepts up to 108 path bytes. Pick
        // the dir length so the property temp path is 103 bytes (5 below
        // the limit); the system temp name is 11 bytes longer (114).
        const SUN_PATH_LIMIT: usize = 108;
        let dir_len = SUN_PATH_LIMIT - 5 - 1 - temp_name(PROPERTY).len();
        // The suffix makes `<base>_` shorter than `dir_len`.
        let base = socket_test_dir("rsprops_svc_len", SUN_PATH_LIMIT + 1 - dir_len);
        let base = format!("{}_", base.to_str().unwrap());
        assert!(base.len() < dir_len, "temp dir path too long: {base}");
        let dir = SocketDir::at(PathBuf::from(format!(
            "{base}{}",
            "x".repeat(dir_len - base.len())
        )));
        assert_eq!(dir.0.join(temp_name(PROPERTY)).as_os_str().len(), 103);
        assert_eq!(dir.0.join(temp_name(SYSTEM)).as_os_str().len(), 114);

        let existing = dir.0.join(PROPERTY);
        plant_socket(&existing);
        let ino = std::fs::symlink_metadata(&existing).unwrap().ino();

        assert!(bind_service_sockets(&dir.0).is_err());
        assert_eq!(std::fs::symlink_metadata(&existing).unwrap().ino(), ino);
        assert_eq!(names_in(&dir.0), [PROPERTY]);
    }

    /// Both final names carry `SOCKET_FILE_MODE`, nothing else is left in
    /// the dir, and the listeners accept connections through tokio.
    #[tokio::test]
    async fn bound_sockets_have_final_mode_and_no_leftover() {
        let dir = SocketDir::new("rsprops_svc_bind_final");
        let path = dir.0.clone();
        let bound = tokio::task::spawn_blocking(move || bind_service_sockets(&path))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(mode_of(&dir.0.join(PROPERTY)), SOCKET_FILE_MODE);
        assert_eq!(mode_of(&dir.0.join(SYSTEM)), SOCKET_FILE_MODE);
        assert_eq!(names_in(&dir.0), [PROPERTY, SYSTEM]);

        let property = UnixListener::from_std(bound.property).unwrap();
        let system = UnixListener::from_std(bound.system).unwrap();
        let _client = UnixStream::connect(dir.0.join(PROPERTY)).await.unwrap();
        property.accept().await.unwrap();
        let _client = UnixStream::connect(dir.0.join(SYSTEM)).await.unwrap();
        system.accept().await.unwrap();
    }

    /// The sweep removes only sockets with this service's temp prefixes.
    #[test]
    fn sweep_removes_only_own_temp_sockets() {
        let dir = SocketDir::new("rsprops_svc_sweep");
        plant_socket(&dir.0.join(".property_service.tmp-4294967295"));
        plant_socket(&dir.0.join(".property_service_for_system.tmp-1"));
        std::fs::write(dir.0.join(".property_service.tmp-2"), b"").unwrap();
        plant_socket(&dir.0.join(".other.tmp-3"));

        sweep_stale_temps(&open_socket_dir(&dir.0).unwrap());

        assert_eq!(
            names_in(&dir.0),
            [".other.tmp-3", ".property_service.tmp-2"]
        );
    }

    /// After the socket dir's path is swapped for another directory, no
    /// unlink, rename or chmod reaches the new directory: startup fails, a
    /// file there named like a final socket keeps its inode and content,
    /// and the opened directory gets no final names. The temp socket
    /// `bind` created through the path is left in the new directory.
    #[test]
    fn operations_stay_in_opened_socket_dir() {
        let base = SocketDir::new("rsprops_svc_swap");
        let opened = base.0.join("sockets");
        let moved = base.0.join("moved");
        std::fs::create_dir(&opened).unwrap();
        let dirfd = open_socket_dir(&opened).unwrap();
        std::fs::rename(&opened, &moved).unwrap();
        std::fs::create_dir(&opened).unwrap();
        let decoy = opened.join(PROPERTY);
        std::fs::write(&decoy, b"decoy").unwrap();
        let ino = std::fs::symlink_metadata(&decoy).unwrap().ino();

        assert!(bind_service_sockets_at(&opened, dirfd).is_err());

        assert_eq!(std::fs::symlink_metadata(&decoy).unwrap().ino(), ino);
        assert_eq!(std::fs::read(&decoy).unwrap(), b"decoy");
        assert!(names_in(&moved).is_empty(), "{:?}", names_in(&moved));
        assert_eq!(
            names_in(&opened),
            [temp_name(PROPERTY), PROPERTY.to_owned()]
        );
    }

    /// Dropping the result of a start that never reached `SocketService`
    /// (failed registration, panic, cancellation) unlinks the final names.
    #[test]
    fn bound_sockets_guard_unlinks_on_drop() {
        let dir = SocketDir::new("rsprops_svc_guard");
        let bound = bind_service_sockets(&dir.0).unwrap();
        assert_eq!(names_in(&dir.0), [PROPERTY, SYSTEM]);
        drop(bound);
        assert!(names_in(&dir.0).is_empty(), "{:?}", names_in(&dir.0));
    }

    #[test]
    fn socket_dir_warning_modes() {
        let warn_for = |mode| socket_dir_warning(Mode::from_raw_mode(mode));
        assert!(warn_for(0o777).is_some());
        assert!(warn_for(0o1777).is_none());
        assert!(warn_for(0o775).is_none());
        assert!(warn_for(0o755).is_none());
    }

    /// A reply to a peer that already closed is `EPIPE`, not `SIGPIPE`,
    /// even with `SIGPIPE` at `SIG_DFL` (C/JNI hosts). libtest ignores
    /// `SIGPIPE`, so the check runs in a child that restores the default.
    #[test]
    fn send_response_to_closed_peer_is_epipe_not_sigpipe() {
        if !in_child() {
            run_in_child(
                "socket_service::tests::send_response_to_closed_peer_is_epipe_not_sigpipe",
            );
            return;
        }
        // SAFETY: `libc::signal` is the libc crate's declaration of C
        // `signal` for this target. `SIG_DFL` installs no handler, so no
        // Rust code runs in signal context. The disposition is
        // process-wide; this child process runs only this test (`--exact`).
        let prev = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        assert_ne!(prev, libc::SIG_ERR, "signal(SIGPIPE, SIG_DFL) failed");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (mut server, client) = UnixStream::pair().unwrap();
            drop(client);
            let err = SocketService::send_response(&mut server, PROP_ERROR_SET_FAILED)
                .await
                .expect_err("peer-closed socket must error");
            assert!(
                matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::BrokenPipe),
                "unexpected error: {err}"
            );
        });
    }

    /// Counts forwarded requests in place of `PropertiesService`, and
    /// refuses the value [`REFUSED_VALUE`].
    struct Recorder(Arc<AtomicUsize>);

    /// The value [`Recorder`] answers with `PROP_ERROR_INVALID_VALUE`.
    const REFUSED_VALUE: &str = "refused-by-recorder";

    impl Actor for Recorder {
        type Args = Arc<AtomicUsize>;
        type Error = std::convert::Infallible;
        type IdleEvent = ();

        async fn on_start(
            count: Self::Args,
            _actor_ref: &ActorRef<Self>,
        ) -> std::result::Result<Self, Self::Error> {
            Ok(Self(count))
        }
    }

    impl rsactor::Message<crate::PropertyMessage> for Recorder {
        type Reply = i32;

        async fn handle(
            &mut self,
            message: crate::PropertyMessage,
            _actor_ref: &ActorRef<Self>,
        ) -> i32 {
            self.0.fetch_add(1, Ordering::SeqCst);
            if message.value == REFUSED_VALUE {
                return PROP_ERROR_INVALID_VALUE;
            }
            PROP_SUCCESS
        }
    }

    /// Sends `frame`, runs one `handle_client`, and returns the reply code.
    async fn exchange(
        frame: &[u8],
        service: ActorRef<Recorder>,
        control: Option<crate::ControlHandler>,
    ) -> i32 {
        exchange_with(frame, service, Handlers::new(control, None)).await
    }

    /// [`exchange`] with both handlers.
    async fn exchange_with(frame: &[u8], service: ActorRef<Recorder>, handlers: Handlers) -> i32 {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(frame).await.unwrap();
        let _ = SocketService::handle_client(server, service, handlers.clone()).await;
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        // Kept until the reply is read, as the socket service keeps its
        // clone: a `ctl.*` request is only queued when `handle_client`
        // returns, and dropping the last clone closes the queue.
        drop(handlers);
        i32::from_ne_bytes(reply)
    }

    fn recorder() -> (ActorRef<Recorder>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let (actor_ref, _join) = rsactor::spawn::<Recorder>(count.clone());
        (actor_ref, count)
    }

    fn v2_frame(name: &str, value: &str) -> Vec<u8> {
        let mut frame = PROP_MSG_SETPROP2.to_ne_bytes().to_vec();
        frame.extend_from_slice(&(name.len() as u32).to_ne_bytes());
        frame.extend_from_slice(name.as_bytes());
        frame.extend_from_slice(&(value.len() as u32).to_ne_bytes());
        frame.extend_from_slice(value.as_bytes());
        frame
    }

    /// A fully received V2 request is applied even though its client
    /// closed the connection first (AOSP init parity).
    #[tokio::test]
    async fn v2_request_from_closed_client_is_applied() {
        let (service, count) = recorder();
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .write_all(&v2_frame("test.k3.closed", "1"))
            .await
            .unwrap();
        drop(client);

        // The reply then fails with EPIPE; only the forwarding matters.
        let _ = SocketService::handle_client(server, service, Handlers::new(None, None)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn v2_request_from_live_client_gets_success_reply() {
        let (service, count) = recorder();
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .write_all(&v2_frame("test.k3.live", "1"))
            .await
            .unwrap();

        SocketService::handle_client(server, service, Handlers::new(None, None))
            .await
            .unwrap();
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(i32::from_ne_bytes(reply), PROP_SUCCESS);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// init's codes for frames that never reach the properties service.
    #[tokio::test]
    async fn protocol_errors_get_init_codes() {
        let (service, count) = recorder();
        let unknown = 0x1234u32.to_ne_bytes();
        assert_eq!(
            exchange(&unknown, service.clone(), None).await,
            PROP_ERROR_INVALID_CMD
        );
        // A whole V1 frame (command word, `PROP_NAME_MAX` name bytes,
        // `PROP_VALUE_MAX` value bytes): refused like an unknown command,
        // where init applies it.
        let mut v1 = PROP_MSG_SETPROP.to_ne_bytes().to_vec();
        for (field, len) in [
            ("test.v1.refused", rsproperties::wire::PROP_NAME_MAX),
            ("1", rsproperties::wire::PROP_VALUE_MAX),
        ] {
            let mut buf = vec![0u8; len];
            buf[..field.len()].copy_from_slice(field.as_bytes());
            v1.extend_from_slice(&buf);
        }
        assert_eq!(
            exchange(&v1, service.clone(), None).await,
            PROP_ERROR_INVALID_CMD
        );
        assert_eq!(
            exchange(&v2_frame("a\0b", "1"), service.clone(), None).await,
            PROP_ERROR_INVALID_NAME
        );
        assert_eq!(
            exchange(&v2_frame("test.nul", "a\0b"), service.clone(), None).await,
            PROP_ERROR_INVALID_VALUE
        );
        let mut huge = PROP_MSG_SETPROP2.to_ne_bytes().to_vec();
        huge.extend_from_slice(&((MAX_WIRE_NAME_LEN + 1) as u32).to_ne_bytes());
        assert_eq!(
            exchange(&huge, service.clone(), None).await,
            PROP_ERROR_READ_DATA
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    /// `ctl.*` never reaches the properties service: without a handler it
    /// is refused, with one the reply the handler sends decides. A handler
    /// that panics drops its reply, which is a failure.
    #[tokio::test]
    async fn control_messages_go_to_the_handler() {
        let (service, count) = recorder();
        assert_eq!(
            exchange(&v2_frame("ctl.start", "svc"), service.clone(), None).await,
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let handler: crate::ControlHandler =
            Arc::new(move |msg: crate::ControlMessage, reply: ControlReply| {
                let handled = msg.target == "known";
                log.lock().unwrap().push(msg);
                reply.send(handled);
            });
        assert_eq!(
            exchange(
                &v2_frame("ctl.restart", "known"),
                service.clone(),
                Some(handler.clone())
            )
            .await,
            PROP_SUCCESS
        );
        assert_eq!(
            exchange(
                &v2_frame("ctl.stop", "other"),
                service.clone(),
                Some(handler.clone())
            )
            .await,
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );
        assert_eq!(
            exchange(
                &v2_frame("ctl..bad", "x"),
                service.clone(),
                Some(handler.clone())
            )
            .await,
            PROP_ERROR_INVALID_NAME
        );
        let panicking: crate::ControlHandler =
            Arc::new(|_: crate::ControlMessage, _: ControlReply| panic!("boom"));
        assert_eq!(
            exchange(
                &v2_frame("ctl.start", "x"),
                service.clone(),
                Some(panicking)
            )
            .await,
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            (seen[0].action.as_str(), seen[0].target.as_str()),
            ("restart", "known")
        );
        assert_eq!(
            (seen[1].action.as_str(), seen[1].target.as_str()),
            ("stop", "other")
        );
        // The sender is this process: `SO_PEERCRED` of a socket pair.
        assert_eq!(seen[0].pid, Some(std::process::id() as i32));
        assert_eq!(
            (seen[0].uid, seen[0].gid),
            (
                Some(rustix::process::getuid().as_raw()),
                Some(rustix::process::getgid().as_raw())
            )
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "ctl.* must not be forwarded"
        );
    }

    /// A `sys.powerctl` set goes to the properties service for its checks
    /// and, when those pass, to the powerctl handler. The reply does not
    /// depend on the handler: it is `PROP_SUCCESS` without one and with
    /// one that panics.
    #[tokio::test]
    async fn powerctl_goes_to_the_handler_after_the_checks() {
        let (service, count) = recorder();
        let frame = |value: &str| v2_frame(crate::POWERCTL_PROPERTY, value);
        assert_eq!(
            exchange(&frame("reboot"), service.clone(), None).await,
            PROP_SUCCESS,
            "without a handler the set is accepted"
        );

        // Every call answers `Ignored`, so the calls are not stopped.
        let (calls, mut called) = tokio::sync::mpsc::unbounded_channel();
        let handler: crate::PowerctlHandler = Arc::new(move |msg: &crate::PowerctlMessage| {
            let _ = calls.send(msg.clone());
            if msg.command == "panic" {
                panic!("boom");
            }
            crate::PowerctlOutcome::Ignored
        });
        let handlers = Handlers::new(None, Some(handler));
        let drain = handlers.drain();
        // The sender is this process: `SO_PEERCRED` of a socket pair.
        let sent = |command: &str| {
            crate::PowerctlMessage::new(command, Some(std::process::id() as i32)).with_uid_gid(
                rustix::process::getuid().as_raw(),
                rustix::process::getgid().as_raw(),
            )
        };
        for (value, code) in [
            ("reboot,recovery", PROP_SUCCESS),
            (REFUSED_VALUE, PROP_ERROR_INVALID_VALUE),
            ("panic", PROP_SUCCESS),
            ("shutdown,userrequested", PROP_SUCCESS),
        ] {
            assert_eq!(
                exchange_with(&frame(value), service.clone(), handlers.clone()).await,
                code,
                "{value:?}"
            );
            if code != PROP_SUCCESS {
                continue;
            }
            // The reply does not wait for the call. Waiting for it here
            // keeps the next command from replacing this one.
            let message = tokio::time::timeout(Duration::from_secs(10), called.recv())
                .await
                .expect("the handler must be called")
                .expect("the handler must be alive");
            assert_eq!(
                message,
                sent(value),
                "a refused set must not reach the handler, and a panic must not stop later calls"
            );
        }
        // Another name with a powerctl command as its value is a plain set.
        // This drops the last clone of `handlers`, which ends the worker.
        assert_eq!(
            exchange_with(&v2_frame("test.other", "reboot"), service, handlers).await,
            PROP_SUCCESS
        );
        tokio::time::timeout(Duration::from_secs(10), drain.wait())
            .await
            .expect("the worker must end");
        assert_eq!(
            called.recv().await,
            None,
            "no other call, and the worker dropped the handler"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            6,
            "every sys.powerctl set must be checked by the properties service"
        );
    }

    /// A properties service that reports each set on `received`, then
    /// answers `PROP_SUCCESS` once `gate` has a permit for it.
    struct Held {
        gate: Arc<tokio::sync::Semaphore>,
        received: tokio::sync::mpsc::UnboundedSender<()>,
    }

    impl Actor for Held {
        type Args = (
            Arc<tokio::sync::Semaphore>,
            tokio::sync::mpsc::UnboundedSender<()>,
        );
        type Error = std::convert::Infallible;
        type IdleEvent = ();

        async fn on_start(
            (gate, received): Self::Args,
            _actor_ref: &ActorRef<Self>,
        ) -> std::result::Result<Self, Self::Error> {
            Ok(Self { gate, received })
        }
    }

    impl rsactor::Message<crate::PropertyMessage> for Held {
        type Reply = i32;

        async fn handle(
            &mut self,
            _message: crate::PropertyMessage,
            _actor_ref: &ActorRef<Self>,
        ) -> i32 {
            let _ = self.received.send(());
            self.gate.acquire().await.unwrap().forget();
            PROP_SUCCESS
        }
    }

    /// A `sys.powerctl` set whose connection task is dropped while the
    /// properties service holds it (as `CLIENT_TIMEOUT` drops it) still
    /// reaches the powerctl handler once the properties service accepts
    /// it.
    #[tokio::test]
    async fn powerctl_accepted_after_the_connection_ended_reaches_the_handler() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (received_tx, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (service, _join) = rsactor::spawn::<Held>((gate.clone(), received_tx));
        let (calls, mut called) = tokio::sync::mpsc::unbounded_channel();
        let handler: crate::PowerctlHandler = Arc::new(move |msg: &crate::PowerctlMessage| {
            let _ = calls.send(msg.command.clone());
            crate::PowerctlOutcome::Ignored
        });
        let handlers = Handlers::new(None, Some(handler));

        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .write_all(&v2_frame(crate::POWERCTL_PROPERTY, "reboot"))
            .await
            .unwrap();
        let exchange = SocketService::handle_client(server, service, handlers.clone());
        tokio::select! {
            _ = exchange => panic!("the exchange must wait for the properties service"),
            r = received.recv() => r.expect("the set must reach the properties service"),
        }
        // The exchange is dropped; the properties service now accepts.
        gate.add_permits(1);
        let command = tokio::time::timeout(Duration::from_secs(10), called.recv())
            .await
            .expect("the handler must be called")
            .expect("the handler must be alive");
        assert_eq!(command, "reboot");
        drop(handlers);
    }

    /// A control handler and a powerctl handler for the tests of
    /// [`handler_worker`]. Each call reports itself on `begun` as it
    /// begins. A control call for the target `block` then waits for one
    /// `release` send (or for `release` to be dropped), and one for the
    /// target `panic` panics; every other control call answers `true`
    /// before it returns. The powerctl handler answers `reboot` with
    /// `ShuttingDown` and every other command with `Ignored`.
    struct Gated {
        control: crate::ControlHandler,
        powerctl: crate::PowerctlHandler,
        begun: tokio::sync::mpsc::UnboundedReceiver<String>,
        release: std::sync::mpsc::Sender<()>,
        /// The highest number of calls that were running at once.
        most_at_once: Arc<AtomicUsize>,
    }

    fn gated() -> Gated {
        let (begun_tx, begun) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let running = Arc::new(AtomicUsize::new(0));
        let most_at_once = Arc::new(AtomicUsize::new(0));

        let (calls, running_in, most) = (begun_tx.clone(), running.clone(), most_at_once.clone());
        let control: crate::ControlHandler =
            Arc::new(move |msg: crate::ControlMessage, reply: ControlReply| {
                most.fetch_max(
                    running_in.fetch_add(1, Ordering::SeqCst) + 1,
                    Ordering::SeqCst,
                );
                let _ = calls.send(format!("ctl.{} {}", msg.action, msg.target));
                if msg.target == "block" {
                    let _ = gate.lock().unwrap().recv();
                }
                running_in.fetch_sub(1, Ordering::SeqCst);
                if msg.target == "panic" {
                    panic!("boom");
                }
                reply.send(true);
            });
        let most = most_at_once.clone();
        let powerctl: crate::PowerctlHandler = Arc::new(move |msg: &crate::PowerctlMessage| {
            most.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            let _ = begun_tx.send(format!("powerctl {}", msg.command));
            running.fetch_sub(1, Ordering::SeqCst);
            if msg.command == "reboot" {
                crate::PowerctlOutcome::ShuttingDown
            } else {
                crate::PowerctlOutcome::Ignored
            }
        });
        Gated {
            control,
            powerctl,
            begun,
            release,
            most_at_once,
        }
    }

    /// The next call that a [`Gated`] handler reported.
    async fn next_call(begun: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(10), begun.recv())
            .await
            .expect("a handler call must begin")
            .expect("the handlers must be alive")
    }

    /// Returns once `len` control messages are queued for the worker. Only
    /// for a worker that is held in a `block` call, which takes none.
    async fn queued(handlers: &Handlers, len: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while handlers.shared.pending().control.len() != len {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the control messages must be queued");
    }

    /// One `ctl.<action>` = `target` request and its client, in a task of
    /// their own: the request is made as a connection task makes it, and
    /// the task then waits for the reply as the client does. Aborting the
    /// task closes the client's end.
    fn control_call(
        handlers: &Handlers,
        action: &str,
        target: &str,
    ) -> tokio::task::JoinHandle<i32> {
        let (handlers, name, target) =
            (handlers.clone(), format!("ctl.{action}"), target.to_owned());
        tokio::spawn(async move {
            let (mut client, server) = UnixStream::pair().unwrap();
            SocketService::handle_control(server, name, target, Peer::default(), &handlers)
                .await
                .unwrap();
            let mut reply = [0u8; 4];
            client.read_exact(&mut reply).await.unwrap();
            i32::from_ne_bytes(reply)
        })
    }

    /// A control handler that passes each message and its reply on, as a
    /// host does that handles the messages in a task of its own.
    fn deferring() -> (
        crate::ControlHandler,
        tokio::sync::mpsc::UnboundedReceiver<(crate::ControlMessage, ControlReply)>,
    ) {
        let (pass, passed) = tokio::sync::mpsc::unbounded_channel();
        let control: crate::ControlHandler =
            Arc::new(move |msg: crate::ControlMessage, reply: ControlReply| {
                let _ = pass.send((msg, reply));
            });
        (control, passed)
    }

    /// Makes one `ctl.<action>` request and returns it with the reply that
    /// the [`deferring`] handler passed on for it.
    async fn held_call(
        handlers: &Handlers,
        passed: &mut tokio::sync::mpsc::UnboundedReceiver<(crate::ControlMessage, ControlReply)>,
        action: &str,
    ) -> (tokio::task::JoinHandle<i32>, ControlReply) {
        let call = control_call(handlers, action, "svc");
        let (message, reply) = tokio::time::timeout(Duration::from_secs(10), passed.recv())
            .await
            .expect("the handler must be called")
            .expect("the handler must be alive");
        assert_eq!(message.action, action);
        (call, reply)
    }

    /// `call` has not been answered: its client is still waiting.
    async fn assert_unanswered(call: &mut tokio::task::JoinHandle<i32>) {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), call)
                .await
                .is_err(),
            "the client must still be waiting for its reply"
        );
    }

    fn powerctl_set(handlers: &Handlers, command: &str) -> i32 {
        SocketService::notify_powerctl(command.to_owned(), Peer::default(), handlers)
    }

    /// The worker makes one handler call at a time, in the order the
    /// requests were queued (init handles control messages on its main
    /// thread). A request that finds `MAX_UNANSWERED_CONTROL_MESSAGES`
    /// unanswered (here one in its call and the rest queued) is refused
    /// without waiting, a queued request whose client has closed its
    /// connection is still passed to the handler, and a handler panic
    /// fails its own request only.
    #[tokio::test]
    async fn control_calls_are_serial_and_the_queue_is_bounded() {
        let Gated {
            control,
            mut begun,
            release,
            most_at_once,
            ..
        } = gated();
        let handlers = Handlers::new(Some(control), None);

        let blocked = control_call(&handlers, "start", "block");
        assert_eq!(next_call(&mut begun).await, "ctl.start block");
        // init `QueueControlMessage` drops a message that finds more than
        // 100 queued, so its queue holds up to 101 while its main thread
        // handles one.
        const QUEUED: usize = MAX_UNANSWERED_CONTROL_MESSAGES - 1;
        assert_eq!(QUEUED, 101);
        let mut waiting = Vec::new();
        for i in 0..QUEUED {
            waiting.push(control_call(&handlers, "start", &format!("job{i}")));
            queued(&handlers, i + 1).await;
        }
        assert_eq!(
            handlers.shared.replies.unanswered(),
            MAX_UNANSWERED_CONTROL_MESSAGES
        );
        let over = tokio::time::timeout(
            Duration::from_secs(10),
            control_call(&handlers, "start", "over"),
        )
        .await
        .expect("a request over the bound must not wait for the worker");
        assert_eq!(over.unwrap(), PROP_ERROR_HANDLE_CONTROL_MESSAGE);

        // The client of `job0` closes its connection before its turn.
        let gone = waiting.remove(0);
        gone.abort();
        assert!(gone.await.unwrap_err().is_cancelled());

        release.send(()).unwrap();
        assert_eq!(blocked.await.unwrap(), PROP_SUCCESS);
        for i in 0..QUEUED {
            assert_eq!(next_call(&mut begun).await, format!("ctl.start job{i}"));
        }
        for task in waiting {
            assert_eq!(task.await.unwrap(), PROP_SUCCESS);
        }

        assert_eq!(
            control_call(&handlers, "start", "panic").await.unwrap(),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );
        assert_eq!(
            control_call(&handlers, "stop", "svc").await.unwrap(),
            PROP_SUCCESS,
            "a panic must not end the worker"
        );
        assert_eq!(most_at_once.load(Ordering::SeqCst), 1);
        assert_eq!(handlers.shared.replies.unanswered(), 0);
    }

    /// The reply is the host's to send: the client waits after the handler
    /// call has returned, the next request reaches the handler while the
    /// first is unanswered, and the host answers in the order it chooses,
    /// also from a thread that has no runtime.
    #[tokio::test]
    async fn reply_is_sent_when_the_host_answers() {
        let (control, mut passed) = deferring();
        let handlers = Handlers::new(Some(control), None);

        let (mut first, first_reply) = held_call(&handlers, &mut passed, "start").await;
        assert_unanswered(&mut first).await;
        let (second, second_reply) = held_call(&handlers, &mut passed, "stop").await;
        assert_eq!(handlers.shared.replies.unanswered(), 2);
        assert_eq!(handlers.shared.pending().control.len(), 0);

        std::thread::spawn(move || second_reply.send(false))
            .join()
            .unwrap();
        assert_eq!(second.await.unwrap(), PROP_ERROR_HANDLE_CONTROL_MESSAGE);
        assert_unanswered(&mut first).await;
        first_reply.send(true);
        assert_eq!(first.await.unwrap(), PROP_SUCCESS);
        assert_eq!(handlers.shared.replies.unanswered(), 0);
    }

    /// A reply that the host drops is a failure. Once the handler calls
    /// are stopped, one for `ctl.stop` is a success, which is the reply a
    /// `ctl.stop` request read after the stop gets; a reply that the host
    /// sends is what the host said, before the stop and after it.
    #[tokio::test]
    async fn reply_dropped_by_the_host_answers_by_the_stop_flag() {
        let (control, mut passed) = deferring();
        let handlers = Handlers::new(Some(control), None);
        let stop = handlers.stop_switch();

        let (dropped, reply) = held_call(&handlers, &mut passed, "stop").await;
        drop(reply);
        assert_eq!(dropped.await.unwrap(), PROP_ERROR_HANDLE_CONTROL_MESSAGE);

        let (dropped_stop, dropped_stop_reply) = held_call(&handlers, &mut passed, "stop").await;
        let (dropped_start, dropped_start_reply) = held_call(&handlers, &mut passed, "start").await;
        let (refused_stop, refused_stop_reply) = held_call(&handlers, &mut passed, "stop").await;
        let (handled_start, handled_start_reply) = held_call(&handlers, &mut passed, "start").await;
        stop.stop();

        drop(dropped_stop_reply);
        assert_eq!(dropped_stop.await.unwrap(), PROP_SUCCESS);
        drop(dropped_start_reply);
        assert_eq!(
            dropped_start.await.unwrap(),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );
        refused_stop_reply.send(false);
        assert_eq!(
            refused_stop.await.unwrap(),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );
        handled_start_reply.send(true);
        assert_eq!(handled_start.await.unwrap(), PROP_SUCCESS);
        assert_eq!(handlers.shared.replies.unanswered(), 0);
    }

    /// The replies the host holds count toward
    /// `MAX_UNANSWERED_CONTROL_MESSAGES` although the queue is empty, and
    /// each answer makes room for one request.
    #[tokio::test]
    async fn replies_held_by_the_host_count_toward_the_bound() {
        let (control, mut passed) = deferring();
        let handlers = Handlers::new(Some(control), None);
        let refused = |handlers: &Handlers| {
            let call = control_call(handlers, "start", "over");
            async move {
                tokio::time::timeout(Duration::from_secs(10), call)
                    .await
                    .expect("a request over the bound must not wait for a reply")
                    .unwrap()
            }
        };

        let mut held = Vec::new();
        for _ in 0..MAX_UNANSWERED_CONTROL_MESSAGES {
            held.push(held_call(&handlers, &mut passed, "start").await);
        }
        assert_eq!(handlers.shared.pending().control.len(), 0);
        assert_eq!(refused(&handlers).await, PROP_ERROR_HANDLE_CONTROL_MESSAGE);
        assert!(
            passed.try_recv().is_err(),
            "the refused request must not reach the handler"
        );

        let (answered, reply) = held.pop().unwrap();
        reply.send(true);
        assert_eq!(answered.await.unwrap(), PROP_SUCCESS);
        held.push(held_call(&handlers, &mut passed, "start").await);
        assert_eq!(refused(&handlers).await, PROP_ERROR_HANDLE_CONTROL_MESSAGE);

        // A drop makes room as an answer does.
        let (dropped, reply) = held.pop().unwrap();
        drop(reply);
        assert_eq!(dropped.await.unwrap(), PROP_ERROR_HANDLE_CONTROL_MESSAGE);
        held.push(held_call(&handlers, &mut passed, "start").await);

        for (call, reply) in held {
            reply.send(true);
            assert_eq!(call.await.unwrap(), PROP_SUCCESS);
        }
        assert_eq!(handlers.shared.replies.unanswered(), 0);
    }

    /// A runtime that goes away answers the requests still queued (their
    /// replies are dropped with the queue) and leaves the replies the host
    /// holds usable: one held by a handler call that is still running, and
    /// one the host kept after its call returned.
    #[test]
    fn queued_requests_are_answered_and_held_replies_usable_without_the_runtime() {
        use std::io::Read;

        fn request(
            handlers: &Handlers,
            target: &str,
        ) -> impl std::future::Future<Output = std::os::unix::net::UnixStream> {
            let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            server.set_nonblocking(true).unwrap();
            let (handlers, target) = (handlers.clone(), target.to_owned());
            async move {
                let server = UnixStream::from_std(server).unwrap();
                SocketService::handle_control(
                    server,
                    "ctl.start".to_owned(),
                    target,
                    Peer::default(),
                    &handlers,
                )
                .await
                .unwrap();
                client
            }
        }
        fn read_code(mut client: std::os::unix::net::UnixStream) -> i32 {
            let mut code = [0u8; 4];
            client.read_exact(&mut code).unwrap();
            i32::from_ne_bytes(code)
        }

        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let (pass, mut passed) = tokio::sync::mpsc::unbounded_channel();
        let (begin, mut begun) = tokio::sync::mpsc::unbounded_channel();
        let control: crate::ControlHandler =
            Arc::new(move |msg: crate::ControlMessage, reply: ControlReply| {
                if msg.target == "block" {
                    let _ = begin.send(());
                    let _ = gate.lock().unwrap().recv();
                    reply.send(true);
                } else {
                    let _ = pass.send(reply);
                }
            });

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (handlers, kept_client, kept_reply, blocked_client, queued_client) =
            runtime.block_on(async {
                let handlers = Handlers::new(Some(control), None);
                let kept_client = request(&handlers, "kept").await;
                let kept_reply = passed.recv().await.unwrap();
                let blocked_client = request(&handlers, "block").await;
                begun.recv().await.unwrap();
                let queued_client = request(&handlers, "queued").await;
                queued(&handlers, 1).await;
                (
                    handlers,
                    kept_client,
                    kept_reply,
                    blocked_client,
                    queued_client,
                )
            });
        // Not a drop, which would wait for the blocked handler call.
        runtime.shutdown_background();
        // In the service every clone is owned by a task of the runtime.
        drop(handlers);

        assert_eq!(read_code(queued_client), PROP_ERROR_HANDLE_CONTROL_MESSAGE);
        release.send(()).unwrap();
        assert_eq!(read_code(blocked_client), PROP_SUCCESS);
        kept_reply.send(true);
        assert_eq!(read_code(kept_client), PROP_SUCCESS);
    }

    /// init `TriggerShutdown` keeps one command: a `sys.powerctl` set made
    /// before the handler was called for the previous one replaces it. The
    /// command is passed before the control messages that were queued
    /// earlier, as init's main loop handles it first.
    #[tokio::test]
    async fn powerctl_keeps_one_command_and_goes_before_control() {
        let Gated {
            control,
            powerctl,
            mut begun,
            release,
            most_at_once,
        } = gated();
        let handlers = Handlers::new(Some(control), Some(powerctl));

        let blocked = control_call(&handlers, "start", "block");
        assert_eq!(next_call(&mut begun).await, "ctl.start block");
        let next = control_call(&handlers, "start", "next");
        queued(&handlers, 1).await;
        assert_eq!(powerctl_set(&handlers, "first"), PROP_SUCCESS);
        assert_eq!(powerctl_set(&handlers, "second"), PROP_SUCCESS);

        release.send(()).unwrap();
        assert_eq!(next_call(&mut begun).await, "powerctl second");
        assert_eq!(next_call(&mut begun).await, "ctl.start next");
        assert_eq!(blocked.await.unwrap(), PROP_SUCCESS);
        assert_eq!(next.await.unwrap(), PROP_SUCCESS);
        assert_eq!(most_at_once.load(Ordering::SeqCst), 1);
    }

    /// A powerctl handler call that returns `ShuttingDown` stops the calls
    /// (init `HandlePowerctlMessage` → `StopSendingMessages`): the control
    /// messages queued behind it get init's shutdown replies without a
    /// call, and so does every later request.
    #[tokio::test]
    async fn shutting_down_outcome_stops_the_handler_calls() {
        let Gated {
            control,
            powerctl,
            mut begun,
            release,
            ..
        } = gated();
        let handlers = Handlers::new(Some(control), Some(powerctl));
        let (stop, drain) = (handlers.stop_switch(), handlers.drain());

        let blocked = control_call(&handlers, "start", "block");
        assert_eq!(next_call(&mut begun).await, "ctl.start block");
        let queued_stop = control_call(&handlers, "stop", "svc");
        queued(&handlers, 1).await;
        let queued_start = control_call(&handlers, "start", "svc");
        queued(&handlers, 2).await;
        assert_eq!(powerctl_set(&handlers, "reboot"), PROP_SUCCESS);
        assert!(!stop.is_stopped(), "the set itself does not stop the calls");

        release.send(()).unwrap();
        assert_eq!(next_call(&mut begun).await, "powerctl reboot");
        assert_eq!(blocked.await.unwrap(), PROP_SUCCESS);
        assert_eq!(queued_stop.await.unwrap(), PROP_SUCCESS);
        assert_eq!(
            queued_start.await.unwrap(),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );
        assert!(stop.is_stopped());

        assert_eq!(
            control_call(&handlers, "stop", "svc").await.unwrap(),
            PROP_SUCCESS
        );
        assert_eq!(
            control_call(&handlers, "restart", "svc").await.unwrap(),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );
        assert_eq!(powerctl_set(&handlers, "reboot"), PROP_SUCCESS);

        handlers.close();
        tokio::time::timeout(Duration::from_secs(10), drain.wait())
            .await
            .expect("the worker must end");
        assert_eq!(
            begun.recv().await,
            None,
            "no handler call may begin after the stop"
        );
    }

    /// Closing the queue (the socket service's `Drop`) refuses later
    /// requests and the control messages still queued. The worker ends
    /// after the call it was making and after the call for a
    /// `sys.powerctl` command recorded before the close, and
    /// [`HandlerCallsDrain::wait`] returns only then, with both handlers
    /// dropped.
    #[tokio::test]
    async fn close_ends_the_worker_after_its_calls() {
        let Gated {
            control,
            powerctl,
            mut begun,
            release,
            ..
        } = gated();
        let dropped = (Arc::downgrade(&control), Arc::downgrade(&powerctl));
        let handlers = Handlers::new(Some(control), Some(powerctl));
        let drain = handlers.drain();

        let blocked = control_call(&handlers, "start", "block");
        assert_eq!(next_call(&mut begun).await, "ctl.start block");
        let waiting = control_call(&handlers, "stop", "waiting");
        queued(&handlers, 1).await;
        assert_eq!(powerctl_set(&handlers, "recorded"), PROP_SUCCESS);

        handlers.close();
        assert_eq!(powerctl_set(&handlers, "late"), PROP_ERROR_SET_FAILED);
        assert_eq!(
            control_call(&handlers, "start", "late").await.unwrap(),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), drain.wait())
                .await
                .is_err(),
            "the wait must not return while a handler call is running"
        );

        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), drain.wait())
            .await
            .expect("the worker must end once its call has returned");
        assert!(
            dropped.0.upgrade().is_none() && dropped.1.upgrade().is_none(),
            "the worker must have dropped both handlers"
        );
        assert_eq!(blocked.await.unwrap(), PROP_SUCCESS);
        assert_eq!(
            waiting.await.unwrap(),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE,
            "the calls are not stopped, so the queued ctl.stop is refused"
        );
        assert_eq!(next_call(&mut begun).await, "powerctl recorded");
        assert_eq!(
            begun.recv().await,
            None,
            "the queued control message must not be passed to the handler"
        );
    }

    /// A [`Handlers`] that no [`SocketService`] was built from (a failed
    /// `on_start`) ends its worker when its last clone is dropped.
    #[tokio::test]
    async fn dropping_every_clone_ends_the_worker() {
        let Gated { control, .. } = gated();
        let handlers = Handlers::new(Some(control), None);
        let drain = handlers.drain();
        let clone = handlers.clone();
        drop(handlers);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), drain.wait())
                .await
                .is_err(),
            "a clone is still alive"
        );
        drop(clone);
        tokio::time::timeout(Duration::from_secs(10), drain.wait())
            .await
            .expect("the worker must end");
    }

    /// After [`HandlerCallsStop::stop`] the replies are init's for
    /// `accept_messages` false, with handlers and without: `ctl.stop`
    /// succeeds, every other `ctl.*` action is refused, an illegal name is
    /// still `PROP_ERROR_INVALID_NAME`, `sys.powerctl` is checked by the
    /// properties service and answered with its code, and other names are
    /// plain sets. No handler is called. The flag also decides the reply
    /// once the queue is closed.
    #[tokio::test]
    async fn stopped_handler_calls_get_init_shutdown_replies() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (control_calls, powerctl_calls) = (calls.clone(), calls.clone());
        let control: crate::ControlHandler =
            Arc::new(move |_: crate::ControlMessage, reply: ControlReply| {
                control_calls.fetch_add(1, Ordering::SeqCst);
                reply.send(true);
            });
        let powerctl: crate::PowerctlHandler = Arc::new(move |_: &crate::PowerctlMessage| {
            powerctl_calls.fetch_add(1, Ordering::SeqCst);
            crate::PowerctlOutcome::Ignored
        });
        const AFTER_STOP: [(&str, &str, i32); 8] = [
            ("ctl.stop", "svc", PROP_SUCCESS),
            ("ctl.start", "svc", PROP_ERROR_HANDLE_CONTROL_MESSAGE),
            ("ctl.restart", "svc", PROP_ERROR_HANDLE_CONTROL_MESSAGE),
            (
                "ctl.interface_stop",
                "svc",
                PROP_ERROR_HANDLE_CONTROL_MESSAGE,
            ),
            ("ctl..stop", "svc", PROP_ERROR_INVALID_NAME),
            (crate::POWERCTL_PROPERTY, "reboot", PROP_SUCCESS),
            (
                crate::POWERCTL_PROPERTY,
                REFUSED_VALUE,
                PROP_ERROR_INVALID_VALUE,
            ),
            ("test.other", "1", PROP_SUCCESS),
        ];

        for (label, handlers) in [
            ("handlers", Handlers::new(Some(control), Some(powerctl))),
            ("no handlers", Handlers::new(None, None)),
        ] {
            let (service, count) = recorder();
            let stop = handlers.stop_switch();
            let drain = handlers.drain();
            let reply = |name: &'static str, value: &'static str| {
                let (service, handlers) = (service.clone(), handlers.clone());
                async move { exchange_with(&v2_frame(name, value), service, handlers).await }
            };
            if !handlers.shared.has_control {
                assert_eq!(
                    reply("ctl.stop", "svc").await,
                    PROP_ERROR_HANDLE_CONTROL_MESSAGE,
                    "{label}: before the stop, ctl.stop without a handler is refused"
                );
            }

            stop.stop();
            // A second call changes nothing.
            stop.stop();
            for round in ["stopped", "stopped and closed"] {
                for (name, value, code) in AFTER_STOP {
                    assert_eq!(reply(name, value).await, code, "{label}, {round}: {name}");
                }
                handlers.close();
            }
            drain.wait().await;
            assert_eq!(
                count.load(Ordering::SeqCst),
                6,
                "{label}: sys.powerctl and other names still go to the properties service"
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no handler call may begin after the stop"
        );
    }

    /// init answers `PROP_ERROR_READ_CMD` when the command word cannot be
    /// read.
    #[tokio::test]
    async fn truncated_command_word_gets_read_cmd() {
        let (service, count) = recorder();
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&[0u8; 2]).await.unwrap();
        client.shutdown().await.unwrap();

        assert!(
            SocketService::handle_client(server, service, Handlers::new(None, None))
                .await
                .is_ok()
        );
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(i32::from_ne_bytes(reply), PROP_ERROR_READ_CMD);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    /// init reads both fields before checking either, and checks the name
    /// first: an illegal name wins over an invalid value, and a value that
    /// cannot be read wins over an invalid name.
    #[tokio::test]
    async fn v2_checks_follow_init_order() {
        let (service, count) = recorder();
        assert_eq!(
            exchange(&v2_frame(".bad", "a\0b"), service.clone(), None).await,
            PROP_ERROR_INVALID_NAME
        );
        let mut truncated = v2_frame("a\0b", "");
        truncated.truncate(truncated.len() - 4);
        truncated.extend_from_slice(&5u32.to_ne_bytes());
        truncated.extend_from_slice(b"ab");
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&truncated).await.unwrap();
        client.shutdown().await.unwrap();
        let _ = SocketService::handle_client(server, service, Handlers::new(None, None)).await;
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(i32::from_ne_bytes(reply), PROP_ERROR_READ_DATA);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    /// A V2 frame that ends inside the name-length field gets a
    /// `PROP_ERROR_READ_DATA` reply, like other unreadable frames.
    #[tokio::test]
    async fn v2_truncated_name_len_gets_error_reply() {
        let (service, count) = recorder();
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .write_all(&PROP_MSG_SETPROP2.to_ne_bytes())
            .await
            .unwrap();
        client.write_all(&[0u8; 2]).await.unwrap();
        client.shutdown().await.unwrap();

        assert!(
            SocketService::handle_client(server, service, Handlers::new(None, None))
                .await
                .is_err()
        );
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(i32::from_ne_bytes(reply), PROP_ERROR_READ_DATA);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
