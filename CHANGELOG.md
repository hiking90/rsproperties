# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: minor bumps may include API changes).

## [0.6.0] - 2026-07-18

Consolidated correctness and hardening release from three successive
full-source reviews, spanning the seqlock protocol, the mmap access
model, untrusted-input validation, socket I/O, and the error contract.
(Internal iterations were numbered 0.6.0–0.8.0 but never published;
this entry covers everything since 0.5.0.)

### Fixed

Seqlock / memory model:

- **Writer fences**: `apply_write` now issues a `Release` fence between
  publishing the dirty serial and storing value bytes (matching bionic's
  `atomic_thread_fence`), and the dirty-backup rewrite fences before
  touching the shared slot. Without them, ARM readers could accept a
  torn read that passed the serial re-check.
- Dirty-path reads re-read the shared backup slot *after* the serial
  re-check, so a writer starting its next update could hand the reader a
  torn value that still passed validation. The backup is now snapshotted
  byte-wise atomically into a stack buffer *before* the fence, sized
  from the serial's length bits — bionic's protocol.
- Name/value C-string scans in the mmap are byte-wise atomic
  (`cstr_at`), removing a formal data race with cross-process writers
  rewriting adjacent slots.
- All trailing-name and long-value access now goes through the mmap base
  pointer (offset-based) instead of pointers derived from `&T`
  references — removing provenance-escaping arithmetic (Stacked/Tree
  Borrows UB) and, with it, the unsound-by-contract helper functions.
- The 92-byte short/long boundary used `>` instead of bionic's `>=`,
  silently truncating an exactly-92-byte value while recording length 92
  in the serial word.
- futex wake failures after a completed publish no longer return `Err`
  (the value was already visible; bionic ignores the wake result), and
  the global serial bump is no longer skipped.

Waits:

- **Same-process deadlock removed**: per-property waits are now
  *sliced* — the context node's read lock is released and re-acquired
  every ~100ms with a serial re-check, so a same-process builder writer
  (which needs the write lock) is delayed by at most one slice instead
  of blocking behind an unbounded futex wait. A three-way futex outcome
  (`changed` / `timed out` / `failed`) keeps persistent syscall failures
  from turning retry loops into busy-spins.
- `SystemProperties::serial` waits for a mid-update dirty serial to
  clear (bionic parity — one logical update no longer observable as two
  transitions), bounded to 200ms in case the writer crashed mid-update
  (bionic hangs forever there).
- `SystemProperties::wait` no longer panics on huge `tv_sec` timeouts
  (`Instant` overflow degrades to an infinite wait, matching bionic).

Untrusted-input validation (property files are cross-process shared
state and must not be trusted):

- `property_info` files are validated on load exactly like bionic's
  `PropertyInfoAreaFile::LoadPath`: unsupported
  `minimum_supported_version` and header-size/file-size mismatches are
  rejected up front instead of degrading into per-lookup errors.
- Context names from `property_info` are validated to be plain
  filenames, closing a path-traversal window that could unlink files
  outside the properties directory on the writable path; reserved names
  (`.writer_lock`, `properties_serial`, `property_info`, case-folded)
  and duplicate context names are rejected — a crafted file could
  otherwise alias the writer lock or serial bookkeeping onto a context
  area file.
- The context table is bounds-checked against the file (a corrupt
  4-byte count can no longer drive a huge allocation) and capped.
- Corrupt trie count fields now fail validation instead of silently
  reinterpreting adjacent file data; untrusted `namelen`/offsets are
  bounds-checked (a corrupt file could previously trigger an
  out-of-bounds scan past the mapping); BST walks carry cycle bounds
  instead of hanging; long-value offsets are validated against the
  entry layout.
- Interior NUL bytes are rejected on every write path — client `set`,
  server V2 decoder, the builder API (`add`/`update`), and trie
  building/serialization (entries *and* defaults). The storage format
  is C strings: a NUL-carrying value desynced the recorded length from
  the stored bytes, and a NUL-carrying name silently retargeted the
  write to a *different* property key.
- Property-area files must be regular files (a directory or device file
  now fails with a typed error instead of a confusing mmap errno), and
  the skipped-ownership-check path warns once instead of silently.
- `metadata.len() as usize` truncation on 32-bit targets could map fewer
  bytes than the validated file size and turn a load-time invariant into
  a reachable panic.
- `MemoryMap` tracks writability and rejects mutable access to
  read-only mappings; `ContextNode` no longer lazily maps read-only
  areas on the writer path (a write would have raised SIGSEGV).

Socket client (no-hang guarantees):

- All property-service socket I/O is bounded — **including connect**
  (`UnixStream::connect` against a full AF_UNIX backlog blocks before
  any read/write timeout can apply). Send *and* receive enforce the 2s
  timeout as a **total budget** rather than per-syscall (`SO_SNDTIMEO`/
  `SO_RCVTIMEO` re-arm on every syscall, so a peer trickling one byte
  per window could stretch "2 seconds" into hours). `EINTR` during the
  connect poll retries with the remaining budget instead of failing.
- The non-blocking connect path sets `SOCK_CLOEXEC` (the switch away
  from `UnixStream::connect` had silently dropped it — an fd leak into
  fork/exec'd children).
- **V1 protocol**: `sys.powerctl` is routed to the
  `property_service_for_system` socket by property name on V1 as well
  as V2 (bionic parity; the V1 arm previously passed a literal socket
  name, so `sys.powerctl` never reached the dedicated socket, with
  fallback preserved for devices without it). The V1 close-wait ack is
  infallible like bionic's: a drain error no longer retroactively fails
  a SET the server already applied.
- The wire protocol version follows bionic's order of authority: the
  `ro.property_service.version` property when the store is initialized
  (present-but-unparseable → V1), then the `PROPERTY_SERVICE_VERSION`
  env var, then a documented V2 default — and reading it no longer
  latches the default properties directory as a side effect of `set()`.
- `socket_dir()` reads `PROPERTY_SERVICE_SOCKET_DIR` via `var_os`, so a
  non-UTF-8 configured directory is used instead of silently swapped
  for the default (client and server could otherwise disagree on the
  socket path).

Service:

- Service sockets are bound via bind-to-temp → chmod → rename, so they
  are never connectable with umask-derived permissions (the previous
  bind-then-chmod window let early connections survive the chmod).
- The socket service's actor loop no longer awaits connection permits
  inline (64 slow clients could stall both listeners and graceful
  shutdown for 10 s each); accepted-but-waiting connections are capped
  so a connect flood can no longer exhaust file descriptors.
- The `.writer_lock` file is created `O_NOFOLLOW` with mode 0600 — a
  0644 lock file in the 0711 properties dir let any local user squat
  the writer lock via `flock` on a read-only fd.
- `try_init` pre-check + commit is now atomic (a global init lock also
  taken by the first-use latches), so a lost race can no longer leave
  the directory globals half-applied.

build.prop loading:

- `load_properties_from_file` supports `import` statements (recursive,
  depth-capped, `${property}` expansion) instead of failing the whole
  file; non-UTF-8 lines and empty keys are skipped with a warning.
- Duplicate imports **re-apply with last-wins semantics**, matching
  AOSP init's `LoadProperties` (an interim load-once dedup silently
  changed final values for real-world override patterns); a total
  file-loads budget (surfaced as `Error::LimitExceeded`) bounds the
  re-parse amplification that re-applying makes possible.
- The parser opens the canonicalized path it validated (best-effort
  TOCTOU narrowing), and property-context source lines are processed as
  raw bytes so one non-UTF-8 byte no longer aborts
  `PropertyInfoEntry::parse_from_file`'s documented per-line error
  collection; parse errors now carry line numbers.

Lookup/API behavior:

- `SystemProperties::find` flattened every lookup error to "not found",
  which could turn a corrupt-file error into a silently-successful
  no-op `set`; conversely a name matching *no context* returned `Err`
  where bionic returns null. Genuine absence — including the
  no-matching-context case — now maps to `Ok(None)`, and real errors
  are surfaced.
- `validate_property_name` now matches AOSP `IsLegalPropertyName`:
  only a leading `.` is rejected — leading `-`, `@`, `:` are legal and
  were previously refused, rejecting writes Android itself accepts.
- `set()` validates name and value client-side for both protocol
  versions before connecting; property values are masked as `<N bytes>`
  in service and client error logs, and wire UTF-8 errors carry
  position info without the failed bytes (`PropertyMessage` also
  dropped its value-leaking `Debug`).
- Doc examples no longer parse booleans with `get_or("...", false)` —
  Android boolean properties are `"0"`/`"1"`, which Rust's
  `bool: FromStr` does not parse, so the documented pattern silently
  always returned the default.
- Property-miss logging on the get hot path downgraded to `debug!`
  (was duplicated `warn!` + `error!` per miss); per-lookup success log
  downgraded to `trace!`; a corrupt-at-init context slot no longer
  logs `error!` on every affected lookup.

### Changed

- **Breaking:** `Error` is `#[non_exhaustive]`; `LockError` renamed to
  `Lock`; `From<OsString>` and the never-constructed `Conversion`
  variant removed; `Utf8Error`/`ParseIntError` now convert to
  source-preserving `Utf8`/`ParseInt` variants instead of stringifying
  into `Encoding`/`Parse`. New variants: `InvalidArgument` (API misuse,
  previously misfiled under `FileValidation`), `AlreadyInitialized`
  (first-write-wins conflicts, previously a misleading
  `PermissionDenied`), `AreaFull` (operational area exhaustion,
  previously misreported as corruption), `LimitExceeded` (global
  resource budgets), `ServiceError { name, code }` (protocol-level
  rejection, previously a fabricated `Error::Io`), and
  `Init(Arc<Error>)` (cached initialization failures preserve the
  original variant and `source()` chain).
- **Breaking:** `wire::validate_property_name` /
  `wire::validate_value_len` return the crate's typed
  `Result<()>` instead of `Result<(), String>`.
- **Breaking:** `socket_dir()` returns `&'static Path` instead of
  `&'static PathBuf`.
- **Breaking:** `SystemProperties::update` returns `Result<()>` — the
  previous `Result<bool>` had no `false` path.
- **Breaking:** `rsproperties_service::run`'s error type is now
  `Box<dyn Error + Send + Sync>` (spawnable / `anyhow`-convertible),
  and a failed startup stops both actors explicitly.
- **Breaking:** the `ContextWithLocation` trait gained
  `with_context_location` (lazy message closure — no allocation on the
  success path of per-line loops).
- **Breaking:** public-API surface reduction — internal items are no
  longer exported: `check_permissions` (no-op placeholder),
  `errors::validate_file_metadata`, and the never-reachable parser
  internals (`PropertyInfo`, `PropertyInfoArea`, `PropertyInfoAreaFile`,
  `PropertyInfoAreaHeader`, `TrieNode`) are now `pub(crate)`;
  `PropertyConfig::from_optional_path` (redundant with
  `From<PathBuf>`/`Default`) is removed.
- `try_init` with a socket-only config no longer commits the default
  properties directory, so a later properties-dir init still works.
- Serialized tries are byte-for-byte deterministic (prefix ties broken
  by name).
- `PROP_VALUE_MAX` at the crate root is a re-export of
  `wire::PROP_VALUE_MAX` (was an independent duplicate definition).
- `TrieNodeArena` is backed by `Vec<u32>`, making the arena's alignment
  a guarantee instead of a runtime zerocopy check; unsafe casts were
  already replaced with validated views, bounds are checked against the
  allocated extent, and the serialized size is validated to fit the u32
  offset space.

### Added

- `get_or_else` (lazy default) and a `Timespec` re-export (callers no
  longer need to depend on the exact `rustix` version to build wait
  timeouts).
- `PropertyIndex` derives `Clone`, `Copy`, `Debug`.
- `PropertyInfoEntry::new` public constructor (validated, returns
  `Result`) and getters (`name`, `context`, `type_str`, `exact_match`);
  `PropertiesServiceArgs::new`.
- `wire::MAX_WIRE_NAME_LEN` / `wire::MAX_WIRE_VALUE_LEN` — V2 wire caps
  shared by client and server (previously server-only, so the client
  could build frames the server always rejected).
- Linux regression tests for the fixes above (cross-instance
  futex wait/wake, duplicate-import last-wins, interior-NUL rejection,
  V1 `sys.powerctl` socket routing, socket total-timeout budgets) and
  criterion lookup benchmarks (`benches/props_bench.rs`, written
  against the API subset that exists since 0.5.0 for A/B comparison).

### Performance

Measured against v0.5.0 (criterion, Linux x86_64): value reads got
faster — `get` of a short value −16%, long value −11% (allocation-free
read path, long-entry resolution hoisted out of the seqlock retry
loop). Name resolution got slower — deep-name `get` +22%, `find` +19%,
miss +37% (~100–240 ns absolute) — the cost of the per-node bounds,
alignment, and cycle checks that now validate the trie/BST walk over
untrusted shared memory. This trade is deliberate: lookups stay
sub-microsecond, and the walk validation is what turns a corrupt or
hostile property file from UB/hangs into typed errors.

### Migration from 0.5.0

- Add a wildcard arm to any exhaustive `match` on `Error` (now
  `#[non_exhaustive]`), rename `Error::LockError` patterns to
  `Error::Lock`, and drop uses of `Error::Conversion` /
  `From<OsString>`. Code that matched `PermissionDenied` to detect
  "already initialized/mapped" conflicts should match
  `AlreadyInitialized`.
- `wire` validator calls that inspected the `String` error payload now
  get a typed `Error`.
- Treat `find(name) == Ok(None)` as the only "absent" signal; names
  matching no context no longer return `Err`.
- Replace removed items: `check_permissions` (was a no-op),
  `PropertyConfig::from_optional_path` (use `From<PathBuf>` /
  `Default`), `validate_file_metadata` (internal).
- `update()` now returns `Result<()>`; `socket_dir()` returns `&Path`.
- Boolean properties: parse `"0"`/`"1"` explicitly (see the crate docs)
  instead of `get::<bool>`-style patterns.

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
