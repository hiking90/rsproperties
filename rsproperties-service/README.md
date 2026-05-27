# rsproperties-service

An async, tokio-based service implementation for Android system properties with Unix domain socket support.

[![Crates.io](https://img.shields.io/crates/v/rsproperties-service.svg)](https://crates.io/crates/rsproperties-service)
[![Documentation](https://docs.rs/rsproperties-service/badge.svg)](https://docs.rs/rsproperties-service)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## Overview

`rsproperties-service` provides a high-performance, async property service that mimics Android's property system. It features a Unix domain socket server that handles property operations using an actor-based architecture powered by the `rsactor` framework.

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
- Supports SETPROP2 command for property setting
- Manages concurrent client sessions

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
rsproperties-service = "0.4"
```

Or add it with the builder feature:

```toml
[dependencies]
rsproperties-service = { version = "0.4", features = ["builder"] }
```

## Quick Start

### Basic Service Setup

```rust
use rsproperties_service;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create configuration
    let config = rsproperties::PropertyConfig::with_both_dirs(
        PathBuf::from("/tmp/properties"),
        PathBuf::from("/tmp/sockets")
    );

    // Start the services
    let (socket_service, properties_service) = rsproperties_service::run(
        config,
        vec![], // property_contexts_files
        vec![], // build_prop_files
    ).await?;

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
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Setup directories
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

    // Start services with configuration files
    let (socket_service, properties_service) = rsproperties_service::run(
        config,
        property_contexts,
        build_props,
    ).await?;

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
same socket the external clients use.

## Protocol Compatibility

The socket service implements the Android property service protocol:

- **Socket Names**: Uses standard Android socket names (`property_service`, `property_service_for_system`)
- **Commands**: Supports `PROP_MSG_SETPROP2` (0x00020001) command
- **Response Codes**: Returns `PROP_SUCCESS` (0) or `PROP_ERROR` (-1)
- **Message Format**: Compatible with Android's binary protocol

## Error Handling

The service provides comprehensive error handling:

```rust
match rsproperties_service::run(config, contexts, props).await {
    Ok((socket_service, properties_service)) => {
        // Services started successfully
        println!("All services running");
    }
    Err(e) => {
        eprintln!("Failed to start services: {}", e);
        // Handle specific error types
        if e.to_string().contains("Permission denied") {
            eprintln!("Check directory permissions");
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
├── property_info          # Property metadata (generated)
├── properties_serial      # Property versioning
└── u:object_r:*:s0       # SELinux context files

socket_dir/
├── property_service                    # Main property socket
└── property_service_for_system        # System property socket
```

## Security & Hardening

### Hardening highlights (0.4.0)

- **Socket permissions**: bound Unix sockets are `chmod`ed to `0o660`
  immediately after `bind()` so the file does not inherit the process
  umask. Matches the AOSP init policy for the property-service sockets.
- **Backpressure**: the connection-limit semaphore permit is acquired
  *before* `accept()`. Saturation parks the accept loop and lets the
  kernel backlog queue connect attempts, instead of accept-then-stall.
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
  into a `BTreeMap` before apply. Earlier `HashMap` iteration meant the
  "winning" value on a key conflict varied per run due to hash-seed
  randomisation.

### Size limits

The wire layer caps the upfront allocation for a single message at
1 KB (name) / 8 KB (value) to bound damage from a hostile peer. These
are *transport* caps; the actual property-system policy is stricter
and enforced after the bytes arrive:

- **Names**: `validate_property_name` — ASCII alphanumeric plus
  `_ . - @ :`, no leading `. - @ :`; `PROP_NAME_MAX = 32` bytes on the
  V1 wire path.
- **Values**: `validate_value_len` — `PROP_VALUE_MAX = 92` is the
  buffer size including the trailing NUL, so user content is capped
  at 91 bytes for non-`ro.*` properties. `ro.*` properties may exceed
  this via the long-property out-of-line storage path.

### Other security features

- **Path validation**: all file and directory paths are validated
- **SELinux**: property-context files are honoured when supplied
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
- **WARN**: Graceful shutdowns, configuration issues
- **INFO**: Service lifecycle, property operations
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
