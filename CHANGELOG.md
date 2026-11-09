# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: minor bumps may include API changes).

## [Unreleased]

### Added

- `AreaWriterLock` (`builder` feature): the properties directory's writer
  lock (an exclusive `flock` on `<dir>/.writer_lock`) as a value.
  `AreaWriterLock::acquire(dir)` creates a missing `dir` (last component
  only, mode 0711 minus the umask's bits), takes the lock, and removes the
  temp files a crashed writer left in `dir`; `publish_property_info(&trie)`
  writes `property_info` under the lock (temp file, mode 0444, checked by
  the same loader readers use, then renamed into place);
  `SystemProperties::new_area_locked(lock)` creates the areas while still
  holding it; `dir()` returns the directory it guards. A second writer
  fails in `acquire` with `Error::Lock` before it changes `property_info`
  or any area file. `SystemProperties::new_area` keeps its signature.
- `wire::AOSP_MAX_WIRE_STRING_LEN` (65535): the largest V2 name or value
  length AOSP init accepts.
- Service: `StartupError` and `StartupStage`, the error type
  `rsproperties_service::run` now returns (see Changed);
  `PropertiesServiceArgs::with_writer_lock` and `properties_service::spawn`
  for code that spawns the actors itself.

### Changed

- `rsproperties_service::run` returns `Result<_, StartupError>` instead of
  `Box<dyn Error + Send + Sync>`. `StartupError::stage()` names the step
  that failed (`Config` for `try_init`, `WriterLock`, `PropertiesService`,
  `SocketService`) and `source()` is the failed actor's own startup error
  (e.g. the `std::io::Error` naming a missing `property_contexts` file);
  0.6.0 returned only rsactor's mailbox error behind a `Failed to start …
  service:` prefix. It displays as `<stage> failed to start: <cause>`. `?`
  into `Box<dyn Error + Send + Sync>` or `anyhow::Error` still compiles.
- `run` takes the properties directory's writer lock before it starts
  either actor and hands it to the properties service. Both actors still
  start together, as in 0.6.0.
- `rsproperties-service` emulates the Android property service on Linux
  and compiles only for `target_os = "linux"`; on other targets, Android
  and macOS included, a `compile_error!` names the reason as the first
  error. On Android, init owns the property service; use the
  `rsproperties` client there.
- Supported platforms are stated: the `rsproperties` client (`get`, `set`,
  `wait`) supports Linux and Android; the `builder` feature and
  `rsproperties-service` target Linux hosts (on Android, init creates the
  property areas). macOS is not supported: the client crate still
  compiles for it, but CI neither builds nor tests it.
- `SocketService` no longer implements `Drop`. Dropping it still unlinks
  both socket files (a private field does that now); only code that
  names `SocketService: Drop` in a bound is affected.
- `PropertyIndex` records which `SystemProperties` instance returned it;
  it grows from 8 to 16 bytes and its `Debug` output gains an `instance`
  field (see Fixed).
- `property_info` is still mapped read-only and shared, like bionic. A
  writer must replace it by renaming a complete new file over it, as
  `AreaWriterLock::publish_property_info` does, and never rewrite it in
  place: a reader that has it mapped can fault with `SIGBUS` or read a
  torn trie. Readers now log a `warn` line when `property_info` has the
  owner-write bit set — the usual sign of an in-place writer such as
  `std::fs::write` or `File::create`; with the `strict-file-validation`
  feature such a file is refused with `Error::PermissionDenied`.
- The socket service logs a warning at startup when the socket dir is
  world-writable without the sticky bit; startup continues.

### Fixed

- `load_properties_from_file`: `import <path> <filter>` lines split the
  filter off the path and load only the selected keys, like AOSP
  `LoadProperties`. Previously the whole `<path> <filter>` string was
  used as the path, so every filtered import failed to open and was
  skipped with a warning. This includes a file that is still being
  loaded higher up the import chain: a filtered load follows no `import`
  lines, so only unfiltered imports are cycle-checked.
- `import` path expansion now matches AOSP `ExpandProps` in three more
  cases: a trailing lone `$` is dropped (was an error), and a
  zero-length name (`${:-d}`) or an empty default (`${x:-}`) for a
  missing/empty property is an error (was expanded).
- `PropertyInfoEntry::parse_from_file(path, true)` again rejects a
  two-token `<property> <context>` line (missing match operation), as
  AOSP `ParsePropertyInfoLine` does. 0.6.0 accepted it as a prefix match
  under a wrong parity claim. With `false` (what every in-repo caller
  passes) the line is still accepted.
- A set but non-UTF-8 `PROPERTY_SERVICE_VERSION` now selects V1 with a
  warning, as documented, instead of being treated as unset (V2).
- `wire::validate_property_name` error messages print the name
  `Debug`-escaped, so a name carrying newlines or escape sequences cannot
  forge log lines where the error is logged. The same applies to the
  empty-segment `Error::Parse` from `SystemProperties::add` (and its
  `error` log line), to the `Error::FileValidation` that
  `SystemProperties::add`, `set`, `find` and `get_with_result` /
  `read_with` (and `get`) return for a name whose context was skipped at
  init, to the `Error::NotFound` for a name that maps to no context, to
  the `error` / `debug` / `trace` log lines of those lookups (the
  `Error::NotFound` for a name missing from its area still carries the
  name verbatim), to the disallowed-key `error` log of
  `load_properties_from_file`, and to the line and tokens that
  `PropertyInfoEntry::parse_from_file` logs and returns for a line it
  cannot parse, to the `Error::PermissionDenied` that
  `SystemProperties::update` (and `set` on an existing `ro.` name)
  returns and logs, and to the `build_trie` errors and log lines that
  name a property and the invalid-type error of `PropertyInfoEntry::new`
  (the former quoted the name, the latter the type string, with `'…'`).
- `load_properties_from_file` no longer logs the old and new values when
  a key is overridden (it logs the `Debug`-escaped key only), and logs
  the override only when the value changes, like AOSP `LoadProperties`
  (a duplicate `import` re-applying the same values no longer warns once
  per key).
- Linux/Android: `set` sends its frame with rustix `send(MSG_NOSIGNAL)`
  instead of `writev`, so a socket the service already closed yields
  `Error::Io` (`BrokenPipe`) instead of `SIGPIPE`, whatever std version
  the crate is built with (std's `UnixStream::write` is a plain
  `write(2)` on 1.77 through at least 1.86). Hosts that keep
  `SIGPIPE` at its default (C/C++ programs linking a staticlib/cdylib,
  JNI) were killed; Rust binaries ignore `SIGPIPE` and already got the
  error. macOS was already covered by `SO_NOSIGPIPE`.
- `SystemProperties::get_with_result` / `read_with` (and `get`): a miss
  inside a trie node's children now reports `Error::NotFound` with the
  full property name, like the other in-area miss paths; it carried only the
  last-looked-up segment (`foo` for `ro.foo`).
- `get::<T>` parse errors no longer include the property value (they
  give the name and the value length), so logging the error does not
  log the value. The message still ends with `T::Err`'s `Display`, which
  for std's parse errors carries no input but for a third-party `T` may
  echo part of the value.
- The property area header's `bytes_used` is an atomic, as the module's
  shared-mmap protocol requires for fields rewritten after readers
  attach (no layout or API change).
- Readers no longer make `Acquire` atomic loads on the `PROT_READ`
  mappings of area files and `properties_serial`: trie link words and
  serial words are loaded `Relaxed` and followed by `fence(Acquire)`,
  which gives the same ordering. std defines only small `Relaxed` atomic
  loads on read-only memory (`core::sync::atomic`, "Atomic accesses to
  read-only memory"), so the `Acquire` loads of `get`, `find`,
  `read_with`, `serial`, `context_serial` and `wait`, also in 0.6.0, were
  undefined behavior under that contract. The code generated for the
  supported targets makes no store there, so no misbehavior was
  observed (no layout or API change).
- `examples/setprop` uses the library's name validation, so legal names
  containing `@` or `:` are accepted (its length limits: see the
  `examples/setprop` entry below).
- `load_properties_from_file` skips (with a warning) a line whose key is
  not a legal property name (e.g. `foo bar=1`), like AOSP
  `LoadProperties`. Such keys were stored in the map, and the service
  added them to the area, where no `set` could change them.
- `SystemProperties::add` no longer logs the property value at `debug`
  level (it logs the name and the value length). With `RUST_LOG=debug`
  every new key, including client-set values in the service, was logged
  in clear despite the value masking elsewhere.
- Android: a mutable (non-`ro.`) property whose serial has bit 16 set
  can be read again. init's update counter sets that bit during updates
  32768–65535 of every 65536; bionic reads that bit as the LONG flag
  only on `ro.` names, while this crate read it on every name and failed
  with `Error::FileValidation` ("Long property offset … points inside
  the entry"), also in 0.6.0 — or, if the property once held a value of 57
  bytes or more, could return an empty or unrelated value read at an
  offset formed by that value's leftover bytes 56..59.
  `SystemProperties::update` likewise treats only `ro.` entries as long.
  A long read also checks the name stored in the entry (the name bionic
  decides by), so in a corrupt area where a `ro.` lookup reaches an entry
  stored under a non-`ro.` name with bit 16 set, the read fails with
  `Error::FileValidation` instead of reading the entry's value bytes as
  an offset (bionic returns the entry's short value there; conversely, a
  non-`ro.` lookup that reaches a long entry stored under an `ro.` name
  reads the short slot, the legacy error string, where bionic and 0.6.0
  read the long value).
- `SystemProperties::update` carries the serial counter past bit 16
  instead of dropping the carry, so a serial no longer returns to an
  earlier value after 32768 same-length updates (also in 0.6.0); it now
  repeats after 2^22 updates. A `wait` given a serial read exactly
  32768·k updates earlier did not return until the next change.
- `property_info` is opened with `O_NOFOLLOW`, like bionic
  `PropertyInfoAreaFile::LoadPath`: a symlink as the last path component
  fails to open (`ELOOP`) instead of mapping the file it points to. 0.6.0
  followed the symlink.
- `SystemProperties::new_area` builds each property area file under a
  temporary name (`.area.tmp-<pid>-<n>`) in the properties dir and
  renames it into place after its header is written. A reader opening an
  area while the writer (re)creates it no longer finds an empty file or a
  zero header; for `properties_serial`, which the global reader opens once,
  that failure lasted until the process exited. On a first start, or for a
  context that did not exist before, a reader that looks before the rename
  still finds no file. Temp area and `property_info` files that a crashed
  writer left behind are removed once the next writer holds the lock.
- `SystemProperties::new_area` takes the properties dir's writer lock
  before it reads `property_info` (0.6.0 read it first). Another writer
  that replaced `property_info` and released the lock between that read
  and the lock left `new_area` building areas for the previous file's
  contexts while readers opened the new file. A dir without
  `property_info` still fails before anything is created.
- `SystemProperties::new_area` sets each area file, `properties_serial`
  included, to mode 0444 whatever the umask, like `property_info` and
  AOSP init (which runs with umask 0). 0.6.0 created them 0444 minus the
  umask's bits: under umask 066 or 077 they were 0400, and a reader
  running as another uid failed to open `properties_serial` (`EACCES`),
  so every `get` of that process failed.
- A context name starting with `.` in `property_info` is skipped with a
  warning, like other invalid context entries, so no area file can take
  the name of a writer temp file or of `.writer_lock`.
- A `property_info` built by AOSP from `property_contexts` lines with an
  empty segment is read the way AOSP `GetPropertyInfoIndexes` reads it.
  AOSP's builder writes an empty-named child node for `.a`, `a..b` or
  prefix `a..`, and an empty-named exact entry for exact `a..`; this
  crate's `build_trie` still rejects such lines. 0.6.0 treated both as
  corruption: every lookup that ended at a node with an empty exact
  entry logged a `warn` line, legal names included, and so did a child
  search that compared a segment with the empty-named child. Lookups of
  legal names resolve as before.
- `load_properties_from_file` trims only the C `isspace` characters
  (space, `\t`, `\n`, `\v`, `\f`, `\r`) from lines, keys, values, and
  `import` paths and filters, like AOSP `LoadProperties`. `str::trim` also
  removed Unicode whitespace such as U+00A0 and U+3000, so
  `ro.x=foo\u{a0}` was stored as `foo`.
- `load_properties_from_file` opens every prop file like AOSP init's
  `ReadFile`: with `O_NOFOLLOW`, so a symlink as the last path component
  fails to open (an error for the root file; an `import` is skipped with
  a warning), and a file writable by group or others is skipped with a
  warning (for the root file the call returns `Ok(())` with nothing
  loaded). 0.6.0 followed symlinks and loaded such files.
- Android: `set` accepts V2 names and `ro.` values up to 65535 bytes, the limit
  AOSP init enforces (bionic `__system_property_set` sends such `ro.`
  values; init rejects lengths above 0xffff). The client rejected names
  over 1024 and values over 8192 bytes, the limits of this crate's
  service, which still apply on Linux.
- Android: `set` waits for init's V2 reply without a timeout, like bionic
  `__system_property_set`. It returned `TimedOut` after 2 s although init
  applies a fully received request whether or not the client still
  waits, and a reply later than 2 s is normal while init serves another
  connection or writes a `persist.` property. Linux keeps the 2 s bound.
- `get`, `get_with_result` and `read_with` return `Error::NotFound` for a
  name with an empty segment (`""`, `"foo."`, `".foo"`, `"a..b"`), and
  `SystemProperties::find` returns `Ok(None)`, like bionic
  `__system_property_find`. They returned `Error::Parse` and logged two
  `error` lines per lookup, also from `get_or`.
- `SystemProperties::update`, `serial` and `wait` reject a `PropertyIndex`
  returned by a different `SystemProperties` instance (`update`:
  `Error::InvalidArgument`; `serial`/`wait`: `None`), including another
  instance on the same directory such as the global reader. An index from
  another directory's instance, or from the instance before a writer
  restart, addressed an unrelated layout, and `update` could overwrite
  trie bytes of the area without an error.
- `SystemProperties::wait` with an index compares the property's serial
  with `old_serial` before it applies the timeout, like the global-serial
  path and bionic `__system_property_wait` (the kernel's `FUTEX_WAIT`
  returns `EAGAIN` for a changed word with a zero timeout and `EINVAL`
  for an out-of-range one, and bionic reloads the serial after both).
  With a zero timeout, or a negative or out-of-range one (`tv_nsec` ≥
  10^9), it returned `None` without comparing, also in 0.6.0, so a
  zero-timeout poll never reported a change. With an unchanged serial, a
  negative or out-of-range timeout still returns `None` after that one
  comparison, without waiting; bionic retries the `EINVAL` in a loop of
  `futex` syscalls until the serial changes.
- `examples/setprop` prints nothing on success, like AOSP `setprop` (it
  printed the name and value), and `examples/setprop`/`getprop` ignore
  errors writing their output: with stdout connected to a pipe whose
  reader had exited, setprop panicked with exit code 101 after a
  successful `set`. `examples/getprop` prints an empty line for a missing
  or empty property without a default, like toybox `getprop` (it printed
  nothing). `examples/setprop` no longer applies its own length limits
  (256-byte names, 8192-byte values); `set` checks them.

Service (`rsproperties-service`, git dependency only):

- `property_info` is published under the properties dir's writer lock
  (`AreaWriterLock::publish_property_info`) by writing a temp file and
  renaming it over the old one, with mode 0444 regardless of umask.
  Rewriting the existing inode in place could SIGBUS readers that still
  had it mapped, and under umask 0002 the service rejected its own
  group-writable file at startup.
- A build.prop entry the area rejects (e.g. a non-`ro.` value of
  `PROP_VALUE_MAX` bytes or more) is logged and skipped, like AOSP init,
  instead of failing service startup.
- The startup sweep of stale temp sockets only removes this service's own
  `.property_service*.tmp-<pid>` sockets, not every dot-file with
  `.tmp-` in a shared socket dir.
- Stale `.property_info.tmp-<pid>` files from crashed runs under other
  pids are removed once the writer lock is held; only the own pid's temp
  was removed before.
- Client-supplied property names are `Debug`-escaped in service logs.
- A second service instance started on the same properties dir fails on
  the writer lock before it starts either actor, so it no longer replaces
  `property_info` or renames its sockets over the running instance's
  sockets (which were unlinked again when the second instance stopped).
- `run` no longer replaces `property_info` when loading a build.prop or
  `property_contexts` file fails; all input files are read before
  `property_info` or any area file is written.
- Each socket's mode is set on the socket fd before `bind`, so the socket
  file exists only with mode 0660 minus the umask's bits; when the umask
  cleared any of those bits (e.g. group write under umask 022), it is
  raised to exactly 0660 through an `O_PATH|O_NOFOLLOW` descriptor (via
  `/proc/thread-self/fd`, or `/proc/self/fd` on kernels before 3.17,
  which lack `/proc/thread-self`). Only then does the socket listen, so a
  `connect` before that fails with `ECONNREFUSED`, and only after both
  sockets listen are they renamed into place. 0.6.0 bound and listened on
  the temp socket with a umask-derived mode and chmod'ed it afterwards;
  under a umask without the other-write bit (e.g. 0000) another local user
  could connect in between and keep that connection.
- Both sockets are bound under temp names before either is renamed, and a
  failure after the rename (registering the listeners, a panic, a
  cancelled start) unlinks the final names again, so a failed start no
  longer leaves `property_service` pointing at a closed listener.
- The socket dir is opened once at startup and every unlink, rename and
  chmod of socket files is made relative to that descriptor, so a socket
  dir path swapped during startup cannot make the service unlink or
  change files in another directory.
- A missing socket dir, and any missing parent directory, is created with
  mode 0755 minus the umask's bits; 0.6.0 used 0777 minus the umask's
  bits, which left them world-writable under umask 0000.
- Status replies are sent with `send(MSG_NOSIGNAL)`, so a client that
  closes before reading the reply cannot kill a service built with Rust
  1.77–1.86 in a host process that leaves `SIGPIPE` at its default.
- A V2 frame that ends inside the name-length field gets a `PROP_ERROR`
  reply like other malformed frames.
- Socket and `property_info` errors name the path they concern.
- When the socket service fails to start, `run` returns only after the
  properties service has stopped. It returned while that service could
  still be writing `property_info` and the area files (also in 0.6.0).
  Likewise, when the properties service fails to start, `run` returns
  only after the socket service has stopped and unlinked both socket
  names; it returned while they could still accept connections, and a
  retry on the same socket dir could have its new sockets unlinked by the
  previous attempt (also in 0.6.0).
- `examples/example_service` no longer deletes the `--properties-dir` /
  `--socket-dir` directories at startup; files other than
  `property_info`, the area files, `properties_serial` and the sockets
  (which are recreated) and the writer and socket temp files a crashed
  run left (which are removed) are left in place.

### Documentation

- The 0.6.0 notes are amended: the missing `From<ParseIntError>`
  removal is added to Changed, the Migration section gains the omitted
  breaking changes, an error-variant mapping table, newly rejected
  inputs, and changed protocol defaults, and the two-token
  `property_contexts` parity claim is marked as wrong.
- The `SystemProperties::wait` rustdoc states that with an index the
  returned serial can carry the dirty bit while a writer is mid-update,
  as in 0.6.0 and bionic `__system_property_wait`; `serial` waits (up to
  200 ms) for the clean serial. The `serial` rustdoc lists the cases that
  return a dirty serial: the 200 ms bound expiring, a failed futex wait
  (returned at once), and macOS (no wait).
- README fixes:
  - `wait` argument list, boolean parsing, `PropertyConfig`
    construction, `init`/`try_init` error contract, error-variant list,
    name-length and leading-character rules, and service backpressure /
    socket-binding descriptions.
  - `try_init` failure condition, the builder examples now
    create `property_info` before the areas (and `init` the directory
    before reading), the
    service "Hardening highlights" heading no longer dates 0.6.0 behavior
    to 0.4.0, and the docs.rs badge of the unpublished service crate is
    removed.
  - the service examples' `main` returns
    `Box<dyn Error + Send + Sync>` (the snippets did not compile), the
    builder examples state the root-ownership requirement of release
    builds, `try_init` also conflicts after `set()` latched the socket dir,
    `ro.*` is write-once on Linux too, and the service README states its
    access model (socket mode only, no per-property authorization) and
    that `set` blocks the calling thread.
  - service users take `rsproperties` from the same git
    source as `rsproperties-service` (a crates.io copy is a separate crate
    whose `PropertyConfig` `run` rejects) and add `tokio`; the service
    examples state which directories `run` creates. The
    `load_properties_from_file` import-expansion rustdoc states how its
    expansion source differs from AOSP.
  - supported platforms (client: Linux and Android; service
    and `builder`: Linux; macOS unsupported; the `rsproperties` package
    description no longer claims cross-platform support); boolean properties use
    `0`/`1` or `true`/`false` depending on the property; the
    `SystemProperties::find` entry lists `Ok(None)` for an empty-segment
    name; the builder examples publish `property_info` with
    `AreaWriterLock` and state the rename-only contract for that file; the
    service README drops the `builder` feature snippet, which changed
    nothing (the service always enables `rsproperties/builder`); the root
    README points at `rsproperties/examples/` and
    `rsproperties-service/examples/` and builds only `rsproperties` for
    Android.
- The 0.6.0 Migration section also lists the client-side property-name
  rule check in `set` (new in 0.6.0), and the 0.6.0 notes on
  `PROPERTY_SERVICE_VERSION` now state that a non-UTF-8 value still
  selected V2 in that release.
- The 0.6.0 notes are corrected: the variant `try_init` returned for an
  "already initialized" conflict in 0.5.0 was `FileValidation`, not
  `PermissionDenied` (Changed, Migration bullet, and error-variant
  table). The table's "already mapped" condition is dropped: 0.5.0
  returned `Ok` there, and the public API reaches it only on freshly
  created context nodes, which are never mapped yet.
- The 0.6.0 Service note claiming the sockets were "never connectable
  with umask-derived permissions" is marked as corrected (see the
  socket-mode fix under Fixed).
- Docs: the `set` rustdoc states that V1 cannot report a server-side
  rejection (`Ok(())`); the service README and `SOCKET_FILE_MODE` doc no
  longer claim AOSP's socket modes (AOSP creates `property_service`
  0666) and state that a peer can also set an `ro.` property that has
  no value yet.
- Rustdoc: `get`/`get_or` no longer state that Android stores all
  booleans as `0`/`1` (`ro.debuggable` does, `ro.apex.updatable` uses
  `true`/`false`); `get_or_else` lists every case in which the default is
  used, including a non-UTF-8 value and other read errors; `try_init`
  names `set()`/`socket_dir()` as the implicit latch of the socket
  directory; `wire::validate_value_len` names `SystemProperties::add` (not
  `update`) as a caller; `find`/`update`/`serial`/`wait` state that an
  index is valid only on the instance that returned it.
- The `set` rustdoc states that a `TimedOut` while waiting for the
  service's reply does not mean the property was not set (the service
  applies a fully received request), while a `TimedOut` while connecting
  or sending means the request was not delivered.
- The `rsproperties_service::run` rustdoc states the startup order and
  what happens to a `set` that reaches the socket before the property
  areas exist (the socket service starts together with the properties
  service, as in 0.6.0): it waits and is applied once the areas exist,
  after the build.prop entries. The socket service closes the connection
  without a reply after 10 s (up to 20 s when all 64 handler slots are
  taken); a request already queued then is still applied once the areas
  exist, like one whose client gave up on its own timeout, and one not yet
  queued (mailbox full) is dropped. A `get` in that window that is the
  process's first fixes its reader for the process lifetime, so it fails
  for good if `property_info` or `properties_serial` does not exist yet.
  If the properties
  service fails to start, the request is not applied; a SETPROP2 client
  whose connection was already accepted gets an error reply, and one that
  connected after the socket service's last accept sees the connection
  reset or refused.
- The service docs state the socket-dir contract: the service changes
  only its own entries directly in the socket dir, relative to a
  descriptor opened at startup, and never follows symlinks there (only
  `bind` takes a path: if the socket dir's path is swapped during
  startup, the temp socket is created at the new location and left
  there); any
  other uid that can write the socket dir or one of its ancestors can
  impersonate the service, which is out of scope; default ACLs are not
  managed; one instance per socket dir. The `SocketService` /
  `socket_service::run` rustdoc states that the socket service must be
  spawned only while the properties dir's writer lock is held.
- The service README states that a `set` returning `TimedOut` has an
  unknown outcome: like AOSP init, the service applies every request it
  received in full, also when the client already gave up.
- The `serial`, `wait`, `read_with` and `get` rustdoc no longer state
  that a same-process `builder` writer contends on the reader's lock (the
  context-node lock belongs to one instance, and that instance's own
  writes take `&mut self`), and the 0.6.0 Waits notes say so. The `serial`
  rustdoc no longer calls its clean-serial wait bionic parity: current
  bionic `__system_property_serial` returns the raw serial.

### Migration from 0.6.0

- **Newly rejected inputs** (succeeded in 0.6.0, now `Err` or skipped):
  - an `import` path containing a space: `import <path> <filter>` is now
    split at the first space after the path (AOSP parity), so
    `import /a/my props/b.prop` opens `/a/my` with filter `props/b.prop`
    and the import is skipped with a warning (0.6.0 loaded the file).
    Rename such files or directories;
  - in an `import` path, `${name:-}` for a missing/empty property and a
    zero-length name with a default (`${:-d}`) now make the import be
    skipped with a warning (AOSP `ExpandProps`; 0.6.0 expanded them to an
    empty string and to `d`);
  - `PropertyInfoEntry::parse_from_file(path, true)` reports two-token
    `<property> <context>` lines (no match operation) in the error `Vec`
    instead of returning them as prefix entries, so a trie built from the
    returned entries classifies those names under the default context.
    Pass `false`, or add `prefix` to the lines;
  - with `PROPERTY_SERVICE_VERSION` set to a non-UTF-8 value, `set`
    rejects names of 32 bytes or more and values of 92 bytes or more
    (`InvalidArgument`), because that environment now selects V1 (see
    Changed defaults);
  - `load_properties_from_file` skips a line whose key fails
    `wire::validate_property_name` (spaces, `*`, `/`, leading/trailing or
    doubled `.`), logging a warning; 0.6.0 put the key in the map, and the
    service stored it in the area. Rename such keys in prop files;
  - service: when the umask clears any bit of 0660 (e.g. 022 clears group
    write), raising the sockets to 0660 needs `/proc` (`/proc/thread-self`,
    or `/proc/self` on kernels before 3.17); without it `run` fails with
    an error naming `/proc/thread-self/fd` and `/proc/self/fd`. Mount
    `/proc`, or run with a umask that keeps owner and group read/write
    (002 or 007);
  - `load_properties_from_file` keeps non-ASCII whitespace (e.g. U+00A0,
    U+3000) at the edges of keys, values, `import` paths and filters: a
    value keeps it (stored bytes differ from 0.6.0), and a key that starts
    or ends with it fails `wire::validate_property_name`, so the line is
    skipped with a warning. Remove such characters from prop files;
  - a `property_info` context name starting with `.` is skipped with a
    warning (properties of that context fail with `FileValidation`).
    Rename such contexts;
  - `load_properties_from_file` skips a prop file whose mode has the
    group- or other-write bit (with a warning; for the root file the call
    returns `Ok(())` with nothing loaded), and fails to open a prop file
    whose last path component is a symlink (`Error::Context` for the root
    file; such an `import` is skipped). Files created under umask 002
    (`File::create`, `std::fs::write`) are group-writable: create them
    with an explicit mode such as 0644 or chmod them, and pass the
    symlink's target instead of the link;
  - with the `strict-file-validation` feature, a `property_info` with the
    owner-write bit set (e.g. 0644 from `std::fs::write` or
    `File::create` under umask 022) is refused with
    `Error::PermissionDenied`; without the feature it is still accepted,
    with a `warn` log line. Publish it with
    `AreaWriterLock::publish_property_info` (mode 0444, renamed into
    place);
  - a properties directory whose `property_info` is a symlink fails to
    load: `SystemProperties::new_area` and the global reader return an
    error whose source chain ends in `Error::Io` (`ELOOP`). Replace the
    link with the file (e.g. `AreaWriterLock::publish_property_info`), or
    point the configuration at the directory that holds the real file; a
    symlinked *directory* still works.
- **Newly accepted inputs** (failed in 0.6.0, now succeed):
  - `import <path> <filter>` now loads the keys the filter selects, and
    those keys may override values set earlier in the importing file
    (0.6.0 failed to open the path and skipped the line);
  - an `import` path ending in a lone `$` loads the path without the `$`
    (AOSP `ExpandProps`; 0.6.0 skipped the import);
  - a non-`ro.` property whose serial has bit 16 set (init's counter sets
    it during updates 32768–65535 of every 65536) can be read: `get` /
    `read_with` return its value
    (0.6.0: `Error::FileValidation`, or — if the property once held a
    value of 57 bytes or more — possibly an empty or unrelated value),
    and `SystemProperties::update`
    writes it (0.6.0: `Error::InvalidArgument`, "in-place update of long
    property is not supported");
  - service: a build.prop entry the area rejects (non-`ro.` value of
    `PROP_VALUE_MAX` bytes or more, invalid name, full area) no longer
    fails `rsproperties_service::run`; it is logged at `error` and
    skipped, and once a context's area is full, later entries for that
    context that no longer fit are skipped too.
    Check the logs, or `get` the expected keys, if startup failure was
    relied on to catch bad build.prop files;
  - Android: `set` with a name of 1025–65535 bytes or an `ro.` value of
    8193–65535 bytes is sent to init (0.6.0: `InvalidArgument` without
    connecting);
  - service: `run` creates a missing properties dir (the last component
    only — its parent must exist — with mode 0711 minus the umask's bits;
    under umask 077 that is 0700, which other users' readers cannot
    enter) instead of failing to write `property_info`;
  - service: a temp socket path (`<socket_dir>/.<name>.tmp-<pid>`) of
    exactly 108 bytes binds; 0.6.0's std-based bind accepted at most 107
    bytes.
- **Changed defaults**:
  - a set but non-UTF-8 `PROPERTY_SERVICE_VERSION` selects V1 (0.6.0:
    V2): the client sends V1 frames in that environment. Set the
    variable to a number to pin the protocol;
  - service: `property_info` is published with mode 0444 (read-only for
    owner, group and others) via temp file + rename, so every start creates a new
    inode (0.6.0: `File::create`, umask-derived mode, inode reused).
    Tools that rewrite it in place must replace it (write elsewhere,
    then rename) instead of opening it for write, and tools that track
    it by inode or hardlink must re-resolve the path after a restart;
  - builder: `SystemProperties::new_area` creates each area file as
    `.area.tmp-<pid>-<n>` in the properties dir and renames it into place
    (0.6.0: unlinked the old file and created the new one at its final
    path). Tools that watch or list the dir see a rename and, briefly, the
    dot-file; ignore dot-files when listing. A reader that opened an area
    before that area's rename, or `properties_serial` before its rename,
    keeps the previous file. During a restart the writer (`rsproperties_service::run`,
    or `publish_property_info` followed by `new_area_locked`) publishes
    `property_info` first, renames the area files into place one by one,
    and renames `properties_serial` last, so a reader that starts in
    between maps the new `property_info` with the previous
    `properties_serial`, and the previous file of every context it opens
    before that context's rename; a read of a context whose area file
    does not exist yet fails, and the next access opens it again. Such a
    reader does not see the restarted writer's updates in the previous
    files, and `wait` on the global serial does not wake. Start or restart
    readers after the writer's `new_area` / `new_area_locked` (or
    `rsproperties_service::run`) has returned;
  - builder: area files and `properties_serial` get mode 0444 whatever
    the umask (0.6.0: 0444 minus the umask's bits);
  - service: a missing socket dir, and any missing parent directory, is
    created 0755 minus the umask's bits (0.6.0: 0777 minus the umask's
    bits);
  - service: when either service fails to start,
    `rsproperties_service::run` returns only after both actors have
    ended (0.6.0: right after requesting both stops). After a socket
    service failure it waits for the properties service to finish its
    startup (input files, `property_info`, areas), so the `Err` can arrive
    as late as a successful start would; after a properties service
    failure it waits for the socket service to stop and unlink both
    socket names;
  - builder: once one property has been updated 32768 times,
    `SystemProperties::update` publishes serials whose bits 17..23 keep
    counting (0.6.0: the counter wrapped to 0), so serial values from then
    on differ from 0.6.0's. Code that only compares serials for equality
    needs no change;
  - `SystemProperties::wait(Some(index), Some(old), Some(t))` returns
    `Some(serial)` when the serial already differs from `old`, also for
    a zero, negative or out-of-range `t` (0.6.0: `None` for those
    timeouts). With an unchanged serial these timeouts still return
    `None` without waiting. Any `wait` with an index and such a timeout
    (also with `old_serial` `None`) now takes
    the context node's read lock (0.6.0 returned before taking it), so it
    can wait for that lock while another thread of the same instance is
    opening that area for the first time;
  - supported platforms: the `rsproperties` client is supported on Linux
    and Android, the `builder` feature on Linux hosts only (it still
    compiles for Android, where init creates the areas), and macOS is not
    supported. 0.6.0 stated Android and Linux support (its README: "for
    Linux and Android", "## Platform Support" with Android and Linux) but
    not whether macOS is supported (its package description said
    "cross-platform support" and its notes list macOS fixes, but its CI
    did not build macOS); the crate still compiles for
    macOS, and CI neither builds nor tests it. `rsproperties-service`
    compiles only for Linux;
  - examples: `examples/setprop` prints nothing on stdout on success
    (0.6.0: `Property '<name>' set to '<value>'`) and passes names over
    256 bytes and values over 8192 bytes to `set` instead of rejecting
    them itself; `examples/getprop` prints an empty line for a missing or
    empty property without a default (0.6.0: nothing);
    `examples/example_service` keeps other files in its directories
    (0.6.0 deleted the directories at startup; property values still do
    not carry over to a rerun: `property_info`, the area files,
    `properties_serial` and the sockets are recreated; stale writer and
    socket temp files are removed), and creates missing
    directories, parents included, with mode 0711 (properties dir) /
    0755 (socket dir) minus the umask's bits (0.6.0: 0777 minus the
    umask's bits). Scripts that parse the output or rely on a clean
    directory must change;
  - Android: `set` no longer returns `TimedOut` while waiting for init's
    reply; it blocks until init replies or closes the connection, like
    bionic. Code that relied on the 2 s bound must apply its own (e.g.
    call `set` on another thread). Connect and send keep the 2 s bound;
  - service: code that spawns `PropertiesService` itself
    (`properties_service::run`, or `rsactor::spawn` with
    `PropertiesServiceArgs::new`) still compiles; the actor now takes the
    properties dir's writer lock after reading its input files and before
    writing `property_info`. To keep a second instance from touching a
    running instance's sockets, take the lock first
    (`AreaWriterLock::acquire`), pass it with
    `PropertiesServiceArgs::with_writer_lock`, and spawn the socket
    service only after that, as `run` does.
- **Changed API**:
  - `rsproperties_service::run` returns `Result<_, StartupError>` (see
    Changed error values);
  - `SocketService` has no `Drop` impl; dropping it still unlinks both
    sockets;
  - `PropertyIndex` is 16 bytes (was 8), and only the `SystemProperties`
    instance that returned it accepts it (see Changed error values).
- **Changed error values**:
  - `rsproperties_service::run` returns `StartupError` (0.6.0:
    `Box<dyn Error + Send + Sync>` holding a `Failed to start … service:`
    message). `stage()` names the failed step; the cause is `source()` —
    `std::io::Error` from the properties service, `rsproperties::Error`
    from `try_init`, the writer lock, or the socket service, a
    `tokio::task::JoinError` when an actor task panicked, or a message
    when the actor stopped without an error. Code that called
    `downcast_ref` on the returned error must call it on `source()`; code
    that matched the `Failed to start` prefix stops matching. A second
    instance on the same properties dir fails with
    `StartupStage::WriterLock` and `rsproperties::Error::Lock`.
  - service: socket errors are `rsproperties::Error::Context` naming the
    path, wrapping the OS error (0.6.0: `rsproperties::Error::Io` whose
    `raw_os_error()` was the errno); walk `source()` to reach the
    `io::Error` or `Errno`. `property_info` publish errors are an
    `std::io::Error` wrapping such a `Context`.
  - Linux/Android: `set` on a socket the service already closed returns
    `Error::Io` with kind `BrokenPipe`; in 0.6.0 a host with `SIGPIPE`
    at its default was killed by the signal instead (Rust binaries
    already received this error).
  - `Error::NotFound` for a name missing from its area (from
    `get_with_result` / `read_with` / `get`) is the full property name;
    in 0.6.0 a miss inside an existing node's children carried only the
    last segment. Code that compared the payload with that segment must
    compare with the full name.
  - `Error::NotFound` for a name that maps to no context (from `get`,
    `get_with_result`, `read_with`, `SystemProperties::add` and `set`)
    reads `no context for property "<name>"`, the name `Debug`-escaped
    (0.6.0: `no context for property <name>`). The `FileValidation`
    message for a name whose context was skipped at init, and the
    `PermissionDenied` message of `SystemProperties::update` (and `set`)
    for an existing `ro.` name, quote the name the same way (0.6.0:
    unquoted). Code that compared or searched these payloads for the raw
    name must compare with the escaped form or match the variant only.
  - The other error payloads that embed a property name, line, token or
    type string print it `Debug`-escaped (`"…"`; 0.6.0: raw or in `'…'`):
    the `InvalidArgument` of `wire::validate_property_name`, which
    `rsproperties::set` returns for an illegal name; the empty-segment
    `Parse` of `SystemProperties::add` (and of `set` on such a name); the
    `Parse` and `FileValidation` of `build_trie` (empty segment, no name
    parts, duplicate exact or prefix entry, string-table invariant); the
    invalid-type `InvalidArgument` of `PropertyInfoEntry::new`; and the
    per-line `Parse` errors of `PropertyInfoEntry::parse_from_file`
    (missing property or context entry, invalid match operation or type).
    The `Parse` of `get::<T>` reads `Failed to parse property "<name>"
    (<N bytes>): …` (0.6.0: `Failed to parse '<value>' for property
    '<name>': …`). Match the variant instead of the text.
  - `SystemProperties::new_area`: the `Error::Context` messages for
    creating and sizing an area file or `properties_serial` name the temp
    file `.area.tmp-<pid>-<n>` (0.6.0: the final path), and the new mode
    and rename steps fail with their own `Context` (`Failed to set mode of
    …`, `Failed to rename … to …`).
  - `SystemProperties::new_area` on a dir whose existing
    `property_info` cannot be loaded returns the writer-lock error when
    the lock cannot be taken (0.6.0: the load error): `Error::Lock` when
    another instance holds it, `Error::Context` when `.writer_lock`
    cannot be created or opened or its mode cannot be reset (for example
    `EACCES` in another user's dir, `ELOOP` for a symlink at
    `.writer_lock`). A missing `property_info` still returns the load
    error and creates neither the dir nor `.writer_lock`; one that exists
    but fails to load after the lock is taken leaves `.writer_lock` in
    the dir (0.6.0 did not create it) and has removed leftover writer
    temp files (`.area.tmp-*`, `.property_info.tmp-*`). This includes a
    dangling symlink at `property_info`, which `lstat` finds but the
    `O_NOFOLLOW` open refuses.
  - Log lines that print a property name, prop-file key,
    `property_contexts` line or default context/type print it
    `Debug`-escaped, and no log line prints a property value any more
    (the `debug` line of `SystemProperties::add` gives the length, the
    override `warn` of `load_properties_from_file` gives the key only).
    Log filters written against the 0.6.0 text must change.
  - Empty names in `property_info` (see Fixed): the `warn` lines
    `Exact match entry <i> has an empty name` and `child node <i> has
    empty (corruption-fallback) name` are gone. A child search logs
    `child node <i> has an empty name but is not child 0 (children out
    of order)` only when it finds an empty-named child at an index other
    than 0, which AOSP's sorted output never has, and then misses as
    before. `Prefix entry <i> has an empty name` is unchanged. A name
    with an empty segment (`a..b`) now selects the context of the
    empty-named child, and a name ending in `.` (`a.`) the context of the
    empty exact entry, like AOSP (0.6.0: the context of the parent
    node). Once that context's area is open, the result does not depend
    on which context was selected: `get` returns `Error::NotFound` and
    `find` `Ok(None)` for such names (0.6.0: `Error::Parse` and
    `Err(Error::Parse)`; see the empty-segment entry below). Only the
    context consulted first can differ: if it was skipped at init (e.g.
    a context name starting with `.`, see Fixed), `get`, `find`, `set` and
    `add` return `Error::FileValidation` (`add` checks the value length
    first); if its area fails to open, that open error is returned.
  - `SystemProperties::new_area` no longer logs `Removed stale property
    area file: <path>` (`debug`) or `Failed to remove stale property area
    file <path>: …` (`warn`): the rename replaces the previous file, and
    nothing is unlinked first. `Failed to set SELinux context for <path>`
    still names the area's final path.
  - `get` / `get_with_result` / `read_with` on a name with an empty
    segment return `Error::NotFound` (0.6.0: `Error::Parse`), and
    `SystemProperties::find` returns `Ok(None)` (0.6.0: `Err(Error::Parse)`).
    `SystemProperties::set` on such a name now goes to `add`, which returns
    `InvalidArgument` when the value fails `wire::validate_value_len` and
    `Parse` otherwise (0.6.0: `Parse` from `find`). Code that matched
    `Parse` to detect malformed names must validate with
    `wire::validate_property_name` instead. `SystemProperties::add` still
    returns `Parse` for such names. If the context the name selects was
    skipped at init, these calls return `Error::FileValidation` instead
    (`add` only after its value-length check; see the empty-names entry
    above).
  - `SystemProperties::update` with a `PropertyIndex` from another
    instance returns `Error::InvalidArgument`, and `serial` / `wait`
    return `None`. This includes an index from another instance on the
    same directory — e.g. `rsproperties::system_properties().find(..)`
    passed to a writer created with `new_area` — which worked in 0.6.0
    because both mapped the same files. Call `find` on the instance the
    index is passed to.
  - service: a V2 frame that ends inside the name-length field gets the
    4-byte `PROP_ERROR` reply before the connection is closed (0.6.0
    closed it without a reply). A client that reads the reply after such
    a frame gets `PROP_ERROR` instead of end-of-file.
  - service: a failed start unlinks the socket names it created. A client
    that connects after binding `property_service_for_system` failed gets
    `ENOENT` (`Error::Io` of kind `NotFound`) unless an earlier run left
    `property_service` behind; 0.6.0 left this start's `property_service`
    without a listener, and the client got `ECONNREFUSED`.

## [0.6.0] - 2026-07-18

Consolidated correctness and hardening release from four successive
full-source reviews, spanning the seqlock protocol, the mmap access
model, untrusted-input validation, socket I/O, platform contracts
(macOS), AOSP parity, and the error contract. (Internal iterations were
numbered 0.6.0–0.8.0 but never published; this entry covers everything
since 0.5.0.)

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
  from turning retry loops into busy-spins. **Correction:** the lock is
  per `SystemProperties` instance and a builder writer is a separate
  instance, so no same-process writer waited on a reader's lock (this
  instance's own writes take `&mut self`). Slicing bounds how long a
  `wait` holds the lock while another thread of the same instance opens
  that area for the first time; see Unreleased.
- `SystemProperties::serial` waits for a mid-update dirty serial to
  clear (bionic parity — one logical update no longer observable as two
  transitions), bounded to 200ms in case the writer crashed mid-update
  (bionic hangs forever there). **Correction:** current bionic
  `__system_property_serial` returns the raw serial without waiting, so
  this wait is not bionic parity and bionic does not hang there.
- `SystemProperties::wait` no longer panics on huge `tv_sec` timeouts
  (`Instant` overflow degrades to an infinite wait, matching bionic).
- **macOS `wait` stale-serial fast path**: the documented "already
  changed → return immediately" contract of `old_serial` now holds on
  macOS too (it is a plain atomic load, no futex needed); previously the
  macOS paths returned `None` unconditionally.

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
- Non-ASCII context names are rejected: APFS also aliases Unicode case
  and NFC/NFD normalization variants onto one file, which the
  ASCII-case-fold duplicate check could not catch.
- Serializer arena offsets are bounded to `u32` **at allocation time**;
  the old "final `try_from` retroactively validates every intermediate
  `as u32`" argument is gone, along with the casts it excused. A
  compile-time guard also rejects arena types with alignment > 4, whose
  runtime pointer-based check was allocator-dependent.

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
- `PROPERTY_SERVICE_VERSION` env values that are set but unparseable now
  select **V1** with a warning (same policy as the
  `ro.property_service.version` property) instead of silently defaulting
  to V2. (A non-UTF-8 value still selected V2 in 0.6.0; fixed under
  Unreleased.)
- **macOS `SO_NOSIGPIPE`**: the rustix-created client socket now sets
  `SO_NOSIGPIPE` like std does on Apple platforms — writing to a
  peer-closed property-service socket delivers `EPIPE` instead of killing
  hosts that don't ignore `SIGPIPE` (e.g. a cdylib embedded in a C
  process).
- The send loop no longer propagates macOS's spurious
  `set_write_timeout` `EINVAL` (peer FIN processed) — the next write
  surfaces the real `EPIPE`/`WriteZero` instead, mirroring the receive
  path.

Service:

- Service sockets are bound via bind-to-temp → chmod → rename, so they
  are never connectable with umask-derived permissions (the previous
  bind-then-chmod window let early connections survive the chmod).
  **Correction:** the temp socket was bound directly in the socket dir,
  so under a umask without the other-write bit it was connectable
  before the chmod; fixed under Unreleased.
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
- `.writer_lock` permissions are re-asserted with `fchmod` on open — a
  leftover lock file with wider modes (from earlier versions) no longer
  defeats the anti-squat rationale of mode 0600.

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
- **AOSP parity — import path expansion**: `expand_import_path` now
  supports `$$` escapes and `${name:-default}` fallbacks (also taken for
  *empty* values), and rejects a bare `$x`, matching `ExpandProps`.
- **AOSP parity — legacy two-token property_contexts lines**: a *missing*
  match operation is accepted (defaults to prefix match) even with
  `require_prefix_or_exact`, matching `ParsePropertyInfoLine`; only a
  present-but-invalid token is an error. **Correction:** this parity
  claim was wrong — AOSP rejects a missing operation when
  `require_prefix_or_exact` is set; reverted under Unreleased.
- Prop-file line length is bounded (64 KiB, skip-with-warn): a crafted
  newline-less file can no longer grow a single read buffer without
  bound.

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
- **Corruption no longer masquerades as absence**: a context slot that
  was corrupt at init is reported as `Error::FileValidation` instead of
  `NotFound`, so `SystemProperties::find` propagates it as an error
  rather than folding it into `Ok(None)`.
- `PropertyArea::add` validates empty name segments *before* walking the
  trie, so an invalid name (`"a."`, `"a..b"`) no longer leaves
  freshly-allocated (unreferenced-but-reusable) trie nodes behind.
- V1 name-length documentation in `wire` was off by one (`<`, not `<=` —
  the fixed buffer needs its NUL terminator).

### Changed

- **Breaking:** `Error` is `#[non_exhaustive]`; `LockError` renamed to
  `Lock`; `From<OsString>` and the never-constructed `Conversion`
  variant removed; `Utf8Error` now converts to a source-preserving
  `Utf8` variant instead of stringifying into `Encoding`, while `Parse`
  remains the single parse-failure variant (`FromStr` causes are
  flattened into its message). New variants: `InvalidArgument` (API misuse,
  previously misfiled under `FileValidation`), `AlreadyInitialized`
  (first-write-wins conflicts, previously misfiled under
  `FileValidation`), `AreaFull` (operational area exhaustion,
  previously misreported as corruption), `LimitExceeded` (global
  resource budgets), `ServiceError { name, code }` (protocol-level
  rejection, previously a fabricated `Error::Io`), and
  `Init(Arc<Error>)` (cached initialization failures preserve the
  original variant and `source()` chain).
- **Breaking:** `impl From<std::num::ParseIntError> for Error` is
  removed, so `s.parse::<u32>()?` in a function returning
  `rsproperties::Result` no longer compiles (E0277). *(Omitted from the
  notes as released; added afterwards.)*
- **Breaking:** `wire::validate_property_name` /
  `wire::validate_value_len` return the crate's typed
  `Result<()>` instead of `Result<(), String>`.
- **Breaking:** `PropertyConfig` is now `#[non_exhaustive]`: construct
  via the builder, `with_*` constructors, or `From` impls.
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
- `wire::validate_value_len` no longer honors an empty-name sentinel to
  disable the `ro.` long-value exemption; the in-place update path uses
  a dedicated internal `validate_short_value_len`.
- The root-ownership file check can be enforced in
  `debug-assertions`-enabled builds via the new opt-in
  `strict-file-validation` feature (profiles enabling debug-assertions
  for overflow checks no longer silently relax validation with no
  recourse).
- `get`/`get_or`/`get_or_else` document that the `FromStr` parse runs
  under the property area's read lock (must not block or re-enter the
  property API); crate-level doc example is no longer compiled out on
  host (`cfg(android)` removed), and misleading `set("ro.…")` examples
  replaced.
- `MemoryMap`'s `Debug` no longer prints the raw mapping pointer (ASLR
  address), matching the module's no-address-in-logs policy.

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
  `From<OsString>`. Code that matched `FileValidation` to detect
  "already initialized" `try_init` conflicts should match
  `AlreadyInitialized`.
- `PropertyConfig` is `#[non_exhaustive]`: replace struct-literal
  construction with the builder, `with_*` constructors, or `From`
  impls.
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

*Added after the 0.6.0 release (omitted from the notes as published):*

- `From<ParseIntError>` is removed: replace `?` on `parse::<int>()` with
  `.map_err(|e| Error::Parse(e.to_string()))?`.
- External `impl ContextWithLocation for ...` must also implement the
  new required method `with_context_location`.
- `rsproperties_service::run` (git dependency only) returns
  `Box<dyn Error + Send + Sync>` instead of `Box<dyn Error>`.
- Error variants that changed for the same condition. These arms still
  compile but silently stop matching — re-target them:

  | Condition | 0.5.0 | 0.6.0 |
  |---|---|---|
  | `Utf8Error` conversion | `Encoding(String)` | `Utf8(Utf8Error)` |
  | client-side name/value length rejection in `set` | `FileValidation` | `InvalidArgument` |
  | illegal property name in `set` (empty, `..`, leading/trailing `.`, chars outside `[A-Za-z0-9_.\-@:]`) | `Io` (V2, server reply) or `Ok` (V1, no ack) | `InvalidArgument` |
  | property service replied with an error code | `Io` | `ServiceError { name, code }` |
  | property area full on `add` | `FileSize` | `AreaFull` |
  | cached `SystemProperties` init failure (`try_system_properties`, `get`) | `FileValidation(String)` | `Init(Arc<Error>)` |
  | `try_init` conflict (already initialized) | `FileValidation` | `AlreadyInitialized` |

- **Newly rejected inputs** (succeeded in 0.5.0, now `Err` or skipped):
  - name or value with an interior NUL in `set`, `add`/`update`, and
    `build_trie` — strip NULs before the call;
  - a property-area path that is not a regular file (e.g. a device
    file, which 0.5.0 mapped) — point the properties dir at real files;
  - `property_info` with an unsupported `minimum_supported_version` or a
    header-size/file-size mismatch, and context names that are reserved
    (`.writer_lock`, `properties_serial`, `property_info`, case-folded),
    duplicate, non-ASCII, or path-shaped — regenerate the file with
    `build_trie`;
  - prop-file lines over 64 KiB are skipped with a warning;
  - a `set` with an illegal property name is rejected client-side
    before connecting (0.5.0 checked only the length: V1 returned `Ok`
    because it waits for no ack, V2 returned `Io` from the server's
    error reply) — validate names with `wire::validate_property_name`.
- **Changed defaults**:
  - wire protocol selection: `ro.property_service.version` now takes
    precedence over the `PROPERTY_SERVICE_VERSION` env var, and a set but
    non-numeric env value selects V1 (0.5.0: V2; a non-UTF-8 value still
    selected V2 in 0.6.0, fixed under Unreleased). The frame format a
    client sends can differ in the same environment; set
    `PROPERTY_SERVICE_VERSION` to a number (or expose the property) to
    pin it.
- **Changed semantics** of `load_properties_from_file`: a file imported
  twice is re-applied (last wins) instead of loaded once, and exceeding
  the total load budget aborts the whole load with `LimitExceeded`.

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
