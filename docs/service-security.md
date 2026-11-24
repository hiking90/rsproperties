# rsproperties-service: files, access control and hardening

The files `rsproperties-service` creates, who may use the service, and
the limits it applies to its clients. The API is documented on
[docs.rs](https://docs.rs/rsproperties-service); the protocol behaviour
is in [service-protocol.md](service-protocol.md).

## Directory layout

```text
properties_dir/
├── .writer_lock           # flock held by the running writer
├── property_info          # property contexts trie (replaced by rename;
│                          #   on Android written once at its name)
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
`property_info` back before it renames it into place (on Android,
where it is written at its name, before it labels it). In such a build,
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
  of `ctl.<action>$<target>`; for `ctl.start`, `ctl.stop` and
  `ctl.restart` also the context of `ctl.<target>`, which init checks
  first, as `SetRequest::legacy_control_context`), the socket the
  request came in on, and the sender's pid, uid and gid from
  `SO_PEERCRED`. A name that no
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
- **More about the sender, on request**: `PeerInfo::groups` gives the
  sender's supplementary groups (`SO_PEERGROUPS`, Linux 4.13),
  `PeerInfo::security_label` its security label (`SO_PEERSEC`, the value
  init's `getpeercon` reads), and `PeerInfo::pidfd` a pidfd for the
  process that connected (`SO_PEERPIDFD`, Linux 6.5), which a reused pid
  cannot mislead. The label is in the format of the security module that
  labels the socket: under SELinux the context
  (`system_u:system_r:init_t:s0`); under AppArmor the profile followed by
  its mode (`/usr/bin/client (enforce)`, `client (complain)`), or
  `unconfined`, so a comparison includes the mode (measured for a single
  profile outside a policy namespace; a stacked or namespaced label was
  not measured); under Smack the process label (`_` unless one was set).
  Each is one `getsockopt` made when the authorizer calls it (and a
  second with the size the kernel asked for when the sender has more than
  32 groups or a label that does not fit in 256 bytes), and fails with
  the kernel's error (`ENOPROTOOPT`) where the option does not exist or
  no security module labels sockets. Under AppArmor the result depends on
  the kernel: Linux 6.13 answered `ENOPROTOOPT` for confined senders too,
  Linux 7.2 returned the label. The control and powerctl handlers do not
  get them: they run after the connection task, so a decision
  that needs them is made in the authorizer.
- **`ro.` properties are write-once, not read-only for clients**: the
  first set of a `ro.` name without a value is stored, as in init, where
  sepolicy lets some daemons publish such values. An authorizer that
  must keep clients from doing that denies names that start with `ro.`.
- **A rule-based authorizer**: `AccessPolicy` decides by rules, for a
  host that does not write its own authorizer:
  `ServiceOptions::authorizer(policy.into_authorizer())`. A rule names
  the senders (`Subjects`: uids, gids, or everyone) that may set one
  name, the names with one prefix, or the names of one
  `property_contexts` context. One rule decides a request, the most
  specific that covers it: the rule for its name, else the one with the
  longest prefix, else the one for its context; a sender that rule does
  not name is denied, also when a wider rule names it. A `ctl.*` request
  is matched as `ctl.<action>$<target>`. A gid matches the sender's
  effective gid or one of its supplementary groups. A request no rule
  covers is denied, or, in a policy made with
  `AccessPolicy::allow_unmatched`, allowed. A request whose name starts
  with `ro.` or `ctl.`, or is `sys.powerctl`, is decided more narrowly,
  in both kinds of policy: only a rule that names it covers it, which is
  the rule for its name, or a prefix rule whose prefix itself starts
  with `ro.` (for a `ro.` name) or `ctl.` (for a `ctl.*` request). A
  shorter prefix (the empty one, `ro`, `sys.`) and a context rule do
  not cover it, and without such a rule it is denied. A context rule is
  left out because a name without a `property_contexts` entry has the
  default context, so a rule for that context would open `ro.` and
  `ctl.*` names that nobody listed. No uid is allowed implicitly;
  `trust_uid(0)` allows root everything, these requests included, with
  no rule. The policy does not look at the value (other than the target
  of a `ctl.*` request), the socket or the sender's security label; a
  closure around `AccessPolicy::decide` adds such checks.
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
  socket dir's path is swapped after startup opened it, the socket being
  bound is created at the new location (on Android, unless a file has
  its name there), startup fails, and that socket is left there. Any uid other than the service's and root that can write to the
  socket dir or one of its ancestors can replace the sockets with its
  own and so impersonate the service; that is out of scope. So is what
  such a uid can do to a socket's group and mode during startup: between
  `bind` and the `O_PATH` open of the name just bound (see
  [Socket permissions](#socket-permissions)) it can put another entry at
  that name. The `fstat` check refuses an entry that is not a socket
  owned by the service's euid, so a socket that uid bound itself fails
  startup. The check does not establish that the entry is the socket
  just bound: another socket owned by the service's euid (the service's
  other socket, when the two names are exchanged) passes it.
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

## Running on Android

On Android the service is for a userspace that runs without init's
property service, such as a guest in a mount namespace of its own. It
does not replace the device's init. bionic opens `/dev/__properties__`
and the sockets in `/dev/socket` (`property_service`, and
`property_service_for_system` for `sys.powerctl`), and `rsproperties`
uses the same directories by default, so the host gives the service and
its clients a mount namespace in which those two directories are the
service's properties dir and socket dir, and starts the clients there.

What the host sets up, and what init does in its place:

- **An authorizer is required.** `run` fails at `StartupStage::Config`
  without one: processes of one uid set different properties under the
  SELinux policy, which the socket files cannot tell apart. init asks
  `selinux_check_access(source, target, "property_service", "set")` for
  every request. The authorizer gets both contexts: the source is
  `PeerInfo::security_label()` (what init reads with `getpeercon`), the
  target `SetRequest::context()`. For `ctl.start`, `ctl.stop` and
  `ctl.restart` it also gets `SetRequest::legacy_control_context()`,
  the context init checks first, and init allows the request when the
  policy allows either. The service does not make the SELinux check
  itself; the host calls libselinux or the kernel's selinuxfs from the
  authorizer.
- **Socket files.** init creates `property_service` with mode `0666` and
  `property_service_for_system` with mode `0660` and group `system`
  (1000), both labeled `u:object_r:property_socket:s0`. The service's
  defaults are `0o660` without a label on both; pass init's values with
  `ServiceOptions::socket_access` and `SocketAccess::with_label`. On
  Android each socket is bound at its final name after an unlink of
  what is there, as init's `CreateSocket` does, not under a temp name
  that is then renamed as on Linux: init's policy has no `rename` on
  `property_socket` (`private/init.te`). On that type the service's
  domain needs `create setattr getattr unlink`, which init's policy
  grants. Each name is missing from its unlink to its `bind`, so start
  the clients after `run` has returned.
- **`property_info`.** init labels it `u:object_r:property_info:s0`;
  pass that with `ServiceOptions::property_info_label`. On Android the
  service writes the file at `property_info` itself and labels it last,
  as init does, so on that type its domain needs only `relabelto`, the
  one permission init's policy grants there. On the type a new file in
  the properties dir gets, it needs `create`, `write`, `setattr` (the
  mode), `open read getattr map` (the check that reads the file back),
  `relabelfrom` (the label) and `unlink` to remove the file when a step
  fails; see "Which domain can run it".
- **Area files.** Each is labeled with its `property_contexts` context,
  and `properties_serial` with `u:object_r:properties_serial:s0`, as
  bionic does. On Android a label that cannot be set fails startup at
  `StartupStage::PropertiesService`, as it stops init: a client reads an
  area only when the policy lets its domain read the file's type. The
  label is the last step (bionic labels first): each file is created,
  given its mode, sized, has its header written and is mapped before
  it, which needs `create`, `open read write map`, `setattr` and, for the
  label, `relabelfrom` on the type a new file in the properties dir
  gets. A failure up to and including the label removes the file, which
  needs `unlink` on that type. `properties_serial` is created at its
  name, as bionic creates it, and nothing follows its label, so on its
  type it needs only `relabelto`, which init's policy grants. The area
  of each context is created under a temp name and renamed into place
  after its label, which needs `relabelto` and `rename` on its type,
  `unlink` on the type of an existing file it replaces, and `unlink` on
  its own type to remove the temp file when the rename fails; init's
  policy grants these on `property_type`.
- **A fresh properties dir.** Because `property_info` and
  `properties_serial` are created at their names with `O_EXCL`, `run`
  fails at `StartupStage::PropertiesService` when either exists: init
  starts on the tmpfs `/dev` of each boot, and a host restarts the
  service on a new directory (a new tmpfs, for example). Start the
  clients after `run` has returned: until then a client can open these
  files before they are complete.
- **Which domain can run it.** AOSP's policy has `neverallow { domain
  -init }` rules for writing `property_type`, `properties_device` and
  `properties_serial` files (`private/domain.te`); `relabelfrom` is one
  of the permissions they forbid. On that policy only the `init` domain
  can write the property files. The permissions listed above on the
  label types are the ones init's policy has. On the type a new file in
  the properties dir gets, the service's domain needs, for each file it
  creates there:
  - `.writer_lock`, which `run` creates first (`AreaWriterLock::acquire`,
    `StartupStage::WriterLock`): `create open write` (opened with
    `O_CREAT`), `setattr` (`fchmod` to `0600`) and `lock` (`flock`). The
    file keeps that type; it is not relabeled or removed.
  - The temp file of each context's area and `properties_serial`:
    `create open read write` (opened read-write), `setattr` (`fchmod`),
    `write` (`ftruncate` and the header's `pwrite`), `map` (`mmap`),
    `relabelfrom` (the label) and `unlink` when a step fails.
  - `property_info`: `create open write`, `setattr`, `read getattr map`
    (the loader's read back), `relabelfrom` and `unlink` when a step
    fails.

  Which permissions the policy grants on that type depends on the
  properties dir's label, because the policy has no type transition for
  files created in `properties_device`:
  - In a directory labeled `properties_device`, a new file is
    `properties_device`. init has `create_file_perms` (`lock` included)
    on that type but not `relabelfrom` (`private/init.te`), and the
    neverallow above
    keeps every other domain from having it, so every label step fails
    and `run` fails at `StartupStage::PropertiesService`.
  - init's own files do not have this type. init creates
    `/dev/__properties__` and its files (`PropertyInit`) before it runs
    `restorecon` on `/dev` and `/dev/__properties__`
    (`SelinuxRestoreContext`), so the directory is then still `tmpfs`,
    and `tmpfs_domain(init)` (`private/init.te`, `public/te_macros`)
    makes a file init creates in a `tmpfs` directory `init_tmpfs`. On
    that type init has `read write getattr map` (`tmpfs_domain`) and
    `create getattr open read write setattr relabelfrom unlink map` (the
    `file_type` rule in `private/init.te`), and no other file
    permission, `lock` included (also checked in the compiled
    `prebuilts/api/202604/202604_plat_sepolicy.cil`). init relabels the
    directory to `properties_device` afterwards.

  So on that policy the host follows init's order:

  1. Start the service in the `init` domain on a fresh directory labeled
     `tmpfs`, such as the root of a new tmpfs mount (`fs_use_trans tmpfs`
     in `private/fs_use`), so each new file gets `init_tmpfs` and init
     may relabel it (the `file_type` rule above).
  2. After `run` has returned and before the clients start, relabel the
     properties dir to `u:object_r:properties_device:s0`, the label
     `file_contexts` gives `/dev/__properties__`. init does the same:
     `SelinuxRestoreContext` (`init/selinux.cpp`) runs after
     `PropertyInit` and restores `/dev/__properties__` without
     recursion, so the files keep their labels. init's policy grants
     `tmpfs:dir relabelfrom` and `properties_device:dir relabelto`
     (`private/init.te`). Every domain may search both directory types
     (`private/domain.te`).

  Of the permissions listed above on the type a new file gets, init's
  policy has every one on `init_tmpfs` except `lock`. On an unmodified
  AOSP policy step 1 therefore fails at the writer lock: the `flock` on
  `.writer_lock` is refused with `EACCES`, and `run` fails at
  `StartupStage::WriterLock` with that errno (`Error::Lock`, "already
  owned by another instance", is only for a lock another writer holds).
  The writer lock stays on Android: it keeps a second instance from
  unlinking the first one's sockets and binding its own. The procedure
  needs a host policy that adds `allow init init_tmpfs:file lock;`; no
  neverallow in AOSP's policy covers that rule (checked in the compiled
  policy above).

  Measured on the API 34, 36 and 37 emulator images (userdebug,
  enforcing), with the service switched to the `init` domain at its
  start and run in a mount namespace with new `tmpfs` mounts at
  `/dev/__properties__` and `/dev/socket`. In permissive mode the only
  permission of the `init` domain the policy lacked was `lock` on
  `init_tmpfs`. Enforcing, on the device's policy `run` failed at
  `StartupStage::WriterLock` with `EACCES`; on that policy recompiled
  with the rule above it started, the files had the labels listed
  above, the relabel of the directory succeeded, and so did a restart
  over sockets left by a killed instance. Stock `getprop` in the `shell`
  and `untrusted_app` domains read the areas; `setprop` from `shell` was
  allowed for `debug.*` and refused, by an authorizer that asks
  selinuxfs as `selinux_check_access` does, for `sys.*`, `ro.*` and
  `ctl.start`; `untrusted_app` could not connect (`connectto` on `init`
  is refused, as for init's own socket).
- **The properties dir.** Its label is the one of the directory the host
  creates or mounts; the service does not set it. That label decides the
  type each new file in it starts with, so on AOSP's policy the host
  starts with a `tmpfs` one and relabels it to `properties_device` after
  `run` (see "Which domain can run it"). The directory must be on a
  `tmpfs` there in any case: the policy lets `property_type` files be
  associated with `tmpfs` only (`allow property_type
  tmpfs:filesystem associate;`, `private/property.te`), so on another
  filesystem the label of each context's area is refused with `EACCES`,
  whatever the domain (on `/data`, `ext4`: `denied { associate }`
  against `labeledfs`), and `run` fails at
  `StartupStage::PropertiesService`.
- **Not provided.** The service does not create the
  `appcompat_override` directory, which only API 35–36 readers use:
  zygote bind mounts it over `/dev/__properties__`, so without it bionic
  reads every property without overrides. API 37 readers do not use it:
  in a process with `BIONIC_APPCOMPAT_OVERRIDE` set, bionic looks up
  `ro.appcompat_override.<name>` in the ordinary areas first, so such an
  entry stored in this service's areas overrides `<name>` (not measured
  against this service). The service also does not run `restorecon` for
  `selinux.restorecon_recursive` (it stores the value), and does not
  serve the V1 command (bionic sends V2 when `ro.property_service.version`
  is 2 or more, which the service sets).

The crate documentation's "Running on Android" section has these options
as code.

## Hardening

### Socket permissions

Each socket's mode — `0o660`, or the one given to
`ServiceOptions::socket_access` for that socket — is set on the socket
fd before `bind`, so the file exists only with that mode minus the
umask's bits. The socket file is then opened as an `O_PATH|O_NOFOLLOW`
descriptor and checked with `fstat` to be a socket owned by the
service's euid (this does not show it to be the socket just bound; see
the socket dir contract above). Through that descriptor a configured
group is set (`fchownat` with `AT_EMPTY_PATH`), and if the umask cleared
any bits the mode is raised to exactly the configured mode (needs
`/proc`: `/proc/thread-self/fd`, or `/proc/self/fd` on kernels before
3.17). Only then does the socket `listen` — a `connect` before that
fails with `ECONNREFUSED`. Both sockets are bound as `.<name>.tmp-<pid>`
and renamed onto their final names only after both listen; on Android
each is bound at its final name instead (see "Running on Android").

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

Before the waiting room, each accepted connection is counted for its
sender's uid (`SO_PEERCRED`). A uid that already has
`ServiceOptions::max_connections_per_uid` connections open (32 by
default) gets the new one closed without a reply, logged at `warn`. A
connection is counted until the service closes its descriptor: while it
waits for a handler permit, during its exchange, and, for a `ctl.*`
request, until the host has answered or dropped the request's
`ControlReply`. So the clients of one uid hold at most 32 of the 64
handler permits by connecting and sending nothing, and leave at most 32
of the 102 `ctl.*` requests unanswered. uid 0 and the service's own
effective uid are not counted, since a process of either can stop the
service by other means; connections whose credentials cannot be read are
counted together. The limit bounds one uid: a client that has several
uids gets the limit for each. init has no such limit.

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
  request's context up under the name `ctl.<action>$<target>`, and, for
  `ctl.start`, `ctl.stop` and `ctl.restart`, also under `ctl.<target>`
  (init's legacy name), whether or not the authorizer reads that
  context; `rsproperties` logs a name whose lookup ends at an exact
  `property_contexts` entry (`Property "ctl.<action>$<target>"
  resolved: ...` or `Property "ctl.<target>" resolved: ...`, the name
  escaped as by `{:?}`). A target without an
  exact entry under either name is not logged, and no other log line
  prints the target.
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
