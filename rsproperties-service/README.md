# rsproperties-service

[![Crates.io](https://img.shields.io/crates/v/rsproperties-service.svg)](https://crates.io/crates/rsproperties-service)
[![Documentation](https://docs.rs/rsproperties-service/badge.svg)](https://docs.rs/rsproperties-service)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](https://github.com/hiking90/rsproperties/blob/main/LICENSE)

The Android system property service as a library, for Linux hosts and
for Android userspaces that run without init's property service: a host
binary calls `run` on a tokio runtime, and the service creates the
property areas from `property_contexts` and build.prop files and serves
the property service sockets, so
[`rsproperties`](https://crates.io/crates/rsproperties) and bionic
clients can `get`, `set` and wait.

- Handles the V2 set command (`PROP_MSG_SETPROP2`) and answers with
  init's `PROP_ERROR_*` codes; the V1 command is refused.
- Checks values against their `property_contexts` type, and expands
  build.prop values, as init does.
- Can store `persist.*` properties in AOSP's `persistent_properties`
  format, in a file the host names.
- Passes `ctl.*` and `sys.powerctl` requests to handlers the host
  provides, in the order init handles them.
- Asks an authorizer the host provides whether each request may go on,
  where init asks SELinux (`AccessPolicy` is one made of uid and gid
  rules); the owner group, mode and SELinux label of each socket file,
  and the label of `property_info`, are configurable.

Without an authorizer it checks no sender beyond the socket files' modes.
It compiles for Linux and Android. On Android, where init normally runs
the property service, it serves a userspace that runs without one, and
`run` requires an authorizer
([Running on Android](https://github.com/hiking90/rsproperties/blob/main/docs/service-security.md#running-on-android)).

## Installation

```toml
[dependencies]
rsproperties = "0.8"
rsproperties-service = "0.8"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
```

`run` takes an `rsproperties::PropertyConfig`, so the host depends on
`rsproperties` too. The two crates are released together with the same
version; take both from the same source (crates.io, or the same git
tag), or `rsproperties` is built twice and `run` does not accept the
other copy's `PropertyConfig`. The crate has no features of its own and
always enables `rsproperties/builder`; the minimum supported Rust version
is 1.85.

## Example

```rust,no_run
use rsproperties_service::ServiceOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = rsproperties::PropertyConfig::new()
        .properties_dir("/run/props")
        .socket_dir("/run/props/sockets");
    let options = ServiceOptions::new()
        .property_contexts_files(["/system/etc/selinux/property_contexts"])
        .build_prop_files(["/system/build.prop"]);

    let mut service = rsproperties_service::run(config, options).await?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = service.join() => {}
    }
    service.shutdown().await?;
    Ok(())
}
```

A release build of `rsproperties` accepts property files only when root
owns them, so a release build of the service runs as root.

## Documentation

- [docs.rs](https://docs.rs/rsproperties-service): the API, with an
  overview of starting and stopping the service, the handlers, setting
  from the service's own process, and errors.
- [Protocol and init behaviour](https://github.com/hiking90/rsproperties/blob/main/docs/service-protocol.md):
  response codes, `sys.powerctl`, the type check, startup properties,
  size limits, and how a host answers `ctl.*` as init does.
- [Files, access control and hardening](https://github.com/hiking90/rsproperties/blob/main/docs/service-security.md):
  the directory layout, file ownership, who may use the service, running
  on Android, socket permissions and backpressure.

The repository's `example_service` (`rsproperties-service/examples/`)
runs the service from the command line; the
[workspace README](https://github.com/hiking90/rsproperties#readme) shows
how to run it with the `getprop` and `setprop` examples.

## License

Licensed under the Apache License, Version 2.0. See
[LICENSE](https://github.com/hiking90/rsproperties/blob/main/LICENSE).
