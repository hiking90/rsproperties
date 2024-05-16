// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::net::UnixStream;
use std::io::{prelude::*, IoSlice};
use std::fs;
use std::sync::OnceLock;

use zerocopy::AsBytes;
use zerocopy_derive::{FromBytes, FromZeroes, AsBytes};

use crate::errors::*;

const PROPERTY_SERVICE_SOCKET: &str = "/dev/socket/property_service";
const PROPERTY_SERVICE_FOR_SYSTEM_SOCKET: &str = "/dev/socket/property_service_for_system";
const SERVICE_VERSION_PROPERTY_NAME: &str = "ro.property_service.version";
const PROP_SERVICE_NAME: &str = "property_service";
// const PROP_SERVICE_FOR_SYSTEM_NAME: &str = "property_service_for_system";

const PROP_MSG_SETPROP: u32 = 1;
const PROP_MSG_SETPROP2: u32 = 0x00020001;
const PROP_SUCCESS: i32 = 0;

const PROP_NAME_MAX: usize = 32;
const PROP_VALUE_MAX: usize = 92;

struct ServiceConnection {
    stream: UnixStream,
}

impl ServiceConnection {
    fn new(name: &str) -> Result<Self> {
        let socket_name = if name == "sys.powerctl" &&
            fs::metadata(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET)
                .map(|metadata| metadata.permissions().readonly() == false)
                .is_ok() {
            PROPERTY_SERVICE_FOR_SYSTEM_SOCKET
        } else {
            PROPERTY_SERVICE_SOCKET
        };

        let stream = UnixStream::connect(socket_name)
            .map_err(Error::new_io)?;
        Ok(Self { stream })
    }

    fn recv_i32(&mut self) -> Result<i32> {
        let value: i32 = 0;
        self.stream.read_exact(&mut value.to_ne_bytes())
            .map_err(Error::new_io)?;
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
        conn.stream.write_vectored(&self.buffers)
            .map_err(Error::new_io)?;
        conn.stream.flush()
            .map_err(Error::new_io)?;
        Ok(())
    }
}

enum ProtocolVersion {
    V1 = 1,
    V2 = 2,
}

#[derive(AsBytes, FromZeroes, FromBytes, Debug)]
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

        name_buf[..name_bytes.len()].copy_from_slice(name_bytes);
        value_buf[..value_bytes.len()].copy_from_slice(value_bytes);

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
        let ver_str = crate::get_with_default(SERVICE_VERSION_PROPERTY_NAME, "1");
        let version = ver_str.parse().unwrap_or(1);
        if version >= 2 {
            ProtocolVersion::V2
        } else {
            ProtocolVersion::V1
        }
    })
}

use rustix::fd::{AsFd, BorrowedFd};
use rustix::event;

fn wait_for_socket_close(socket_fd: BorrowedFd<'_>) -> Result<()> {
    let mut fds = [event::PollFd::new(&socket_fd, event::PollFlags::HUP)];

    // Poll with a timeout of 250 milliseconds
    loop {
        match event::poll(&mut fds, 250) {
            Ok(0) => {
                // Timeout reached, treat as a success due to server delay
                log::info!("Timeout reached, but treating as success.");
                break;
            }
            Ok(_) => {
                if fds[0].revents().contains(event::PollFlags::HUP) {
                    // Socket has closed
                    log::info!("Socket closed.");
                    break;
                }
            }
            Err(e) => {
                // Handle possible errors
                return Err(Error::new_errno(e))
            }
        }
    }

    Ok(())
}


pub(crate) fn set(name: &str, value: &str) -> Result<()> {
    match protocol_version() {
        ProtocolVersion::V1 => {
            if name.len() >= PROP_NAME_MAX {
                return Err(Error::new_custom(
                    format!("Property name is too long: {}", name.len())));
            }

            if value.len() >= PROP_VALUE_MAX {
                return Err(Error::new_custom(
                    format!("Property value is too long: {}", value.len())));
            }

            let mut conn = ServiceConnection::new(PROP_SERVICE_NAME)?;

            let prop_msg = PropertyMessage::new(PROP_MSG_SETPROP, name, value);

            ServiceWriter::new()
                .write_bytes(prop_msg.as_bytes())
                .send(&mut conn)?;
            wait_for_socket_close(conn.stream.as_fd())?;
        }
        ProtocolVersion::V2 => {
            println!("Protocol version 2");
            let value_len = value.len() as u32;
            let name_len = name.len() as u32;
            if value.len() >= PROP_VALUE_MAX && name.starts_with("ro.") == false {
                return Err(Error::new_custom(
                    format!("Property value is too long: {}", value.len())));
            }

            let mut conn = ServiceConnection::new(name)?;
            ServiceWriter::new()
                .write_u32(&PROP_MSG_SETPROP2)
                .write_str(name, &name_len)
                .write_str(value, &value_len)
                .send(&mut conn)?;

            let res = conn.recv_i32()?;
            println!("res: {}", res);
            if res != PROP_SUCCESS {
                return Err(Error::new_custom(
                    format!("Unable to set property \"{name}\" to \"{value}\": error code: 0x{res:X}")));
            }
        }
    }

    Ok(())
}