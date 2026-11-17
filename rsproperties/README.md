# rsproperties

[![Crates.io](https://img.shields.io/crates/v/rsproperties.svg)](https://crates.io/crates/rsproperties)
[![Documentation](https://docs.rs/rsproperties/badge.svg)](https://docs.rs/rsproperties)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](https://github.com/hiking90/rsproperties/blob/main/LICENSE)

Android's system property system in Rust, for Android and Linux. Reads
map the property area files directly, without bionic's
`__system_property_*` functions; `set` goes through the property service
socket, as bionic does. Supported Android versions are Android 9 (API
level 28) to Android 17 (API level 37).

- Typed reads (`get`, `get_or`) that parse `bool` and integers by AOSP
  libbase's rules, and `read_with`, which reads a value without
  allocating.
- Waits for a value or a creation, `PropertyWatcher` (libbase
  `CachedProperty`) and `AreaWatcher`, blocking or as `async fn`s on any
  executor.
- `set` over the property service socket, and `asynchronous::set` on
  tokio (`tokio` feature).
- Creating property areas on a Linux host (`builder` feature), which
  [`rsproperties-service`](https://crates.io/crates/rsproperties-service)
  uses to emulate the property service.

## Installation

```toml
[dependencies]
rsproperties = "0.7"
```

| Feature | Adds |
|---|---|
| `builder` | Creating property areas and `property_info` (Linux hosts) |
| `tokio` | `asynchronous::set`; the async waits need no feature |
| `strict-file-validation` | The root-ownership check on property files in debug builds too, and refusal of a `property_info` whose owner-write bit is set |

The minimum supported Rust version is 1.85. Upgrading from 0.6: see
[Migration from 0.6.0](https://github.com/hiking90/rsproperties/blob/main/CHANGELOG.md#migration-from-060).

## Example

```rust,no_run
use std::time::Duration;

fn main() -> rsproperties::Result<()> {
    let sdk: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
    println!("SDK {sdk}");

    let props = rsproperties::system_properties()?;
    if props.wait_for_value_timeout("sys.boot_completed", "1", Duration::from_secs(30))? {
        // Needs a running property service: init on Android,
        // rsproperties-service on Linux.
        rsproperties::set("debug.my_app.ready", "1")?;
    }
    Ok(())
}
```

## Documentation

The API documentation is on [docs.rs](https://docs.rs/rsproperties). The
crate page there has an overview of reading, waiting, setting, errors and
creating property areas; each item documents its behaviour and where it
differs from bionic.

The repository has command-line examples, `getprop` and `setprop`
(`rsproperties/examples/`), and the [workspace README](https://github.com/hiking90/rsproperties#readme)
shows how to run them against `rsproperties-service` on Linux.

## License

Licensed under the Apache License, Version 2.0. See
[LICENSE](https://github.com/hiking90/rsproperties/blob/main/LICENSE).
