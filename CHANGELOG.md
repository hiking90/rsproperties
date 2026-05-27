# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: minor bumps may include API changes).

## [0.4.0] - 2026-05-27

Hardening release: protocol correctness, panic-safety, allocation-free
read/write hot paths, and tighter service defaults.

### Added

- `rsproperties::try_init` and `rsproperties::try_system_properties` —
  fallible initialization that surfaces errors as `Result` instead of
  panicking. The outcome is cached in `OnceLock`, so repeated calls
  see a consistent result.
- `rsproperties::wire` — public module of wire-protocol constants and
  shared validators (single source-of-truth shared by client/server,
  preventing drift between length/charset checks).
- `SystemProperties::read_with(name, |&str| -> R)` — bionic-style
  callback reader that hands the seqlock-validated value to the
  caller without materialising a `String`. `get<T>` / `get_or<T>`
  now route through this path, so the parse-and-discard read flow
  allocates nothing.
- `Error::Context` variant carrying `panic::Location`, plus a
  `format_error_chain` helper for flattened `source()` formatting.

### Changed

- Property update writer (`SystemProperties::update`) streams the
  current short-variant value directly from the byte-atomic mmap
  slot into a stack buffer and writes raw bytes to the dirty-backup
  area. Removes one heap allocation per update — meaningful on
  `build.prop` load where thousands of updates run during service
  start.
- `PropertyInfo` long-value reads borrow directly from the mmap
  (`Vec<u8>` → `&[u8]`); long entries are write-once so the borrow
  is stable for the mapping lifetime.
- `PropertyInfo::data` is now byte-wise atomic via `UnsafeCell` with
  documented `unsafe impl Sync`; layout assertions keep the on-disk
  format bionic-compatible. Trie atomic orderings tightened to
  `Acquire` on the read side.
- `#![warn(unsafe_op_in_unsafe_fn)]` enabled across the crate for
  Rust 2024 forward-compat.
- `rsactor` bumped to 0.15.

### Fixed

- `futex_wait` timeout no longer drifts across spurious wakes; the
  deadline is tracked as an absolute instant and the remaining
  relative timeout is recomputed each iteration.
- `sys.powerctl` socket selection no longer races: the client now
  attempts `connect()` and falls back on failure instead of probing
  with `fs::metadata` (TOCTOU between the probe and the connect).
- Trie / property-area lookup helpers (`prefix`, `exact_match`,
  entry-name UTF-8 decode) now log `warn!` on failure instead of
  silently `continue`ing, so on-disk corruption is observable.
- `rsproperties::try_init` failures propagate out of the service's
  `on_start`, so misconfigured paths fail loudly at startup instead
  of silently binding the old directories.

### Security / Hardening (service)

- Bound Unix sockets are `chmod`ed to `0o660` after `bind()` to avoid
  inheriting the process umask.
- Connection-limit permit is acquired *before* `accept()`; saturation
  parks the accept loop and lets the kernel backlog queue connect
  attempts instead of accepting-then-stalling them.
- `accept()` errors trigger a 100 ms back-off so `EMFILE` / `ENFILE`
  no longer saturates the log.
- `PropertyMessage::value` is masked in `Debug` output and service
  logs to keep property values out of captured traces.
- Blocking I/O in `PropertiesService::on_start` is moved onto
  `spawn_blocking`, so the tokio worker can drive `SocketService`
  during init instead of stalling.
- `build.prop` entries are now collected into a deterministic
  `BTreeMap` before apply — previous `HashMap` iteration meant which
  file "wins" on a conflict varied per run due to hash-seed
  randomisation.

### Docs

- Fix `cargo doc -D warnings` failures (private intra-doc link,
  unresolved `Self::value_as_string`).
- README installation snippets updated to the current major
  (`0.4`).

## [0.3.0] - earlier

See git history (`git log v0.3.0`).
