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
            UnixStream::connect(&system_socket)
                .or_else(|first_err| {
                    log::warn!(
                        "Connect to {system_socket:?} failed ({first_err}); falling back to {property_service_socket:?}"
                    );
                    UnixStream::connect(&property_service_socket)
                })?
        } else {
            UnixStream::connect(&property_service_socket)?
        };

        Ok(Self { stream })
    }

    fn recv_i32(&mut self) -> Result<i32> {
        let mut buf = [0u8; 4];
        self.stream.read_exact(&mut buf)?;
        let value = i32::from_ne_bytes(buf);
        Ok(value)
    }
}

struct ServiceWriter<'a> {
    // Raw byte slices rather than `IoSlice`s: the short-write loop in
    // `send` needs to re-slice past already-written bytes, and `IoSlice`
    // doesn't expose its inner slice on stable (`advance_slices` is
    // 1.81+, above this crate's MSRV).
    buffers: Vec<&'a [u8]>,
}

impl<'a> ServiceWriter<'a> {
    fn new() -> Self {
        Self {
            buffers: Vec::with_capacity(4),
        }
    }

    fn write_str(self, value: &'a str, len: &'a u32) -> Self {
        // The length prefix and payload arrive as separate arguments (the
        // `&'a u32` needs to outlive the buffer list); a mismatched pair
        // would silently desynchronise the length-prefixed frame.
        debug_assert_eq!(
            *len as usize,
            value.len(),
            "write_str: length prefix does not match payload"
        );
        let mut thiz = self.write_u32(len);
        thiz.buffers.push(value.as_bytes());
        thiz
    }

    fn write_u32(mut self, value: &'a u32) -> Self {
        self.buffers.push(value.as_bytes());
        self
    }

    fn write_bytes(mut self, value: &'a [u8]) -> Self {
        self.buffers.push(value);
        self
    }

    fn send(self, conn: &mut ServiceConnection) -> Result<()> {
        // A single `write_vectored` may write fewer bytes than requested
        // (signal after a partial transfer, full socket buffer). Loop until
        // every byte is on the wire — a short write would otherwise
        // desynchronise the length-prefixed protocol and leave the server
        // waiting for bytes that never arrive.
        let total: usize = self.buffers.iter().map(|b| b.len()).sum();
        let mut written = 0usize;
        while written < total {
            // Rebuild the IoSlice list, skipping what already went out.
            let mut skip = written;
            let mut slices: Vec<IoSlice<'_>> = Vec::with_capacity(self.buffers.len());
            for buf in &self.buffers {
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
                Err(e) => return Err(Error::Io(e)),
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
            return Err(Error::FileValidation(format!(
                "Property name length {} exceeds PROP_NAME_MAX - 1 = {}",
                name_bytes.len(),
                PROP_NAME_MAX - 1
            )));
        }
        if value_bytes.len() >= PROP_VALUE_MAX {
            return Err(Error::FileValidation(format!(
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

fn protocol_version() -> ProtocolVersion {
    static PROTOCOL_VERSION: OnceLock<ProtocolVersion> = OnceLock::new();

    *PROTOCOL_VERSION.get_or_init(|| {
        // Try to get version from environment variable first
        let version = env::var("PROPERTY_SERVICE_VERSION")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2);

        if version >= 2 {
            ProtocolVersion::V2
        } else {
            ProtocolVersion::V1
        }
    })
}

/// Wait for the V1 server to close the connection by signalling EOF on read.
/// The server uses connection close as an implicit ack — block until the peer
/// shuts down its write side or until `timeout` elapses.
fn wait_for_socket_close(stream: &mut UnixStream, timeout: Duration) -> Result<()> {
    // Half-close our write side so the server can finish; then drain.
    let _ = stream.shutdown(Shutdown::Write);
    let original_timeout = stream.read_timeout().ok().flatten();
    let _ = stream.set_read_timeout(Some(timeout));

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
        let _ = stream.set_read_timeout(Some(remaining));
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF — server closed.
            Ok(_) => {}     // Discard any trailing bytes.
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                let _ = stream.set_read_timeout(original_timeout);
                return Err(Error::Io(e));
            }
        }
    }
    let _ = stream.set_read_timeout(original_timeout);
    Ok(())
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
        return Err(Error::FileValidation(e));
    }
    if let Err(e) = crate::wire::validate_value_len(name, value) {
        log::error!("setprop reject: {e}");
        return Err(Error::FileValidation(e));
    }

    match protocol_version() {
        ProtocolVersion::V1 => {
            if name.len() >= PROP_NAME_MAX {
                log::error!(
                    "Property name too long for V1 protocol: {} >= {}",
                    name.len(),
                    PROP_NAME_MAX
                );
                return Err(Error::FileValidation(format!(
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
                return Err(Error::FileValidation(format!(
                    "Property value is too long: {}",
                    value.len()
                )));
            }

            let mut conn = ServiceConnection::new(PROP_SERVICE_NAME)?;
            let prop_msg = PropertyMessage::new(PROP_MSG_SETPROP, name, value)?;

            ServiceWriter::new()
                .write_bytes(prop_msg.as_bytes())
                .send(&mut conn)?;

            wait_for_socket_close(&mut conn.stream, Duration::from_millis(250))?;
        }
        ProtocolVersion::V2 => {
            // `try_from`, not `as`: V2 names have no wire-level length cap
            // and `ro.` values are unbounded, so a silent truncation here
            // would emit a length prefix smaller than the payload pushed
            // into the writer — exactly the frame desync this module's
            // send loop exists to prevent.
            let name_len = u32::try_from(name.len()).map_err(|_| {
                Error::FileValidation(format!("Property name too long: {} bytes", name.len()))
            })?;
            let value_len = u32::try_from(value.len()).map_err(|_| {
                Error::FileValidation(format!("Property value too long: {} bytes", value.len()))
            })?;

            // (Name/value policy is validated at the top of `set` — shared
            // with the V1 arm.)
            // Mirror the server's wire caps so an oversized frame fails
            // here with a clear message instead of the server's opaque
            // error status.
            if name.len() > crate::wire::MAX_WIRE_NAME_LEN {
                return Err(Error::FileValidation(format!(
                    "Property name exceeds the wire cap: {} > {}",
                    name.len(),
                    crate::wire::MAX_WIRE_NAME_LEN
                )));
            }
            if value.len() > crate::wire::MAX_WIRE_VALUE_LEN {
                return Err(Error::FileValidation(format!(
                    "Property value exceeds the wire cap: {} > {}",
                    value.len(),
                    crate::wire::MAX_WIRE_VALUE_LEN
                )));
            }

            let mut conn = ServiceConnection::new(name)?;

            ServiceWriter::new()
                .write_u32(&PROP_MSG_SETPROP2)
                .write_str(name, &name_len)
                .write_str(value, &value_len)
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
                return Err(Error::Io(std::io::Error::other(format!(
                    "Unable to set property \"{name}\" (<{} bytes>): error code: 0x{res:X}",
                    value.len()
                ))));
            }
        }
    }

    Ok(())
}
