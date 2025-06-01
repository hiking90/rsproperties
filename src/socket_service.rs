// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]

use std::os::unix::net::{UnixListener, UnixStream};
use std::io::prelude::*;
use std::{fs, thread};
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::Context;

use crate::errors::*;

const PROPERTY_SERVICE_SOCKET_NAME: &str = "property_service";
const PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME: &str = "property_service_for_system";

const PROP_MSG_SETPROP2: u32 = 0x00020001;
const PROP_SUCCESS: i32 = 0;
const PROP_ERROR: i32 = -1;

/// Represents a property key-value pair to be sent through the channel
#[derive(Debug, Clone)]
pub struct PropertyMessage {
    pub key: String,
    pub value: String,
}

pub struct PropertySocketService {
    property_listener: UnixListener,
    system_listener: UnixListener,
    property_sender: Sender<PropertyMessage>,
    system_sender: Sender<PropertyMessage>,
}

impl PropertySocketService {
    pub fn new(system_sender: Sender<PropertyMessage>, property_sender: Sender<PropertyMessage>) -> Result<Self> {
        let socket_dir = crate::system_property_set::get_socket_dir();
        log::info!("Creating property socket service at: {}", socket_dir.display());

        // Create parent directory if it doesn't exist
        if !socket_dir.exists() {
            log::debug!("Creating parent directory: {:?}", socket_dir);
            fs::create_dir_all(socket_dir)
                .context("Failed to create parent directory for socket")?;
        }

        // Create socket paths
        let property_socket_path = socket_dir.join(PROPERTY_SERVICE_SOCKET_NAME);
        let system_socket_path = socket_dir.join(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME);

        // Remove existing socket files if they exist
        if property_socket_path.exists() {
            log::debug!("Removing existing property socket file: {}", property_socket_path.display());
            fs::remove_file(&property_socket_path)
                .context("Failed to remove existing property socket file")?;
        }

        if system_socket_path.exists() {
            log::debug!("Removing existing system socket file: {}", system_socket_path.display());
            fs::remove_file(&system_socket_path)
                .context("Failed to remove existing system socket file")?;
        }

        // Bind both sockets
        log::trace!("Binding property service Unix domain socket: {}", property_socket_path.display());
        let property_listener = UnixListener::bind(&property_socket_path)
            .context("Failed to bind property service Unix domain socket")?;

        log::trace!("Binding system property service Unix domain socket: {}", system_socket_path.display());
        let system_listener = UnixListener::bind(&system_socket_path)
            .context("Failed to bind system property service Unix domain socket")?;

        log::info!("Property socket services successfully created at: {} and {}",
                  property_socket_path.display(), system_socket_path.display());

        Ok(Self {
            property_listener,
            system_listener,
            property_sender,
            system_sender,
        })
    }

    pub fn run(&self) -> Result<()> {
        // Clone the sender for use in threads
        let property_sender = self.property_sender.clone();
        let system_sender = self.property_sender.clone();

        // Start property service socket handler in a separate thread
        let property_listener = self.property_listener.try_clone()
            .context("Failed to clone property listener")?;
        let property_thread = thread::spawn(move || {
            Self::handle_socket_connections(property_listener, property_sender, "property")
        });

        // Start system property service socket handler in a separate thread
        let system_listener = self.system_listener.try_clone()
            .context("Failed to clone system listener")?;
        let system_thread = thread::spawn(move || {
            Self::handle_socket_connections(system_listener, system_sender, "system")
        });

        // Wait for both threads to complete (they run indefinitely)
        if let Err(e) = property_thread.join() {
            log::error!("Property socket thread panicked: {:?}", e);
        }

        if let Err(e) = system_thread.join() {
            log::error!("System socket thread panicked: {:?}", e);
        }

        Ok(())
    }

    fn handle_socket_connections(
        listener: UnixListener,
        sender: Sender<PropertyMessage>,
        socket_type: &'static str
    ) -> Result<()> {
        log::info!("Starting {} socket handler", socket_type);

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    log::debug!("New {} client connection received", socket_type);

                    // Clone sender for this connection
                    let connection_sender = sender.clone();

                    // Handle each connection in a separate thread
                    thread::spawn(move || {
                        if let Err(e) = Self::handle_client(stream, connection_sender) {
                            log::error!("Error handling {} client: {}", socket_type, e);
                        }
                    });
                }
                Err(e) => {
                    log::error!("Error accepting {} connection: {}", socket_type, e);
                }
            }
        }

        Ok(())
    }

    fn handle_client(mut stream: UnixStream, sender: Sender<PropertyMessage>) -> Result<()> {
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
                Self::handle_setprop2(&mut stream, sender)?;
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

    fn handle_setprop2(stream: &mut UnixStream, sender: Sender<PropertyMessage>) -> Result<()> {
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

                // Send property data through channel if sender is available
                let property_msg = PropertyMessage {
                    key: name.clone(),
                    value: value.clone(),
                };

                if let Err(e) = sender.send(property_msg) {
                    log::warn!("Failed to send property message through channel: {}", e);
                    // Don't fail the operation if channel send fails
                } else {
                    log::debug!("Property message sent through channel: '{}' = '{}'", name, value);
                }

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
        let socket_dir = crate::system_property_set::get_socket_dir();

        // Remove property service socket
        let property_socket_path = socket_dir.join(PROPERTY_SERVICE_SOCKET_NAME);
        if property_socket_path.exists() {
            if let Err(e) = fs::remove_file(&property_socket_path) {
                log::warn!("Failed to remove property socket file {}: {}", property_socket_path.display(), e);
            } else {
                log::debug!("Property socket file removed: {}", property_socket_path.display());
            }
        }

        // Remove system property service socket
        let system_socket_path = socket_dir.join(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME);
        if system_socket_path.exists() {
            if let Err(e) = fs::remove_file(&system_socket_path) {
                log::warn!("Failed to remove system socket file {}: {}", system_socket_path.display(), e);
            } else {
                log::debug!("System socket file removed: {}", system_socket_path.display());
            }
        }
    }
}

/// Creates a channel for receiving property messages
/// Returns (sender, receiver) pair where sender should be passed to PropertySocketService
/// and receiver can be used to receive property key-value pairs
pub fn create_property_channel() -> (Sender<PropertyMessage>, Receiver<PropertyMessage>) {
    mpsc::channel()
}