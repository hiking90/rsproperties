// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]

use std::os::unix::net::{UnixListener, UnixStream};
use std::io::prelude::*;
use std::{fs, thread};
use std::path::Path;

use anyhow::Context;

use crate::errors::*;

const PROPERTY_SERVICE_SOCKET: &str = "/dev/socket/property_service";

const PROP_MSG_SETPROP2: u32 = 0x00020001;
const PROP_SUCCESS: i32 = 0;
const PROP_ERROR: i32 = -1;

pub struct PropertySocketService {
    listener: UnixListener,
    socket_path: String,
}

impl PropertySocketService {
    pub fn new(socket_path: Option<&str>) -> Result<Self> {
        let socket_path = socket_path.unwrap_or(PROPERTY_SERVICE_SOCKET).to_string();

        log::info!("Creating property socket service at: {}", socket_path);

        // Remove existing socket file if it exists
        if Path::new(&socket_path).exists() {
            log::debug!("Removing existing socket file: {}", socket_path);
            fs::remove_file(&socket_path)
                .context("Failed to remove existing socket file")?;
        }

        // Create parent directory if it doesn't exist
        if let Some(parent) = Path::new(&socket_path).parent() {
            if !parent.exists() {
                log::debug!("Creating parent directory: {:?}", parent);
                fs::create_dir_all(parent)
                    .context("Failed to create parent directory for socket")?;
            }
        }

        log::trace!("Binding Unix domain socket: {}", socket_path);
        let listener = UnixListener::bind(&socket_path)
            .context("Failed to bind Unix domain socket")?;

        log::info!("Property socket service successfully created at: {}", socket_path);

        Ok(Self {
            listener,
            socket_path,
        })
    }

    pub fn run(&self) -> Result<()> {
        log::info!("Starting property socket service on: {}", self.socket_path);

        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    log::debug!("New client connection received");

                    // Handle each connection in a separate thread
                    thread::spawn(move || {
                        if let Err(e) = Self::handle_client(stream) {
                            log::error!("Error handling client: {}", e);
                        }
                    });
                }
                Err(e) => {
                    log::error!("Error accepting connection: {}", e);
                }
            }
        }

        Ok(())
    }

    fn handle_client(mut stream: UnixStream) -> Result<()> {
        log::trace!("Handling new client connection");

        // Read the command (u32)
        let mut cmd_buf = [0u8; 4];
        stream.read_exact(&mut cmd_buf)
            .context("Failed to read command from client")?;
        let cmd = u32::from_ne_bytes(cmd_buf);

        log::debug!("Received command: 0x{:08X}", cmd);

        match cmd {
            PROP_MSG_SETPROP2 => {
                log::trace!("Processing SETPROP2 command");
                Self::handle_setprop2(&mut stream)?;
            }
            _ => {
                log::warn!("Unknown command received: 0x{:08X}", cmd);
                Self::send_response(&mut stream, PROP_ERROR)?;
                return Err(Error::new_context(format!("Unknown command: 0x{:08X}", cmd)).into());
            }
        }

        log::trace!("Client connection handled successfully");
        Ok(())
    }

    fn handle_setprop2(stream: &mut UnixStream) -> Result<()> {
        log::trace!("Handling SETPROP2 request");

        // Read name length and name
        let name_len = Self::read_u32(stream)
            .context("Failed to read name length")?;
        log::trace!("Name length: {}", name_len);

        if name_len > 1024 { // Reasonable limit
            log::error!("Name length too large: {}", name_len);
            Self::send_response(stream, PROP_ERROR)?;
            return Err(Error::new_context(format!("Name length too large: {}", name_len)).into());
        }

        let name = Self::read_string(stream, name_len as usize)
            .context("Failed to read property name")?;
        log::debug!("Property name: '{}'", name);

        // Read value length and value
        let value_len = Self::read_u32(stream)
            .context("Failed to read value length")?;
        log::trace!("Value length: {}", value_len);

        if value_len > 8192 { // Reasonable limit for property values
            log::error!("Value length too large: {}", value_len);
            Self::send_response(stream, PROP_ERROR)?;
            return Err(Error::new_context(format!("Value length too large: {}", value_len)).into());
        }

        let value = Self::read_string(stream, value_len as usize)
            .context("Failed to read property value")?;
        log::debug!("Property value: '{}'", value);

        // Process the property setting
        match Self::process_property_set(&name, &value) {
            Ok(()) => {
                log::info!("Successfully set property: '{}' = '{}'", name, value);
                Self::send_response(stream, PROP_SUCCESS)?;
            }
            Err(e) => {
                log::error!("Failed to set property '{}' = '{}': {}", name, value, e);
                Self::send_response(stream, PROP_ERROR)?;
                return Err(e);
            }
        }

        Ok(())
    }

    fn read_u32(stream: &mut UnixStream) -> Result<u32> {
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf)
            .context("Failed to read u32")?;
        Ok(u32::from_ne_bytes(buf))
    }

    fn read_string(stream: &mut UnixStream, len: usize) -> Result<String> {
        if len == 0 {
            return Ok(String::new());
        }

        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf)
            .context("Failed to read string data")?;

        // Remove null terminator if present
        if let Some(null_pos) = buf.iter().position(|&x| x == 0) {
            buf.truncate(null_pos);
        }

        String::from_utf8(buf)
            .context("Invalid UTF-8 in string data")
    }

    fn send_response(stream: &mut UnixStream, response: i32) -> Result<()> {
        log::trace!("Sending response: {}", response);
        stream.write_all(&response.to_ne_bytes())
            .context("Failed to send response")?;
        stream.flush()
            .context("Failed to flush response")?;
        log::trace!("Response sent successfully");
        Ok(())
    }

    fn process_property_set(name: &str, value: &str) -> Result<()> {
        log::info!("Processing property set request: '{}' = '{}'", name, value);

        // Here you would implement the actual property setting logic
        // For now, we just validate and log the property

        // Basic validation
        if name.is_empty() {
            return Err(Error::new_context("Property name cannot be empty".to_string()).into());
        }

        if name.len() > 256 {
            return Err(Error::new_context("Property name too long".to_string()).into());
        }

        // Check for invalid characters in property name
        if !name.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '_') {
            return Err(Error::new_context("Invalid characters in property name".to_string()).into());
        }

        // Log the property setting (in a real implementation, you would store it)
        log::info!("Property set: {} = {}", name, value);

        // TODO: Implement actual property storage mechanism
        // This could involve:
        // - Writing to property files
        // - Updating in-memory property store
        // - Notifying property change listeners
        // - Applying property-specific validation rules

        Ok(())
    }
}

impl Drop for PropertySocketService {
    fn drop(&mut self) {
        log::debug!("Cleaning up socket service");
        if Path::new(&self.socket_path).exists() {
            if let Err(e) = fs::remove_file(&self.socket_path) {
                log::warn!("Failed to remove socket file {}: {}", self.socket_path, e);
            } else {
                log::debug!("Socket file removed: {}", self.socket_path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_service_creation() {
        let temp_socket = "/tmp/test_property_service";

        // Clean up any existing socket
        let _ = fs::remove_file(temp_socket);

        let service = PropertySocketService::new(Some(temp_socket))
            .expect("Failed to create socket service");

        // Verify socket file exists
        assert!(Path::new(temp_socket).exists());

        // Clean up
        drop(service);
        assert!(!Path::new(temp_socket).exists());
    }

    #[test]
    fn test_property_validation() {
        assert!(PropertySocketService::process_property_set("valid.property", "value").is_ok());
        assert!(PropertySocketService::process_property_set("", "value").is_err());
        assert!(PropertySocketService::process_property_set("invalid-char!", "value").is_err());
    }
}