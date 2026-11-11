// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

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
    PROP_MSG_SETPROP, PROP_MSG_SETPROP2, PROP_NAME_MAX, PROP_SUCCESS, PROP_VALUE_MAX,
};
use rsproperties::{
    Error, Result, PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME, PROPERTY_SERVICE_SOCKET_NAME,
};

use crate::properties_service::PropertiesService;
use crate::SetProtocol;

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
const MAX_CONCURRENT_CLIENTS: usize = 64;

/// Upper bound on accepted connections *waiting* for a handler permit.
/// Every waiting task holds an accepted `UnixStream` (one fd), so without
/// this cap a connect flood while all handler permits are taken would
/// accumulate fds until EMFILE and take the whole process down. Beyond
/// `MAX_CONCURRENT_CLIENTS + MAX_WAITING_CLIENTS`, new connections are
/// dropped immediately; well-behaved clients see ECONNRESET and retry.
const MAX_WAITING_CLIENTS: usize = 256;

/// Wall-clock timeout for an entire `handle_client` exchange. Trusted
/// clients (init, system services) complete well under this; untrusted or
/// stuck clients are torn down rather than tying up a task indefinitely.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on control handler calls running or queued for a
/// blocking-pool thread at once. A call that outlasts `CLIENT_TIMEOUT`
/// keeps its blocking-pool thread after its connection (and connection
/// permit) is gone, so `MAX_CONCURRENT_CLIENTS` does not bound them;
/// without this cap, repeated `ctl.*` sets to a slow handler fill tokio's
/// blocking pool, and the persistent file writes queued behind them stall
/// every set. The permit is taken before `spawn_blocking`
/// ([`ControlCalls`]), so a refused request uses no blocking thread. With
/// a host runtime whose `max_blocking_threads` is at most this cap, slow
/// calls can still fill the pool.
const MAX_CONCURRENT_CONTROL_CALLS: usize = 16;

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
    /// Handles `ctl.*` sets; `None` refuses them.
    pub(crate) control_handler: Option<crate::ControlHandler>,
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
    control: Option<ControlCalls>,
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
            control: args.control_handler.map(ControlCalls::new),
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
        //    MAX_WAITING_CLIENTS instead of growing until EMFILE;
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
        let control = self.control.clone();
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
                // it receives in full. Dropping at the client's budget
                // would also discard V1 requests whose clients (bionic:
                // success after a 250 ms poll) already reported them set.
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
                Self::handle_client(stream, connection_sender, control),
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

/// The service's control handler and the semaphore that caps its calls at
/// `MAX_CONCURRENT_CONTROL_CALLS`. [`SocketService::handle_control`] takes
/// a permit before `spawn_blocking` and moves it into the blocking
/// closure, so a call counts against the cap from the moment it is queued
/// for a blocking thread until `handler` returns or panics — not until its
/// connection ends — and a request over the cap is refused without using a
/// blocking thread.
#[derive(Clone)]
struct ControlCalls {
    handler: crate::ControlHandler,
    calls: Arc<Semaphore>,
}

impl ControlCalls {
    fn new(handler: crate::ControlHandler) -> Self {
        Self {
            handler,
            calls: Arc::new(Semaphore::new(MAX_CONCURRENT_CONTROL_CALLS)),
        }
    }
}

impl SocketService {
    /// Handles a client connection. Generic over the receiving actor so
    /// tests can record forwarded requests; production passes
    /// `PropertiesService`.
    async fn handle_client<A>(
        mut stream: UnixStream,
        service: ActorRef<A>,
        control: Option<ControlCalls>,
    ) -> Result<()>
    where
        A: rsactor::Message<crate::PropertyMessage, Reply = i32>,
    {
        trace!("Handling new client connection");
        let pid = stream.peer_cred().ok().and_then(|cred| cred.pid());

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
            PROP_MSG_SETPROP => {
                trace!("Processing SETPROP (V1) command");
                Self::handle_setprop_v1(&mut stream, service, control, pid).await?;
            }
            PROP_MSG_SETPROP2 => {
                trace!("Processing SETPROP2 command");
                Self::handle_setprop2(&mut stream, service, control, pid).await?;
            }
            _ => {
                warn!("Unknown command received: 0x{cmd:08X}");
                Self::send_response(&mut stream, PROP_ERROR_INVALID_CMD).await?;
            }
        }

        trace!("Client connection handled successfully");
        Ok(())
    }

    /// The response code for one decoded set. A `ctl.*` name goes to the
    /// control handler and never reaches the properties service, so it is
    /// never stored (init: `HandlePropertySet` sends it to
    /// `SendControlMessage`); any other name goes to the properties
    /// service, whose reply is the code ([`crate::PropertiesService`]).
    /// `protocol` is the command the set arrived in; the properties
    /// service stores a persistent name for V2 only.
    async fn dispatch<A>(
        name: String,
        value: String,
        protocol: SetProtocol,
        pid: Option<i32>,
        service: ActorRef<A>,
        control: Option<ControlCalls>,
    ) -> i32
    where
        A: rsactor::Message<crate::PropertyMessage, Reply = i32>,
    {
        if name.starts_with("ctl.") {
            return Self::handle_control(name, value, pid, control).await;
        }
        let message = crate::PropertyMessage {
            name,
            value,
            protocol,
        };
        match service.ask(message).await {
            Ok(code) => code,
            Err(e) => {
                error!("Failed to send property message through channel: {e}");
                PROP_ERROR_SET_FAILED
            }
        }
    }

    /// init `CheckPermissions` + `SendControlMessage` for `ctl.<action>` =
    /// `<target>`: an illegal name is `PROP_ERROR_INVALID_NAME`; without a
    /// handler, or when the handler returns `false` or panics, the reply is
    /// `PROP_ERROR_HANDLE_CONTROL_MESSAGE`. The handler runs on the
    /// blocking pool; a call that outlasts the connection's `CLIENT_TIMEOUT`
    /// still runs to completion, but its client gets no reply. A request
    /// beyond `MAX_CONCURRENT_CONTROL_CALLS` running or queued calls gets
    /// `PROP_ERROR_HANDLE_CONTROL_MESSAGE` without reaching the blocking
    /// pool ([`ControlCalls`]).
    async fn handle_control(
        name: String,
        value: String,
        pid: Option<i32>,
        control: Option<ControlCalls>,
    ) -> i32 {
        if let Err(e) = validate_property_name(&name) {
            error!("Rejected control message: {e}");
            return PROP_ERROR_INVALID_NAME;
        }
        let Some(control) = control else {
            warn!("No control handler; refusing {name:?}");
            return PROP_ERROR_HANDLE_CONTROL_MESSAGE;
        };
        let Ok(permit) = control.calls.try_acquire_owned() else {
            warn!("{MAX_CONCURRENT_CONTROL_CALLS} control handler calls are pending; refusing {name:?}");
            return PROP_ERROR_HANDLE_CONTROL_MESSAGE;
        };
        let handler = control.handler;
        let message = crate::ControlMessage {
            action: name["ctl.".len()..].to_owned(),
            target: value,
            pid,
        };
        match tokio::task::spawn_blocking(move || {
            // Released when the handler returns or panics.
            let _permit = permit;
            handler(&message)
        })
        .await
        {
            Ok(true) => {
                info!("Control message {name:?} handled");
                PROP_SUCCESS
            }
            Ok(false) => {
                warn!("Control message {name:?} failed");
                PROP_ERROR_HANDLE_CONTROL_MESSAGE
            }
            Err(e) => {
                error!("Control handler for {name:?} panicked: {e}");
                PROP_ERROR_HANDLE_CONTROL_MESSAGE
            }
        }
    }

    /// Handles the legacy V1 SETPROP command: after the already-consumed
    /// command word, a fixed-size payload of `PROP_NAME_MAX` name bytes and
    /// `PROP_VALUE_MAX` value bytes, both NUL-padded.
    ///
    /// V1 clients (bionic included) never read a status reply — closing the
    /// connection is the implicit ack — so failures are logged and the
    /// connection is closed without a response. Validation still happens in
    /// `PropertiesService::handle`, identical to the V2 path.
    ///
    /// Every V1 request is logged at `warn!`: init sets
    /// `ro.property_service.version` to 2 and so does this service, so a
    /// client that follows bionic's rule sends V2. A V1 frame comes from a
    /// client that does not read that property, and such a client cannot
    /// learn that its set was rejected, nor that a `persist.` /
    /// `next_boot.` value was not stored ([`SetProtocol::V1`]). init does
    /// not log an accepted V1 set.
    async fn handle_setprop_v1<A>(
        stream: &mut UnixStream,
        service: ActorRef<A>,
        control: Option<ControlCalls>,
        pid: Option<i32>,
    ) -> Result<()>
    where
        A: rsactor::Message<crate::PropertyMessage, Reply = i32>,
    {
        trace!("Handling SETPROP (V1) request");

        let mut name_buf = [0u8; PROP_NAME_MAX];
        stream.read_exact(&mut name_buf).await?;
        let mut value_buf = [0u8; PROP_VALUE_MAX];
        stream.read_exact(&mut value_buf).await?;
        // AOSP V1 parity: init forces the last byte of both fields to NUL
        // before use (`prop_name[PROP_NAME_MAX-1] = 0`), capping names at
        // 31 chars and values at 91 bytes. Without this a non-bionic client
        // could submit a 32-char name that AOSP would have truncated.
        name_buf[PROP_NAME_MAX - 1] = 0;
        value_buf[PROP_VALUE_MAX - 1] = 0;

        let name = Self::string_from_fixed(&name_buf)?;
        let value = Self::string_from_fixed(&value_buf)?;
        let peer = pid.map_or_else(|| "an unknown pid".to_owned(), |pid| format!("pid {pid}"));
        warn!(
            "V1 set request (PROP_MSG_SETPROP) for {name:?} ({} bytes) from {peer}: this legacy \
             command gives the client no result code, and a persist. / next_boot. value set \
             with it is not stored. The client should send PROP_MSG_SETPROP2",
            value.len()
        );

        // The property name was already logged by the `warn!` above.
        let code = Self::dispatch(name, value, SetProtocol::V1, pid, service, control).await;
        if code != PROP_SUCCESS {
            warn!("V1 property was rejected: error code {code:#x}");
        }

        Ok(())
    }

    /// Decodes a fixed-size NUL-padded V1 wire field into a `String`.
    ///
    /// Truncation at the first NUL is inherent to the V1 format — padding
    /// and interior NULs are indistinguishable in a fixed buffer, and
    /// bionic decodes identically (strlen). The rsproperties client
    /// rejects NUL-carrying names/values before sending; a foreign V1
    /// client sending them gets the same truncation AOSP would apply.
    fn string_from_fixed(buf: &[u8]) -> Result<String> {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        // `Utf8(e.utf8_error())`, not a `FromUtf8Error` passthrough: the
        // owned-error form retains the failed bytes — which here may be a
        // property *value* — and would leak them into any `{e:?}` log,
        // against this file's don't-log-values policy. `Utf8Error` keeps
        // the position diagnostics and the source chain without the bytes.
        String::from_utf8(buf[..end].to_vec()).map_err(|e| Error::Utf8(e.utf8_error()))
    }

    /// Handles SETPROP2 command
    async fn handle_setprop2<A>(
        stream: &mut UnixStream,
        service: ActorRef<A>,
        control: Option<ControlCalls>,
        pid: Option<i32>,
    ) -> Result<()>
    where
        A: rsactor::Message<crate::PropertyMessage, Reply = i32>,
    {
        trace!("Handling SETPROP2 request");

        // Protocol consistency: every V2 failure after the command word
        // answers with a status code before closing — a bare connection
        // close left the client's `recv_i32` to diagnose an EOF instead of
        // a definite error. The codes are init's: a field that cannot be
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
        let code = Self::dispatch(name, value, SetProtocol::V2, pid, service, control).await;
        if code != PROP_SUCCESS {
            warn!("Property set was rejected: error code {code:#x}");
        }
        Self::send_response(stream, code).await?;

        Ok(())
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

        // See `string_from_fixed`: drop the failed bytes (possibly a
        // sensitive value), keep the positional diagnostics + source chain.
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

    fn in_child() -> bool {
        std::env::var_os(CHILD_ENV).is_some()
    }

    /// Re-runs `test_path` alone in a child process, for tests that change
    /// process-global state (umask, SIGPIPE disposition); a regression
    /// fails or kills only the child.
    fn run_in_child(test_path: &str) {
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

    /// Counts forwarded requests in place of `PropertiesService`.
    struct Recorder(Arc<AtomicUsize>);

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
            _message: crate::PropertyMessage,
            _actor_ref: &ActorRef<Self>,
        ) -> i32 {
            self.0.fetch_add(1, Ordering::SeqCst);
            PROP_SUCCESS
        }
    }

    /// Sends `frame`, runs one `handle_client`, and returns the reply code.
    async fn exchange(
        frame: &[u8],
        service: ActorRef<Recorder>,
        control: Option<crate::ControlHandler>,
    ) -> i32 {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(frame).await.unwrap();
        let _ = SocketService::handle_client(server, service, control.map(ControlCalls::new)).await;
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
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
        let _ = SocketService::handle_client(server, service, None).await;
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

        SocketService::handle_client(server, service, None)
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
    /// is refused, with one the handler decides.
    #[tokio::test]
    async fn control_messages_go_to_the_handler() {
        let (service, count) = recorder();
        assert_eq!(
            exchange(&v2_frame("ctl.start", "svc"), service.clone(), None).await,
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let handler: crate::ControlHandler = Arc::new(move |msg: &crate::ControlMessage| {
            log.lock().unwrap().push(msg.clone());
            msg.target == "known"
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
        let panicking: crate::ControlHandler = Arc::new(|_: &crate::ControlMessage| panic!("boom"));
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
        assert_eq!(seen[0].pid, Some(std::process::id() as i32));
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "ctl.* must not be forwarded"
        );
    }

    /// At most `MAX_CONCURRENT_CONTROL_CALLS` handler calls are pending at
    /// once; one more is refused before it reaches the blocking pool, and
    /// the permits come back when the calls return. The runtime's pool has
    /// exactly `MAX_CONCURRENT_CONTROL_CALLS` threads, all held by blocked
    /// calls: a refusal that needed a blocking thread (the permit taken
    /// inside `spawn_blocking`) would wait in the pool's queue instead of
    /// returning.
    #[test]
    fn control_calls_are_capped_before_the_blocking_pool() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(MAX_CONCURRENT_CONTROL_CALLS)
            .enable_all()
            .build()
            .unwrap();
        let entered = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(std::sync::RwLock::new(()));
        let closed = gate.write().unwrap();
        let (counter, inner_gate) = (entered.clone(), gate.clone());
        let calls = ControlCalls::new(Arc::new(move |msg: &crate::ControlMessage| {
            counter.fetch_add(1, Ordering::SeqCst);
            if msg.target == "block" {
                drop(inner_gate.read().unwrap());
            }
            true
        }));
        let call = |target: &'static str, calls: ControlCalls| {
            SocketService::handle_control(
                "ctl.start".to_owned(),
                target.to_owned(),
                None,
                Some(calls),
            )
        };

        let blocked: Vec<_> = (0..MAX_CONCURRENT_CONTROL_CALLS)
            .map(|_| runtime.spawn(call("block", calls.clone())))
            .collect();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while entered.load(Ordering::SeqCst) < MAX_CONCURRENT_CONTROL_CALLS {
            assert!(std::time::Instant::now() < deadline, "calls did not start");
            std::thread::sleep(Duration::from_millis(1));
        }
        let over_cap = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), call("other", calls.clone())).await
        });
        assert_eq!(
            over_cap.expect("a call over the cap must not wait for a blocking thread"),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );
        assert_eq!(entered.load(Ordering::SeqCst), MAX_CONCURRENT_CONTROL_CALLS);

        drop(closed);
        for task in blocked {
            assert_eq!(runtime.block_on(task).unwrap(), PROP_SUCCESS);
        }
        assert_eq!(
            runtime.block_on(call("other", calls)),
            PROP_SUCCESS,
            "permits are released on return"
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

        assert!(SocketService::handle_client(server, service, None)
            .await
            .is_ok());
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
        let _ = SocketService::handle_client(server, service, None).await;
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

        assert!(SocketService::handle_client(server, service, None)
            .await
            .is_err());
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(i32::from_ne_bytes(reply), PROP_ERROR_READ_DATA);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
