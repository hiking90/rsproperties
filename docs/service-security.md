# rsproperties-service: files, access control and hardening

The files `rsproperties-service` creates, who may use the service, and
the limits it applies to its clients. The API is documented on
[docs.rs](https://docs.rs/rsproperties-service); the protocol behaviour
is in [service-protocol.md](service-protocol.md).

## Directory layout

```text
properties_dir/
├── .writer_lock           # flock held by the running writer
├── property_info          # property contexts trie (replaced by rename)
├── properties_serial      # global serial
└── u:object_r:*:s0        # one area file per SELinux context

socket_dir/
├── property_service                    # main property socket
└── property_service_for_system         # system property socket
```

The property area files and `property_info` stay in the properties dir
after the service has ended; readers that mapped them keep working and
see no further changes.

## File ownership: a release build runs as root

`rsproperties` checks the metadata of every property file it maps
(`property_info` and the area files). A release build (no
`debug-assertions`), and any build with the `strict-file-validation`
feature of `rsproperties`, requires the file to be owned by uid 0 and
gid 0, as bionic does (`prop_area.cpp`).

The service creates these files as its own user and reads
`property_info` back before it renames it into place. In such a build,
`run` as any other user fails at `StartupStage::PropertiesService` with
`Error::FileOwnership` ("File not owned by root: uid=…, gid=…"), and a
client built the same way refuses the files a non-root service created.
Run the service as uid 0 with gid 0. No option accepts another owner.

That stage's `source()` is a `std::io::Error` that carries the
`rsproperties::Error`. To recognise the ownership failure, take
`source()`, downcast it to `std::io::Error`, take its `get_ref()`,
downcast that to `rsproperties::Error`, and check that its
`root_cause()` is `Error::FileOwnership` (the error may sit inside
`Error::Context`).

A debug build without `strict-file-validation` skips the ownership
check and logs one warning ("root-ownership check on property files is
disabled"), so a non-root service starts there. The check first applies
in the release build.

## Access control

- **Who can connect**: each socket file's owner, group and mode (`0o660`
  by default; see `ServiceOptions::socket_access`) plus the socket dir's
  permissions.
- **What a connected peer may do**: the host's authorizer
  (`ServiceOptions::authorizer`) decides each request, where init's
  `CheckPermissions` asks SELinux. It is called for a set of an ordinary
  property, a `ctl.*` request and a `sys.powerctl` set, after the name
  check and before the type check, with the name, the value, the
  context `property_contexts` gives the name (for `ctl.*`, the context
  of `ctl.<action>$<target>`), the socket the request came in on, and
  the sender's pid, uid and gid from `SO_PEERCRED`. A name that no
  `property_contexts` entry matches has the default context,
  `u:object_r:build_prop:s0`, and is passed to the authorizer with it:
  the service does not refuse such a name, so an authorizer that allows
  that context allows every name without an entry. A denied request is
  answered with `PROP_ERROR_PERMISSION_DENIED`, a denied `ctl.*` request
  with `PROP_ERROR_HANDLE_CONTROL_MESSAGE` and without a call of the
  control handler; both are logged at `warn`. A request is refused in
  the same way, without a call, when the sender's credentials cannot be
  read, when the properties service fails to start (a request that
  arrives before that service has started waits for it), and when the
  name's context cannot be read. A panic in the authorizer is caught:
  the request is refused with the same codes and logged at `error`. A
  request whose name passed the name check and whose value is not a
  string (an interior NUL, not UTF-8) is answered with
  `PROP_ERROR_INVALID_VALUE` before the authorizer is asked, and is not
  logged as a denial; init checks the access first.
  The properties the service sets itself at startup (build.prop files,
  persistent properties) do not go through the authorizer.
- **The credentials are those of `connect()`**: `SO_PEERCRED` records
  the process that connected. A process that was passed the connected
  descriptor sends with the same credentials, and the pid may have been
  reused; the gid is the effective gid only, without supplementary
  groups.
- **`ro.` properties are write-once, not read-only for clients**: the
  first set of a `ro.` name without a value is stored, as in init, where
  sepolicy lets some daemons publish such values. An authorizer that
  must keep clients from doing that denies names that start with `ro.`.
- **Without an authorizer** the socket files and the socket dir are the
  only access control. Any peer that can connect may set any non-`ro.`
  property, and any `ro.` property that has no value yet (the first
  writer fixes it), and send any `ctl.*` control message to the
  configured control handler and any `sys.powerctl` command to the
  configured powerctl handler. A handler that reboots the host must then
  check the sender (`uid`, `gid`, `pid` from `SO_PEERCRED`) or the
  command itself. `property_contexts` maps property names to contexts
  and types; the values are type-checked, the senders are not.
- **Socket dir contract**: the service opens the socket dir once at
  startup and changes only entries directly in it named
  `property_service`, `property_service_for_system`, or
  `.<either name>.tmp-<pid>`, relative to that descriptor, never
  following a symlink at those names. `bind` itself takes a path: if the
  socket dir's path is swapped after startup opened it, the temp socket
  is created at the new location, startup fails, and that socket is left
  there. Any uid other than the service's and root that can write to the
  socket dir or one of its ancestors can replace the sockets with its
  own and so impersonate the service; that is out of scope. So is what
  such a uid can do to a socket's group and mode during startup: between
  `bind` and the `O_PATH` open of the temp name (see
  [Socket permissions](#socket-permissions)) it can put another entry at
  that name. The `fstat` check refuses an entry that is not a socket
  owned by the service's euid, so a socket that uid bound itself fails
  startup. The check does not establish that the entry is the socket
  just bound: another socket owned by the service's euid (the service's
  other temp socket, when the two temp names are exchanged) passes it.
  That socket then gets the group and mode configured for this name, and
  the socket just bound listens with the group it was created with and
  its mode minus the umask's bits; startup reports no error. Keep the
  socket dir and its ancestors writable only by the service's user and
  root. A world-writable socket dir without the sticky bit is reported
  with a warning at startup; this is not an access check. A default ACL
  on the socket dir gives its named users and groups access beyond the
  mode bits; the service does not inspect or remove ACLs.
- **Single instance**: `run` takes the properties dir's writer lock
  before starting either actor, so a second `run` on the same properties
  dir fails (`StartupStage::WriterLock`) without touching
  `property_info` or the sockets. Run one instance per socket dir: two
  instances with different properties dirs must not share a socket dir.

## Hardening

### Socket permissions

Each socket's mode — `0o660`, or the one given to
`ServiceOptions::socket_access` for that socket — is set on the socket
fd before `bind`, so the file exists only with that mode minus the
umask's bits. The temp socket is then opened as an `O_PATH|O_NOFOLLOW`
descriptor and checked with `fstat` to be a socket owned by the
service's euid (this does not show it to be the socket just bound; see
the socket dir contract above). Through that descriptor a configured
group is set (`fchownat` with `AT_EMPTY_PATH`), and if the umask cleared
any bits the mode is raised to exactly the configured mode (needs
`/proc`: `/proc/thread-self/fd`, or `/proc/self/fd` on kernels before
3.17). Only then does the socket `listen` — a `connect` before that
fails with `ECONNREFUSED`. Both sockets are bound as `.<name>.tmp-<pid>`
and renamed onto their final names only after both listen.

`run` refuses the setuid, setgid, sticky and execute bits, and, for a
service without an authorizer, a bit for others: with such a bit, every local
uid that can reach the socket dir could set any property. AOSP init
creates `property_service` 0666, relying on SELinux, and
`property_service_for_system` 0660 with group `system`. A service with an
authorizer can do the same: `SocketAccess::new(0o666)` for
`Listener::Property` and
`SocketAccess::new(0o660).with_group(<system gid>)` for
`Listener::System`.

### Backpressure

`accept()` does not wait for a permit. Each accepted connection must
take a waiting-room permit with `try_acquire` (`MAX_WAITING_CLIENTS` =
256); when the waiting room is full the connection is closed
immediately. The spawned task then waits up to `CLIENT_TIMEOUT` (10 s)
for one of `MAX_CONCURRENT_CLIENTS` (64) handler permits and drops the
connection on timeout. The exchange itself is bounded by
`CLIENT_TIMEOUT` too: on expiry the connection is closed without a
reply, so one connection holds the service for at most 20 s. The accept
loop never parks, and accepted fds are capped at 64 + 256. A `ctl.*`
request gives its handler permit back once it is queued for the control
handler: its connection then belongs to the `ControlReply`, which
`CLIENT_TIMEOUT` no longer bounds, and at most 102 of those exist (see
[Control messages](service-protocol.md#control-messages)), so the
service holds at most 64 + 256 + 102 client fds.

After an `accept()` error the accept loop sleeps 100 ms, so an
`EMFILE`/`ENFILE` storm cannot spin the worker and flood the log.

### A `TimedOut` has an unknown outcome

Like AOSP init, the service applies every request it received in full,
also when the client has already stopped waiting. Outside Android, the
blocking `rsproperties::set` waits at most 2 s for the reply (the async
`rsproperties::asynchronous::set` waits until its caller's timeout, if
any), so a `set` that returns `TimedOut` while waiting for the reply
may have been applied (a `TimedOut` while connecting or sending means
the request was not delivered). Retrying with the same value is
harmless except for `ro.*`, where the retry gets
`rsproperties::Error::SetRejected` (`SetRejection::ReadOnlyProperty`)
if the first attempt was applied, and `ctl.*`, whose handler is called
once per request also when the client has stopped waiting (a request
stays queued after the client's `TimedOut`, and one the host holds
stays with the host, so a retry can run the action twice). The service
does not time a `ctl.*` reply out, so for `ctl.*` the blocking client's
2 s are the time the host has to answer it; an answer sent later is
discarded.

### Other measures

- **No `SIGPIPE`**: replies are sent with `send(MSG_NOSIGNAL)`, so a
  client that closed before reading its reply yields `EPIPE`, not
  `SIGPIPE`, also in a host process that leaves `SIGPIPE` at its
  default (a C or JNI host embedding the service).
- **Value masking**: property values are masked in service logs
  (`<N bytes>` placeholder). Property values may carry tokens or device
  identifiers; names are still logged in full. One value is logged: the
  target of a `ctl.*` request (its wire value), at `trace` level, and
  only by a service that has an authorizer. That service looks the
  request's context up under the name `ctl.<action>$<target>`, and
  `rsproperties` logs a name whose lookup ends at an exact
  `property_contexts` entry (`Property "ctl.<action>$<target>"
  resolved: ...`, the name escaped as by `{:?}`). A target without such
  an entry is not logged, and no other log line prints the target.
- **Init errors propagate**: `run()` calls `rsproperties::init`, so a
  directory already set to a different path surfaces as a `Result`
  (`StartupStage::Config`) rather than silently leaving the service
  bound to whatever paths a previous instance committed.
- **Deterministic build.prop apply order**: entries are collected into a
  `BTreeMap` before apply and added to the area in key order, so the
  trie layout is the same on every run. Which file wins a key conflict
  does not depend on this: `load_properties_from_file_with` overwrites
  in call order, so the later file wins (an entry the type check drops
  overwrites nothing).
