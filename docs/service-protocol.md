# rsproperties-service: protocol and init behaviour

How `rsproperties-service` answers the property service protocol, and
where it follows or departs from AOSP init (Android 17). The API is
documented on [docs.rs](https://docs.rs/rsproperties-service); this page
describes the behaviour a client observes. Security limits are in
[service-security.md](service-security.md).

## Protocol

The socket service implements the Android property service protocol:

- **Socket names**: the standard Android names, `property_service` and
  `property_service_for_system`.
- **Commands**: `PROP_MSG_SETPROP2` (0x00020001) only.
- **V1 is not supported**: the legacy V1 `PROP_MSG_SETPROP` (1) is
  answered like an unknown command, with `PROP_ERROR_INVALID_CMD`; its
  name and value are not read, nothing is set, and the request is logged
  at `warn` with the peer pid. AOSP init still applies a V1 set. A V1
  client does not read a reply (bionic and this workspace's client wait
  for the socket to close and then report success), so the client sees
  `Ok` for a set that was not applied. Clients choose V2 when
  `ro.property_service.version` is 2 or more, which the service sets at
  startup; this workspace's client sends V1 only when
  `PROPERTY_SERVICE_VERSION` selects it and the process has not opened
  the property area yet.
- **Response codes**: `PROP_SUCCESS` (0) or init's error codes
  (`rsproperties::wire::PROP_ERROR_*`): `READ_CMD` when the command word
  cannot be read, `READ_DATA` for an unreadable frame or a name/value
  length over the wire caps (see [Size limits](#size-limits)),
  `INVALID_CMD`, `INVALID_NAME`, `INVALID_VALUE` (`PROP_VALUE_MAX` bytes
  or more for a non-`ro.` name, an interior NUL, not UTF-8, or not
  matching the `property_contexts` type), `READ_ONLY_PROPERTY`,
  `HANDLE_CONTROL_MESSAGE` (`ctl.*` refused or failed), `SET_FAILED`,
  and `PERMISSION_DENIED`. The service sends `PERMISSION_DENIED` only
  when it has an authorizer (`ServiceOptions::authorizer`), for a
  request whose name is not `ctl.*` (`sys.powerctl` included) and
  passed the name check, in these cases: the authorizer denied it or
  panicked, the sender's credentials could not be read, the properties
  service failed to start, or the name's context could not be read. A
  `ctl.*` request gets `HANDLE_CONTROL_MESSAGE` in the same cases.
  Both V2 fields are read before either is checked. The checks then
  come in this order: that the name is a string (no interior NUL,
  UTF-8); the name check; that the value is a string; the authorizer,
  when the service has one; then, for a name that is not `ctl.*`, the
  value's `property_contexts` type, its length, and whether a `ro.`
  name already has a value. So a request that fails the name check
  gets `INVALID_NAME` also when its value is not a string. That is
  init's order (name, access, type) except for a value that is not a
  string, which init checks after the access: such a request, when its
  name passed the name check, gets `INVALID_VALUE` here whoever sent
  it, the authorizer is not asked, and no denial is logged. A frame that
  cannot be read (`READ_CMD`, `READ_DATA`) and a command other than
  `PROP_MSG_SETPROP2` (`INVALID_CMD`) are answered before the
  authorizer too.
- **`sys.powerctl`**: never stored, as in init, so a `get` finds no
  value. A set the authorizer denies gets `PERMISSION_DENIED`, also when
  its value is `reboot,userspace`. Of the others, `reboot,userspace` is
  answered with
  `INVALID_VALUE`, as in init (Android 17). Any other value that passes
  the checks every set
  gets (name, length, UTF-8, `property_contexts` type, and the
  authorizer when the service has one) is answered with
  `PROP_SUCCESS` and passed to `ServiceOptions::powerctl_handler` with
  the sender's pid. The reply does not wait for the handler and does
  not say whether the command was recognized: init also accepts
  `sys.powerctl=anything` and only logs `unrecognized command`.
  Without a handler the set is answered with `PROP_SUCCESS`, logged at
  `warn`, and has no effect. As init does, the service keeps one
  pending command: a set made before the handler was called for the
  previous one replaces it, and the command is passed to the handler
  before the next `ctl.*` request. A handler that returns
  `PowerctlOutcome::ShuttingDown` stops the handler calls, as init's
  `HandlePowerctlMessage` does for a command it acts on; after that, or
  after `ServiceHandle::stop_handler_calls`, a set that passes the
  checks is answered with `PROP_SUCCESS` and reaches no handler, as in
  init once it has begun a shutdown. A request read after the service
  has stopped gets `SET_FAILED`, or `PERMISSION_DENIED` when the
  authorizer denies it. The value is not logged (init logs it).
- **`selinux.restorecon_recursive`**: stored like any property. init
  runs a recursive restorecon on the path and stores the path when that
  is done, so a client that waits for the property to equal the path it
  set sees the same result; no restorecon runs here.
- **Type check**: values are checked against the `property_contexts` type
  (`bool`, `int`, `uint`, `double`, `size`, `enum ...`) as init's
  `CheckType` does; an empty value is always accepted. This covers client
  sets, persistent records and build.prop entries; a build.prop entry
  that fails is logged and dropped when the file is read, so it does not
  replace an earlier entry of the same name. A build.prop value is
  expanded before the check, as in init (`${name}`, `${name:-default}`,
  `$$` for a `$`); an entry whose value cannot be expanded is logged and
  dropped. A reference is looked up among the build.prop entries read
  so far, in `build_prop_files` order; init looks it up in the property
  area, which does not hold those entries yet, and drops such an entry.
  An expanded value longer than 64 KiB is dropped like one that cannot
  be expanded. A `build_prop_files` entry (its imports included) whose
  expanded values total more than 4 MiB fails `run`
  (`StartupStage::PropertiesService`) instead.
- **`ctl.*`**: never stored; passed to `ServiceOptions::control_handler`
  with a `ControlReply`, or refused without a handler; a service with
  an authorizer asks it first and refuses what it denies. See
  [Control messages](#control-messages).
- **Startup properties**: `ro.property_service.version=2`, the persistent
  properties, then `ro.persistent_properties.ready=true`, after the
  build.prop entries. A build.prop file that sets
  `ro.property_service.version` to a value that passes the name's
  `property_contexts` type check (any value when the name has no type)
  but is not a decimal number from 2 to 4294967295 makes `run` fail
  (`StartupStage::PropertiesService`, `InvalidInput`) before the
  properties dir is written: `ro.` names are write-once, so that value
  would stay and tell clients to send V1. Under the `int` type of AOSP's
  `property_contexts` that includes `1`, an empty value, `0x10` and
  `4294967296`; a value the type check refuses, such as `v2`, is
  dropped instead and the service sets `2`.

## Size limits

The wire layer caps the upfront allocation for a single message at
1 KB (name) / 8 KB (value) to bound damage from a hostile peer. These
are *transport* caps; the actual property-system policy is stricter
and enforced after the bytes arrive:

- **Names**: `validate_property_name` — ASCII alphanumeric plus
  `_ . - @ :`, no leading or trailing `.`, no `..`; at most 31 bytes
  (`PROP_NAME_MAX - 1`, the 32-byte buffer includes the NUL) on the
  client's V1 wire path, which this service does not handle.
- **Values**: `validate_value_len` — `PROP_VALUE_MAX = 92` is the
  buffer size including the trailing NUL, so user content is capped
  at 91 bytes for non-`ro.*` properties. `ro.*` properties may exceed
  this via the long-property out-of-line storage path.

## Control messages

The service makes the handler calls in the order in which init's main
thread handles the same requests, one call at a time: `ctl.*` requests
in the order they were read, and a `sys.powerctl` command before the
next `ctl.*` request. A call that blocks delays every call behind it;
init has the same property, since its control functions run on its main
thread.

In a service with an authorizer, this section describes the requests
the authorizer allows. A `ctl.*` request it denies is answered with
`HANDLE_CONTROL_MESSAGE` before any of the following: it is not queued,
does not count as unanswered, and gets that reply also when it is
`ctl.stop` and the handler calls are stopped.

A `ctl.*` client is answered when the host answers the `ControlReply`
of its request: `reply.send(true)` is `PROP_SUCCESS`,
`reply.send(false)` is `HANDLE_CONTROL_MESSAGE`. `send` does not wait
and needs no runtime, so the host can call it in the handler call or
later, from any thread or task. init answers a control message when its
main thread has handled it (`init.cpp` `HandleControlMessages`), and a
host does the same in one of two ways:

- answer in the handler call. The messages are then handled one at a
  time, in the order they were read, as in init;
- pass the message and the reply on to a task or thread of the host and
  return. The service then calls the handler for the next request at
  once, and the host decides in which order the messages are handled
  and answered. One task that reads them from a channel keeps the order
  of the calls.

A reply that is dropped without an answer is a failure
(`HANDLE_CONTROL_MESSAGE`), so a handler that panics, or a host task
that ends with requests it has not handled, leaves no client waiting.
Once the handler calls are stopped, a dropped reply for `ctl.stop` is
`PROP_SUCCESS`, the reply a `ctl.stop` request read after the stop
gets.

The service sets no time limit for the reply. It refuses a request that
finds 102 others unanswered (queued for their call, in their call, or
held by the host) with `HANDLE_CONTROL_MESSAGE`; init refuses a control
message when 101 are queued, whether or not one is being handled, so
the two limits agree while a control message is in its call. An
unanswered request holds one fd (the connection of its client) and none
of the connection permits under
[Backpressure](service-security.md#backpressure), so other sets are
served meanwhile. It does count toward its sender's uid
(`ServiceOptions::max_connections_per_uid`, also under Backpressure)
until it is answered or dropped: a uid with that many unanswered
requests gets every further connection closed without a reply. uid 0
and the service's own effective uid are not counted.

Once the handler calls are stopped (a powerctl handler returned
`PowerctlOutcome::ShuttingDown`, or `ServiceHandle::stop_handler_calls`)
no handler is called: `ctl.stop` is answered with `PROP_SUCCESS` and
every other action (`ctl.interface_stop` included) with
`HANDLE_CONTROL_MESSAGE`, as in init during shutdown. **Not mirrored**:
init leaves the messages still in its queue unanswered from then on, so
their clients wait until the device goes down; here they get the same
replies as a request read after the stop, and so does a request whose
reply the host drops afterwards.

### Behaving as init does

What handling a message means is the host's part. A host that is to be
indistinguishable from init for the clients of `ctl.*` and
`sys.powerctl` (`android::base::SetProperty("ctl.stop", ...)`,
`WaitForProperty("init.svc.<name>", "stopped")`, `reboot`) follows these
rules, which are init's (`init.cpp` `HandleControlMessage`,
`service.cpp` `StopOrReset` / `NotifyStateChange`, `reboot.cpp`
`HandlePowerctlMessage`, Android 17). "Handler result" is the argument
of `ControlReply::send` for a `ctl.*` row and the return value of the
powerctl handler for a `sys.powerctl` row:

| Request | What init does | Handler result |
|---|---|---|
| `ctl.start` = `<name>` | Starts the process of the service and sets `init.svc.<name>` to `running`. Does not wait for the process to be ready. A service that is already running is left as it is. | `true`, also when it was already running; `false` when the process could not be started |
| `ctl.stop` = `<name>`, running | Records that the service must not be restarted, sends `SIGKILL` to its process group and sets `init.svc.<name>` to `stopping`. Does not wait for the process to exit; when it has been reaped, sets `init.svc.<name>` to `stopped`. | `true` |
| `ctl.stop` = `<name>`, not running | Sets `init.svc.<name>` to `stopped`. | `true` |
| `ctl.restart` = `<name>` | As `ctl.stop`, but the service is started again when the process has been reaped (at once when it was not running). | `true` |
| `ctl.interface_start` / `interface_stop` / `interface_restart` = `<interface>` | The same for the service that declares the interface. | as above |
| `ctl.sigstop_on` / `sigstop_off` / `oneshot_on` / `oneshot_off` = `<name>` | Changes one flag of the service. | `true` |
| a name that is no service, or any other action | Logs the message. | `false` |
| `sys.powerctl` = `shutdown[,<reason>]` or `reboot[,<target>[,...]]` | Stops accepting control messages, then stops every service (`SIGTERM`, then `SIGKILL` after a timeout) and reboots or powers off. | `PowerctlOutcome::ShuttingDown` |
| `sys.powerctl` = anything else | Logs `unrecognized command`. | `PowerctlOutcome::Ignored` |

None of init's control functions waits for a process: the reply to
`ctl.stop` follows the `SIGKILL`, not the exit. A host that answers
only when the process has exited keeps the client waiting for that
time, which init does not. Outside Android, `rsproperties::set` gives up
after 2 s with `TimedOut`; `rsproperties::asynchronous::set` waits
without a bound unless its caller wraps it in `tokio::time::timeout`.

The host sets `init.svc.<name>` itself, with `rsproperties::set` from a
thread that may block (the handler runs on tokio's blocking pool, so the
handler may do it); the service gives the name no special treatment, so
any client that can connect can set it as well, unless the service has
an authorizer that denies the request (the host's own set goes through
the socket and the authorizer like any other). The values init uses
are `running`, `stopping`, `stopped` and `restarting`. With `running`
init also sets `ro.boottime.<name>` (once) and
`init.svc_debug_pid.<name>`.

What the service cannot reproduce: init checks with SELinux whether the
sender may set the `ctl.*` name for that target
(`CheckControlPropertyPerms`) and answers `HANDLE_CONTROL_MESSAGE`
before the message is queued. A service with an authorizer
(`ServiceOptions::authorizer`) asks it at the same point, before the
queue, with the context `property_contexts` gives
`ctl.<action>$<target>`: a request it denies gets
`HANDLE_CONTROL_MESSAGE`, does not reach the handler and does not count
toward the 102. The authorizer decides in place of SELinux, by the
sender's pid, uid and gid, and one lookup of init's is not made: for
`ctl.start`, `ctl.stop` and `ctl.restart` init first checks the legacy
name `ctl.<target>` and allows the request when that check passes; here
the context is always that of `ctl.<action>$<target>`. In a service
without an authorizer every sender that can connect reaches the
handler, which can refuse by the sender's `uid`, `gid` or `pid` (it has
no SELinux context to check) with `reply.send(false)`; its client then
gets the same `HANDLE_CONTROL_MESSAGE`. Such a request counts toward
the 102 until it is answered, where init refuses it before its queue.
