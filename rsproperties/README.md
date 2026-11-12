# rsproperties

[![Crates.io](https://img.shields.io/crates/v/rsproperties.svg)](https://crates.io/crates/rsproperties)
[![Documentation](https://docs.rs/rsproperties/badge.svg)](https://docs.rs/rsproperties)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

A pure Rust implementation of Android's property system, providing cross-platform access to Android system properties on both Linux and Android platforms.

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
// `bool` and the integers are read by libbase's rules: "1"/"0",
// "true"/"false", "y"/"n", "yes"/"no", "on"/"off" for a `bool`, and
// leading whitespace or a `0x` prefix for an integer.
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

### Initialization

`init` sets the directories before the first property access, and
`system_properties` returns the global reader, or `Error::Init` when the
store cannot be opened.

```rust
use rsproperties::{init, system_properties, PropertyConfig};

fn boot() -> rsproperties::Result<()> {
    init(PropertyConfig::new().properties_dir("/dev/__properties__"))?;
    let props = system_properties()?;                 // &'static SystemProperties
    let sdk: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
    Ok(())
}
```

Each directory is set once per process: by `init`, or by its first use,
which latches the default (a property read latches the properties dir, a
`set()` or `socket_dir()` call the socket dir). `init` succeeds when each
directory it names is unset or already holds that same path, and fails
with `Error::AlreadyInitialized` when one holds a different path — then
it changes neither. Directories absent from the config are left
untouched. `system_properties` keeps the first reader it opens; a failure
is not kept, so a store created after the process started (a service
started later) is picked up by the next call.

### Zero-Allocation Reads (`read_with`)

`get<T>` and `get_or<T>` already route through a zero-allocation path,
but if you want raw access to the validated value without parsing,
`SystemProperties::read_with` hands you a `&str` borrowed from the
seqlock-protected mmap buffer:

```rust
let props = rsproperties::system_properties()?;

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

Wait once for a value (libbase `WaitForProperty`):

```rust,no_run
use std::time::Duration;

fn main() -> rsproperties::Result<()> {
    let props = rsproperties::system_properties()?;
    // Returns when the property exists and equals "1"; `false` is a timeout.
    if !props.wait_for_value("sys.boot_completed", "1", Some(Duration::from_secs(30)))? {
        eprintln!("not booted after 30 s");
    }
    Ok(())
}
```

Follow one property through every change with a `PropertyWatcher`
(libbase `CachedProperty`). It keeps the property's index, the serial of
the value it last read and that value, so the loop has no index or serial
to carry:

```rust,no_run
fn main() -> rsproperties::Result<()> {
    let props = rsproperties::system_properties()?;
    let mut level = props.watch("debug.my_service.log_level");
    loop {
        // `None` until the property is created. The store is read again
        // only after a change.
        println!("log level: {:?}", level.value()?);
        // Blocks until the property is created or updated. A change made
        // since the `value()` above returns at once, so none is missed.
        level.wait_for_change(None)?;
    }
}
```

One update is reported as one change, also when it stores the value the
property already had; updates that land between two calls are reported as
one change with the latest value.

Wait for a change to any property on the global serial:

```rust,no_run
use std::time::Duration;

fn main() -> rsproperties::Result<()> {
    let props = rsproperties::system_properties()?;
    // `old_serial` is the serial the caller last saw: a change made
    // between `area_serial()` and `wait` returns at once.
    let mut serial = props.area_serial();
    loop {
        match props.wait(None, serial, Some(Duration::from_secs(60)))? {
            Some(new_serial) => {
                println!("a property changed, serial {new_serial}");
                serial = new_serial;
            }
            None => println!("no change for 60 s"),
        }
    }
}
```

The watcher is built on `find`, `read_index_with` and
`wait(Some(index), old_serial, timeout)`; they stay public for a caller
that keeps its own index and serial.

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

`Error` is a `thiserror`-derived, non-exhaustive enum with `From`
conversions for `std::io::Error` (every OS error is `Error::Io`) and
`Utf8Error`.
Some paths wrap the failure: `Error::Context` adds a message and the
`panic::Location` of the failing call, and `Error::Init` marks a failed
creation of the global reader. `Error::root_cause()` returns the error
under those layers, so one `match` covers both the wrapped and the
unwrapped form.

```rust
use rsproperties::{Error, Result};

// Batch property operations with error handling
fn set_app_config() -> Result<()> {
    for (key, value) in [
        ("debug.my_app.enabled", "true"),
        ("debug.my_app.log_level", "info"),
        ("debug.my_app.trace", "disabled"),
    ] {
        if let Err(e) = rsproperties::set(key, value) {
            match e.root_cause() {
                // Rejected by the service: `reason` is a `SetRejection`,
                // e.g. `SetRejection::ReadOnlyProperty`.
                Error::SetRejected { reason, .. } => eprintln!("{key} rejected: {reason}"),
                // No service socket, a closed connection, a timeout.
                Error::Io(io) => eprintln!("{key}: I/O failure: {io}"),
                _ => eprintln!("{key}: {e}"),
            }
            return Err(e);
        }
        println!("set {key}: {value}");
    }
    Ok(())
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

- `PropertyConfig::new().properties_dir(dir).socket_dir(dir)` — the
  directories to set; either may be left out
- `init(config)` — set them once per process; `Ok` again for the same
  paths, `Err(AlreadyInitialized)` for a different one
- `properties_dir()` — currently-configured properties directory
- `socket_dir()` — currently-configured socket directory.
  Priority: `PropertyConfig::socket_dir` (via `init`) >
  `PROPERTY_SERVICE_SOCKET_DIR` env var > `/dev/socket`

### Property Operations

- `get<T>(name)` — typed read; `Err` on missing / a value that is not a `T`
- `get_or<T>(name, default)` / `get_or_else<T>(name, || default)` —
  infallible read with fallback (missing, empty, not a `T`, failed read)
- `FromProperty` — the trait behind `T`. `String` is the value unchanged;
  `bool` and `i8`–`i64`, `isize`, `u8`–`u64`, `usize` follow AOSP
  libbase's `GetBoolProperty` / `GetIntProperty` rules
  (`parse::parse_bool` / `parse::parse_int`: `yes`/`on`/`1`…, leading
  whitespace, `0x` hex). Implement it for a type of your own; read a
  float or another `FromStr` type with
  `system_properties()?.read_with(name, |v| v.parse::<f64>())`
- `set(name, value)` — `value: impl Display` (`set("a.b", 42)`,
  `set("a.b", "text")`, `set("a.b", &owned)`); the formatted value is
  sent to the property service over the socket; V2 names and `ro.` values may be up to 65535
  bytes on Android (AOSP init's limit) and up to `wire::MAX_WIRE_NAME_LEN`
  / `MAX_WIRE_VALUE_LEN` elsewhere (non-`ro.` values: at most 91 bytes).
  On Linux it waits at most 2 s for the reply, and a `TimedOut` from that
  wait does not mean the property was not set; on Android it waits for
  init's reply without a timeout, like bionic, while connecting and
  sending keep the 2 s bound (bionic has none)
- `system_properties()` — `&'static SystemProperties` or `Err`
  (`Error::Init`); a failure is retried by the next call

### `SystemProperties` (read side)

- `SystemProperties::open(dir)` — a read-only instance on `dir`,
  independent of `init` and of the global reader: for a second properties
  directory in one process, or a library that must not set the
  process-wide one
- `read_with(name, |&str| -> R)` — zero-alloc callback reader
- `get(name)` — the value as a `String`; `Err(NotFound)` when missing
- `find(name)` — `Result<Option<PropertyIndex>>`; `Ok(None)` for a
  missing property or a malformed name (empty segment), `Err` for I/O /
  mmap / corruption problems. The index is valid only on the instance
  that returned it
- `read_index_with(index, |name, value, serial| -> R)` — read by index,
  no name lookup; name, value and serial from one seqlock-validated read
  (bionic `__system_property_read_callback`)
- `foreach(|index| ...)` — every readable property's index (bionic
  `__system_property_foreach`); areas the process may not open are skipped
- `list(|name, value| ...)` — every readable property's name and value
  (libcutils `property_list`), in `foreach` order; bytes that are not
  UTF-8 arrive as U+FFFD
- `serial(index)` / `area_serial()` — a property's serial / the global
  serial that any change advances (bionic `__system_property_area_serial`)
- `wait(index, old_serial, timeout)` — futex-wait until the serial
  differs from `old_serial` (bionic `__system_property_wait`).
  `index: Some(i)` waits on that property, with `old_serial` from
  `serial(i)`; `index: None` waits on any property, with `old_serial`
  from `area_serial()`. `timeout: Option<Duration>`; `Ok(Some(serial))`
  on a change, `Ok(None)` on timeout, `Err` on failure
- `wait_for_creation(name, timeout)` / `wait_for_value(name, expected,
  timeout)` — libbase `WaitForPropertyCreation` / `WaitForProperty`;
  a timeout is `Ok(None)` / `Ok(false)`, a failure is `Err`
- `watch(name)` — a `PropertyWatcher` (libbase `CachedProperty`):
  `value()` is `Ok(None)` until the property exists and reads the store
  again only after a change; `wait_for_change(timeout)` blocks until the
  creation or the next update, `Ok(false)` on timeout. One update is one
  change (the dirty and the clean serial store are not reported
  separately), and changes are not queued
- `PropertyIndex` — the handle `find` / `foreach` return; `Copy`, `Eq`
  and `Hash`. Two indexes are equal when the same instance returned them
  for the same entry, so an index can be a `HashMap` key

On Android API 37+, a process started with `BIONIC_APPCOMPAT_OVERRIDE`
in its environment (zygote sets it) reads `ro.appcompat_override.<name>`
first, like bionic; see `SystemProperties::read_with`. This follows the
AOSP `android-17.0.0_r1` sources and returned the same values as
`getprop` on an API 37 emulator.
On API 35–36 the override is a bind mount over `/dev/__properties__`,
which a reader created in that process follows without extra code.

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

- `Error` — non-exhaustive enum (`Io`, `NotFound`, `Utf8`, `Parse`,
  `FileValidation`, `InvalidArgument`, `AlreadyInitialized`,
  `LimitExceeded`, `SetRejected`, `ReadOnlyProperty`, `PermissionDenied`,
  `FileSize`, `AreaFull`, `FileOwnership`, `Lock`, `Init`, `Context`)
- `SetRejection` — why the service rejected a set, in
  `Error::SetRejected { name, reason }`: a non-exhaustive enum with one
  variant per `wire::PROP_ERROR_*` code (`ReadOnlyProperty`,
  `InvalidName`, `InvalidValue`, `PermissionDenied`, …) and
  `Unknown(i32)`; `code()` returns the number
- `Error::root_cause()` — the error under the `Context` / `Init` layers
- `Result<T>` = `Result<T, Error>`

### `SystemProperties` write side (`builder` feature)

- `AreaWriterLock::acquire(dir)` / `lock.publish_property_info(trie)` /
  `SystemProperties::new_area_locked(lock)` — take the directory's
  writer lock, publish `property_info` (0444, temp + rename) and create
  the areas under it; prefer this over writing `property_info` yourself.
  `lock.dir()` returns the directory the lock guards. Dropping the lock,
  or the `SystemProperties` that holds it, releases it with
  `flock(LOCK_UN)`, so the next `acquire` succeeds even while a child
  process that has not yet `exec`ed holds a copy of the fd
- `SystemProperties::new_area(dir)` — create the areas for an existing
  `<dir>/property_info`; it takes the writer lock before it reads that
  file, and without the file it fails before `dir` or `.writer_lock` is
  created
- `SystemProperties::add(name, value)` — append a new entry
- `SystemProperties::update(index, value)` — overwrite an existing
  entry (seqlock-guarded)
- `SystemProperties::set(name, value)` — add-or-update
- `load_properties_from_file(path, filter, &mut HashMap)` —
  parse build.prop entries; like AOSP init it opens each file with
  `O_NOFOLLOW` and skips (with a warning) a file writable by group or
  others. Returns `PropFileOutcome`: `Loaded`, or
  `SkippedWritable { mode }` when `path` itself was skipped (a skipped
  `import` is only logged)
- `PropertyInfoEntry::parse_from_file(path, MatchKeyword)` —
  parse a `property_contexts`-format file into
  `(Vec<PropertyInfoEntry>, Vec<Error>)` (the entries that survived
  and the per-line errors). A line without a third token is a prefix
  match. `MatchKeyword::Checked` rejects a third token other than
  `prefix` / `exact` (AOSP `require_prefix_or_exact = true`);
  `MatchKeyword::Unchecked` takes such a line as a prefix match
- `PropertyInfoEntry::exact(name, context, type_str)` /
  `PropertyInfoEntry::prefix(name, context, type_str)` — build an entry
  without a file
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
    build_trie, load_properties_from_file, AreaWriterLock, MatchKeyword, PropFileOutcome,
    PropertyConfig, PropertyInfoEntry, SystemProperties,
};
use std::collections::HashMap;
use std::path::Path;

// Load properties from Android build.prop files. A file writable by group
// or others (0664 from a umask 002 checkout) is not read: the call then
// returns `PropFileOutcome::SkippedWritable { mode }`.
let mut properties = HashMap::new();
let outcome = load_properties_from_file(
    Path::new("system_build.prop"),
    None,
    &mut properties
)?;
assert_eq!(outcome, PropFileOutcome::Loaded);

// Take the directory's writer lock first, then publish the context trie as
// `<dir>/property_info` and create the areas under that lock. Do not write
// `property_info` with `std::fs::write`: readers map it, so it must only be
// replaced by rename (see below). Release builds (no `debug-assertions`)
// and the `strict-file-validation` feature require `property_info` to be
// owned by root (uid 0, gid 0).
let (entries, _errors) = PropertyInfoEntry::parse_from_file(
    Path::new("property_contexts"),
    MatchKeyword::Unchecked,
)?;
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
rsproperties::init(PropertyConfig::new().properties_dir("./test_props"))?;
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

The library exposes Android-compatible constants. `PROP_DIRNAME` is at
the crate root; the size limits, wire-protocol opcodes and validators
are in the `wire` module:

```rust
use rsproperties::PROP_DIRNAME;
use rsproperties::wire::{
    PROP_NAME_MAX, PROP_VALUE_MAX, PROP_MSG_SETPROP2, validate_property_name,
    validate_value_len,
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
