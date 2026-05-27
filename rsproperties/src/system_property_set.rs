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

    SOCKET_DIR.set(dir_path.clone()).is_ok()
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
pub fn socket_dir() -> &'static PathBuf {
    SOCKET_DIR.get_or_init(|| {
        let dir = env::var("PROPERTY_SERVICE_SOCKET_DIR")
            .unwrap_or_else(|_| DEFAULT_SOCKET_DIR.to_string());
        PathBuf::from(dir)
    })
}

/// Get the full path to the property service socket
fn get_property_service_socket() -> String {
    let socket_path = socket_dir().join(PROPERTY_SERVICE_SOCKET_NAME);
    socket_path.to_string_lossy().into_owned()
}

/// Get the full path to the system property service socket
fn get_property_service_for_system_socket() -> String {
    let socket_path = socket_dir().join(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME);
    socket_path.to_string_lossy().into_owned()
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
                        "Connect to {system_socket} failed ({first_err}); falling back to {property_service_socket}"
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
    buffers: Vec<IoSlice<'a>>,
}

impl<'a> ServiceWriter<'a> {
    fn new() -> Self {
        Self {
            buffers: Vec::with_capacity(4),
        }
    }

    fn write_str(self, value: &'a str, len: &'a u32) -> Self {
        let mut thiz = self.write_u32(len);
        thiz.buffers.push(IoSlice::new(value.as_bytes()));
        thiz
    }

    fn write_u32(mut self, value: &'a u32) -> Self {
        self.buffers.push(IoSlice::new(value.as_bytes()));
        self
    }

    fn write_bytes(mut self, value: &'a [u8]) -> Self {
        self.buffers.push(IoSlice::new(value));
        self
    }

    fn send(self, conn: &mut ServiceConnection) -> Result<()> {
        conn.stream
            .write_vectored(&self.buffers)
            .map_err(Error::Io)?;
        conn.stream.flush()?;
        Ok(())
    }
}

enum ProtocolVersion {
    V1 = 1,
    V2 = 2,
}

#[derive(FromBytes, Immutable, IntoBytes, Debug)]
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

        if name_bytes.len() > PROP_NAME_MAX {
            return Err(Error::FileValidation(format!(
                "Property name length {} exceeds PROP_NAME_MAX={}",
                name_bytes.len(),
                PROP_NAME_MAX
            )));
        }
        if value_bytes.len() > PROP_VALUE_MAX {
            return Err(Error::FileValidation(format!(
                "Property value length {} exceeds PROP_VALUE_MAX={}",
                value_bytes.len(),
                PROP_VALUE_MAX
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

fn protocol_version() -> &'static ProtocolVersion {
    static PROTOCOL_VERSION: OnceLock<ProtocolVersion> = OnceLock::new();

    PROTOCOL_VERSION.get_or_init(|| {
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
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF — server closed.
            Ok(_) => {}     // Discard any trailing bytes.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= timeout {
                    log::warn!("wait_for_socket_close: timed out after {timeout:?}");
                    break;
                }
            }
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
            let value_len = value.len() as u32;
            let name_len = name.len() as u32;

            // Shared client/server policy — pre-change V2 was `>= 92` here
            // while the server validator used `> 92`. Single function avoids
            // the drift.
            if let Err(e) = crate::wire::validate_value_len(name, value) {
                log::error!("V2 reject: {e}");
                return Err(Error::FileValidation(e));
            }

            let mut conn = ServiceConnection::new(name)?;

            ServiceWriter::new()
                .write_u32(&PROP_MSG_SETPROP2)
                .write_str(name, &name_len)
                .write_str(value, &value_len)
                .send(&mut conn)?;

            let res = conn.recv_i32()?;

            if res != PROP_SUCCESS {
                log::error!("Property service returned error for '{name}' = '{value}': 0x{res:X}");
                return Err(Error::Io(std::io::Error::other(format!(
                    "Unable to set property \"{name}\" to \"{value}\": error code: 0x{res:X}"
                ))));
            }
        }
    }

    Ok(())
}
