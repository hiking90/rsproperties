// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::io::{prelude::*, IoSlice};
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::{
    env, fs,
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

const PROP_MSG_SETPROP: u32 = 1;
const PROP_MSG_SETPROP2: u32 = 0x00020001;
const PROP_SUCCESS: i32 = 0;

const PROP_NAME_MAX: usize = 32;
const PROP_VALUE_MAX: usize = 92;

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

        let socket_name = if name == "sys.powerctl" {
            let system_socket = get_property_service_for_system_socket();
            if fs::metadata(&system_socket)
                .map(|metadata| !metadata.permissions().readonly())
                .is_ok()
            {
                system_socket
            } else {
                log::warn!("System property service socket is not writable or does not exist, falling back to default: {property_service_socket}");
                property_service_socket
            }
        } else {
            property_service_socket
        };

        let stream: UnixStream = UnixStream::connect(&socket_name).map_err(Error::new_io)?;

        Ok(Self { stream })
    }

    fn recv_i32(&mut self) -> Result<i32> {
        let mut buf = [0u8; 4];
        self.stream.read_exact(&mut buf).map_err(Error::new_io)?;
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
            .map_err(Error::new_io)?;
        conn.stream.flush().map_err(Error::new_io)?;
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
    fn new(cmd: u32, name: &str, value: &str) -> Self {
        let mut name_buf = [0; PROP_NAME_MAX];
        let mut value_buf = [0; PROP_VALUE_MAX];

        let name_bytes = name.as_bytes();
        let value_bytes = value.as_bytes();

        if name_bytes.len() > PROP_NAME_MAX {
            log::warn!(
                "Property name truncated from {} to {} bytes",
                name_bytes.len(),
                PROP_NAME_MAX
            );
        }
        if value_bytes.len() > PROP_VALUE_MAX {
            log::warn!(
                "Property value truncated from {} to {} bytes",
                value_bytes.len(),
                PROP_VALUE_MAX
            );
        }

        name_buf[..name_bytes.len().min(PROP_NAME_MAX)]
            .copy_from_slice(&name_bytes[..name_bytes.len().min(PROP_NAME_MAX)]);
        value_buf[..value_bytes.len().min(PROP_VALUE_MAX)]
            .copy_from_slice(&value_bytes[..value_bytes.len().min(PROP_VALUE_MAX)]);

        Self {
            cmd,
            name: name_buf,
            value: value_buf,
        }
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

use rustix::fd::{AsFd, BorrowedFd};

fn wait_for_socket_close(_socket_fd: BorrowedFd<'_>) -> Result<()> {
    use std::thread;
    use std::time::Duration;

    // Simple timeout approach - sleep for 250ms
    thread::sleep(Duration::from_millis(250));

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
                return Err(Error::new_file_validation(format!(
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
                return Err(Error::new_file_validation(format!(
                    "Property value is too long: {}",
                    value.len()
                )));
            }

            let mut conn = ServiceConnection::new(PROP_SERVICE_NAME)?;
            let prop_msg = PropertyMessage::new(PROP_MSG_SETPROP, name, value);

            ServiceWriter::new()
                .write_bytes(prop_msg.as_bytes())
                .send(&mut conn)?;

            wait_for_socket_close(conn.stream.as_fd())?;
        }
        ProtocolVersion::V2 => {
            let value_len = value.len() as u32;
            let name_len = name.len() as u32;

            if value.len() >= PROP_VALUE_MAX && !name.starts_with("ro.") {
                log::error!(
                    "Property value too long for V2 protocol (non-ro property): {} >= {}",
                    value.len(),
                    PROP_VALUE_MAX
                );
                return Err(Error::new_file_validation(format!(
                    "Property value is too long: {}",
                    value.len()
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
                log::error!("Property service returned error for '{name}' = '{value}': 0x{res:X}");
                return Err(Error::new_io(std::io::Error::other(format!(
                    "Unable to set property \"{name}\" to \"{value}\": error code: 0x{res:X}"
                ))));
            }
        }
    }

    Ok(())
}
