# rsproperties-service

Linux emulation of the Android system property service: a tokio-based daemon that serves the property socket protocol and the property area files on a Linux host. It is not usable on Android, where init owns the property service, and compiles only for Linux; use the `rsproperties` client crate on Android.

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## Overview

`rsproperties-service` is an async property service that emulates the Android property service on Linux. It features a Unix domain socket server that handles property operations using an actor-based architecture powered by the `rsactor` framework.

## Key Features

- **🔄 Async Operations**: Built on tokio for high-performance async I/O
- **🎭 Actor-Based Architecture**: Uses rsactor for reliable message passing and state management
- **🔌 Unix Domain Socket Server**: Compatible with Android's property service protocol
- **⚡ High Performance**: Non-blocking property operations with concurrent client handling
- **🛡️ Robust Error Handling**: Comprehensive error handling with graceful degradation
- **📂 File-Based Configuration**: Supports property contexts and build.prop file loading
- **🔧 Configurable**: Flexible directory and socket path configuration

## Architecture

The service consists of two main components running as separate actors:

### PropertiesService
- Manages the actual property storage and retrieval
- Loads property contexts from files
- Processes build.prop files
- Handles property addition, updates, and lookups
- Maintains system property state

### SocketService
- Provides Unix domain socket interface
- Handles client connections and commands
- Implements Android-compatible property service protocol
- Supports SETPROP2 (V2) and legacy SETPROP (V1) commands for property setting
- Manages concurrent client sessions

## Installation

This crate is not published on crates.io; depend on it via git, and take
`rsproperties` (used for `PropertyConfig` in the examples below) from the same
git source. A crates.io `rsproperties` would be a separate crate whose
`PropertyConfig` `run` does not accept:

```toml
[dependencies]
rsproperties = { git = "https://github.com/hiking90/rsproperties" }
rsproperties-service = { git = "https://github.com/hiking90/rsproperties" }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
```

`rsproperties-service` always enables `rsproperties/builder`; its own `builder` feature adds nothing.

Supported platform: Linux only. On any other target (Android and macOS
included) the crate fails to compile with a `compile_error!` that names
the reason.

## Quick Start

### Basic Service Setup

```rust
use rsproperties_service;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // `run` creates a missing properties dir (last component only, 0711) and
    // socket dir (0755, parents included), both minus the umask's bits;
    // create them yourself beforehand to choose owner and mode. These
    // paths suit a single-user dev box only: under `/tmp` any local user
    // can create the directories first, and `run` uses an existing
    // directory as is (see "Socket dir contract").

    // Create configuration
    let config = rsproperties::PropertyConfig::with_both_dirs(
        PathBuf::from("/tmp/properties"),
        PathBuf::from("/tmp/sockets")
    );

    // Start the services (no input files, no persistence)
    let (socket_service, properties_service) =
        rsproperties_service::run(config, rsproperties_service::ServiceOptions::new()).await?;

    println!("Services started successfully!");

    // Keep services running
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("Shutdown signal received");
        }
        result = socket_service.join_handle => {
            if let Err(e) = result {
                eprintln!("Socket service error: {}", e);
            }
        }
        result = properties_service.join_handle => {
            if let Err(e) = result {
                eprintln!("Properties service error: {}", e);
            }
        }
    }

    Ok(())
}
```

### Advanced Configuration with Files

```rust
use rsproperties_service;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Linux host paths laid out like an Android rootfs; the socket dir must
    // be writable only by the service's user and root.
    let properties_dir = PathBuf::from("/system/etc/properties");
    let socket_dir = PathBuf::from("/dev/socket");

    // Configure property context files
    let property_contexts = vec![
        PathBuf::from("/system/etc/selinux/property_contexts"),
        PathBuf::from("/vendor/etc/selinux/vendor_property_contexts"),
    ];

    // Configure build.prop files
    let build_props = vec![
        PathBuf::from("/system/build.prop"),
        PathBuf::from("/vendor/build.prop"),
        PathBuf::from("/product/build.prop"),
    ];

    let config = rsproperties::PropertyConfig::with_both_dirs(
        properties_dir,
        socket_dir
    );

    // Start services with configuration files. `persist.*` properties are
    // kept in an AOSP-format persistent_properties file (its directory must
    // exist); `ctl.*` sets go to the handler instead of being stored.
    let options = rsproperties_service::ServiceOptions::new()
        .property_contexts_files(property_contexts)
        .build_prop_files(build_props)
        .persistent_properties_file("/data/property/persistent_properties")
        .control_handler(|msg| {
            eprintln!("ctl.{} {} from pid {:?}", msg.action, msg.target, msg.pid);
            false // no services to control: report failure
        });
    let (socket_service, properties_service) =
        rsproperties_service::run(config, options).await?;

    // Services are now running with full Android-compatible configuration
    tokio::join!(
        socket_service.join_handle,
        properties_service.join_handle
    );

    Ok(())
}
```

## Service Components

### ServiceContext

Each service returns a `ServiceContext` containing:

```rust
use rsactor::{Actor, ActorRef, ActorResult};
use tokio::task::JoinHandle;

pub struct ServiceContext<T: Actor> {
    pub actor_ref: ActorRef<T>,                  // send messages to the actor
    pub join_handle: JoinHandle<ActorResult<T>>, // await actor termination
}
```

### Message Types

`PropertyMessage` and `ReadyMessage` are crate-private — the
ready-handshake and property-update message round-trips are performed
by `run()` itself and by the socket-server task respectively. External
callers drive property updates by **connecting to the Unix socket and
sending an AOSP `SETPROP2` frame**, exactly as the Android client does;
the in-process actor surface is intentionally not part of the public
API. See [Protocol Compatibility](#protocol-compatibility) below.

If you need to read or set properties from the same process that owns
the service, use the `rsproperties` crate directly
(`rsproperties::get_or`, `rsproperties::set`, etc.) — it talks to the
same socket the external clients use. `set` is blocking socket I/O and
waits up to 2 s for the reply: called from a task on the runtime that
runs the service (e.g. a `current_thread` runtime), it keeps the service
from accepting, returns a timeout error, and the already-sent value may still be
applied once the runtime resumes. Run the service on its own runtime/thread, or
call `set` through `tokio::task::spawn_blocking` on a multi-thread
runtime.

## Protocol Compatibility

The socket service implements the Android property service protocol:

- **Socket Names**: Uses standard Android socket names (`property_service`, `property_service_for_system`)
- **Commands**: Supports `PROP_MSG_SETPROP2` (0x00020001) and the legacy V1 `PROP_MSG_SETPROP` (1), which gets no reply (AOSP V1 behavior)
- **Response Codes**: `PROP_SUCCESS` (0) or init's error codes
  (`rsproperties::wire::PROP_ERROR_*`): `READ_DATA` for an unreadable
  frame, `INVALID_CMD`, `INVALID_NAME`, `INVALID_VALUE` (too long, not
  UTF-8, or not matching the `property_contexts` type), `READ_ONLY_PROPERTY`,
  `HANDLE_CONTROL_MESSAGE` (`ctl.*` refused or failed), `SET_FAILED`
- **Type check**: values are checked against the `property_contexts` type
  (`bool`, `int`, `uint`, `double`, `size`, `enum ...`) as init's
  `CheckType` does; an empty value is always accepted
- **`ctl.*`**: never stored; passed to `ServiceOptions::control_handler`,
  or refused without one
- **Startup properties**: `ro.property_service.version=2`, the persistent
  properties, then `ro.persistent_properties.ready=true`, after the
  build.prop entries
- **Message Format**: Compatible with Android's binary protocol

## Error Handling

`run` returns a `StartupError`: `stage()` names the step that failed
(`StartupStage::Config`, `WriterLock`, `PropertiesService`,
`SocketService`) and `source()` is the cause — `std::io::Error` from the
properties service, `rsproperties::Error` from `try_init`, the writer
lock or the socket service.

```rust
use rsproperties_service::StartupStage;

match rsproperties_service::run(config, options).await {
    Ok((_socket_service, _properties_service)) => {
        println!("All services running");
    }
    Err(e) => {
        // Displays as "<stage> failed to start: <cause>".
        eprintln!("{e}");
        let cause = std::error::Error::source(&e);
        if e.stage() == StartupStage::WriterLock
            && matches!(
                cause.and_then(|c| c.downcast_ref::<rsproperties::Error>()),
                Some(rsproperties::Error::Lock(_))
            )
        {
            eprintln!("another instance owns the properties dir");
        }
        // Walk the rest of the chain, e.g. down to the OS error.
        let mut next = cause.and_then(|c| c.source());
        while let Some(c) = next {
            eprintln!("  caused by: {c}");
            next = c.source();
        }
    }
}
```

## Performance Features

- **Concurrent Connections**: Each client connection handled in separate tasks
- **Non-blocking I/O**: All operations use async/await for optimal performance
- **Memory Efficient**: Property data shared between services using actor references
- **Fast Lookups**: Optimized property storage for quick access

## Directory Structure

The service expects this directory layout:

```
properties_dir/
├── .writer_lock           # flock held by the running writer
├── property_info          # Property metadata (generated, replaced by rename)
├── properties_serial      # Property versioning
└── u:object_r:*:s0       # SELinux context files

socket_dir/
├── property_service                    # Main property socket
└── property_service_for_system        # System property socket
```

## Security & Hardening

### Hardening highlights

- **Socket permissions**: each socket's mode is set to `0o660` on the
  socket fd before `bind`, so the file exists only with `0o660` minus the
  umask's bits; if the umask cleared any of those bits, the mode is
  raised to exactly `0o660` through an `O_PATH|O_NOFOLLOW` descriptor
  (needs `/proc`: `/proc/thread-self/fd`, or `/proc/self/fd` on kernels
  before 3.17). Only then does the socket `listen` — a `connect`
  before that fails with `ECONNREFUSED`. Both sockets are bound as
  `.<name>.tmp-<pid>` and renamed onto their final names only after both
  listen. Stricter than AOSP init, which creates `property_service` 0666
  (relying on SELinux) and only `property_service_for_system` 0660.
- **Backpressure**: `accept()` does not wait for a permit. Each accepted
  connection must take a waiting-room permit with `try_acquire`
  (`MAX_WAITING_CLIENTS` = 256); when the waiting room is full the
  connection is closed immediately. The spawned task then waits up to
  `CLIENT_TIMEOUT` (10 s) for one of `MAX_CONCURRENT_CLIENTS` (64)
  handler permits and drops the connection on timeout. The accept loop
  never parks, and accepted fds are capped at 64 + 256.
- **`TimedOut` has an unknown outcome**: like AOSP init, the service
  applies every request it received in full, also when the client has
  already stopped waiting. The Linux client waits at most 2 s for the
  reply, so a `set` that returns `TimedOut` while waiting for the reply
  may have been applied (a `TimedOut` while connecting or sending means
  the request was not delivered). Retrying with the same value is
  harmless except for `ro.*`, where the retry gets `ServiceError` if the
  first attempt was applied.
- **`accept()` back-off**: a 100 ms sleep is inserted after an `accept()`
  error so an `EMFILE`/`ENFILE` storm cannot spin the worker and flood
  the log.
- **Value masking**: `PropertyMessage::value` is masked in `Debug`
  output and service logs (`<N bytes>` placeholder). Property values
  may carry tokens or device identifiers; names are still logged in
  full.
- **Panic-free init propagation**: `run()` calls `rsproperties::try_init`,
  so a misconfigured directory or a double-init surfaces as a `Result`
  rather than silently leaving the service bound to whatever paths a
  previous instance committed.
- **Deterministic `build.prop` apply order**: entries are collected
  into a `BTreeMap` before apply, so the order in which they are added
  to the area (and thus the trie layout) no longer varies per run with
  `HashMap`'s hash seed. Which file wins a key conflict does not depend
  on this: `load_properties_from_file` overwrites in call order, so the
  later file wins.

### Size limits

The wire layer caps the upfront allocation for a single message at
1 KB (name) / 8 KB (value) to bound damage from a hostile peer. These
are *transport* caps; the actual property-system policy is stricter
and enforced after the bytes arrive:

- **Names**: `validate_property_name` — ASCII alphanumeric plus
  `_ . - @ :`, no leading `.`; at most 31 bytes
  (`PROP_NAME_MAX - 1`, the 32-byte buffer includes the NUL) on the V1
  wire path.
- **Values**: `validate_value_len` — `PROP_VALUE_MAX = 92` is the
  buffer size including the trailing NUL, so user content is capped
  at 91 bytes for non-`ro.*` properties. `ro.*` properties may exceed
  this via the long-property out-of-line storage path.

### Other security features

- **Access control**: the socket file mode (`0o660`) plus the socket
  dir's permissions are the only access control. Any peer that can
  connect may set any non-`ro.` property, and any `ro.` property that
  has no value yet (the first writer fixes it).
- **Socket dir contract**: the service opens the socket dir once at
  startup and changes only entries directly in it named
  `property_service`, `property_service_for_system`, or
  `.<either name>.tmp-<pid>`, relative to that descriptor, never following
  a symlink at those names. `bind` itself takes a path: if the socket
  dir's path is swapped after startup opened it, the temp socket is
  created at the new location, startup fails, and that socket is left
  there. Any uid other than the service's and root that
  can write to the socket dir or one of its ancestors can replace the
  sockets with its own and so impersonate the service; that is out of
  scope. Keep the socket dir and its ancestors writable only by the
  service's user and root. A world-writable socket dir without the sticky
  bit is reported with a warning at startup; this is not an access check.
  A default ACL on the socket dir gives its named users and groups access
  beyond the mode bits; the service does not inspect or remove ACLs.
- **Single instance**: `run` takes the properties dir's writer lock before
  starting either actor, so a second `run` on the same properties dir
  fails (`StartupStage::WriterLock`) without touching `property_info` or
  the sockets. Run one instance per socket dir: two instances with
  different properties dirs must not share a socket dir.
- **property_contexts**: only maps property names to areas/contexts; no
  per-property (SELinux or `SO_PEERCRED`) authorization is performed.
- **Permission handling**: respects file system permissions

## Logging

The service provides detailed logging at multiple levels:

```rust
// Enable logging
env_logger::Builder::from_env(
    env_logger::Env::default().default_filter_or("info")
).init();
```

Log levels:
- **ERROR**: Service failures, connection errors
- **WARN**: Killed (non-graceful) shutdowns, configuration issues
- **INFO**: Service lifecycle (including graceful shutdowns), property operations
- **DEBUG**: Client connections, message details
- **TRACE**: Protocol-level details, fine-grained operations

## Examples

### Running the Example Service

From the workspace root:

```bash
cargo run -p rsproperties-service --example example_service -- \
    --properties-dir /tmp/test_properties \
    --socket-dir /tmp/test_sockets
```

### Testing with netcat

```bash
# Connect to the property service socket
nc -U /tmp/test_sockets/property_service
```

## Testing

Run the comprehensive test suite:

```bash
# Run all tests
cargo test

# Run with logging
RUST_LOG=debug cargo test -- --nocapture

# Run a single integration-test binary (file under `tests/`)
cargo test --test integration_tests
cargo test --test performance_tests
```

## Dependencies

- **tokio**: Async runtime and I/O
- **rsactor**: Actor framework for message passing
- **rsproperties**: Core property system implementation
- **log**: Logging framework
- **thiserror**: Error handling

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Contributing

Contributions are welcome! Please see the main [rsproperties](../README.md) project for contribution guidelines.

## Related Projects

- **[rsproperties](../rsproperties/)**: Core property system library
- **Android Property System**: Original implementation this project emulates
