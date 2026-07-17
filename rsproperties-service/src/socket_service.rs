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
use rsproperties::wire::{
    MAX_WIRE_NAME_LEN, MAX_WIRE_VALUE_LEN, PROP_ERROR, PROP_MSG_SETPROP, PROP_MSG_SETPROP2,
    PROP_NAME_MAX, PROP_SUCCESS, PROP_VALUE_MAX,
};

/// Upper bound on simultaneously *serviced* client connections. Each
/// handler task holds one permit for the duration of the exchange.
const MAX_CONCURRENT_CLIENTS: usize = 64;

/// Upper bound on accepted connections *waiting* for a handler permit.
/// Every waiting task holds an accepted `UnixStream` (one fd), so without
/// this cap a connect flood while all handler permits are taken would
/// accumulate fds until EMFILE and take the whole process down. Beyond
/// `MAX_CONCURRENT_CLIENTS + MAX_WAITING_CLIENTS`, new connections are
/// dropped immediately; well-behaved clients see ECONNRESET and retry.
const MAX_WAITING_CLIENTS: usize = 256;

/// Wall-clock timeout for an entire `handle_client` exchange. Trusted
/// clients (init, system services) complete well under this; untrusted or
/// stuck clients are torn down rather than tying up a task indefinitely.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Permissions applied to the bound Unix socket files. `0o660`
/// (rw-rw----) matches the AOSP init policy for property service sockets
/// — readable/writable by owner and group, denied to others. Without
/// this explicit chmod the file would inherit the process umask, which
/// is environment-dependent and frequently leaves the socket
/// world-readable.
///
/// **Access model.** This file mode is the service's *entire* access
/// control: any peer that can `connect()` (i.e. has write permission on
/// the socket file — owner or group) may set any non-`ro.` property.
/// Unlike AOSP init, no per-property SO_PEERCRED / property_contexts
/// authorization is performed, and the "property" / "system" sockets
/// share one handler; peer credentials are logged for auditability only.
/// This is an intentional simplification for host-side deployments —
/// restrict the socket directory / group membership accordingly.
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

/// Binds a Unix socket so that it is never reachable at `path` with
/// permissions wider than `SOCKET_FILE_MODE`: bind under a temporary name,
/// chmod, then atomically rename into place (replacing any previous socket
/// with no ENOENT window). A plain bind + chmod leaves a window with
/// umask-derived permissions in which a connection can be established (and
/// survive the later chmod). Deliberately NOT done by locking the process
/// umask around the bind — umask is process-global and would race any
/// concurrent file creation (e.g. the sibling PropertiesService writing
/// `property_info` from `spawn_blocking`).
///
/// Note: the temp name is `.{name}.tmp-{pid}`, slightly longer than the
/// final path — socket dirs near the `sun_path` limit (~104/108 bytes)
/// need that much headroom. Stale temps from crashed runs (different pid)
/// are swept by `on_start`.
async fn bind_socket_with_mode(path: &Path) -> std::io::Result<UnixListener> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::other(format!("invalid socket path: {path:?}")))?;
    let tmp_path = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));

    // Clear our own leftover temp (pid reuse / repeated on_start).
    match fs::remove_file(&tmp_path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let listener = UnixListener::bind(&tmp_path)?;
    let finalize = async {
        chmod_socket(&tmp_path).await?;
        // `rename` atomically replaces any existing file at `path`, so the
        // socket appears there already carrying 0o660.
        fs::rename(&tmp_path, path).await
    };
    if let Err(e) = finalize.await {
        // Tokio's UnixListener does not unlink on drop — clean up the temp
        // so failed starts don't litter the socket directory.
        let _ = fs::remove_file(&tmp_path).await;
        return Err(e);
    }
    Ok(listener)
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
    /// Limits accepted-but-not-yet-serviced connections (fd backpressure);
    /// see `MAX_WAITING_CLIENTS`.
    waiting_sem: Arc<Semaphore>,
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
        // No pre-unlink of existing socket files: `bind_socket_with_mode`
        // replaces them atomically via rename, so a restart never exposes
        // an ENOENT window to connecting clients. Only sweep stale *temp*
        // sockets left by crashed previous runs (their names embed another
        // pid, so per-name cleanup can't catch them).
        if let Ok(mut entries) = fs::read_dir(&args.socket_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with('.') && name.contains(".tmp-") {
                        debug!("Removing stale temp socket: {:?}", entry.path());
                        let _ = fs::remove_file(entry.path()).await;
                    }
                }
            }
        }
        info!(
            "Property socket services will be created at: {} and {}",
            property_socket_path.display(),
            system_socket_path.display()
        );
        // Bind both sockets via the chmod-then-rename pattern so they are
        // never connectable with permissions wider than SOCKET_FILE_MODE
        // (see `bind_socket_with_mode`).
        trace!(
            "Binding property service Unix domain sockets: {} and {}",
            property_socket_path.display(),
            system_socket_path.display()
        );
        let property_listener = bind_socket_with_mode(&property_socket_path).await?;
        let system_listener = bind_socket_with_mode(&system_socket_path).await?;
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
            waiting_sem: Arc::new(Semaphore::new(MAX_WAITING_CLIENTS)),
        })
    }

    async fn on_idle(
        &mut self,
        event: Self::IdleEvent,
        _actor_weak: &ActorWeak<Self>,
    ) -> std::result::Result<(), Self::Error> {
        // IMPORTANT: the actor runtime awaits `on_idle` inline in its
        // single `select!` loop — any await that parks here stalls the
        // mailbox (stop/ask) AND the other listener's accepts. Keep this
        // body non-blocking except for the short, bounded error backoff.
        let (accepted, source) = event;

        let stream = match accepted {
            Ok(stream) => stream,
            Err(e) => {
                // Permanent conditions (EMFILE, ENFILE, broken listener)
                // would otherwise let the idle loop spin at full speed and
                // saturate the log with the same error every microsecond. A
                // small sleep dampens the loop and gives the kernel time to
                // recover; it does block the actor loop, but only for the
                // fixed 100ms — unlike an unbounded permit wait.
                // `UnixListenerStream` keeps yielding after an error, so the
                // listener is not lost.
                error!("Error accepting connection on {source} listener: {e}");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                return Ok(());
            }
        };

        // Peer credentials: logged for auditability — see the access-model
        // note on `SOCKET_FILE_MODE` (no per-property authorization).
        if let Ok(cred) = stream.peer_cred() {
            debug!(
                "Client connected on {source} listener (uid={}, gid={})",
                cred.uid(),
                cred.gid()
            );
        }

        // Bound the number of concurrently in-flight client handlers
        // WITHOUT awaiting in the actor loop — the previous inline
        // `acquire_owned().await` parked the whole loop (mailbox + the
        // other listener) for up to CLIENT_TIMEOUT per saturated
        // connection. Two-level backpressure, both non-blocking here:
        // 1. a *waiting-room* permit is taken via try_acquire; if the room
        //    is full the connection is dropped immediately, which caps the
        //    total accepted fds at MAX_CONCURRENT_CLIENTS +
        //    MAX_WAITING_CLIENTS instead of growing until EMFILE;
        // 2. the spawned task then waits (deadline-bounded) for a handler
        //    permit, holding its waiting-room slot until it gets one.
        let waiting = match self.waiting_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!("Waiting room full; dropping {source} connection");
                return Ok(()); // `stream` dropped → connection closed
            }
        };
        let sem = self.connection_sem.clone();
        let connection_sender = self.properties_service.clone();
        tokio::spawn(async move {
            let permit = {
                let _waiting = waiting; // released once a handler slot is ours
                match tokio::time::timeout(CLIENT_TIMEOUT, sem.acquire_owned()).await {
                    Ok(Ok(p)) => p,
                    Ok(Err(_)) => {
                        error!("Connection semaphore closed");
                        return;
                    }
                    Err(_elapsed) => {
                        warn!(
                            "No handler slot available within {CLIENT_TIMEOUT:?}, dropping {source} connection"
                        );
                        return;
                    }
                }
            };
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
        // A graceful stop is normal operation — keep it at `info!` so log
        // monitors don't alarm on routine shutdowns; only a kill warrants
        // `warn!`.
        if killed {
            warn!("SocketService killed — cleaning up resources");
        } else {
            info!("SocketService stopping gracefully");
        }
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
        if let Err(e) = stream.read_exact(&mut cmd_buf).await {
            // Connect-then-close without writing (port probes, health
            // checks) is routine — not worth an `error!` in the caller.
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                debug!("Client closed the connection before sending a command");
                return Ok(());
            }
            return Err(e.into());
        }
        let cmd = u32::from_ne_bytes(cmd_buf);

        debug!("Received command: 0x{cmd:08X}");

        match cmd {
            PROP_MSG_SETPROP => {
                trace!("Processing SETPROP (V1) command");
                Self::handle_setprop_v1(&mut stream, service).await?;
            }
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

    /// Handles the legacy V1 SETPROP command: after the already-consumed
    /// command word, a fixed-size payload of `PROP_NAME_MAX` name bytes and
    /// `PROP_VALUE_MAX` value bytes, both NUL-padded.
    ///
    /// V1 clients (bionic included) never read a status reply — closing the
    /// connection is the implicit ack — so failures are logged and the
    /// connection is closed without a response. Validation still happens in
    /// `PropertiesService::handle`, identical to the V2 path.
    async fn handle_setprop_v1(
        stream: &mut UnixStream,
        service: ActorRef<crate::PropertiesService>,
    ) -> Result<()> {
        trace!("Handling SETPROP (V1) request");

        let mut name_buf = [0u8; PROP_NAME_MAX];
        stream.read_exact(&mut name_buf).await?;
        let mut value_buf = [0u8; PROP_VALUE_MAX];
        stream.read_exact(&mut value_buf).await?;
        // AOSP V1 parity: init forces the last byte of both fields to NUL
        // before use (`prop_name[PROP_NAME_MAX-1] = 0`), capping names at
        // 31 chars and values at 91 bytes. Without this a non-bionic client
        // could submit a 32-char name that AOSP would have truncated.
        name_buf[PROP_NAME_MAX - 1] = 0;
        value_buf[PROP_VALUE_MAX - 1] = 0;

        let name = Self::string_from_fixed(&name_buf)?;
        let value = Self::string_from_fixed(&value_buf)?;
        info!("Forwarding V1 property: '{name}' ({} bytes)", value.len());

        let property_msg = crate::PropertyMessage { name, value };
        match service.ask(property_msg).await {
            Ok(true) => {}
            // The property name was already logged by the `info!` above;
            // mirroring the V2 handler, the result logs omit it.
            Ok(false) => warn!("V1 property was rejected by service"),
            Err(e) => error!("Failed to forward V1 property: {e}"),
        }

        Ok(())
    }

    /// Decodes a fixed-size NUL-padded V1 wire field into a `String`.
    ///
    /// Truncation at the first NUL is inherent to the V1 format — padding
    /// and interior NULs are indistinguishable in a fixed buffer, and
    /// bionic decodes identically (strlen). The rsproperties client
    /// rejects NUL-carrying names/values before sending; a foreign V1
    /// client sending them gets the same truncation AOSP would apply.
    fn string_from_fixed(buf: &[u8]) -> Result<String> {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        // `Utf8(e.utf8_error())`, not a `FromUtf8Error` passthrough: the
        // owned-error form retains the failed bytes — which here may be a
        // property *value* — and would leak them into any `{e:?}` log,
        // against this file's don't-log-values policy. `Utf8Error` keeps
        // the position diagnostics and the source chain without the bytes.
        String::from_utf8(buf[..end].to_vec())
            .map_err(|e| rsproperties::errors::Error::Utf8(e.utf8_error()))
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

        if name_len as usize > MAX_WIRE_NAME_LEN {
            error!("Name length too large: {name_len} (max {MAX_WIRE_NAME_LEN})");
            // Best-effort like every other V2 failure response: `?` here
            // would replace the real error with a write failure when the
            // peer is already gone.
            let _ = Self::send_response(stream, PROP_ERROR).await;
            return Err(rsproperties::errors::Error::FileValidation(format!(
                "Name length too large: {name_len}"
            )));
        }

        // Protocol consistency: every V2 failure after the command word
        // answers with a status code before closing, like the length-cap
        // paths above — a bare connection close left the client's
        // `recv_i32` to diagnose an EOF instead of a definite error.
        // (`send_response` results are ignored: the peer may already be
        // gone, which is fine — the response is best-effort.)
        let name = match Self::read_string(stream, name_len as usize).await {
            Ok(name) => name,
            Err(e) => {
                let _ = Self::send_response(stream, PROP_ERROR).await;
                return Err(e);
            }
        };
        debug!("Property name: '{name}'");

        // Read value length and value
        let value_len = match Self::read_u32(stream).await {
            Ok(len) => len,
            Err(e) => {
                let _ = Self::send_response(stream, PROP_ERROR).await;
                return Err(e);
            }
        };
        trace!("Value length: {value_len}");

        if value_len as usize > MAX_WIRE_VALUE_LEN {
            error!("Value length too large: {value_len} (max {MAX_WIRE_VALUE_LEN})");
            // Best-effort — see the name-length branch above.
            let _ = Self::send_response(stream, PROP_ERROR).await;
            return Err(rsproperties::errors::Error::FileValidation(format!(
                "Value length too large: {value_len}"
            )));
        }

        let value = match Self::read_string(stream, value_len as usize).await {
            Ok(value) => value,
            Err(e) => {
                let _ = Self::send_response(stream, PROP_ERROR).await;
                return Err(e);
            }
        };
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

        // Reject NUL bytes instead of truncating at the first one: V2
        // strings are length-prefixed and sent without a terminator
        // (bionic does the same), so a NUL inside the declared length is a
        // malformed frame. Truncating would silently retarget the write —
        // a name "a\0b" would become property "a" and *pass* the
        // downstream validators, which never see the NUL. AOSP init
        // likewise rejects such names (IsLegalPropertyName).
        if buf.contains(&0) {
            return Err(rsproperties::errors::Error::Encoding(
                "wire string contains an interior NUL byte".into(),
            ));
        }

        // See `string_from_fixed`: drop the failed bytes (possibly a
        // sensitive value), keep the positional diagnostics + source chain.
        String::from_utf8(buf).map_err(|e| rsproperties::errors::Error::Utf8(e.utf8_error()))
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
        //
        // Note: this unlinks by *name* without verifying the file is the
        // one this instance bound. If a new SocketService instance binds
        // the same paths before an old instance drops, the old Drop would
        // remove the new instance's live sockets — don't run two instances
        // against one socket_dir (the design assumes a single service).
        for socket_name in [
            rsproperties::PROPERTY_SERVICE_SOCKET_NAME,
            rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME,
        ] {
            let path = self.socket_dir.join(socket_name);
            // No `exists()` pre-check (TOCTOU): just remove and ignore
            // NotFound.
            match std::fs::remove_file(&path) {
                Ok(()) => debug!("Socket file removed: {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!("Failed to remove socket file {}: {e}", path.display()),
            }
        }
    }
}
