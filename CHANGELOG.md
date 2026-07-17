# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: minor bumps may include API changes).

## [0.6.0] - 2026-07-17

Soundness release: fixes from a full-source review spanning the seqlock
read protocol, the mmap access model, wire validation, and the service's
socket handling.

### Changed

- **Breaking:** `socket_dir()` returns `&'static Path` instead of
  `&'static PathBuf`.
- **Breaking:** `SystemProperties::update` returns `Result<()>` — the
  previous `Result<bool>` had no `false` path.
- **Breaking:** new `Error::Init(Arc<Error>)` variant. Cached
  initialization failures (`try_system_properties`) now preserve the
  original error variant and `source()` chain instead of flattening to a
  `FileValidation` string.
- **Breaking:** `rsproperties_service::run`'s error type is now
  `Box<dyn Error + Send + Sync>` (spawnable / `anyhow`-convertible), and
  a failed startup stops both actors explicitly.
- **Breaking:** the `ContextWithLocation` trait gained
  `with_context_location` (lazy message closure — no allocation on the
  success path of per-line loops).
- `try_init` with a socket-only config no longer commits the default
  properties directory, so a later properties-dir init still works.
- `validate_property_name` now matches AOSP `IsLegalPropertyName`:
  only a leading `.` is rejected — leading `-`, `@`, `:` are legal and
  were previously refused, rejecting writes Android itself accepts.
- `set()` validates name and value client-side for both protocol
  versions before connecting; property values are masked as `<N bytes>`
  in service and client error logs.

### Added

- `PropertyIndex` derives `Clone`, `Copy`, `Debug`.
- `PropertyInfoEntry` getters (`name`, `context`, `type_str`,
  `exact_match`) and `PropertiesServiceArgs::new`.
- `wire::MAX_WIRE_NAME_LEN` / `wire::MAX_WIRE_VALUE_LEN` — V2 wire caps
  shared by client and server (previously server-only, so the client
  could build frames the server always rejected).

### Fixed

- Seqlock dirty-path reads re-read the shared backup slot *after* the
  serial re-check, so a writer starting its next update could hand the
  reader a torn value that still passed validation. The backup is now
  snapshotted byte-wise atomically into a stack buffer *before* the
  fence, sized from the serial's length bits — bionic's protocol.
- Interior NUL bytes in names and values are rejected on every write
  path (`validate_value_len`, client `set`, server V2 decoder). The
  storage format is C strings: a NUL-carrying value desynced the serial
  length from the stored length (leaking stale backup bytes to dirty
  readers), and a NUL-carrying name was silently truncated into a
  *different* property key.
- All trailing-name and long-value access now goes through the mmap base
  pointer (offset-based) instead of pointers derived from `&T`
  references — removing provenance-escaping arithmetic (Stacked/Tree
  Borrows UB) and, with it, the unsound-by-contract helper functions.
- Untrusted trie `namelen`/offsets are bounds-checked (a corrupt file
  could previously trigger an out-of-bounds scan past the mapping), BST
  walks carry cycle bounds instead of hanging, and long-value offsets
  are validated against the entry layout.
- `MemoryMap` tracks writability and rejects mutable access to
  read-only mappings; `ContextNode` no longer lazily maps read-only
  areas on the writer path (a write would have raised SIGSEGV).
- `TrieNodeArena` unsafe casts replaced with runtime-validated zerocopy
  views; bounds are checked against the allocated extent; the serialized
  size is validated to fit the u32 offset space.
- The 92-byte short/long boundary used `>` instead of bionic's `>=`,
  silently truncating an exactly-92-byte value while recording length 92
  in the serial word.
- `SystemProperties::find` flattened every lookup error to "not found",
  which could turn a corrupt-file error into a silently-successful
  no-op `set`. Only genuine absence maps to `None` now.
- futex wake failures after a completed publish no longer return `Err`
  (the value was already visible; bionic ignores the wake result), and
  the global serial bump is no longer skipped.
- `metadata.len() as usize` truncation on 32-bit targets could map fewer
  bytes than the validated file size and turn a load-time invariant into
  a reachable panic.
- Service sockets are bound via bind-to-temp → chmod → rename, so they
  are never connectable with umask-derived permissions (the previous
  bind-then-chmod window let early connections survive the chmod).
- The socket service's actor loop no longer awaits connection permits
  inline (64 slow clients could stall both listeners and graceful
  shutdown for 10 s each); accepted-but-waiting connections are capped
  so a connect flood can no longer exhaust file descriptors.
- Property-name trie construction rejects empty segments (`a..b`),
  which were indistinguishable from the parser's corruption fallback
  and silently degraded lookups.

## [0.5.0] - 2026-06-12

Android-parity release: futex wait correctness, SELinux labeling, service
restart support, and V1 wire-protocol support in the service.

### Changed

- **Breaking:** `SystemProperties::wait` now takes an `old_serial:
  Option<u32>` parameter — `wait(index, old_serial, timeout)` — mirroring
  bionic `__system_property_wait(pi, old_serial, …)`. Passing the serial
  observed at read time closes the lost-wakeup window between reading a
  value and entering the wait; `None` keeps the previous sample-at-entry
  behavior.
- `SystemProperties::new_area` now treats the target directory as
  "build a fresh area": stale area files left by a previous writer
  instance are removed before the exclusive create, so a service restart
  over an existing directory succeeds (AOSP's fresh-/dev-tmpfs assumption
  doesn't hold for arbitrary dirs). A `.writer_lock` file (non-blocking
  exclusive `flock`, held for the writer's lifetime) makes a concurrent
  second writer fail fast instead of silently destroying the first
  writer's files.

### Added

- V1 (`PROP_MSG_SETPROP`) wire-protocol support in
  `rsproperties-service`: fixed 128-byte frames are decoded with AOSP
  parity (last byte of name/value forced to NUL) and answered V1-style —
  connection close as the implicit ack, no status word.
- Doc on `SystemProperties::add` stating the bionic-parity contract:
  adding an existing name is a silent no-op that does not update the
  value.

### Fixed

- `futex_wait` treated every syscall error as fatal. `EAGAIN` (the serial
  changed between the caller's load and the wait — the common race) now
  re-reads and returns the new serial like bionic; `EINTR` retries with
  the remaining timeout; `ETIMEDOUT` returns `None` without logging an
  error.
- SELinux labeling never worked: the xattr name was the bare `"selinux"`
  (kernel rejects it; the correct name is `"security.selinux"`,
  bionic's `XATTR_NAME_SELINUX`), and per-context area files were created
  with no context at all. `ContextNode` now carries its context and labels
  the file on create, matching bionic `context_node::open`.
- `ServiceWriter::send` issued a single `write_vectored` with no
  short-write handling; a partial write would desynchronise the
  length-prefixed protocol. It now loops until all bytes are written,
  retrying on `EINTR`.

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
