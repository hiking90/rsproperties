// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::net::UnixStream;
use std::io::{prelude::*, IoSlice};
use std::{fs, env, path::{Path, PathBuf}};
use std::sync::OnceLock;

use zerocopy_derive::*;
use zerocopy::IntoBytes;

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
    log::info!("Attempting to set socket directory to: {}", dir_path.display());

    match SOCKET_DIR.set(dir_path.clone()) {
        Ok(_) => {
            log::info!("Successfully set socket directory to: {}", dir_path.display());
            true
        }
        Err(_) => {
            log::warn!("Socket directory already set, ignoring new value: {}", dir_path.display());
            false
        }
    }
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
        let dir_path = PathBuf::from(dir);
        log::debug!("Initialized socket directory to: {}", dir_path.display());
        dir_path
    })
}

/// Get the full path to the property service socket
fn get_property_service_socket() -> String {
    let socket_path = socket_dir().join(PROPERTY_SERVICE_SOCKET_NAME);
    log::trace!("Property service socket path: {}", socket_path.display());
    socket_path.to_string_lossy().into_owned()
}

/// Get the full path to the system property service socket
fn get_property_service_for_system_socket() -> String {
    let socket_path = socket_dir().join(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME);
    log::trace!("System property service socket path: {}", socket_path.display());
    socket_path.to_string_lossy().into_owned()
}

struct ServiceConnection {
    stream: UnixStream,
}

impl ServiceConnection {
    fn new(name: &str) -> Result<Self> {
        log::debug!("Creating service connection for property: {}", name);

        let property_service_socket = get_property_service_socket();

        let socket_name = if name == "sys.powerctl" {
            let system_socket = get_property_service_for_system_socket();
            if fs::metadata(&system_socket)
                .map(|metadata| !metadata.permissions().readonly())
                .is_ok() {
                log::debug!("Using system property service socket: {}", system_socket);
                system_socket
            } else {
                log::warn!("System property service socket is not writable or does not exist, falling back to default: {}", property_service_socket);
                property_service_socket
            }
        } else {
            log::debug!("Using default property service socket: {}", property_service_socket);
            property_service_socket
        };

        log::trace!("Connecting to Unix domain socket: {}", socket_name);
        let stream: UnixStream = UnixStream::connect(&socket_name)
            .map_err(|e| Error::new_io(e))?;

        log::debug!("Successfully connected to property service socket: {}", socket_name);
        Ok(Self { stream })
    }

    fn recv_i32(&mut self) -> Result<i32> {
        log::trace!("Receiving i32 response from property service");
        let mut buf = [0u8; 4];
        self.stream.read_exact(&mut buf)
            .map_err(|e| Error::new_io(e))?;
        let value = i32::from_ne_bytes(buf);
        log::debug!("Received i32 response from property service: {}", value);
        Ok(value)
    }
}

struct ServiceWriter<'a> {
    buffers: Vec<IoSlice<'a>>,
}

impl<'a> ServiceWriter<'a> {
    fn new() -> Self {
        log::trace!("Creating new ServiceWriter");
        Self {
            buffers: Vec::with_capacity(4),
        }
    }

    fn write_str(self, value: &'a str, len: &'a u32) -> Self {
        log::trace!("Writing string to ServiceWriter: '{}' (length: {})", value, len);
        let mut thiz = self.write_u32(len);
        thiz.buffers.push(IoSlice::new(value.as_bytes()));
        thiz
    }

    fn write_u32(mut self, value: &'a u32) -> Self {
        log::trace!("Writing u32 to ServiceWriter: {}", value);
        self.buffers.push(IoSlice::new(value.as_bytes()));
        self
    }

    fn write_bytes(mut self, value: &'a [u8]) -> Self {
        log::trace!("Writing {} bytes to ServiceWriter", value.len());
        self.buffers.push(IoSlice::new(value));
        self
    }

    fn send(self, conn: &mut ServiceConnection) -> Result<()> {
        log::debug!("Sending {} buffers to property service", self.buffers.len());
        conn.stream.write_vectored(&self.buffers)
            .map_err(|e| Error::new_io(e))?;
        log::trace!("Flushing property service connection");
        conn.stream.flush()
            .map_err(|e| Error::new_io(e))?;
        log::debug!("Successfully sent data to property service");
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
        log::trace!("Creating PropertyMessage with cmd: {}, name: '{}', value: '{}'", cmd, name, value);

        let mut name_buf = [0; PROP_NAME_MAX];
        let mut value_buf = [0; PROP_VALUE_MAX];

        let name_bytes = name.as_bytes();
        let value_bytes = value.as_bytes();

        if name_bytes.len() > PROP_NAME_MAX {
            log::warn!("Property name truncated from {} to {} bytes", name_bytes.len(), PROP_NAME_MAX);
        }
        if value_bytes.len() > PROP_VALUE_MAX {
            log::warn!("Property value truncated from {} to {} bytes", value_bytes.len(), PROP_VALUE_MAX);
        }

        name_buf[..name_bytes.len().min(PROP_NAME_MAX)].copy_from_slice(&name_bytes[..name_bytes.len().min(PROP_NAME_MAX)]);
        value_buf[..value_bytes.len().min(PROP_VALUE_MAX)].copy_from_slice(&value_bytes[..value_bytes.len().min(PROP_VALUE_MAX)]);

        log::trace!("PropertyMessage created successfully");
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
        log::debug!("Determining property service protocol version");

        // Try to get version from environment variable first
        let version = env::var("PROPERTY_SERVICE_VERSION")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                log::debug!("No PROPERTY_SERVICE_VERSION environment variable set, defaulting to V2");
                2
            });

        log::debug!("Using service version: {}", version);

        let protocol_version = if version >= 2 {
            log::info!("Using property service protocol version 2");
            ProtocolVersion::V2
        } else {
            log::info!("Using property service protocol version 1");
            ProtocolVersion::V1
        };

        protocol_version
    })
}

use rustix::fd::{AsFd, BorrowedFd};

fn wait_for_socket_close(_socket_fd: BorrowedFd<'_>) -> Result<()> {
    use std::time::Duration;
    use std::thread;

    log::trace!("Waiting for socket close with timeout approach");
    // Simple timeout approach - sleep for 250ms
    thread::sleep(Duration::from_millis(250));
    log::debug!("Timeout reached after 250ms, treating as success");

    Ok(())
}


// Set a system property via local domain socket.
pub(crate) fn set(name: &str, value: &str) -> Result<()> {
    log::info!("Setting system property: '{}' = '{}'", name, value);

    match protocol_version() {
        ProtocolVersion::V1 => {
            log::debug!("Using protocol version 1 for property setting");

            if name.len() >= PROP_NAME_MAX {
                log::error!("Property name too long for V1 protocol: {} >= {}", name.len(), PROP_NAME_MAX);
                return Err(Error::new_file_validation(format!("Property name is too long: {}", name.len())).into());
            }

            if value.len() >= PROP_VALUE_MAX {
                log::error!("Property value too long for V1 protocol: {} >= {}", value.len(), PROP_VALUE_MAX);
                return Err(Error::new_file_validation(format!("Property value is too long: {}", value.len())).into());
            }

            log::trace!("Creating service connection for V1 protocol");
            let mut conn = ServiceConnection::new(PROP_SERVICE_NAME)?;

            log::trace!("Creating property message for V1 protocol");
            let prop_msg = PropertyMessage::new(PROP_MSG_SETPROP, name, value);

            log::debug!("Sending property message via V1 protocol");
            ServiceWriter::new()
                .write_bytes(prop_msg.as_bytes())
                .send(&mut conn)?;

            log::trace!("Waiting for socket close after V1 property set");
            wait_for_socket_close(conn.stream.as_fd())?;
            log::info!("Successfully set property '{}' using V1 protocol", name);
        }
        ProtocolVersion::V2 => {
            log::debug!("Using protocol version 2 for property setting");

            let value_len = value.len() as u32;
            let name_len = name.len() as u32;

            log::trace!("Property lengths - name: {}, value: {}", name_len, value_len);

            if value.len() >= PROP_VALUE_MAX && !name.starts_with("ro.") {
                log::error!("Property value too long for V2 protocol (non-ro property): {} >= {}", value.len(), PROP_VALUE_MAX);
                return Err(Error::new_file_validation(format!("Property value is too long: {}", value.len())).into());
            }

            log::trace!("Creating service connection for V2 protocol with property name: {}", name);
            let mut conn = ServiceConnection::new(name)?;

            log::debug!("Sending property data via V2 protocol");
            ServiceWriter::new()
                .write_u32(&PROP_MSG_SETPROP2)
                .write_str(name, &name_len)
                .write_str(value, &value_len)
                .send(&mut conn)?;

            log::trace!("Receiving response from property service");
            let res = conn.recv_i32()?;

            if res != PROP_SUCCESS {
                log::error!("Property service returned error for '{}' = '{}': 0x{:X}", name, value, res);
                return Err(Error::new_io(std::io::Error::new(std::io::ErrorKind::Other, format!("Unable to set property \"{name}\" to \"{value}\": error code: 0x{res:X}"))).into());
            }

            log::info!("Successfully set property '{}' using V2 protocol", name);
        }
    }

    log::debug!("Property setting operation completed successfully");
    Ok(())
}