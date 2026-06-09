// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, trace, warn};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt;

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

/// Sanity upper bound on a V2 wire-protocol property name length. The real
/// AOSP limit is `PROP_NAME_MAX = 32`, but the wire format is
/// length-prefixed so we accept up to a much higher cap here and let
/// `validate_property_name` reject anything actually malformed. The cap
/// only exists to bound an upfront allocation against a hostile peer.
const MAX_WIRE_NAME_LEN: u32 = 1024;

/// Sanity upper bound on a V2 wire-protocol property value length.
/// Long-value `ro.` properties can legitimately exceed `PROP_VALUE_MAX`,
/// so the cap is generous; the actual length policy is enforced by
/// `validate_value_len` after the bytes are read.
const MAX_WIRE_VALUE_LEN: u32 = 8192;

/// Permissions applied to the bound Unix socket files. `0o660`
/// (rw-rw----) matches the AOSP init policy for property service sockets
/// — readable/writable by owner and group, denied to others. Without
/// this explicit chmod the file would inherit the process umask, which
/// is environment-dependent and frequently leaves the socket
/// world-readable.
const SOCKET_FILE_MODE: u32 = 0o660;

/// Backoff applied when `accept()` returns an error. Without it, a
/// permanent failure (EMFILE, ENFILE, listener torn down) would re-enter
/// `on_idle` immediately and spin the worker producing a high-rate log
/// flood. 100ms is short enough to recover quickly when the condition
/// clears, long enough to dampen the loop.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Applies `SOCKET_FILE_MODE` to a freshly-bound Unix socket file.
/// `UnixListener::bind` creates the socket with permissions derived from
/// the process umask; an explicit chmod removes that environmental
/// dependency.
async fn chmod_socket(path: &Path) -> std::io::Result<()> {
    fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_FILE_MODE)).await
}

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
    // `with_idle()` is required in 0.16: the idle-event channel is opt-in, and
    // this actor drives its accept loop through `subscribe_idle` / `on_idle`.
    // Without it `subscribe_idle` would return `IdleChannelNotEnabled` and the
    // listeners would never be polled.
    let (actor_ref, join_handle) =
        rsactor::spawn_with_options(args, rsactor::SpawnOptions::new().with_idle());
    crate::ServiceContext {
        actor_ref,
        join_handle,
    }
}

/// Tokio-based property socket service
pub struct SocketService {
    socket_dir: PathBuf,
    properties_service: ActorRef<crate::PropertiesService>,
    /// Limits concurrently in-flight client tasks.
    connection_sem: Arc<Semaphore>,
}

impl Actor for SocketService {
    type Args = SocketServiceArgs;
    type Error = rsproperties::errors::Error;
    /// Each idle event is one accepted connection (or an `accept()` error)
    /// tagged with the listener it came from, so logging can name the source.
    /// In 0.16 the accept loop is modelled as `Stream`s of connections
    /// subscribed via `subscribe_idle`, replacing the removed `on_run`.
    type IdleEvent = (std::io::Result<UnixStream>, &'static str);

    async fn on_start(
        args: Self::Args,
        actor_ref: &ActorRef<Self>,
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
        chmod_socket(&property_socket_path).await?;
        trace!(
            "Binding system property service Unix domain socket: {}",
            system_socket_path.display()
        );
        let system_listener = UnixListener::bind(&system_socket_path)?;
        chmod_socket(&system_socket_path).await?;
        info!("AsyncPropertySocketService started successfully");

        // Model each listener as a `Stream` of accepted connections and hand
        // it to the actor's idle loop. The runtime owns the stream state, so
        // — unlike the old `on_run` — accept progress is never lost to
        // `select!` cancellation. Subscribing from `on_start` is safe:
        // `subscribe_idle` is synchronous (`try_send`) and the runtime drains
        // queued subscriptions before the first loop iteration.
        actor_ref
            .subscribe_idle(UnixListenerStream::new(property_listener).map(|r| (r, "property")))
            .map_err(|e| std::io::Error::other(format!("subscribe property listener: {e}")))?;
        actor_ref
            .subscribe_idle(UnixListenerStream::new(system_listener).map(|r| (r, "system")))
            .map_err(|e| std::io::Error::other(format!("subscribe system listener: {e}")))?;

        Ok(Self {
            socket_dir: args.socket_dir,
            properties_service: args.properties_service,
            connection_sem: Arc::new(Semaphore::new(MAX_CONCURRENT_CLIENTS)),
        })
    }

    async fn on_idle(
        &mut self,
        event: Self::IdleEvent,
        _actor_weak: &ActorWeak<Self>,
    ) -> std::result::Result<(), Self::Error> {
        let (accepted, source) = event;

        let stream = match accepted {
            Ok(stream) => stream,
            Err(e) => {
                // Permanent conditions (EMFILE, ENFILE, broken listener)
                // would otherwise let the idle loop spin at full speed and
                // saturate the log with the same error every microsecond. A
                // small sleep both dampens the loop and gives the kernel time
                // to recover the resource. `UnixListenerStream` keeps yielding
                // after an error, so the listener is not lost.
                error!("Error accepting connection on {source} listener: {e}");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                return Ok(());
            }
        };

        // Bound the number of concurrently in-flight client handlers. The old
        // on_run acquired the permit *before* accepting; here the stream has
        // already accepted one connection, so we acquire after. The idle loop
        // is not polled again until this returns, so when all 64 permits are
        // taken at most one extra connection is held here while the rest queue
        // in the kernel's listen backlog — the same backpressure, off by one.
        let permit = match self.connection_sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                error!("Connection semaphore closed");
                return Ok(());
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
        Ok(())
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

        if name_len > MAX_WIRE_NAME_LEN {
            error!("Name length too large: {name_len} (max {MAX_WIRE_NAME_LEN})");
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

        if value_len > MAX_WIRE_VALUE_LEN {
            error!("Value length too large: {value_len} (max {MAX_WIRE_VALUE_LEN})");
            Self::send_response(stream, PROP_ERROR).await?;
            return Err(rsproperties::errors::Error::FileValidation(format!(
                "Value length too large: {value_len}"
            )));
        }

        let value = Self::read_string(stream, value_len as usize).await?;
        // Do NOT log the value; it may carry sensitive payloads. Names
        // are public on the wire (and surface in getprop output), but
        // values are not.
        debug!("Property value length: {} bytes", value.len());

        info!("Forwarding property: '{name}' ({} bytes)", value.len());

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
