# rsproperties

[![Crates.io](https://img.shields.io/crates/v/rsproperties.svg)](https://crates.io/crates/rsproperties)
[![Documentation](https://docs.rs/rsproperties/badge.svg)](https://docs.rs/rsproperties)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](https://github.com/hiking90/rsproperties/blob/main/LICENSE)

A pure Rust implementation of Android's property system: access to Android system properties on Linux and Android.

## Supported Android Versions

This library supports Android versions from Android 9 (API level 28) to Android 17 (API level 37).
The Android 17 appcompat-override lookup is implemented from the AOSP `android-17.0.0_r1` sources
and was compared with `getprop` on an API 37 emulator.

## Features

- **Complete Android Properties Implementation**: Full Rust implementation of Android's property system - read, write, and monitor properties with bionic's semantics; where an API differs from bionic, its entry below says how
- **Linux and Android**: The same client API on Android devices and Linux hosts
- **Pure Rust Solution**: No dependencies on Android's C libraries or JNI - everything implemented in Rust (`unsafe` is limited to the mmap / shared-memory layer, the `repr(transparent)` reference cast behind `as_async`, and the async waiter's load of a registered serial word)
- **Real-time Property Monitoring**: Watch for property changes in real-time, enabling reactive applications
- **High Performance**: Reads map the property areas directly and allocate nothing on the heap (`read_with`): a short value is copied to a stack buffer, a long `ro.` value is read in place from the mmap
- **Drop-in Replacement**: Compatible with Android's property naming conventions and value constraints
- **Linux Emulation**: With the `builder` feature and `rsproperties-service`, the Android property system on a Linux host for development and testing
- **Thread-Safe Design**: Safe to use across multiple threads without external synchronization

## Quick Start

`rsproperties` requires Rust 1.85 or newer. Upgrading from 0.6: see
[Migration from 0.6.0](https://github.com/hiking90/rsproperties/blob/main/CHANGELOG.md#migration-from-060)
in the changelog.

Add `rsproperties` to your `Cargo.toml`:

```toml
[dependencies]
rsproperties = "0.7"
```

Creating property areas (`SystemProperties::new_area`, `build_trie`,
`load_properties_from_file`; Linux hosts) needs the `builder` feature:

```toml
[dependencies]
rsproperties = { version = "0.7", features = ["builder"] }
```

### Basic Usage

```rust,no_run
fn main() {
    // Get property with default value (no initialization needed for default configuration)
    let sdk_version: String = rsproperties::get_or("ro.build.version.sdk", "0".to_string());
    println!("SDK Version: {sdk_version}");

    // Get property with type parsing and default fallback
    let sdk_version: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
    // `bool` and the integers are read by libbase's rules: "1"/"0",
    // "true"/"false", "y"/"n", "yes"/"no", "on"/"off" for a `bool`, and
    // leading whitespace or a `0x` prefix for an integer.
    let is_debuggable: bool = rsproperties::get_or("ro.debuggable", false);
    println!("SDK {sdk_version}, debuggable: {is_debuggable}");

    // Get property with error handling
    match rsproperties::get::<String>("ro.build.version.release") {
        Ok(version) => println!("Android Version: {version}"),
        Err(e) => eprintln!("Failed to get version: {e}"),
    }

    // Set property (requires property service to be running)
    if let Err(e) = rsproperties::set("debug.my_app.enabled", "true") {
        eprintln!("Failed to set property: {e}");
    }
}
```

### Initialization

`init` sets the directories before the first property access, and
`system_properties` returns the global reader, or `Error::Init` when the
store cannot be opened.

```rust,no_run
use rsproperties::{init, system_properties, PropertyConfig};

fn main() -> rsproperties::Result<()> {
    init(PropertyConfig::new().properties_dir("/dev/__properties__"))?;
    let props = system_properties()?; // &'static SystemProperties
    let sdk: String = props.get("ro.build.version.sdk")?;
    // The free functions read through the same global reader.
    let sdk_or_zero: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
    println!("SDK {sdk} ({sdk_or_zero})");
    Ok(())
}
```

Each directory is set once per process: by `init`, or by its first use,
which latches the default (a property read or a `properties_dir()` call
latches the properties dir, a `set()` or `socket_dir()` call the socket
dir). `init` succeeds when each
directory it names is unset or already holds that same path, and fails
with `Error::AlreadyInitialized` when one holds a different path — then
it changes neither. Directories absent from the config are left
untouched. `system_properties` keeps the first reader it opens; a failure
is not kept, so a store created after the process started (a service
started later) is picked up by the next call.

### Zero-Allocation Reads (`read_with`)

`get<T>` and `get_or<T>` already route through a zero-allocation path,
but if you want raw access to the validated value without parsing,
`SystemProperties::read_with` hands you a `&str` borrowed from a
seqlock-validated stack copy (short values) or from the mmap (long `ro.`
values):

```rust,no_run
fn main() -> rsproperties::Result<()> {
    let props = rsproperties::system_properties()?;

    // Compute over the value without ever allocating a String.
    let major_len: usize = props.read_with("ro.build.version.release", |v| {
        v.split('.').next().map_or(0, str::len)
    })?;
    println!("the major version is {major_len} bytes long");

    // Append to a buffer the caller reuses; the callback runs while the
    // bytes are still seqlock-validated, so keep it cheap and non-blocking.
    let mut buf = String::new();
    props.read_with("ro.product.model", |v| buf.push_str(v))?;
    println!("model: {buf}");
    Ok(())
}
```

This mirrors bionic's `__system_property_read_callback` pattern.

### Property Monitoring

Wait once for a value (libbase `WaitForProperty`):

```rust,no_run
use std::time::Duration;

fn main() -> rsproperties::Result<()> {
    let props = rsproperties::system_properties()?;
    // Returns when the property exists and equals "1"; `false` is a timeout.
    if !props.wait_for_value_timeout("sys.boot_completed", "1", Duration::from_secs(30))? {
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
        level.wait_for_change()?;
    }
}
```

One update is reported as one change, also when it stores the value the
property already had; updates that land between two calls are reported as
one change with the latest value.

Wait for a change to any property with an `AreaWatcher`, which keeps the
global serial between calls:

```rust,no_run
use std::time::Duration;

fn main() -> rsproperties::Result<()> {
    let props = rsproperties::system_properties()?;
    // Starts at the global serial of this call.
    let mut area = props.watch_area();
    loop {
        // A change made after a timeout, or while the previous one was
        // handled, returns the next call at once.
        if area.wait_for_change_timeout(Duration::from_secs(60))? {
            println!("a property changed, serial {}", area.serial());
        } else {
            println!("no change for 60 s");
        }
    }
}
```

`PropertyWatcher` is built on `find`, `read_index_with` and
`wait(index, old_serial)`; they stay public for a caller that keeps its
own index and serial. A wait for any property is `watch_area()` only;
`area_serial()` stays for a check without a wait.

### Async Waits

`rsproperties::asynchronous` has the same waits as `async fn`s, with the
same arguments and no timeout argument (wrap the future in your runtime's
timeout; dropping it cancels the wait). They need no feature and no
particular runtime:

```rust,no_run
async fn follow() -> rsproperties::Result<()> {
    let props = rsproperties::asynchronous::system_properties()?;
    props.wait_for_value("sys.boot_completed", "1").await?;

    let mut level = props.watch("debug.my_service.log_level");
    loop {
        println!("log level: {:?}", level.value()?);
        level.wait_for_change().await?;
    }
}
```

A futex has no fd an executor could poll, so the first async wait of an
instance that has to block starts one thread for that instance; a wait
whose condition already holds returns on its first poll and starts
nothing.
`SystemProperties::as_async()` gives the async view of any instance. What
the thread wakes, how it ends, and where the return conditions differ
from the blocking waits are listed in the API Reference under
"`rsproperties::asynchronous`".

### Setting Properties

Setting properties requires a running property service: init on Android,
[`rsproperties-service`](https://crates.io/crates/rsproperties-service) on
Linux.

```rust,no_run
fn main() -> rsproperties::Result<()> {
    // Basic property setting
    if let Err(e) = rsproperties::set("debug.my_app.enabled", "true") {
        eprintln!("Failed to set property: {e}");
    }

    // Set application configuration. The value is any `Display` type.
    rsproperties::set("debug.my_app.log_level", "verbose")?;
    rsproperties::set("debug.my_app.port", 8080)?;

    // Vendor and custom application properties
    rsproperties::set("vendor.my_app.hw_config", "v2")?;
    rsproperties::set("my.company.app.version", "1.2.3")?;

    // Set system properties (may require elevated permissions)
    rsproperties::set("sys.my_service.ready", 1)?;
    rsproperties::set("sys.my_service.pid", std::process::id())?;

    // Set persistent properties: kept across reboots on Android, and by
    // `rsproperties-service` when it is given a persistent properties file
    rsproperties::set("persist.my_app.config", "production")?;
    Ok(())
}
```

`set` sends its request with `send(MSG_NOSIGNAL)`, as does
`asynchronous::set`: a socket the service has already closed is an
`Error::Io` of kind `BrokenPipe`, never a `SIGPIPE`. This matters for a
host that keeps `SIGPIPE` at its default — a C/C++ program linking this
crate as a staticlib or cdylib, or a JNI library — which the signal would
kill.

#### Property Setting Requirements

**On Android:**
- Properties are set through the property service
- Some properties require specific SELinux permissions
- `ro.*` properties are write-once: an unset one can be set, but setting one that already has a value is rejected
- System properties may require root or system privileges

**On Linux:**
- Requires `rsproperties-service` to be running (`rsproperties-service = "0.7"`; see Related Crates)
- Properties are stored in memory-mapped files
- `ro.*` properties are write-once here too (enforced by `rsproperties-service`)

#### Protocol Version

`set` sends the V2 frame (`PROP_MSG_SETPROP2`) unless one of these
selects V1 (`PROP_MSG_SETPROP`), checked in this order:

1. `ro.property_service.version`, when the process has already opened
   the global reader (an earlier `get`, `get_or` or `system_properties()`
   call; `set` does not open it): a number of 2 or more is V2, any other
   value is V1.
2. The `PROPERTY_SERVICE_VERSION` environment variable, when the reader
   is not open or the property is missing: a number of 2 or more is V2,
   any other value is V1 (logged at `warn` when it is not a number).
3. V2 when neither is set. bionic sends V1 when the property is missing.

The first `set` after the reader is open fixes the version for the
process; before that, each `set` checks the environment variable again.

A V1 `set` takes a name of at most 31 bytes and a value of at most 91
(`Error::InvalidArgument` otherwise) and reads no reply: it
returns `Ok` after it has sent the frame, whether or not the value was
applied. `rsproperties-service` refuses V1 frames, so a V1 `set` to it
returns `Ok` and sets nothing. The service sets
`ro.property_service.version=2` at startup: read a property before the
first `set`, or leave `PROPERTY_SERVICE_VERSION` unset, and `set` sends
V2.

#### Setting from tokio (`tokio` feature)

`rsproperties::set` blocks the calling thread on the socket. With the
`tokio` feature, `asynchronous::set` runs the same request on tokio's I/O
and time drivers and returns a `Send + 'static` future. The feature adds
tokio with only its `net` and `time` features and no runtime, so the
program depends on tokio itself for the runtime (and for `tokio::spawn`
below), with both the I/O and the time driver enabled (`#[tokio::main]`,
or `Builder::enable_all()`):

```toml
[dependencies]
rsproperties = { version = "0.7", features = ["tokio"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust,no_run
async fn configure() -> rsproperties::Result<()> {
    rsproperties::asynchronous::set("debug.my_app.workers", 4).await?;
    // The future owns its request, so it can be spawned.
    let mode = tokio::spawn(rsproperties::asynchronous::set("debug.my_app.mode", "fast"));
    mode.await.expect("set task panicked")?;
    Ok(())
}
```

- The name and value are checked, and the protocol version chosen, as
  `set` does, when `asynchronous::set` is called; a refused request fails
  at the `await` with the same error and does not connect.
- Connect and send keep the 2 s bounds. The reply wait has no bound on
  Linux either (`set`: 2 s); wrap the future in `tokio::time::timeout` to
  bound it.
- Dropping the future before the request is sent in full leaves the
  property unchanged. Dropped after that, the service applies the request
  and the outcome is not reported.
- A request that passed the checks panics when polled outside a tokio
  runtime, or in one without both the time and the I/O driver enabled.

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

```rust,no_run
use rsproperties::PropertyConfig;

fn main() -> rsproperties::Result<()> {
    // Both directories, or only the one to change. A directory already set
    // to a different path makes `init` fail with `AlreadyInitialized`.
    rsproperties::init(
        PropertyConfig::new()
            .properties_dir("/custom/properties")
            .socket_dir("/custom/socket"),
    )?;
    Ok(())
}
```

A library that must not set the process-wide directories, or a process
that reads a second properties directory, opens its own reader instead.
It is independent of `init` and of the global reader:

```rust,no_run
use rsproperties::SystemProperties;

fn main() -> rsproperties::Result<()> {
    let props = SystemProperties::open("/custom/properties")?;
    let sdk: String = props.get("ro.build.version.sdk")?;
    println!("SDK {sdk}");
    Ok(())
}
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
- **Property Service**: Use with `rsproperties-service` ([crates.io](https://crates.io/crates/rsproperties-service)) to run the property service on a Linux host

## API Reference

### Configuration

- `PropertyConfig::new().properties_dir(dir).socket_dir(dir)` — the
  directories to set; either may be left out
- `PropertyConfig::get_properties_dir()` / `get_socket_dir()` — the
  directory a config value holds (`Option<&Path>`), without touching the
  process-wide setting
- `init(config)` — set them once per process; `Ok` again for the same
  paths, `Err(AlreadyInitialized)` for a different one
- `properties_dir()` — currently-configured properties directory; the
  first call latches the default (`/dev/__properties__`) when `init` set
  none
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
  sending keep the 2 s bound (bionic has none).
  `asynchronous::set` (`tokio` feature) is the same request without
  blocking the thread, and without a bound on the reply wait
- `system_properties()` — `&'static SystemProperties` or `Err`
  (`Error::Init`); a failure is retried by the next call

Property names follow Android's prefixes: `ro.*` is set once
(`ro.build.version.sdk`), `persist.*` survives a reboot when the
property service stores it (`persist.sys.timezone`), `sys.*` holds
system state (`sys.boot_completed`), `debug.*` is for debugging
(`debug.my_app.log_level`), `vendor.*` belongs to the vendor partition
(`vendor.audio.config`), and `ctl.*` sends a control message to init
instead of being stored.

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
  serial that any change advances (bionic `__system_property_area_serial`).
  While a writer is mid-update (dirty bit set), `serial` waits up to
  200 ms for the clean serial, where bionic `__system_property_serial`
  returns the raw one
- `wait(index, old_serial)` — futex-wait until the property's serial
  differs from `old_serial` (from `serial(index)` or `read_index_with`)
  and return the new serial (bionic `__system_property_wait(pi, ..)`).
  `Err` on failure. As in bionic, the returned serial can carry the dirty
  bit; passed back as the next `old_serial`, it reports that update's
  clean store as a second change (`watch` does not). A wait on any
  property (bionic `__system_property_wait(NULL, ..)`) is `watch_area()`
- `wait_for_creation(name)` / `wait_for_value(name, expected)` — libbase
  `WaitForPropertyCreation` / `WaitForProperty`; `Ok(index)` / `Ok(())`,
  a failure is `Err`
- Each wait above has a `_timeout` form that takes a `Duration` as its
  last argument: `wait_timeout` returns `Ok(Some(serial))` on a change and
  `Ok(None)` on timeout, `wait_for_creation_timeout` `Ok(None)`,
  `wait_for_value_timeout` `Ok(false)`. `Duration::ZERO` checks without
  blocking
- `watch(name)` — a `PropertyWatcher` (libbase `CachedProperty`):
  `value()` is `Ok(None)` until the property exists and reads the store
  again only after a change; `wait_for_change()` blocks until the
  creation or the next update, and `wait_for_change_timeout(timeout)`
  returns `Ok(false)` on timeout. One update is one
  change (the dirty and the clean serial store are not reported
  separately), and changes are not queued. `name()` returns the watched
  name
- `watch_area()` — an `AreaWatcher` on the global serial, starting at
  its value at this call: `wait_for_change()` blocks until any property
  is added or updated, `wait_for_change_timeout(timeout)` returns
  `Ok(false)` on timeout and keeps the serial, and `serial()` returns the
  serial last reported. Changes are not queued
- `PropertyIndex` — the handle `find` / `foreach` return; `Copy`, `Eq`
  and `Hash`. Two indexes are equal when the same instance returned them
  for the same entry, so an index can be a `HashMap` key

A read-only instance (the global reader, `SystemProperties::open`)
created while `BIONIC_APPCOMPAT_OVERRIDE` is set in the environment
(zygote sets it on Android) looks up `ro.appcompat_override.<name>` first
in `find` and `read_with`, like bionic — on Android only when
`ro.build.version.sdk` is 37 or more, on Linux always. Writer instances
(`new_area`, `new_area_locked`) never do; see
`SystemProperties::read_with`. This follows the
AOSP `android-17.0.0_r1` sources and returned the same values as
`getprop` on an API 37 emulator.
On API 35–36 the override is a bind mount over `/dev/__properties__`,
which a reader created in that process follows without extra code.

### `rsproperties::asynchronous`

- `asynchronous::system_properties()` — the global reader's async view
  (`&'static asynchronous::SystemProperties`)
- `SystemProperties::as_async()` / `asynchronous::SystemProperties::as_sync()`
  — the async / blocking view of one instance; the same instance, not a
  copy, so a `PropertyIndex` works with both
- `wait(index, old_serial)`, `wait_for_creation(name)`,
  `wait_for_value(name, expected)`, and `watch(name)` / `watch_area()`
  with an async `wait_for_change()` on `PropertyWatcher` / `AreaWatcher`
  — the blocking waits as `async fn`s with the same arguments and
  results. There is no `_timeout` form: wrap the future in the runtime's
  timeout. Dropping the future cancels the wait; a dropped
  `wait_for_change` leaves the watcher as it was
- `serial(index)` — `async fn`: the clean serial, waiting without the
  blocking `serial`'s 200 ms bound
- `read_with`, `read_index_with`, `foreach`, `list`, `get`, `find`,
  `area_serial` and (`builder` feature) `property_type` — the blocking
  reads, unchanged
- `asynchronous::set(name, value)` (`tokio` feature) — `set` on tokio; see
  "Setting from tokio" above

The futures are `Send`. The first wait of an instance that has to block
starts that instance's waiter thread: the global reader's thread runs
until the process exits, another instance's ends within about a second of
the instance's drop. The thread futex-waits on the global serial and, on
a change, wakes the waits whose property's serial moved and every wait
on the global serial (a creation, an area watcher), which check their
condition again. A per-property wait therefore returns after the
writer's global `FUTEX_WAKE`. A wait that has to block fails when the
thread cannot be started (the next wait tries again); once the thread
has ended on a futex error, every later blocking wait of that instance
fails with that error. A writer that moves the global serial without a wake
(bionic in an appcompat override process on API 35–36) is seen at the
next change that wakes, as with bionic's own waits. A child forked
without exec after the instance's first blocking async wait has no waiter
thread, so its waits that have to block are never woken (not supported).

### Wire-protocol constants & validators (`rsproperties::wire`)

- `PROP_VALUE_MAX`, `PROP_NAME_MAX` — AOSP wire-format size caps
- `MAX_WIRE_NAME_LEN`, `MAX_WIRE_VALUE_LEN` — this crate's service
  limits; `AOSP_MAX_WIRE_STRING_LEN` — AOSP init's limit, used by the
  Android client
- `PROP_MSG_SETPROP`, `PROP_MSG_SETPROP2` — command IDs
- `PROP_SUCCESS` and the `PROP_ERROR_*` codes — V2 response codes, as
  AOSP init sends them (`SetRejection` has one variant per code).
  `PROP_ERROR` (-1) is the generic code the 0.6.0 git release of
  `rsproperties-service` sent for every rejected set
  (`SetRejection::Unspecified`); init and the current service do not
  send it
- `validate_property_name(name)` — name charset; no leading or trailing
  `.`, no `..`
- `validate_value_len(name, value)` — value-length policy with the
  long-`ro.*` exception

Two socket-name constants are re-exported at the crate root for
clients that want to point at a custom socket directory:

- `PROPERTY_SERVICE_SOCKET_NAME` — `"property_service"`
- `PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME` —
  `"property_service_for_system"`

`PROP_DIRNAME` (`"/dev/__properties__"`, the default properties
directory) is at the crate root as well:

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

### Errors

- `Error` — non-exhaustive enum (`Io`, `NotFound`, `Utf8`, `Parse`,
  `FileValidation`, `InvalidArgument`, `AlreadyInitialized`,
  `LimitExceeded`, `SetRejected`, `ReadOnlyProperty`, `PermissionDenied`,
  `FileSize`, `AreaFull`, `FileOwnership`, `Lock`, `Init`, `Context`)
- `SetRejection` — why the service rejected a set, in
  `Error::SetRejected { name, reason }`: a non-exhaustive enum with one
  variant per `wire::PROP_ERROR_*` code (`ReadOnlyProperty`,
  `InvalidName`, `InvalidValue`, `PermissionDenied`, …) and
  `Unknown(i32)` for any other code (`Unspecified` is `wire::PROP_ERROR`,
  -1); `code()` returns the number
- `Error::root_cause()` — the error under the `Context` / `Init` layers
- `Result<T>` = `Result<T, Error>`

### `SystemProperties` write side (`builder` feature)

- `AreaWriterLock::acquire(dir)` / `lock.publish_property_info(trie)` /
  `SystemProperties::new_area_locked(lock)` — take the directory's
  writer lock, publish `property_info` (0444, temp + rename) and create
  the areas under it; prefer this over writing `property_info` yourself.
  `acquire` creates a missing `dir` (the last component only, mode 0711
  minus the umask's bits) and removes the temp files a crashed writer
  left there. `lock.dir()` returns the directory the lock guards. Dropping the lock,
  or the `SystemProperties` that holds it, releases it with
  `flock(LOCK_UN)`, so the next `acquire` succeeds even while a child
  process that has not yet `exec`ed holds a copy of the fd
- `SystemProperties::new_area(dir)` — create the areas for an existing
  `<dir>/property_info`; it takes the writer lock before it reads that
  file, and without the file it fails before `dir` or `.writer_lock` is
  created
- `SystemProperties::add(name, value)` — append a new entry; for a name
  that already exists it changes nothing and returns `Ok(())`, like
  bionic `prop_area::add`
- `SystemProperties::update(index, value)` — overwrite an existing
  entry (seqlock-guarded); a `ro.` entry is never updated:
  `Error::ReadOnlyProperty`, value and serial unchanged
- `SystemProperties::set(name, value)` — add-or-update; a `ro.` property
  that already has a value is refused with `Error::ReadOnlyProperty`
- `SystemProperties::property_type(name)` — the `property_contexts` type
  that `property_info` records for `name` (`"string"`, `"bool"`,
  `"int"`, …), or the trie's default type; `Ok(None)` when there is
  neither. `add` and `set` do not check a value against it
- `load_properties_from_file(path, filter, &mut HashMap)` —
  parse build.prop entries; like AOSP init it opens each file with
  `O_NOFOLLOW` and skips (with a warning) a file writable by group or
  others. Returns `PropFileOutcome`: `Loaded`, or
  `SkippedWritable { mode }` when `path` itself was skipped (a skipped
  `import` is only logged). Values are expanded like AOSP `ExpandProps`
  (`${name}`, `${name:-default}`, `$$` for a `$`) against the entries in
  the map; an entry whose value cannot be expanded is logged and dropped
- `load_properties_from_file_with(path, filter, &mut HashMap, accept)` —
  the same load with `accept(name, value) -> bool` called on every
  entry, with its expanded value, before it enters the map, where AOSP init's `LoadProperties`
  calls `CheckPermissions`. An entry `accept` refuses is logged and
  dropped; a value the map already holds for that name stays
- `property_type_in_trie(trie, name)` — the lookup of
  `SystemProperties::property_type` on the bytes `build_trie` returns,
  for a trie that is not published yet (for example in an `accept`
  closure that checks build.prop values against their types)
- `PropertyInfoEntry::parse_from_file(path, MatchKeyword)` —
  parse a `property_contexts`-format file into
  `(Vec<PropertyInfoEntry>, Vec<Error>)` (the entries that survived
  and the per-line errors). A line without a third token is a prefix
  match. `MatchKeyword::Checked` rejects a third token other than
  `prefix` / `exact` (AOSP `require_prefix_or_exact = true`);
  `MatchKeyword::Unchecked` takes such a line as a prefix match
- `PropertyInfoEntry::exact(name, context, type_str)` /
  `PropertyInfoEntry::prefix(name, context, type_str)` — build an entry
  without a file; `name` and `context` take a `&str` or a `String`.
  Entries compare with `==`
- `build_trie(entries, default_context, default_type)` — serialize a
  property-info trie

## Thread Safety

All operations are thread-safe and can be used concurrently:

```rust,no_run
use std::thread;

fn main() {
    // Multiple threads can safely access properties
    let handles: Vec<_> = (0..10)
        .map(|i| {
            thread::spawn(move || {
                let prop_name = format!("debug.thread.{i}");
                let value: String = rsproperties::get_or(&prop_name, "default".to_string());
                println!("Thread {i}: {prop_name} = {value}");
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
}
```

## Building Property Databases (`builder` feature)

```rust,no_run
use rsproperties::{
    build_trie, load_properties_from_file, AreaWriterLock, MatchKeyword, PropFileOutcome,
    PropertyConfig, PropertyInfoEntry, SystemProperties,
};
use std::collections::HashMap;
use std::path::Path;

fn main() -> rsproperties::Result<()> {
    // Load properties from Android build.prop files. A file writable by group
    // or others (0664 from a umask 002 checkout) is not read: the call then
    // returns `PropFileOutcome::SkippedWritable { mode }`.
    let mut properties = HashMap::new();
    let outcome =
        load_properties_from_file(Path::new("system_build.prop"), None, &mut properties)?;
    assert_eq!(outcome, PropFileOutcome::Loaded);

    // Take the directory's writer lock first, then publish the context trie as
    // `<dir>/property_info` and create the areas under that lock. Do not write
    // `property_info` with `std::fs::write`: readers map it, so it must only be
    // replaced by rename (see below). Release builds (no `debug-assertions`)
    // and the `strict-file-validation` feature require the property files
    // (`property_info`, the area files, `properties_serial`) to be owned by
    // root (uid 0, gid 0), so run this as root there: `publish_property_info`
    // checks its file with the readers' loader and fails otherwise.
    let (mut entries, _errors) =
        PropertyInfoEntry::parse_from_file(Path::new("property_contexts"), MatchKeyword::Unchecked)?;
    // An entry can also be built without a file.
    entries.push(PropertyInfoEntry::prefix(
        "debug.my_app.",
        "u:object_r:debug_prop:s0",
        "string",
    )?);
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
    println!("SDK {sdk_version}");
    Ok(())
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

## Examples

The crate includes Android-compatible command-line tools:

- **`getprop.rs`** - Android-compatible property getter with support for custom directories; without arguments it lists every readable property, sorted by name
- **`setprop.rs`** - Android-compatible property setter with validation and error handling

Run examples with:
```bash
# List all properties
cargo run --example getprop

# Get a property with default value
cargo run --example getprop ro.build.version.sdk 0

# Set a property
cargo run --example setprop debug.my_app.test true
```

## Related Crates

- **[`rsproperties-service`](https://crates.io/crates/rsproperties-service)**
  ([docs](https://docs.rs/rsproperties-service)) - Linux emulation of the
  Android property service (a library that a host binary runs on tokio;
  Linux only, not usable on Android):

  ```toml
  [dependencies]
  rsproperties = "0.7"
  rsproperties-service = "0.7"
  ```

  The two crates are released together and carry the same version. Take
  both crates from the same source: with one from crates.io and the
  other from git, `rsproperties` is built as two separate crates, and
  `rsproperties_service::run` does not accept the `PropertyConfig` of the
  copy your code names.

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

Licensed under the Apache License, Version 2.0. See [LICENSE](https://github.com/hiking90/rsproperties/blob/main/LICENSE) for details.

## Contributing

Contributions are welcome! Please ensure:

1. All tests pass: `cargo test`
2. Code is formatted: `cargo fmt`
3. No clippy warnings: `cargo clippy --all-targets --all-features`

This implementation is based on Android's property system and maintains compatibility with Android's property semantics and behavior.
