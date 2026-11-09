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
- **Pure Rust Solution**: No dependencies on Android's C libraries or JNI - everything implemented in Rust (`unsafe` is limited to the mmap / shared-memory layer)
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
// ro.debuggable is "0"/"1"; other boolean properties (e.g. ro.apex.updatable)
// use "true"/"false" — read those with get_or(.., false).
let is_debuggable: bool = rsproperties::get_or("ro.debuggable", 0) != 0;

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

`system_properties()` panics when initialization fails (e.g. missing
properties directory, corrupt mmap); `init()` does not panic but only logs
its failure with `log::warn!` and returns. The `try_*` variants surface
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

`try_init` fails with `Error::AlreadyInitialized` when a directory it
names is already set — by an earlier `init`/`try_init`, or by the
default being latched: a property read latches the properties dir, a
`set()` (or `socket_dir()`) call latches the socket dir — without
poisoning the global state. Options absent from the config are left untouched.
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
        let old_serial = system_properties.serial(&prop_index);
        if let Some(_) = system_properties.wait(Some(&prop_index), old_serial, None) {
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
- `ro.*` properties are write-once: an unset one can be set, but setting one that already has a value is rejected
- System properties may require root or system privileges

**On Linux:**
- Requires `rsproperties-service` to be running
- Properties are stored in memory-mapped files
- `ro.*` properties are write-once here too (enforced by `rsproperties-service`)

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
`std::io::Error`, `rustix::io::Errno`, and `Utf8Error`.
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
            Err(e @ Error::ServiceError { .. }) => return Err(e), // rejected by the service
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

// Alternatives — call `init` once. A later `init` naming a directory that
// is already set is ignored as a whole (`init` only logs the rejection).

// Configure custom directories
let config = PropertyConfig::with_both_dirs("/custom/properties", "/custom/socket");
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

- **Linux and Android**: the client API (`get`, `set`, `wait`,
  `read_with`) is supported on both.
- **`builder` feature** (creating property areas): for Linux hosts. On
  Android, init creates the property areas.
- **macOS**: not supported. The crate still compiles for it, but CI
  neither builds nor tests it.
- **`rsproperties-service`** compiles only for Linux.

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
- `get_bool_or(name, default)` / `get_int_or::<T>(name, default)` — read
  with AOSP libbase's `GetBoolProperty` / `GetIntProperty` rules
  (`parse::parse_bool` / `parse::parse_int`: `yes`/`on`/`1`…, leading
  whitespace, `0x` hex); `get<T>` uses Rust's `FromStr` instead
- `set<T>(name, value)` — `Display`-format and send to the property
  service over the socket; V2 names and `ro.` values may be up to 65535
  bytes on Android (AOSP init's limit) and up to `wire::MAX_WIRE_NAME_LEN`
  / `MAX_WIRE_VALUE_LEN` elsewhere (non-`ro.` values: at most 91 bytes).
  On Linux it waits at most 2 s for the reply, and a `TimedOut` from that
  wait does not mean the property was not set; on Android it waits for
  init's reply without a timeout, like bionic, while connecting and
  sending keep the 2 s bound (bionic has none)
- `system_properties()` — `&'static SystemProperties` or **panic**
- `try_system_properties()` — `&'static SystemProperties` or `Err`

### `SystemProperties` (read side)

- `read_with(name, |&str| -> R)` — zero-alloc callback reader
- `get_with_result(name)` — `String`-allocating convenience wrapper
- `find(name)` — `Result<Option<PropertyIndex>>`; `Ok(None)` for a
  missing property or a malformed name (empty segment), `Err` for I/O /
  mmap / corruption problems. The index is valid only on the instance
  that returned it
- `read_index_with(index, |name, value, serial| -> R)` — read by index,
  no name lookup; name, value and serial from one seqlock-validated read
  (bionic `__system_property_read_callback`)
- `foreach(|index| ...)` — every readable property's index (bionic
  `__system_property_foreach`); areas the process may not open are skipped
- `serial(index)` / `context_serial()` — current generation counters
- `wait_any()` — futex-wait for any property change
- `wait(index, old_serial, timeout)` — futex-wait for a specific property
- `wait_for_creation(name, timeout)` / `wait_for_value(name, expected,
  timeout)` — libbase `WaitForPropertyCreation` / `WaitForProperty`;
  a timeout is `Ok(None)` / `Ok(false)`, a failure is `Err`

On Android API 37+, a process started with `BIONIC_APPCOMPAT_OVERRIDE`
in its environment (zygote sets it) reads `ro.appcompat_override.<name>`
first, like bionic; see `SystemProperties::read_with`.

### Wire-protocol constants & validators (`rsproperties::wire`)

- `PROP_VALUE_MAX`, `PROP_NAME_MAX` — AOSP wire-format size caps
- `MAX_WIRE_NAME_LEN`, `MAX_WIRE_VALUE_LEN` — this crate's service
  limits; `AOSP_MAX_WIRE_STRING_LEN` — AOSP init's limit, used by the
  Android client
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
  `Utf8`, `Parse`, `FileValidation`, `InvalidArgument`,
  `AlreadyInitialized`, `LimitExceeded`, `ServiceError`,
  `PermissionDenied`, `FileSize`, `AreaFull`, `FileOwnership`, `Lock`,
  `Init`, `Context`)
- `Result<T>` = `Result<T, Error>`
- `ContextWithLocation::context_with_location(msg)` — attach
  `panic::Location` to any `Result<_, impl Into<Error>>`

### `SystemProperties` write side (`builder` feature)

- `AreaWriterLock::acquire(dir)` / `lock.publish_property_info(trie)` /
  `SystemProperties::new_area_locked(lock)` — take the directory's
  writer lock, publish `property_info` (0444, temp + rename) and create
  the areas under it; prefer this over writing `property_info` yourself.
  `lock.dir()` returns the directory the lock guards
- `SystemProperties::new_area(dir)` — create the areas for an existing
  `<dir>/property_info`; it takes the writer lock before it reads that
  file, and without the file it fails before `dir` or `.writer_lock` is
  created
- `SystemProperties::add(name, value)` — append a new entry
- `SystemProperties::update(index, value)` — overwrite an existing
  entry (seqlock-guarded)
- `SystemProperties::set(name, value)` — add-or-update
- `load_properties_from_file(path, filter, context, &mut HashMap)` —
  parse build.prop entries; like AOSP init it opens each file with
  `O_NOFOLLOW` and skips (with a warning) a file writable by group or
  others
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
use rsproperties::{
    build_trie, load_properties_from_file, AreaWriterLock, PropertyConfig, PropertyInfoEntry,
    SystemProperties,
};
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

// Take the directory's writer lock first, then publish the context trie as
// `<dir>/property_info` and create the areas under that lock. Do not write
// `property_info` with `std::fs::write`: readers map it, so it must only be
// replaced by rename (see below). Release builds (no `debug-assertions`)
// and the `strict-file-validation` feature require `property_info` to be
// owned by root (uid 0, gid 0).
let (entries, _errors) =
    PropertyInfoEntry::parse_from_file(Path::new("property_contexts"), false)?;
let trie_data = build_trie(&entries, "u:object_r:build_prop:s0", "string")?;
let lock = AreaWriterLock::acquire(Path::new("./test_props"))?;
lock.publish_property_info(&trie_data)?;

// Create a system properties area for testing or service
let mut system_properties = SystemProperties::new_area_locked(lock)?;

// Add loaded properties to the area
for (key, value) in properties {
    system_properties.add(&key, &value)?;
}

// Point the global reader at the same directory (before any read),
// then properties can be read normally
rsproperties::init(PropertyConfig::with_properties_dir("./test_props"));
let sdk_version: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
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

`SystemProperties::new_area` also replaces each area file by rename (temp
name `.area.tmp-<pid>-<n>`), and `properties_serial` last. A reader that
opened an area before that area's rename, or `properties_serial` before
its rename, keeps the previous file and does not see the restarted
writer's updates in it; a reader that starts after the new `property_info` is published
but before `properties_serial` is renamed is bound to the previous
`properties_serial` for its lifetime, so its global-serial `wait` does not
wake, and its read of a context whose area file does not exist yet fails
until that file is renamed into place. Start readers after `new_area` /
`new_area_locked` (or the service's `run`) has returned.

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

- **`rsproperties-service`** - Linux emulation of the Android property service (async daemon; Linux only, not usable on Android)

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
