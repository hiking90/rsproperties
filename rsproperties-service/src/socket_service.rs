// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, trace, warn};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

use rsactor::{Actor, ActorRef, ActorWeak};

use rsproperties::errors::*;
use rsproperties::wire::{PROP_ERROR, PROP_MSG_SETPROP2, PROP_SUCCESS};

/// Upper bound on simultaneously serviced client connections. Each accepted
/// connection takes one permit from the semaphore; further connections wait
/// in the kernel's accept queue. Sized to allow comfortable concurrency
/// without permitting an unbounded fan-out of spawned tasks.
const MAX_CONCURRENT_CLIENTS: usize = 64;

/// Wall-clock timeout for an entire `handle_client` exchange. Trusted
/// clients (init, system services) complete well under this; untrusted or
/// stuck clients are torn down rather than tying up a task indefinitely.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct SocketServiceArgs {
    pub socket_dir: PathBuf,
    pub properties_service: ActorRef<crate::PropertiesService>,
}

// Run the service in a separate task
/// This function runs the socket service by spawning a new actor with the provided arguments.
///
/// # Returns
/// A reference to the spawned actor that can be used to interact with the socket service.
/// The actor can be stopped by calling `actor_ref.stop()` when the service is no longer needed.
///
pub fn run(args: SocketServiceArgs) -> crate::ServiceContext<SocketService> {
    let (actor_ref, join_handle) = rsactor::spawn(args);
    crate::ServiceContext {
        actor_ref,
        join_handle,
    }
}

/// Tokio-based property socket service
pub struct SocketService {
    socket_dir: PathBuf,
    property_listener: UnixListener,
    system_listener: UnixListener,
    properties_service: ActorRef<crate::PropertiesService>,
    /// Limits concurrently in-flight client tasks.
    connection_sem: Arc<Semaphore>,
}

impl Actor for SocketService {
    type Args = SocketServiceArgs;
    type Error = rsproperties::errors::Error;

    async fn on_start(
        args: Self::Args,
        _actor_ref: &ActorRef<Self>,
    ) -> std::result::Result<Self, Self::Error> {
        // Create parent directory if it doesn't exist. `try_exists`
        // distinguishes "doesn't exist" (Ok(false)) from "couldn't ask"
        // (Err) — propagate the latter so permission/ENOTDIR errors don't
        // silently degrade to `create_dir_all` racing the same error.
        if !fs::try_exists(&args.socket_dir).await? {
            debug!("Creating parent directory: {:?}", args.socket_dir);
            fs::create_dir_all(&args.socket_dir).await?;
        }

        let property_socket_path = args
            .socket_dir
            .join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
        let system_socket_path = args
            .socket_dir
            .join(rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME);
        // Remove existing socket files if they exist
        if fs::try_exists(&property_socket_path).await? {
            debug!(
                "Removing existing property socket file: {}",
                property_socket_path.display()
            );
            fs::remove_file(&property_socket_path).await?;
        }
        if fs::try_exists(&system_socket_path).await? {
            debug!(
                "Removing existing system socket file: {}",
                system_socket_path.display()
            );
            fs::remove_file(&system_socket_path).await?;
        }
        info!(
            "Property socket services successfully created at: {} and {}",
            property_socket_path.display(),
            system_socket_path.display()
        );
        // Bind both sockets
        trace!(
            "Binding property service Unix domain socket: {}",
            property_socket_path.display()
        );
        let property_listener = UnixListener::bind(&property_socket_path)?;
        trace!(
            "Binding system property service Unix domain socket: {}",
            system_socket_path.display()
        );
        let system_listener = UnixListener::bind(&system_socket_path)?;
        info!("AsyncPropertySocketService started successfully");

        Ok(Self {
            socket_dir: args.socket_dir,
            property_listener,
            system_listener,
            properties_service: args.properties_service,
            connection_sem: Arc::new(Semaphore::new(MAX_CONCURRENT_CLIENTS)),
        })
    }

    async fn on_run(
        &mut self,
        _actor_weak: &ActorWeak<Self>,
    ) -> std::result::Result<bool, Self::Error> {
        // Race accept() on both listeners. Whichever fires first wins; the
        // cancelled accept restarts on the next on_run cycle (accept is
        // cancel-safe). Permit is acquired *after* a connection is in hand
        // so we don't burn a permit on every losing select! arm.
        let (stream, source) = tokio::select! {
            r = self.property_listener.accept() => (r, "property"),
            r = self.system_listener.accept() => (r, "system"),
        };

        let (stream, _addr) = match stream {
            Ok(ok) => ok,
            Err(e) => {
                error!("Error accepting connection on {source} listener: {e}");
                return Ok(true);
            }
        };

        let permit = match self.connection_sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                error!("Connection semaphore closed");
                return Ok(true);
            }
        };

        let connection_sender = self.properties_service.clone();
        tokio::spawn(async move {
            let _permit = permit; // dropped when the task ends
            match tokio::time::timeout(
                CLIENT_TIMEOUT,
                Self::handle_client(stream, connection_sender),
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
        Ok(true)
    }

    async fn on_stop(
        &mut self,
        _actor_weak: &ActorWeak<Self>,
        killed: bool,
    ) -> std::result::Result<(), Self::Error> {
        warn!("=====================================");
        warn!("      SOCKET SERVICE SHUTDOWN       ");
        warn!("=====================================");

        if killed {
            error!(
                "*** FORCED TERMINATION *** SocketService is being killed, cleaning up resources."
            );
        } else {
            warn!("*** GRACEFUL SHUTDOWN *** SocketService is stopping gracefully.");
        }

        warn!("SocketService cleanup completed - SERVICE TERMINATED");
        warn!("=====================================");

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

impl SocketService {
    /// Handles a client connection
    async fn handle_client(
        mut stream: UnixStream,
        service: ActorRef<crate::PropertiesService>,
    ) -> Result<()> {
        trace!("Handling new client connection");

        // Read the command (u32)
        let mut cmd_buf = [0u8; 4];
        stream.read_exact(&mut cmd_buf).await?;
        let cmd = u32::from_ne_bytes(cmd_buf);

        debug!("Received command: 0x{cmd:08X}");

        match cmd {
            PROP_MSG_SETPROP2 => {
                trace!("Processing SETPROP2 command");
                Self::handle_setprop2(&mut stream, service).await?;
            }
            _ => {
                warn!("Unknown command received: 0x{cmd:08X}");
                Self::send_response(&mut stream, PROP_ERROR).await?;
            }
        }

        trace!("Client connection handled successfully");
        Ok(())
    }

    /// Handles SETPROP2 command
    async fn handle_setprop2(
        stream: &mut UnixStream,
        service: ActorRef<crate::PropertiesService>,
    ) -> Result<()> {
        trace!("Handling SETPROP2 request");

        // Read name length and name
        let name_len = Self::read_u32(stream).await?;
        trace!("Name length: {name_len}");

        if name_len > 1024 {
            // Reasonable limit
            error!("Name length too large: {name_len}");
            Self::send_response(stream, PROP_ERROR).await?;
            return Err(rsproperties::errors::Error::FileValidation(format!(
                "Name length too large: {name_len}"
            )));
        }

        let name = Self::read_string(stream, name_len as usize).await?;
        debug!("Property name: '{name}'");

        // Read value length and value
        let value_len = Self::read_u32(stream).await?;
        trace!("Value length: {value_len}");

        if value_len > 8192 {
            // Reasonable limit for property values
            error!("Value length too large: {value_len}");
            Self::send_response(stream, PROP_ERROR).await?;
            return Err(rsproperties::errors::Error::FileValidation(format!(
                "Value length too large: {value_len}"
            )));
        }

        let value = Self::read_string(stream, value_len as usize).await?;
        debug!("Property value: '{value}'");

        info!("Forwarding property: '{name}' = '{value}'");

        let property_msg = crate::PropertyMessage { name, value };

        match service.ask(property_msg).await {
            Ok(true) => Self::send_response(stream, PROP_SUCCESS).await?,
            Ok(false) => {
                warn!("Property message was not processed by service");
                Self::send_response(stream, PROP_ERROR).await?;
            }
            Err(e) => {
                error!("Failed to send property message through channel: {e}");
                Self::send_response(stream, PROP_ERROR).await?;
            }
        }

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

        // Remove null terminator if present
        if let Some(null_pos) = buf.iter().position(|&x| x == 0) {
            buf.truncate(null_pos);
        }

        String::from_utf8(buf).map_err(|e| rsproperties::errors::Error::Encoding(e.to_string()))
    }

    /// Sends a response to the client
    async fn send_response(stream: &mut UnixStream, response: i32) -> Result<()> {
        trace!("Sending response: {response}");
        stream.write_all(&response.to_ne_bytes()).await?;
        stream.flush().await?;
        trace!("Response sent successfully");
        Ok(())
    }
}

impl Drop for SocketService {
    fn drop(&mut self) {
        debug!("Cleaning up async socket service");

        // Drop runs in sync context — keep blocking std::fs here (rare path).
        for socket_name in [
            rsproperties::PROPERTY_SERVICE_SOCKET_NAME,
            rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME,
        ] {
            let path = self.socket_dir.join(socket_name);
            if path.exists() {
                match std::fs::remove_file(&path) {
                    Ok(()) => debug!("Socket file removed: {}", path.display()),
                    Err(e) => warn!("Failed to remove socket file {}: {e}", path.display()),
                }
            }
        }
    }
}
