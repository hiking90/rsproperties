// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::io::{prelude::*, IoSlice};
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
const PROP_SERVICE_NAME: &str = "property_service";
// const PROP_SERVICE_FOR_SYSTEM_NAME: &str = "property_service_for_system";

use crate::wire::{
    PROP_MSG_SETPROP, PROP_MSG_SETPROP2, PROP_NAME_MAX, PROP_SUCCESS, PROP_VALUE_MAX,
};

/// Global socket directory configuration
static SOCKET_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set the global socket directory for property services (internal use only).
/// This function can only be called once. Subsequent calls will be ignored.
///
/// Callers must hold `crate::GLOBAL_DIRS_LOCK` (see `lib::try_init`) so the
/// pre-check + set sequence stays atomic against the implicit latch in
/// [`socket_dir`].
///
/// # Arguments
/// * `dir` - The directory path where property service sockets are located
///
/// # Returns
/// * `true` if the directory was successfully set (first call)
/// * `false` if the directory was already set (subsequent calls)
pub(crate) fn set_socket_dir<P: AsRef<Path>>(dir: P) -> bool {
    let dir_path = dir.as_ref().to_path_buf();

    SOCKET_DIR.set(dir_path).is_ok()
}

/// `true` once `set_socket_dir` has succeeded (or `socket_dir()` was called
/// and populated the cell via env/default). Used by `lib::try_init` for
/// pre-flight checks before committing other globals.
pub(crate) fn socket_dir_is_set() -> bool {
    SOCKET_DIR.get().is_some()
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
    // the env/default latch cannot slip between `try_init`'s pre-check and
    // its `set_socket_dir` commit.
    if let Some(dir) = SOCKET_DIR.get() {
        return dir.as_path();
    }
    let _guard = crate::lock_global_dirs();
    SOCKET_DIR
        .get_or_init(|| {
            let dir = env::var("PROPERTY_SERVICE_SOCKET_DIR")
                .unwrap_or_else(|_| DEFAULT_SOCKET_DIR.to_string());
            PathBuf::from(dir)
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

/// Bound on every socket operation against the property service —
/// **including connect** (see `connect_with_timeout`). The V1 path
/// additionally enforces its own (shorter) close-wait budget; this cap
/// exists so a stalled server — one that stopped accepting, never
/// responds, or stops draining our send — cannot block the caller's
/// thread forever, the exact hazard the V1 arm defends against with
/// `wait_for_socket_close`.
const SERVICE_IO_TIMEOUT: Duration = Duration::from_secs(2);

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

    fn recv_i32(&mut self) -> Result<i32> {
        let mut buf = [0u8; 4];
        self.stream
            .read_exact(&mut buf)
            .map_err(|e| map_timeout_err(e, "waiting for property service response"))?;
        let value = i32::from_ne_bytes(buf);
        Ok(value)
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
    // Raw byte fragments rather than `IoSlice`s: the short-write loop in
    // `send` needs to re-slice past already-written bytes, and `IoSlice`
    // doesn't expose its inner slice on stable (`advance_slices` is
    // 1.81+, above this crate's MSRV).
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
        // A single `write_vectored` may write fewer bytes than requested
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
        let deadline = Instant::now() + SERVICE_IO_TIMEOUT;
        let total: usize = self.buffers.iter().map(|b| b.as_slice().len()).sum();
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
            conn.stream.set_write_timeout(Some(remaining))?;
            // Rebuild the IoSlice list, skipping what already went out.
            let mut skip = written;
            let mut slices: Vec<IoSlice<'_>> = Vec::with_capacity(self.buffers.len());
            for buf in &self.buffers {
                let buf = buf.as_slice();
                if skip >= buf.len() {
                    skip -= buf.len();
                    continue;
                }
                slices.push(IoSlice::new(&buf[skip..]));
                skip = 0;
            }
            match conn.stream.write_vectored(&slices) {
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
#[derive(Immutable, IntoBytes, Debug)]
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
/// 2. the `PROPERTY_SERVICE_VERSION` environment variable;
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

    let env_or_default = || {
        let version = env::var("PROPERTY_SERVICE_VERSION")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(2);
        if version >= 2 {
            ProtocolVersion::V2
        } else {
            ProtocolVersion::V1
        }
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

// Set a system property via local domain socket.
pub(crate) fn set(name: &str, value: &str) -> Result<()> {
    // Validate name and value up front, for BOTH protocol versions. This
    // is load-bearing for interior NUL bytes in particular: the server
    // decodes both wire formats as C strings, so a NUL-carrying `&str`
    // (which Rust happily passes) would otherwise be silently truncated —
    // `set("a\0b", v)` would target property "a", retargeting the write
    // to a different key than the caller asked for.
    // `validate_property_name` rejects NUL through its allowed-chars loop;
    // `validate_value_len` rejects NUL in values explicitly.
    if let Err(e) = crate::wire::validate_property_name(name) {
        log::error!("setprop reject: {e}");
        return Err(Error::InvalidArgument(e));
    }
    if let Err(e) = crate::wire::validate_value_len(name, value) {
        log::error!("setprop reject: {e}");
        return Err(Error::InvalidArgument(e));
    }

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

            let mut conn = ServiceConnection::new(PROP_SERVICE_NAME)?;
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
            // Mirror the server's wire caps so an oversized frame fails
            // here with a clear message instead of the server's opaque
            // error status.
            if name.len() > crate::wire::MAX_WIRE_NAME_LEN {
                return Err(Error::InvalidArgument(format!(
                    "Property name exceeds the wire cap: {} > {}",
                    name.len(),
                    crate::wire::MAX_WIRE_NAME_LEN
                )));
            }
            if value.len() > crate::wire::MAX_WIRE_VALUE_LEN {
                return Err(Error::InvalidArgument(format!(
                    "Property value exceeds the wire cap: {} > {}",
                    value.len(),
                    crate::wire::MAX_WIRE_VALUE_LEN
                )));
            }

            let mut conn = ServiceConnection::new(name)?;

            ServiceWriter::new()
                .write_u32(PROP_MSG_SETPROP2)
                .write_str(name)?
                .write_str(value)?
                .send(&mut conn)?;

            let res = conn.recv_i32()?;

            if res != PROP_SUCCESS {
                // Do not log/report the value: property values can carry
                // sensitive data (tokens, identifiers) — same policy as the
                // service side's masked logging.
                log::error!(
                    "Property service returned error for '{name}' (<{} bytes>): 0x{res:X}",
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
