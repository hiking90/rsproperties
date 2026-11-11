// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::io::prelude::*;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::{
    env,
    path::{Path, PathBuf},
};

use zerocopy::IntoBytes;
use zerocopy_derive::*;

use crate::errors::*;

const DEFAULT_SOCKET_DIR: &str = "/dev/socket";
pub const PROPERTY_SERVICE_SOCKET_NAME: &str = "property_service";
pub const PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME: &str = "property_service_for_system";

use crate::wire::{
    PROP_MSG_SETPROP, PROP_MSG_SETPROP2, PROP_NAME_MAX, PROP_SUCCESS, PROP_VALUE_MAX,
};

/// Global socket directory configuration
static SOCKET_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set the global socket directory for property services (internal use only).
/// This function can only be called once. Subsequent calls will be ignored.
///
/// Callers must hold `crate::GLOBAL_DIRS_LOCK` (see `lib::init`) so the
/// pre-check + set sequence stays atomic against the implicit latch in
/// [`socket_dir`].
///
/// # Arguments
/// * `dir` - The directory path where property service sockets are located
///
/// # Returns
/// * `true` if the directory was successfully set (first call)
/// * `false` if the directory was already set (subsequent calls)
pub(crate) fn set_socket_dir(dir: PathBuf) -> bool {
    SOCKET_DIR.set(dir).is_ok()
}

/// The socket directory once `set_socket_dir` has succeeded or
/// `socket_dir()` latched env/default; `None` before. Never latches. Used
/// by `lib::init` to compare a requested directory with the set one.
pub(crate) fn socket_dir_if_set() -> Option<&'static Path> {
    SOCKET_DIR.get().map(PathBuf::as_path)
}

/// Get the current socket directory.
/// Returns the configured socket directory, environment variable, or default.
///
/// Priority order:
/// 1. Directory set via `set_socket_dir()`
/// 2. `PROPERTY_SERVICE_SOCKET_DIR` environment variable
/// 3. Default directory: `/dev/socket`
pub fn socket_dir() -> &'static Path {
    // Lock-free once initialized; the first call takes `GLOBAL_DIRS_LOCK` so
    // the env/default latch cannot slip between `init`'s pre-check and
    // its `set_socket_dir` commit.
    if let Some(dir) = SOCKET_DIR.get() {
        return dir.as_path();
    }
    let _guard = crate::lock_global_dirs();
    SOCKET_DIR
        .get_or_init(|| {
            // `var_os`, not `var`: Unix paths are arbitrary bytes, and a
            // non-UTF-8 configured directory must be *used*, not silently
            // swapped for the default — the same
            // different-path-on-lossy-conversion hazard
            // `get_property_service_socket` documents.
            env::var_os("PROPERTY_SERVICE_SOCKET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_DIR))
        })
        .as_path()
}

/// Get the full path to the property service socket.
/// Returns `PathBuf` (not `String`): a lossy string conversion would make
/// the client connect to a *different* path when the configured directory
/// is not valid UTF-8.
fn get_property_service_socket() -> PathBuf {
    socket_dir().join(PROPERTY_SERVICE_SOCKET_NAME)
}

/// Get the full path to the system property service socket
fn get_property_service_for_system_socket() -> PathBuf {
    socket_dir().join(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME)
}

/// Bound on connect and send against the property service, and on the V2
/// response wait outside Android (see [`RESPONSE_TIMEOUT`]). The V1 path
/// additionally enforces its own (shorter) close-wait budget; this cap
/// exists so a stalled server — one that stopped accepting, never
/// responds, or stops draining our send — cannot block the caller's
/// thread forever, the exact hazard the V1 arm defends against with
/// `wait_for_socket_close`.
///
/// A connect or send timeout means the server did not receive the whole
/// request, so it cannot apply it. A response-wait timeout does not: the
/// request was sent in full, and both AOSP init and this crate's service
/// apply a fully received request whether or not the client still waits
/// for the reply. That `TimedOut` leaves the outcome unknown.
const SERVICE_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Budget for the V2 status reply after the request was sent in full.
///
/// Android: `None` — wait without a timeout, like bionic
/// `__system_property_set` (`recv(MSG_WAITALL)` with no timeout). init's
/// single property thread serves connections in order and may spend up to
/// 2 s receiving each one, and a `persist.` set with async persist writes
/// replies only after a 1 s sleep plus the file write, so a reply later
/// than 2 s is normal there. A client timeout would report failure for a
/// set that init still applies. The cost: a hung init blocks the caller,
/// as it blocks every bionic caller.
///
/// Elsewhere: [`SERVICE_IO_TIMEOUT`], unchanged from 0.6.0. The peer is
/// this crate's service, which receives each connection in its own
/// handler task and writes no persist files, so its reply takes 2 s only
/// when the service is stalled or out of handler slots; the bound keeps
/// such a service from blocking the caller's thread. The outcome of a set
/// that hits this timeout is unknown (see [`SERVICE_IO_TIMEOUT`]).
#[cfg(target_os = "android")]
const RESPONSE_TIMEOUT: Option<Duration> = None;
#[cfg(not(target_os = "android"))]
const RESPONSE_TIMEOUT: Option<Duration> = Some(SERVICE_IO_TIMEOUT);

/// Maps a read/write-timeout expiry to a clearly-labelled `TimedOut` error
/// (preserving the original as text); passes every other error through.
/// Shared by `recv_i32` and `ServiceWriter::send` so both directions of
/// the protocol report timeouts the same way.
fn map_timeout_err(e: std::io::Error, doing: &str) -> Error {
    if matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out {doing} ({SERVICE_IO_TIMEOUT:?}): {e}"),
        ))
    } else {
        Error::Io(e)
    }
}

/// Connects to a unix-domain socket with `timeout` as a hard bound on the
/// connect itself.
///
/// `UnixStream::connect` can block indefinitely when the server's listen
/// backlog is full (an AF_UNIX peculiarity: a listener that stopped
/// calling `accept()` parks new clients inside `connect`, *before* any
/// read/write timeout can apply). A non-blocking socket surfaces that
/// state as `EAGAIN`, which is retried with a short sleep until the
/// deadline; `EINPROGRESS` (possible per POSIX) is awaited with
/// `poll(POLLOUT)` + `SO_ERROR`.
fn connect_with_timeout(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    use rustix::event::{poll, PollFd, PollFlags};
    use rustix::io::Errno;
    use rustix::net as rnet;

    let timed_out = || {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out connecting to {path:?} ({timeout:?})"),
        )
    };

    let addr = rnet::SocketAddrUnix::new(path)?;
    // CLOEXEC: `UnixStream::connect` would set it automatically; going
    // through rustix for the non-blocking connect must not silently drop
    // that guarantee, or the fd leaks into children on fork/exec.
    // `not(macos)` rather than an allowlist so other unix targets (which
    // all support SOCK_CLOEXEC) keep compiling.
    #[cfg(not(target_os = "macos"))]
    let fd = rnet::socket_with(
        rnet::AddressFamily::UNIX,
        rnet::SocketType::STREAM,
        rnet::SocketFlags::CLOEXEC,
        None,
    )?;
    // macOS has no SOCK_CLOEXEC; set the flag via fcntl immediately after
    // creation (same small race std accepts on this platform).
    #[cfg(target_os = "macos")]
    let fd = {
        let fd = rnet::socket(rnet::AddressFamily::UNIX, rnet::SocketType::STREAM, None)?;
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
        // SO_NOSIGPIPE: std sets this on every socket it creates on Apple
        // platforms — same drop-a-std-guarantee hazard as CLOEXEC above.
        // Without it, writing to a peer-closed socket delivers SIGPIPE
        // instead of EPIPE, killing hosts that don't ignore the signal
        // (Rust binaries do at startup; a cdylib embedded in a C process
        // does not).
        rnet::sockopt::set_socket_nosigpipe(&fd, true)?;
        fd
    };
    rustix::io::ioctl_fionbio(&fd, true)?;

    let deadline = Instant::now() + timeout;
    loop {
        match rnet::connect(&fd, &addr) {
            Ok(()) => break,
            // Backlog full: AF_UNIX reports EAGAIN with nothing to poll on
            // (unlike TCP there is no in-flight handshake) — back off and
            // retry until the deadline. The sleep is clamped to the
            // *remaining* budget so the deadline is never overshot.
            Err(Errno::AGAIN) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(timed_out());
                }
                std::thread::sleep(Duration::from_millis(10).min(remaining));
            }
            Err(Errno::INPROGRESS) => {
                // EINTR from `poll` recomputes the remaining budget and
                // retries — a stray signal must not fail the connect (the
                // send/drain loops already treat EINTR the same way).
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(timed_out());
                    }
                    let timespec = rustix::event::Timespec::try_from(remaining).unwrap_or(
                        rustix::event::Timespec {
                            tv_sec: i64::MAX,
                            tv_nsec: 0,
                        },
                    );
                    let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
                    match poll(&mut fds, Some(&timespec)) {
                        Ok(0) => return Err(timed_out()),
                        Ok(_) => break,
                        Err(Errno::INTR) => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                // Writable does not mean connected — fetch the final status.
                rnet::sockopt::socket_error(&fd)??;
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }

    rustix::io::ioctl_fionbio(&fd, false)?;
    Ok(UnixStream::from(fd))
}

struct ServiceConnection {
    stream: UnixStream,
}

impl ServiceConnection {
    fn new(name: &str) -> Result<Self> {
        let property_service_socket = get_property_service_socket();

        // Try the system-property socket for `sys.powerctl`, falling back to
        // the regular service socket if connection fails. Connect itself is
        // the only authoritative check — `fs::metadata` would race the open.
        let stream = if name == "sys.powerctl" {
            let system_socket = get_property_service_for_system_socket();
            connect_with_timeout(&system_socket, SERVICE_IO_TIMEOUT)
                .or_else(|first_err| {
                    log::warn!(
                        "Connect to {system_socket:?} failed ({first_err}); falling back to {property_service_socket:?}"
                    );
                    connect_with_timeout(&property_service_socket, SERVICE_IO_TIMEOUT)
                })?
        } else {
            connect_with_timeout(&property_service_socket, SERVICE_IO_TIMEOUT)?
        };

        // Failure to arm the timeouts would silently drop the no-hang
        // guarantee, so it is an error rather than a `let _ =`.
        stream.set_read_timeout(Some(SERVICE_IO_TIMEOUT))?;
        stream.set_write_timeout(Some(SERVICE_IO_TIMEOUT))?;

        Ok(Self { stream })
    }

    /// Reads the 4-byte V2 status. `budget` bounds the total wait; `None`
    /// waits without a timeout (see [`RESPONSE_TIMEOUT`]).
    fn recv_i32(&mut self, budget: Option<Duration>) -> Result<i32> {
        // SO_RCVTIMEO re-arms per *syscall*: a plain `read_exact` against a
        // server trickling one byte per window would stretch "2 seconds"
        // into 4×. Enforce `budget` as a total budget — the same
        // deadline pattern as `send` and `wait_for_socket_close`.
        let deadline = budget.map(|budget| (budget, Instant::now() + budget));
        if deadline.is_none() {
            // Clear the connect-time static timeout, or it would still end
            // the wait after SERVICE_IO_TIMEOUT. Propagated, unlike the
            // best-effort re-arm below: if it stays armed, a slow reply
            // becomes the `TimedOut` this mode exists to avoid.
            self.stream.set_read_timeout(None)?;
        }
        let mut buf = [0u8; 4];
        let mut filled = 0usize;
        while filled < buf.len() {
            if let Some((budget, deadline)) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "timed out waiting for property service response \
                             ({budget:?} total, {filled}/4 bytes received)"
                        ),
                    )));
                }
                // Best-effort re-arm: on macOS `setsockopt` fails with
                // EINVAL once the peer's FIN has been processed ("the
                // socket has been shut down"), even though the buffered
                // response bytes are still readable. Failing here would
                // drop a response the server already sent; the
                // connect-time static timeout remains armed as the
                // per-syscall bound, and the deadline check above still
                // bounds the total.
                let _ = self.stream.set_read_timeout(Some(remaining));
            }
            match self.stream.read(&mut buf[filled..]) {
                Ok(0) => {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "property service closed before sending a full response",
                    )))
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(map_timeout_err(e, "waiting for property service response")),
            }
        }
        // Deliberately NO timeout-restore here: connections are one-shot
        // (a fresh `ServiceConnection` per request), so the small
        // remaining-budget left armed is never observed — and the server
        // may have already closed its end after responding, where a
        // `setsockopt` would fail with EINVAL on macOS ("the socket has
        // been shut down") depending on FIN arrival timing.
        Ok(i32::from_ne_bytes(buf))
    }
}

/// One wire fragment: caller-borrowed payload bytes, or a 4-byte word the
/// writer materialised itself (command ids, length prefixes).
enum WireBuf<'a> {
    Borrowed(&'a [u8]),
    Word([u8; 4]),
}

impl WireBuf<'_> {
    fn as_slice(&self) -> &[u8] {
        match self {
            WireBuf::Borrowed(b) => b,
            WireBuf::Word(w) => w,
        }
    }
}

struct ServiceWriter<'a> {
    // Raw byte fragments; `send` joins them into one contiguous frame
    // before writing (see the SIGPIPE note there).
    buffers: Vec<WireBuf<'a>>,
}

impl<'a> ServiceWriter<'a> {
    fn new() -> Self {
        Self {
            buffers: Vec::with_capacity(4),
        }
    }

    /// Appends a length-prefixed string. The writer derives the prefix from
    /// the payload itself, so a mismatched pair — which would silently
    /// desynchronise the frame — is unrepresentable at this API.
    fn write_str(mut self, value: &'a str) -> Result<Self> {
        let len = u32::try_from(value.len()).map_err(|_| {
            Error::InvalidArgument(format!("string too long for wire: {} bytes", value.len()))
        })?;
        self.buffers.push(WireBuf::Word(len.to_ne_bytes()));
        self.buffers.push(WireBuf::Borrowed(value.as_bytes()));
        Ok(self)
    }

    fn write_u32(mut self, value: u32) -> Self {
        self.buffers.push(WireBuf::Word(value.to_ne_bytes()));
        self
    }

    fn write_bytes(mut self, value: &'a [u8]) -> Self {
        self.buffers.push(WireBuf::Borrowed(value));
        self
    }

    fn send(self, conn: &mut ServiceConnection) -> Result<()> {
        // A single `write` may write fewer bytes than requested
        // (signal after a partial transfer, full socket buffer). Loop until
        // every byte is on the wire — a short write would otherwise
        // desynchronise the length-prefixed protocol and leave the server
        // waiting for bytes that never arrive.
        //
        // SO_SNDTIMEO re-arms per *syscall*, so with the static timeout a
        // peer draining one byte per window could stretch "2 seconds" into
        // hours across a full frame. Enforce SERVICE_IO_TIMEOUT as a total
        // budget instead: re-arm the write timeout with the remaining
        // budget before every syscall and fail once it hits zero — the
        // same pattern `wait_for_socket_close` uses for reads.
        //
        // One contiguous frame sent with rustix `send(MSG_NOSIGNAL)` on
        // Linux/Android: a plain `write`/`writev` to a peer-closed socket
        // raises SIGPIPE. A Rust `main` ignores SIGPIPE at startup; a
        // staticlib/cdylib/JNI host typically does not, and dies before the
        // EPIPE reaches us. std's `UnixStream::write` is not used here
        // because it is a plain `write(2)` on older std (1.77 through at
        // least 1.86, inside this crate's `rust-version` range). Apple
        // targets get the guarantee from SO_NOSIGPIPE (set in
        // `connect_with_timeout`). The copy is at most one V2 frame
        // (12 + name + value bytes). A regression to std's `write` passes
        // on current stable; the CI `msrv` job runs this module's tests on
        // 1.77 to catch it.
        let deadline = Instant::now() + SERVICE_IO_TIMEOUT;
        let total: usize = self.buffers.iter().map(|b| b.as_slice().len()).sum();
        let mut frame = Vec::with_capacity(total);
        for buf in &self.buffers {
            frame.extend_from_slice(buf.as_slice());
        }
        let mut written = 0usize;
        while written < total {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "timed out sending property service request \
                         ({SERVICE_IO_TIMEOUT:?} total, {written}/{total} bytes sent)"
                    ),
                )));
            }
            // Best-effort re-arm, mirroring `recv_i32`: on macOS this
            // `setsockopt` fails with EINVAL once the peer's FIN has been
            // processed — propagating that would report "Invalid argument"
            // where the *next write's* EPIPE/WriteZero is the real story.
            // The connect-time static timeout stays armed as the
            // per-syscall bound, and the deadline check above still bounds
            // the total.
            let _ = conn.stream.set_write_timeout(Some(remaining));
            // SO_SNDTIMEO expiry is EAGAIN -> `WouldBlock` (handled by
            // `map_timeout_err`); EINTR -> `Interrupted` (retried below).
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let r = rustix::net::send(
                &conn.stream,
                &frame[written..],
                rustix::net::SendFlags::NOSIGNAL,
            )
            .map_err(std::io::Error::from);
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let r = conn.stream.write(&frame[written..]);
            match r {
                Ok(0) => {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "property service socket closed mid-write",
                    )))
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(map_timeout_err(e, "sending property service request")),
            }
        }
        conn.stream.flush()?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ProtocolVersion {
    V1 = 1,
    V2 = 2,
}

// No `FromBytes`: the client only ever serializes this message.
// No `Debug`: a derived impl would print the raw `value` bytes, and
// property values may carry sensitive payloads — the same don't-log-values
// policy as the service crate's masked manual impl. Nothing needs it today;
// if one is ever required, write a manual impl that masks `value`.
#[derive(Immutable, IntoBytes)]
#[repr(C)]
struct PropertyMessage {
    cmd: u32,
    name: [u8; PROP_NAME_MAX],
    value: [u8; PROP_VALUE_MAX],
}

impl PropertyMessage {
    /// Builds a fixed-size wire message. Rejects oversized name/value so the
    /// SET request cannot silently target a different key than the caller asked
    /// for. Length checks at the call site are preserved as a defense in depth,
    /// but this constructor is the type-level enforcement point.
    fn new(cmd: u32, name: &str, value: &str) -> Result<Self> {
        let name_bytes = name.as_bytes();
        let value_bytes = value.as_bytes();

        // `>=`, not `>`: the V1 wire format requires NUL-terminated fields
        // (bionic rejects `strlen >= MAX`, and the server force-NULs the
        // last byte, silently truncating an exactly-full field). For this
        // constructor to be the enforcement point it must be at least as
        // strict as the wire contract.
        if name_bytes.len() >= PROP_NAME_MAX {
            return Err(Error::InvalidArgument(format!(
                "Property name length {} exceeds PROP_NAME_MAX - 1 = {}",
                name_bytes.len(),
                PROP_NAME_MAX - 1
            )));
        }
        if value_bytes.len() >= PROP_VALUE_MAX {
            return Err(Error::InvalidArgument(format!(
                "Property value length {} exceeds PROP_VALUE_MAX - 1 = {}",
                value_bytes.len(),
                PROP_VALUE_MAX - 1
            )));
        }

        let mut name_buf = [0u8; PROP_NAME_MAX];
        let mut value_buf = [0u8; PROP_VALUE_MAX];
        name_buf[..name_bytes.len()].copy_from_slice(name_bytes);
        value_buf[..value_bytes.len()].copy_from_slice(value_bytes);

        Ok(Self {
            cmd,
            name: name_buf,
            value: value_buf,
        })
    }
}

/// Decides the wire protocol version.
///
/// Order of authority — bionic consults the `ro.property_service.version`
/// property, treats a present-but-unparseable value as v1, and defaults to
/// **v1** when it is unset:
/// 1. the `ro.property_service.version` system property, when the global
///    property store is already initialized (never initialized from here —
///    that would latch the default properties directory as a side effect
///    of a `set()` call). A present but unparseable value means an old or
///    odd init → **V1**, like bionic.
/// 2. the `PROPERTY_SERVICE_VERSION` environment variable. Present but
///    unparseable → **V1** with a warning, the same policy as the
///    property above — a typo like `v1` must not silently select the
///    *newer* protocol against an init that only speaks V1;
/// 3. default: **V2**, a deliberate deviation from bionic's v1 default
///    for the *unset* case. V1 frames cannot carry names of
///    `PROP_NAME_MAX` (32) bytes or more — which modern property names
///    routinely exceed — and this crate's own service dispatches both
///    protocols by the leading command word. When talking to a
///    pre-Android-O init that only understands V1, expose the property or
///    set the env var.
///
/// The decision is cached (`OnceLock`) only once the property store is
/// initialized; before that, calls get a *provisional* answer from
/// env/default without latching, so a later `init()` still lets the
/// property win. After the first post-init `set()` the version is fixed
/// for the process lifetime.
fn protocol_version() -> ProtocolVersion {
    static PROTOCOL_VERSION: OnceLock<ProtocolVersion> = OnceLock::new();

    if let Some(v) = PROTOCOL_VERSION.get() {
        return *v;
    }

    // `var_os`, not `var`: a present but non-UTF-8 value must take the
    // "present but unparseable → V1" arm, not `var`'s `NotUnicode` error
    // that is indistinguishable from "unset" in a catch-all `Err` arm.
    let env_or_default = || match env::var_os("PROPERTY_SERVICE_VERSION") {
        Some(v) => match v.to_str().and_then(|s| s.trim().parse::<u32>().ok()) {
            Some(n) if n >= 2 => ProtocolVersion::V2,
            Some(_) => ProtocolVersion::V1,
            // Present but not a number: same policy as the property path
            // (old/odd configuration → V1), and loud — silently defaulting
            // to V2 would send V2 frames to an init that may only
            // understand V1.
            None => {
                log::warn!("PROPERTY_SERVICE_VERSION={v:?} is not a number; assuming V1");
                ProtocolVersion::V1
            }
        },
        None => ProtocolVersion::V2,
    };

    match crate::system_properties_if_initialized() {
        Some(sp) => *PROTOCOL_VERSION.get_or_init(|| {
            sp.read_with("ro.property_service.version", |v| {
                match v.trim().parse::<u32>() {
                    Ok(n) if n >= 2 => ProtocolVersion::V2,
                    // Present but not a parseable ≥2: bionic parity → V1.
                    _ => ProtocolVersion::V1,
                }
            })
            // Property absent (or store read failed): env var, then the
            // documented V2 default.
            .unwrap_or_else(|_| env_or_default())
        }),
        // Store not initialized yet: provisional, deliberately NOT latched.
        None => env_or_default(),
    }
}

/// Wait for the V1 server to close the connection by signalling EOF on read.
/// The server uses connection close as an implicit ack — block until the peer
/// shuts down its write side or until `timeout` elapses.
///
/// Infallible by design: the SET frame already went out before this runs,
/// and bionic reports success regardless of how its 250ms close-wait ends —
/// every drain problem here is logged and swallowed, never propagated.
fn wait_for_socket_close(stream: &mut UnixStream, timeout: Duration) {
    // Half-close our write side so the server can finish; then drain.
    // (No initial `set_read_timeout` here — the loop below re-arms the
    // remaining budget before every read.)
    let _ = stream.shutdown(Shutdown::Write);
    let original_timeout = stream.read_timeout().ok().flatten();

    let started = Instant::now();
    let mut buf = [0u8; 64];
    loop {
        // Enforce `timeout` as a bound on the *total* wait, not per-read:
        // a peer that keeps trickling bytes would otherwise hold this loop
        // open indefinitely (each successful read restarts the read
        // timeout). Recompute the remaining budget every iteration.
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            log::warn!("wait_for_socket_close: timed out after {timeout:?}");
            break;
        }
        // If the timeout can't be armed, the next read could block without
        // bound — the exact thing this loop exists to prevent. Give up on
        // draining instead (the SET itself already went out).
        if let Err(e) = stream.set_read_timeout(Some(remaining)) {
            log::warn!("wait_for_socket_close: couldn't arm read timeout ({e}); skipping drain");
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF — server closed.
            Ok(_) => {}     // Discard any trailing bytes.
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                // The SET frame already went out in full; this drain is
                // only the V1 implicit ack. bionic returns success after
                // its 250ms poll regardless of outcome, and a timeout here
                // is already swallowed — an ECONNRESET-style error (server
                // closed with our bytes still queued) must not retroactively
                // fail a write that was likely applied.
                log::warn!("wait_for_socket_close: drain error ignored ({e})");
                break;
            }
        }
    }
    let _ = stream.set_read_timeout(original_timeout);
}

/// V2 `(name, value)` length caps the client checks before connecting.
/// Android: the peer is init, whose `RecvString` accepts up to 0xffff
/// bytes for each. Elsewhere: this crate's service caps.
#[cfg(target_os = "android")]
const CLIENT_WIRE_CAPS: (usize, usize) = (
    crate::wire::AOSP_MAX_WIRE_STRING_LEN,
    crate::wire::AOSP_MAX_WIRE_STRING_LEN,
);
#[cfg(not(target_os = "android"))]
const CLIENT_WIRE_CAPS: (usize, usize) = (
    crate::wire::MAX_WIRE_NAME_LEN,
    crate::wire::MAX_WIRE_VALUE_LEN,
);

/// Mirrors the server's V2 wire caps `(name, value)` so an oversized frame
/// fails here with a clear message instead of the server's opaque error
/// status. Takes the caps as an argument so both targets' values are
/// testable on the host.
fn check_wire_caps(name: &str, value: &str, (max_name, max_value): (usize, usize)) -> Result<()> {
    if name.len() > max_name {
        return Err(Error::InvalidArgument(format!(
            "Property name exceeds the wire cap: {} > {max_name}",
            name.len()
        )));
    }
    if value.len() > max_value {
        return Err(Error::InvalidArgument(format!(
            "Property value exceeds the wire cap: {} > {max_value}",
            value.len()
        )));
    }
    Ok(())
}

/// Sets a system property via the property service socket.
///
/// An `Error::Io` of kind `TimedOut` from the V2 response wait (outside
/// Android; the message starts with "timed out waiting for property
/// service response") does not mean the property was not set: see
/// [`SERVICE_IO_TIMEOUT`]. On Android the response wait has no timeout.
pub(crate) fn set(name: &str, value: &str) -> Result<()> {
    // Validate name and value up front, for BOTH protocol versions. This
    // is load-bearing for interior NUL bytes in particular: the server
    // decodes both wire formats as C strings, so a NUL-carrying `&str`
    // (which Rust happily passes) would otherwise be silently truncated —
    // `set("a\0b", v)` would target property "a", retargeting the write
    // to a different key than the caller asked for.
    // `validate_property_name` rejects NUL through its allowed-chars loop;
    // `validate_value_len` rejects NUL in values explicitly.
    crate::wire::validate_property_name(name)
        .inspect_err(|e| log::error!("setprop reject: {e}"))?;
    crate::wire::validate_value_len(name, value)
        .inspect_err(|e| log::error!("setprop reject: {e}"))?;

    match protocol_version() {
        ProtocolVersion::V1 => {
            if name.len() >= PROP_NAME_MAX {
                log::error!(
                    "Property name too long for V1 protocol: {} >= {}",
                    name.len(),
                    PROP_NAME_MAX
                );
                return Err(Error::InvalidArgument(format!(
                    "Property name is too long: {}",
                    name.len()
                )));
            }

            if value.len() >= PROP_VALUE_MAX {
                log::error!(
                    "Property value too long for V1 protocol: {} >= {}",
                    value.len(),
                    PROP_VALUE_MAX
                );
                return Err(Error::InvalidArgument(format!(
                    "Property value is too long: {}",
                    value.len()
                )));
            }

            // Pass the *property name* — `ServiceConnection::new` routes
            // `sys.powerctl` to the for_system socket by name, on V1 as
            // well as V2 (bionic's `send_prop_msg` constructs its V1
            // connection from `msg->name` the same way).
            let mut conn = ServiceConnection::new(name)?;
            let prop_msg = PropertyMessage::new(PROP_MSG_SETPROP, name, value)?;

            ServiceWriter::new()
                .write_bytes(prop_msg.as_bytes())
                .send(&mut conn)?;

            wait_for_socket_close(&mut conn.stream, Duration::from_millis(250));
        }
        ProtocolVersion::V2 => {
            // (Name/value policy is validated at the top of `set` — shared
            // with the V1 arm. Length prefixes are derived inside
            // `write_str`, so no separate truncation hazard here.)
            check_wire_caps(name, value, CLIENT_WIRE_CAPS)?;

            let mut conn = ServiceConnection::new(name)?;

            ServiceWriter::new()
                .write_u32(PROP_MSG_SETPROP2)
                .write_str(name)?
                .write_str(value)?
                .send(&mut conn)?;

            let res = conn.recv_i32(RESPONSE_TIMEOUT)?;

            if res != PROP_SUCCESS {
                // Do not log/report the value: property values can carry
                // sensitive data (tokens, identifiers) — same policy as the
                // service side's masked logging.
                log::error!(
                    "Property service returned error for {name:?} (<{} bytes>): 0x{res:X}",
                    value.len()
                );
                // A protocol-level rejection, not a transport failure — the
                // socket round-trip succeeded. A dedicated variant so callers
                // can tell a permanent policy denial from a retryable
                // `Error::Io`.
                return Err(Error::ServiceError {
                    name: name.to_owned(),
                    code: res,
                });
            }
        }
    }

    Ok(())
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    /// SO_RCVTIMEO re-arms per *syscall*, so a peer trickling one byte per
    /// window under the static timeout would stretch "2 seconds" well past
    /// the budget (~3.6s for this schedule). `recv_i32` must enforce
    /// `SERVICE_IO_TIMEOUT` as a total budget instead.
    #[test]
    fn test_recv_i32_total_timeout_budget() {
        let (client, mut server) = UnixStream::pair().unwrap();

        let feeder = std::thread::spawn(move || {
            for _ in 0..3 {
                if server.write_all(&[0u8]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(800));
            }
            // Never send the 4th byte, and keep the socket open past the
            // client's budget so an early EOF can't end the wait for us.
            std::thread::sleep(Duration::from_millis(1500));
        });

        let mut conn = ServiceConnection { stream: client };
        let start = Instant::now();
        let err = conn
            .recv_i32(Some(SERVICE_IO_TIMEOUT))
            .expect_err("3/4 bytes must not satisfy recv_i32");
        let elapsed = start.elapsed();

        let msg = format!("{err}");
        assert!(msg.contains("timed out"), "unexpected error: {msg}");
        assert!(
            elapsed >= Duration::from_millis(1800),
            "budget expired early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(3200),
            "total budget not enforced (per-syscall re-arm?): {elapsed:?}"
        );

        drop(conn);
        let _ = feeder.join();
    }

    /// A server that closes before a full 4-byte status is a protocol
    /// error, reported as EOF — not a hang, not a success.
    #[test]
    fn test_recv_i32_early_close_is_eof_error() {
        let (client, server) = UnixStream::pair().unwrap();
        drop(server);

        let mut conn = ServiceConnection { stream: client };
        let err = conn
            .recv_i32(Some(SERVICE_IO_TIMEOUT))
            .expect_err("closed socket must error");
        assert!(
            format!("{err}").contains("closed before"),
            "unexpected error: {err}"
        );
        // The unbounded (Android) mode ends on EOF the same way.
        let (client, server) = UnixStream::pair().unwrap();
        drop(server);
        let mut conn = ServiceConnection { stream: client };
        let err = conn.recv_i32(None).expect_err("closed socket must error");
        assert!(
            format!("{err}").contains("closed before"),
            "unexpected error: {err}"
        );
    }

    /// The Android mode (`budget = None`) waits past SERVICE_IO_TIMEOUT,
    /// including the static read timeout `ServiceConnection::new` arms: the
    /// reply here arrives after ~2.5 s and must be returned, not
    /// `TimedOut` (init applies the set regardless of the client's wait).
    #[test]
    fn test_recv_i32_without_budget_outlasts_service_io_timeout() {
        let (client, mut server) = UnixStream::pair().unwrap();
        // Same static timeout `ServiceConnection::new` sets.
        client.set_read_timeout(Some(SERVICE_IO_TIMEOUT)).unwrap();

        let feeder = std::thread::spawn(move || {
            std::thread::sleep(SERVICE_IO_TIMEOUT + Duration::from_millis(500));
            let _ = server.write_all(&PROP_SUCCESS.to_ne_bytes());
            // Keep the socket open until the client has read the reply.
            let mut b = [0u8; 1];
            let _ = server.read(&mut b);
        });

        let mut conn = ServiceConnection { stream: client };
        let res = conn.recv_i32(None).expect("late reply must be returned");
        assert_eq!(res, PROP_SUCCESS);
        drop(conn);
        feeder.join().unwrap();
    }

    /// `check_wire_caps` is `>`, not `>=`, for both caps of both targets:
    /// a name or value of exactly the cap is sent, one byte more is not.
    #[test]
    fn check_wire_caps_boundaries() {
        let ok = |n: usize, v: usize, caps| check_wire_caps(&"n".repeat(n), &"v".repeat(v), caps);
        for caps in [
            (
                crate::wire::AOSP_MAX_WIRE_STRING_LEN,
                crate::wire::AOSP_MAX_WIRE_STRING_LEN,
            ),
            (
                crate::wire::MAX_WIRE_NAME_LEN,
                crate::wire::MAX_WIRE_VALUE_LEN,
            ),
        ] {
            let (max_name, max_value) = caps;
            assert!(ok(max_name, max_value, caps).is_ok(), "{caps:?} at cap");
            assert!(
                matches!(ok(max_name + 1, 1, caps), Err(Error::InvalidArgument(_))),
                "{caps:?} name over cap"
            );
            assert!(
                matches!(ok(1, max_value + 1, caps), Err(Error::InvalidArgument(_))),
                "{caps:?} value over cap"
            );
        }
        assert_eq!(crate::wire::AOSP_MAX_WIRE_STRING_LEN, 65535);
        assert_eq!(
            CLIENT_WIRE_CAPS,
            (
                crate::wire::MAX_WIRE_NAME_LEN,
                crate::wire::MAX_WIRE_VALUE_LEN
            )
        );
    }

    /// Writing a frame to a peer-closed socket must surface EPIPE as an
    /// error, not SIGPIPE, even when the host process has SIGPIPE at
    /// SIG_DFL (C/JNI hosts). The libtest runtime ignores SIGPIPE, so the
    /// check runs in a re-executed child that restores SIG_DFL first — a
    /// regression kills only the child.
    ///
    /// Scope: this catches a regression to std's `UnixStream::write` only
    /// on a toolchain whose std writes with plain `write(2)` (1.77 through
    /// at least 1.86). Current stable std adds `MSG_NOSIGNAL` itself, so
    /// the test passes there either way; the CI `msrv` job runs it on 1.77.
    #[test]
    fn test_send_to_closed_peer_is_epipe_not_sigpipe() {
        const CHILD_ENV: &str = "RSPROPERTIES_SIGPIPE_CHILD";
        const TEST_PATH: &str =
            "system_property_set::tests::test_send_to_closed_peer_is_epipe_not_sigpipe";

        if env::var_os(CHILD_ENV).is_some() {
            // SAFETY: `libc::signal` is the libc crate's declaration of C
            // `signal` for this target. `SIG_DFL` installs no handler, so no
            // Rust code runs in signal context. The disposition is
            // process-wide; this child process runs only this test
            // (`--exact`).
            let prev = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            assert_ne!(prev, libc::SIG_ERR, "signal(SIGPIPE, SIG_DFL) failed");

            let (client, server) = UnixStream::pair().unwrap();
            drop(server);
            let mut conn = ServiceConnection { stream: client };
            let err = ServiceWriter::new()
                .write_u32(PROP_MSG_SETPROP2)
                .write_bytes(&[0u8; 64])
                .send(&mut conn)
                .expect_err("peer-closed socket must error");
            assert!(
                matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::BrokenPipe),
                "unexpected error: {err}"
            );
            return;
        }

        let output = std::process::Command::new(env::current_exe().unwrap())
            .args([TEST_PATH, "--exact", "--test-threads=1", "--nocapture"])
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
}
