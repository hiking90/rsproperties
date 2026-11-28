# rsproperties

A Rust implementation of Android's system property system for Linux and
Android.

This repository is a Cargo workspace with two crates. This page gives an
overview, installation, and how to build and test the workspace. The
API, its behaviour and the code examples are on docs.rs, and the
service's protocol and security details are in [`docs/`](docs/):
[service-protocol.md](docs/service-protocol.md) and
[service-security.md](docs/service-security.md). `docs/design/` holds
the maintainers' design notes (in Korean).

**Upgrading from 0.7:** 0.8.0 changes a few signatures, errors and
behaviours (pre-1.0, a minor release can break them). See
[Migration from 0.7.0](https://github.com/hiking90/rsproperties/blob/main/CHANGELOG.md#migration-from-070)
in the changelog.

## Crates

### `rsproperties`

[README](rsproperties/README.md) ·
[crates.io](https://crates.io/crates/rsproperties) ·
[docs.rs](https://docs.rs/rsproperties)

The client library. It reads the property area files directly through a
read-only mapping, without calling bionic's `__system_property_*`
functions, and sets properties through the property service socket, as
bionic does.

- Typed reads (`get`, `get_or`) that parse `bool` and integers by AOSP
  libbase's rules, and `read_with`, which reads a value without
  allocating a `String`.
- Listing every readable property (`list`, `foreach`).
- `set` over the property service socket: V2, or V1 when
  `ro.property_service.version` or `PROPERTY_SERVICE_VERSION` selects it.
- futex-based waits: wait for a value or for a property's creation,
  follow one property (`PropertyWatcher`) or the whole area
  (`AreaWatcher`), and the same waits as `async fn`s that run on any
  executor (`rsproperties::asynchronous`).
- `builder` feature: create property areas and `property_info` on a Linux
  host, or on Android where init does not create them. `tokio` feature:
  `asynchronous::set`, which does not block a thread while it waits for
  the property service.

### `rsproperties-service`

[README](rsproperties-service/README.md) ·
[crates.io](https://crates.io/crates/rsproperties-service) ·
[docs.rs](https://docs.rs/rsproperties-service)

Android's property service as a library, for Linux hosts and for Android
userspaces that run without init's property service: a host binary calls
`run` on a tokio runtime and passes the handlers. It creates the property
areas from `property_contexts` and build.prop files, and serves the
property service sockets, so `rsproperties` and bionic clients can `get`,
`set` and wait. It handles the V2 set command (`PROP_MSG_SETPROP2`)
only, checks values against their `property_contexts` type, can store `persist.*` properties in AOSP's
`persistent_properties` format, and passes `ctl.*` and `sys.powerctl`
requests to handlers the host provides. It asks an authorizer the host
provides whether each request may go on, where init asks SELinux; without
an authorizer it checks no sender beyond the socket files' modes. It
compiles for Linux and Android. On Android, where init normally runs the
property service, it serves a userspace that runs without one, and `run`
requires an authorizer
([Running on Android](docs/service-security.md#running-on-android)).

## Platform Support

| Component | Linux | Android |
|---|---|---|
| `rsproperties` client (`get`, `set`, `wait`) | supported | supported |
| `builder` feature (creating property areas) | supported | supported, where init does not create them |
| `rsproperties-service` | supported | supported, for a userspace without init's property service |

Supported Android versions are Android 9 (API level 28) to Android 17
(API level 37). The Android 17 appcompat-override lookup is implemented
from the AOSP `android-17.0.0_r1` sources and was compared with `getprop`
on an API 37 emulator.

`rsproperties-service` also needs 64-bit atomics (`rsactor` uses
`AtomicU64`); CI checks that the client builds on 32-bit Linux targets
without them (`armv5te-unknown-linux-gnueabi`, `powerpc-unknown-linux-gnu`).

## Installation

The client:

```toml
[dependencies]
rsproperties = "0.8"
```

Creating property areas needs the `builder` feature, and
`asynchronous::set` the `tokio` feature:

```toml
[dependencies]
rsproperties = { version = "0.8", features = ["builder", "tokio"] }
```

The property service. `run` takes an `rsproperties::PropertyConfig`,
so the service also needs `rsproperties`, and tokio for the runtime
(`macros`, `rt-multi-thread` and `signal` are what the service README's
examples use):

```toml
[dependencies]
rsproperties = "0.8"
rsproperties-service = "0.8"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
```

The two crates are released together and carry the same version; use
the same version of both.

The same release from git:

```toml
[dependencies]
rsproperties = { git = "https://github.com/hiking90/rsproperties", tag = "v0.8.0" }
rsproperties-service = { git = "https://github.com/hiking90/rsproperties", tag = "v0.8.0" }
```

Take both crates from the same source: mixing crates.io and git builds
two `rsproperties` crates, and `run` does not accept the `PropertyConfig`
of the other one.

### Minimum Supported Rust Version

Rust 1.85. Cargo refuses to build the crates with an older compiler
(`rust-version`). Running the workspace's tests needs a newer one: the
dev-dependency `criterion` 0.8 declares Rust 1.86.

## Examples

`rsproperties/examples/` has `getprop` and `setprop`, which mimic
Android's tools, and `rsproperties-service/examples/` has
`example_service`. Pass the examples' own options after `--`; otherwise
cargo reads them as its own.

On Linux, start the service first. Without options it uses
`./__properties__` for the property areas and `./__properties__/sockets`
for the sockets, and creates them when missing:

```sh
cargo run -p rsproperties-service --example example_service
```

Then, from another shell:

```sh
# List every property, sorted by name
cargo run -p rsproperties --example getprop -- --properties-dir __properties__

# Read one property; the second argument is the default
cargo run -p rsproperties --example getprop -- --properties-dir __properties__ ro.property_service.version
cargo run -p rsproperties --example getprop -- --properties-dir __properties__ debug.my_app.enabled 0

# Set a property (on success it prints only its log lines, on stderr)
cargo run -p rsproperties --example setprop -- \
    --properties-dir __properties__ --socket-dir __properties__/sockets \
    debug.my_app.enabled 1
```

On Android, `getprop` and `setprop` read `/dev/__properties__` and use
`/dev/socket` without options.

These are debug builds, which any user can run. A release build, or one
with the `strict-file-validation` feature, accepts property files only
when uid 0 / gid 0 own them, so the service then has to run as root.

## Building and Testing

```sh
# Build the workspace
cargo build --workspace
cargo build --workspace --all-features
cargo build --workspace --examples

# Run the tests as a non-root user
cargo test --workspace
RUST_LOG=debug cargo test --workspace
```

`cargo test --all-features` (it enables `strict-file-validation`) and
`cargo test --release` require root-owned property files, so as a
non-root user every test that creates property files fails there. CI
builds as the CI user and starts each test binary as root through cargo's
runner:

```sh
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test --all-features
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test --features builder --release
```

These runs leave root-owned `rsprops_*_<pid>` directories in the temp
dir, which a later non-root run cannot remove; run the non-root tests
first.

Inside `rsproperties-service/`, `--all-features` does not enable
`rsproperties/strict-file-validation` (the service has no features of
its own); `--features rsproperties/strict-file-validation` does. One
integration-test binary runs with `--test <file under tests/>`, for
example `cargo test -p rsproperties-service --test integration_tests`.

### Building for Android

Both crates build for Android. CI builds `rsproperties` and checks
`rsproperties-service` with the Android NDK and
[`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk):

```sh
rustup target add aarch64-linux-android
cargo ndk --target aarch64-linux-android --platform 21 build -p rsproperties --all-features
cargo ndk --target aarch64-linux-android --platform 21 check -p rsproperties-service --all-targets --all-features
```

The Android tests run on an emulator through the `ndk_prepare`,
`ndk_build` and `ndk_sync` functions of `envsetup.sh`; see
`.github/workflows/android-test.yml`.

## Contributing

Contributions are welcome. Before sending a change, check that:

1. The tests pass: `cargo test --workspace`, and the root runs above when
   the change touches file validation or a release-only path.
2. The code is formatted: `cargo fmt --all`.
3. Clippy reports nothing: `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
4. The documentation covers new behaviour: the rustdoc of the item
   (docs.rs shows it; keep the crate READMEs short), `docs/*.md` for the
   service's protocol and security details, and `CHANGELOG.md`.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Acknowledgments

This implementation follows Android's property system (bionic, init and
libbase in AOSP) and is compared with it on Android emulators.
