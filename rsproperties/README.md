# rsproperties

[![Crates.io](https://img.shields.io/crates/v/rsproperties.svg)](https://crates.io/crates/rsproperties)
[![Documentation](https://docs.rs/rsproperties/badge.svg)](https://docs.rs/rsproperties)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

A pure Rust implementation of Android's property system, providing cross-platform access to Android system properties on both Linux and Android platforms.

## Supported Android Versions

This library supports Android versions from Android 9 (API level 28) to Android 16 (API level 36).

## Features

- **Complete Android Properties Implementation**: Full Rust implementation of Android's property system - read, write, and monitor properties exactly like Android native code
- **Cross-Platform Compatibility**: Works seamlessly on both Android devices and Linux systems without modification
- **Pure Rust Solution**: No dependencies on Android's C libraries or JNI - everything implemented in safe Rust
- **Real-time Property Monitoring**: Watch for property changes in real-time, enabling reactive applications
- **High Performance**: Optimized for speed with direct memory access and zero-copy operations
- **Drop-in Replacement**: Compatible with Android's property naming conventions and value constraints
- **Linux Emulation**: Full Android property system emulation on Linux for development and testing
- **Thread-Safe Design**: Safe to use across multiple threads without external synchronization

## Quick Start

Add `rsproperties` to your `Cargo.toml`:

```toml
[dependencies]
rsproperties = "0.6"

# Optional features
[features]
builder = ["rsproperties/builder"]  # Enable property database building
```

### Basic Usage

```rust
use rsproperties;

// Get property with default value (no initialization needed for default configuration)
let sdk_version: String = rsproperties::get_or("ro.build.version.sdk", "0".to_string());
println!("SDK Version: {}", sdk_version);

// Get property with type parsing and default fallback
let sdk_version: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
let is_debuggable: bool = rsproperties::get_or("ro.debuggable", false);

// Get property with error handling
match rsproperties::get::<String>("ro.build.version.release") {
    Ok(version) => println!("Android Version: {}", version),
    Err(e) => eprintln!("Failed to get version: {}", e),
}

// Set property (requires property service to be running)
if let Err(e) = rsproperties::set("debug.my_app.enabled", "true") {
    eprintln!("Failed to set property: {}", e);
}
```

### Panic-free Initialization

`init()` and `system_properties()` panic when initialization fails (e.g.
missing properties directory, corrupt mmap). The `try_*` variants surface
those errors as `Result` instead — preferable for embedded use, tests,
and any code that must not unwind through `OnceLock` initialization.

```rust
use rsproperties::{try_init, try_system_properties, PropertyConfig};

fn boot() -> rsproperties::Result<()> {
    try_init(PropertyConfig::with_properties_dir("/dev/__properties__"))?;
    let props = try_system_properties()?;             // &'static SystemProperties
    let sdk: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
    Ok(())
}
```

`try_init` is first-write-wins: subsequent calls return
`Error::FileValidation` without poisoning the global state.
`try_system_properties` caches both the success and the failure in a
`OnceLock`, so every later call observes the same outcome.

### Zero-Allocation Reads (`read_with`)

`get<T>` and `get_or<T>` already route through a zero-allocation path,
but if you want raw access to the validated value without parsing,
`SystemProperties::read_with` hands you a `&str` borrowed from the
seqlock-protected mmap buffer:

```rust
let props = rsproperties::system_properties();

// Compute over the value without ever allocating a String.
let prefix_len: rsproperties::Result<usize> =
    props.read_with("ro.build.version.release", |v| v.split('.').next().map(str::len).unwrap_or(0));

// Borrow into a stack buffer; the callback runs while the bytes are
// still seqlock-validated, so keep it cheap and non-blocking.
let mut buf = String::new();
props.read_with("ro.product.model", |v| buf.push_str(v))?;
```

This mirrors bionic's `__system_property_read_callback` pattern.

### Property Monitoring

```rust
use rsproperties;

let system_properties = rsproperties::system_properties();

// Wait for any property change
std::thread::spawn(|| {
    if let Some(new_serial) = system_properties.wait_any() {
        println!("Properties changed, new serial: {}", new_serial);
    }
});

// Wait for specific property change
std::thread::spawn(|| {
    if let Ok(Some(prop_index)) = system_properties.find("sys.boot_completed") {
        println!("Waiting for boot completion...");
        if let Some(_) = system_properties.wait(Some(&prop_index), None) {
            println!("System boot completed!");
        }
    }
});

// Monitor multiple properties
let monitored_props = vec![
    "sys.boot_completed",
    "dev.bootcomplete",
    "service.bootanim.exit"
];

for prop_name in monitored_props {
    match system_properties.get_with_result(prop_name) {
        Ok(value) => println!("{}: {}", prop_name, value),
        Err(_) => println!("{}: <not set>", prop_name),
    }
}
```

### Setting Properties

Setting properties requires a running property service (like `rsproperties-service`):

```rust
use rsproperties;

// Basic property setting
if let Err(e) = rsproperties::set("debug.my_app.enabled", "true") {
    eprintln!("Failed to set property: {}", e);
}

// Set application configuration
rsproperties::set("debug.my_app.log_level", "verbose")?;
rsproperties::set("debug.my_app.port", "8080")?;

// Set system properties (may require elevated permissions)
rsproperties::set("sys.my_service.ready", "1")?;

// Set persistent properties (survive reboots on Android)
rsproperties::set("persist.my_app.config", "production")?;
```

#### Property Setting Requirements

**On Android:**
- Properties are set through the property service
- Some properties require specific SELinux permissions
- `ro.*` properties are read-only and cannot be modified
- System properties may require root or system privileges

**On Linux:**
- Requires `rsproperties-service` to be running
- Properties are stored in memory-mapped files
- All properties are writable unless explicitly restricted

#### Property Setting Examples by Type

```rust
// Debug properties - usually writable by applications
rsproperties::set("debug.my_app.trace", "enabled")?;
rsproperties::set("debug.my_app.verbose", "true")?;

// Vendor properties - device-specific configuration
rsproperties::set("vendor.my_app.hw_config", "v2")?;

// Custom application properties
rsproperties::set("my.company.app.version", "1.2.3")?;
rsproperties::set("my.company.app.api_key", "abc123")?;

// System state properties
rsproperties::set("sys.my_service.status", "running")?;
rsproperties::set("sys.my_service.pid", "1234")?;
```

### Error Handling

`Error` is a `thiserror`-derived enum with `#[from]` conversions for
`std::io::Error`, `rustix::io::Errno`, `Utf8Error`, and `ParseIntError`.
The `Error::Context` variant carries a `panic::Location` so the failing
call site is preserved across error boundaries, and `ContextWithLocation`
attaches that context to any `Result` whose `Err` implements
`Into<Error>`.

```rust
use rsproperties::{ContextWithLocation, Error, Result};

fn read_sdk() -> Result<i32> {
    rsproperties::get::<i32>("ro.build.version.sdk")
        .context_with_location("reading ro.build.version.sdk")
}

// Batch property operations with error handling
fn set_app_config() -> Result<()> {
    for (key, value) in [
        ("debug.my_app.enabled", "true"),
        ("debug.my_app.log_level", "info"),
        ("debug.my_app.trace", "disabled"),
    ] {
        match rsproperties::set(key, value) {
            Ok(_)                            => println!("set {key}: {value}"),
            Err(Error::PermissionDenied(m))  => return Err(Error::PermissionDenied(m)),
            Err(Error::Io(e))                => return Err(Error::Io(e)),
            Err(e)                           => return Err(e),
        }
    }
    Ok(())
}
```

### Custom Configuration

> **Warning**: Do not use custom configuration on Android devices. Custom configuration is only intended for Linux environments or development/testing purposes.

```rust
use rsproperties::PropertyConfig;

// Configure custom directories
let config = PropertyConfig {
    properties_dir: Some("/custom/properties".into()),
    socket_dir: Some("/custom/socket".into()),
};
rsproperties::init(config);

// Using the builder pattern
let config = PropertyConfig::builder()
    .properties_dir("/my/properties")
    .socket_dir("/my/socket")
    .build();
rsproperties::init(config);

// Convenience methods
rsproperties::init(PropertyConfig::with_properties_dir("/my/props"));
```

## Platform Support

### Android
- **Native Integration**: Direct access to `/dev/__properties__`
- **Property Contexts**: Full SELinux property context support
- **Bionic Compatibility**: Compatible with Android's property implementation
- **Standard Properties**: Access to all standard Android properties

### Linux
- **Full Emulation**: Complete Android property system emulation
- **Socket Communication**: Unix domain socket property setting
- **Memory Mapping**: Efficient memory-mapped property storage
- **Property Service**: Use with `rsproperties-service` for full daemon functionality

## API Reference

### Configuration

- `PropertyConfig` — public-fields struct describing the properties &
  socket directories
- `PropertyConfig::builder()` / `with_properties_dir()` /
  `with_socket_dir()` / `with_both_dirs()` — construction helpers
- `init(config)` — initialize globals; logs and swallows failures
- `try_init(config)` — initialize globals, returning `Result`
- `properties_dir()` — currently-configured properties directory
- `socket_dir()` — currently-configured socket directory.
  Priority: `PropertyConfig.socket_dir` (via `init`/`try_init`) >
  `PROPERTY_SERVICE_SOCKET_DIR` env var > `/dev/socket`

### Property Operations

- `get<T>(name)` — parse-typed read; `Err` on missing / parse failure
- `get_or<T>(name, default)` — infallible read with fallback
- `set<T>(name, value)` — `Display`-format and send to the property
  service over the socket
- `system_properties()` — `&'static SystemProperties` or **panic**
- `try_system_properties()` — `&'static SystemProperties` or `Err`

### `SystemProperties` (read side)

- `read_with(name, |&str| -> R)` — zero-alloc callback reader
- `get_with_result(name)` — `String`-allocating convenience wrapper
- `find(name)` — `Result<Option<PropertyIndex>>`; `Ok(None)` for a
  missing property, `Err` only for I/O / mmap problems
- `serial(index)` / `context_serial()` — current generation counters
- `wait_any()` — futex-wait for any property change
- `wait(index, timeout)` — futex-wait for a specific property

### Wire-protocol constants & validators (`rsproperties::wire`)

- `PROP_VALUE_MAX`, `PROP_NAME_MAX` — AOSP wire-format size caps
- `PROP_MSG_SETPROP`, `PROP_MSG_SETPROP2` — command IDs
- `PROP_SUCCESS`, `PROP_ERROR` — V2 response codes
- `validate_property_name(name)` — name charset / leading-char check
- `validate_value_len(name, value)` — value-length policy with the
  long-`ro.*` exception

Two socket-name constants are re-exported at the crate root for
clients that want to point at a custom socket directory:

- `PROPERTY_SERVICE_SOCKET_NAME` — `"property_service"`
- `PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME` —
  `"property_service_for_system"`

### Errors

- `Error` — non-exhaustive enum (`Io`, `Errno`, `NotFound`, `Encoding`,
  `Parse`, `FileValidation`, `Conversion`, `PermissionDenied`,
  `FileSize`, `FileOwnership`, `LockError`, `Context`)
- `Result<T>` = `Result<T, Error>`
- `ContextWithLocation::context_with_location(msg)` — attach
  `panic::Location` to any `Result<_, impl Into<Error>>`

### `SystemProperties` write side (`builder` feature)

- `SystemProperties::new_area(dir)` — create/serialize an on-disk
  property area
- `SystemProperties::add(name, value)` — append a new entry
- `SystemProperties::update(index, value)` — overwrite an existing
  entry (seqlock-guarded)
- `SystemProperties::set(name, value)` — add-or-update
- `load_properties_from_file(path, filter, context, &mut HashMap)` —
  parse build.prop entries
- `PropertyInfoEntry::parse_from_file(path, require_prefix_or_exact)` —
  parse a `property_contexts`-format file into
  `(Vec<PropertyInfoEntry>, Vec<Error>)` (the entries that survived
  and the per-line errors)
- `build_trie(entries, default_context, default_type)` — serialize a
  property-info trie

## Thread Safety

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

### Building Property Databases (with `builder` feature)

```rust
#[cfg(feature = "builder")]
use rsproperties::{load_properties_from_file, SystemProperties};
use std::collections::HashMap;
use std::path::Path;

// Load properties from Android build.prop files
let mut properties = HashMap::new();
load_properties_from_file(
    Path::new("system_build.prop"),
    None,
    "u:r:init:s0",
    &mut properties
)?;

// Create a system properties area for testing or service
let mut system_properties = SystemProperties::new_area(Path::new("./test_props"))?;

// Add loaded properties to the area
for (key, value) in properties {
    system_properties.add(&key, &value)?;
}

// Now properties can be read normally
let sdk_version: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
```

## Constants

The library exposes Android-compatible constants. `PROP_VALUE_MAX` is
re-exported at the crate root for source-compatibility; the canonical
home (along with the wire-protocol opcodes and validators) is the
`wire` module:

```rust
use rsproperties::{PROP_DIRNAME, PROP_VALUE_MAX};
use rsproperties::wire::{
    PROP_NAME_MAX, PROP_MSG_SETPROP2, validate_property_name, validate_value_len,
};

assert_eq!(PROP_VALUE_MAX, 92);              // buffer size including NUL; content cap = 91 bytes
assert_eq!(PROP_NAME_MAX, 32);               // V1 wire only; V2 is length-prefixed
assert_eq!(PROP_DIRNAME, "/dev/__properties__");
assert_eq!(PROP_MSG_SETPROP2, 0x00020001);

validate_property_name("ro.build.version.sdk").unwrap();
validate_value_len("ro.long.prop", &"x".repeat(200)).unwrap(); // ro.* may exceed PROP_VALUE_MAX
```

## Performance

- **Memory-mapped access**: Direct memory access for optimal performance
- **Zero-copy reads**: Efficient property value retrieval
- **Atomic operations**: Thread-safe property updates
- **Futex-based waiting**: Efficient property change notifications

## Examples

The crate includes Android-compatible command-line tools:

- **`getprop.rs`** - Android-compatible property getter with support for custom directories
- **`setprop.rs`** - Android-compatible property setter with validation and error handling

Run examples with:
```bash
# Get a property with default value
cargo run --example getprop ro.build.version.sdk 0

# Set a property
cargo run --example setprop debug.my_app.test true
```

## Related Crates

- **`rsproperties-service`** - Full async property service daemon for Linux environments

## Building

```bash
# Build the library
cargo build

# Build with all features
cargo build --all-features

# Run tests
cargo test

# Build documentation
cargo doc --open
```

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Contributing

Contributions are welcome! Please ensure:

1. All tests pass: `cargo test`
2. Code is formatted: `cargo fmt`
3. No clippy warnings: `cargo clippy --all-targets --all-features`

This implementation is based on Android's property system and maintains compatibility with Android's property semantics and behavior.
