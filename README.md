# rsproperties

A pure Rust implementation of Android's property system for Linux and Android.

## Overview

`rsproperties` is a comprehensive Rust library that provides Android system property functionality across platforms:

- **`rsproperties`**: Core library for getting/setting Android properties with memory-mapped access
- **`rsproperties-service`**: Linux emulation of the Android property service (async daemon; not usable on Android)

The library implements Android's property system semantics, including property contexts, SELinux integration, and futex-based property change notifications.

**Note:** This project is actively evolving. While core APIs are stable, some features may be refined in future releases.

## Supported Android Versions

This library supports Android versions from Android 9 (API level 28) to Android 17 (API level 37).
The Android 17 appcompat-override lookup is implemented from the AOSP `android-17.0.0_r1` sources
and was compared with `getprop` on an API 37 emulator.

## Features

- **Complete Android Properties Implementation**: Full Rust implementation of Android's property system - read, write, and monitor properties exactly like Android native code
- **Cross-Platform Compatibility**: Works seamlessly on both Android devices and Linux systems without modification
- **Pure Rust Solution**: No dependencies on Android's C libraries or JNI - everything implemented in Rust (`unsafe` is limited to the mmap / shared-memory layer)
- **Real-time Property Monitoring**: Watch for property changes in real-time, enabling reactive applications
- **High Performance**: Optimized for speed with direct memory access and zero-copy operations
- **Drop-in Replacement**: Compatible with Android's property naming conventions and value constraints
- **Linux Emulation**: Full Android property system emulation on Linux for development and testing
- **Thread-Safe Design**: Safe to use across multiple threads without external synchronization

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
rsproperties = "0.6"

# Optional features
[features]
builder = ["rsproperties/builder"]  # Enable property database building
```

To also run the Linux property service daemon (`rsproperties-service`, not
published on crates.io), take `rsproperties` from the same git source.
Mixing crates.io `rsproperties` with the git `rsproperties-service` builds two
separate `rsproperties` crates: the `PropertyConfig` you pass to `run` has the
wrong type, and the two copies do not share their configured directories.

```toml
[dependencies]
rsproperties = { git = "https://github.com/hiking90/rsproperties" }
rsproperties-service = { git = "https://github.com/hiking90/rsproperties" }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
```

## Quick Start

### Basic Property Operations

```rust
use rsproperties;

// Get property with default value (no initialization needed for default configuration)
let sdk_version: String = rsproperties::get_or("ro.build.version.sdk", "0".to_string());
println!("SDK Version: {}", sdk_version);

// Get property with type parsing and default fallback
let sdk_version: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
// Boolean properties, by libbase's `GetBoolProperty` rules: "1"/"0",
// "true"/"false", "y"/"n", "yes"/"no", "on"/"off".
let is_debuggable: bool = rsproperties::get_bool_or("ro.debuggable", false);

// Get property with error handling
match rsproperties::get::<String>("ro.build.version.release") {
    Ok(version) => println!("Android Version: {}", version),
    Err(e) => eprintln!("Failed to get version: {}", e),
}

// Zero-allocation read: borrow the value as `&str` without
// materializing a `String` (read_with returns an error if the
// property is missing).
let len: usize =
    rsproperties::system_properties()?.read_with("ro.build.version.sdk", |v| v.len())?;

// Set property (requires property service to be running)
if let Err(e) = rsproperties::set("debug.my_app.enabled", "true") {
    eprintln!("Failed to set property: {}", e);
}
```

### Initialization

`init` sets the directories before the first property access, and
`system_properties` returns the global reader, or `Error::Init` when the
store cannot be opened.

```rust
use rsproperties::{init, system_properties, PropertyConfig};

init(PropertyConfig::new().properties_dir("/dev/__properties__"))?;
let props = system_properties()?;              // &'static SystemProperties
let sdk: String = rsproperties::get_or("ro.build.version.sdk", "0".into());
```

Each directory is set once per process: by `init`, or by its first use,
which latches the default (a property read latches the properties dir, a
`set()` or `socket_dir()` call the socket dir). `init` succeeds when each
directory it names is unset or already holds that same path, and fails
with `Error::AlreadyInitialized` when one holds a different path — then
it changes neither. `system_properties` keeps the first reader it opens;
a failure is not kept, so a store created after the process started is
picked up by the next call.

### Property Monitoring and Waiting

```rust
use rsproperties;

let system_properties = rsproperties::system_properties()?;

// Wait for any property change (no timeout: `Ok(Some(serial))` or `Err`).
// `old_serial` is the serial the caller last saw: a change made between
// `area_serial()` and `wait` returns at once instead of being missed.
std::thread::spawn(move || {
    let old_serial = system_properties.area_serial();
    if let Ok(Some(new_serial)) = system_properties.wait(None, old_serial, None) {
        println!("Properties changed, new serial: {}", new_serial);
    }
});

// Wait for a specific property change, at most 30 s
std::thread::spawn(move || {
    if let Ok(Some(prop_index)) = system_properties.find("sys.boot_completed") {
        println!("Waiting for boot completion...");
        let Ok(old_serial) = system_properties.serial(prop_index) else {
            return;
        };
        let timeout = Some(std::time::Duration::from_secs(30));
        if let Ok(Some(_)) = system_properties.wait(Some(prop_index), old_serial, timeout) {
            println!("System boot completed!");
        }
    }
});

// Or wait for a value directly (libbase `WaitForProperty`)
let booted = system_properties
    .wait_for_value("sys.boot_completed", "1", Some(std::time::Duration::from_secs(30)));

// Monitor multiple properties
let monitored_props = vec![
    "sys.boot_completed",
    "dev.bootcomplete",
    "service.bootanim.exit"
];

for prop_name in monitored_props {
    match system_properties.get(prop_name) {
        Ok(value) => println!("{}: {}", prop_name, value),
        Err(_) => println!("{}: <not set>", prop_name),
    }
}
```

### Custom Configuration

> **Warning**: Do not use custom configuration on Android devices. Custom configuration is only intended for Linux environments or development/testing purposes.

```rust
use rsproperties::PropertyConfig;

// Both directories, or only the one to change. A directory already set
// to a different path makes `init` fail with `AlreadyInitialized`.
rsproperties::init(
    PropertyConfig::new()
        .properties_dir("/custom/properties")
        .socket_dir("/custom/socket"),
)?;
```

### Linux Property Service

On a Linux host, `rsproperties-service` emulates the Android property
service (it compiles only for Linux; on Android, init owns the property
service):

```rust
use rsproperties_service;
use rsproperties::PropertyConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // `run` creates a missing properties dir (last component only, 0711) and
    // socket dir (0755, parents included), both minus the umask's bits;
    // create them yourself to choose owner and mode.
    std::fs::create_dir_all("./properties")?;

    // Configure the service
    let config = PropertyConfig::new()
        .properties_dir("./properties")
        .socket_dir("./socket");

    // Optional: Load property contexts and build.prop files
    let property_contexts = vec![
        "plat_property_contexts".into(),
        "vendor_property_contexts".into(),
    ];

    let build_props = vec![
        "system_build.prop".into(),
        "vendor_build.prop".into(),
    ];

    // Start the property service
    let options = rsproperties_service::ServiceOptions::new()
        .property_contexts_files(property_contexts)
        .build_prop_files(build_props);
    let service = rsproperties_service::run(config, options).await?;

    println!("Property service running...");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;
    println!("Property service shutting down...");

    // Unlinks the sockets and releases the properties dir's writer lock.
    service.shutdown().await?;

    Ok(())
}
```

### Command Line Tools

The library includes Android-compatible command line tools:

#### getprop - Get Properties
```bash
# Get specific property
./getprop ro.build.version.sdk

# Get with default value
./getprop ro.build.version.sdk 0

# Use custom properties directory
./getprop --properties-dir ./my_props ro.product.device
```

#### setprop - Set Properties
```bash
# Set a property
./setprop debug.my_app.log_level verbose

# Use custom directories
./setprop --properties-dir ./props --socket-dir ./socket debug.test true
```

## Advanced Usage

### Building Property Databases

With the `builder` feature enabled, you can create property databases:

```rust
#[cfg(feature = "builder")]
use rsproperties::{
    build_trie, load_properties_from_file, AreaWriterLock, PropertyInfoEntry, SystemProperties,
};
use std::{collections::HashMap, path::Path};

// Load build.prop-format entries (key=value) into a HashMap.
let mut properties = HashMap::new();
load_properties_from_file(
    Path::new("system_build.prop"),
    None,                       // optional name filter (e.g. "ro.*")
    &mut properties,
)?;

// Serialize a property-info trie from `property_contexts`-style entries.
let property_infos: Vec<PropertyInfoEntry> = vec![/* parsed entries */];
let trie_data = build_trie(
    &property_infos,
    "u:object_r:build_prop:s0", // default context
    "string",                    // default type
)?;

// Take the directory's writer lock first (this creates a missing
// `./properties`, last component only), then publish the trie as
// `<dir>/property_info` and create the areas under that lock. Writing
// `property_info` yourself and then calling `new_area` lets another writer
// replace it between your write and `new_area`'s lock, and rewriting it in
// place breaks readers that still map it. Release builds (no `debug-assertions`) and the
// `strict-file-validation` feature require `property_info` to be owned by
// root (uid 0, gid 0), so run this as root there.
let lock = AreaWriterLock::acquire(Path::new("./properties"))?;
lock.publish_property_info(&trie_data)?;

// Create the on-disk property area and populate it.
let mut system_properties = SystemProperties::new_area_locked(lock)?;
for (key, value) in properties {
    system_properties.add(&key, &value)?;
}
```

`property_info` is replaced only by rename. Readers map it read-only and
shared, as bionic does, so a writer must write a complete new file and
rename it over the old one (`AreaWriterLock::publish_property_info` does
this, with mode 0444) and must never rewrite the existing file in place
(`std::fs::write`, `File::create`, `O_TRUNC`): a reader that has the file
mapped can fault with `SIGBUS` or read a torn trie. Readers log a warning
when `property_info` has the owner-write bit set, and refuse it with the
`strict-file-validation` feature. A group- or other-writable or symlinked
`property_info` is always refused.

### Error Handling

`rsproperties::Error` is a `thiserror`-derived, non-exhaustive enum with
`From` impls for `std::io::Error` (every OS error is `Error::Io`) and
`Utf8Error`.
Some paths wrap the failure: `Error::Context` adds a message and the
`panic::Location` of the failing call, and `Error::Init` marks a failed
creation of the global reader. `Error::root_cause()` returns the error
under those layers, so one `match` covers both the wrapped and the
unwrapped form.

```rust
use rsproperties::Error;

fn handle_property_operation() {
    match rsproperties::set("debug.my_app.config", "value") {
        Ok(_) => println!("Property set"),
        Err(e) => match e.root_cause() {
            Error::Io(io) => eprintln!("I/O failure: {io}"),
            Error::ServiceError { name, code } => eprintln!("{name} rejected: {code:#x}"),
            _ => eprintln!("other: {e}"),
        },
    }
}
```

### Thread Safety

All operations are thread-safe and can be used concurrently:

```rust
use std::thread;

// Multiple threads can safely access properties
let handles: Vec<_> = (0..10).map(|i| {
    thread::spawn(move || {
        let prop_name = format!("debug.thread.{}", i);
        let value: String = rsproperties::get_or(&prop_name, "default".to_string());
        println!("Thread {}: {} = {}", i, prop_name, value);
    })
}).collect();

for handle in handles {
    handle.join().unwrap();
}
```

## Platform Support

| Component | Linux | Android | macOS |
|---|---|---|---|
| `rsproperties` client (`get`, `set`, `wait`) | supported | supported | not supported |
| `builder` feature (creating property areas) | supported | — (init creates the areas) | not supported |
| `rsproperties-service` | supported | does not compile (init owns the property service) | does not compile |

macOS is not a supported target: the client crate still compiles for it,
but CI neither builds nor tests it.

### Android
- **Native Integration**: Direct access to `/dev/__properties__`
- **Property Contexts**: Full SELinux property context support
- **Bionic Compatibility**: Compatible with Android's property implementation
- **Standard Properties**: Access to all standard Android properties

### Linux
- **Full Emulation**: Complete Android property system emulation
- **Property Service**: Async property service daemon
- **Socket Communication**: Unix domain socket property setting
- **Memory Mapping**: Efficient memory-mapped property storage

### Cross-Platform Features
- **Futex Support**: Property change notifications on Linux and Android
- **Configurable Paths**: Custom property and socket directories
- **Environment Variables**: `PROPERTY_SERVICE_SOCKET_DIR` support

## Property Naming Conventions

The library follows Android property naming conventions:

- **Read-only properties**: `ro.*` (e.g., `ro.build.version.sdk`)
- **System properties**: `sys.*` (e.g., `sys.boot_completed`)
- **Persist properties**: `persist.*` (e.g., `persist.sys.timezone`)
- **Debug properties**: `debug.*` (e.g., `debug.my_app.log_level`)
- **Vendor properties**: `vendor.*` (e.g., `vendor.audio.config`)

### Property Constraints
- **Name length**: 31 bytes max in the V1 wire protocol
  (`name.len() < PROP_NAME_MAX`; the 32-byte buffer includes the NUL).
  V2 is length-prefixed; on Linux, client and service both enforce
  `wire::MAX_WIRE_NAME_LEN` (1024 bytes) on names (and
  `wire::MAX_WIRE_VALUE_LEN`, 8192 bytes, on values). On Android the
  client checks AOSP init's limit, `wire::AOSP_MAX_WIRE_STRING_LEN`
  (65535 bytes), instead.
- **Value length**: `PROP_VALUE_MAX = 92` is the **buffer size including
  the trailing NUL**, so user content is capped at **91 bytes**
  (matching bionic's `__system_property_set`). `ro.*` properties may
  exceed this via the long-property out-of-line storage path.
- **Character set**: ASCII alphanumeric plus `_`, `.`, `-`, `@`, `:`.
  Names must not begin with `.`. See
  [`rsproperties::wire::validate_property_name`] for the canonical check.

[`rsproperties::wire::validate_property_name`]: https://docs.rs/rsproperties/latest/rsproperties/wire/fn.validate_property_name.html

## Performance Characteristics

- **Memory-mapped access**: Direct memory access for optimal performance
- **Zero-copy reads**: Efficient property value retrieval
- **Atomic operations**: Thread-safe property updates
- **Futex-based waiting**: Efficient property change notifications

## Building and Testing

```bash
# Build the entire workspace
cargo build --workspace

# Build with all features
cargo build --workspace --all-features

# Run tests
cargo test --workspace

# Run tests with logging
RUST_LOG=debug cargo test --workspace

# Build examples (workspace-wide)
cargo build --workspace --examples

# Run the property-service example
cargo run -p rsproperties-service --example example_service
```

### Cross-compilation for Android

```bash
# Add Android target
rustup target add aarch64-linux-android

# Build for Android
cargo build --target aarch64-linux-android -p rsproperties  # rsproperties-service is Linux-only
```

## Examples

See `rsproperties/examples/` and `rsproperties-service/examples/` for complete working examples:

- **`getprop.rs`**: Android-compatible property getter
- **`setprop.rs`**: Android-compatible property setter
- **`example_service.rs`** (`rsproperties-service`, Linux only): property service daemon

## Contributing

Contributions are welcome! Please ensure:

1. All tests pass: `cargo test --workspace`
2. Code is formatted: `cargo fmt --all`
3. No clippy warnings: `cargo clippy --workspace --all-targets --all-features`
4. Documentation is updated for new features

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Acknowledgments

This implementation is based on Android's property system and maintains compatibility with Android's property semantics and behavior.
