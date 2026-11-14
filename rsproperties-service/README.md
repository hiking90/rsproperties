# rsproperties-service

Linux emulation of the Android system property service: a tokio-based daemon that serves the property socket protocol and the property area files on a Linux host. It is not usable on Android, where init owns the property service, and compiles only for Linux; use the `rsproperties` client crate on Android.

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## Overview

`rsproperties-service` is an async property service that emulates the Android property service on Linux. It features a Unix domain socket server that handles property operations using an actor-based architecture powered by the `rsactor` framework.

## Key Features

- **🔄 Async Operations**: Built on tokio for high-performance async I/O
- **🎭 Actor-Based Architecture**: Uses rsactor for reliable message passing and state management
- **🔌 Unix Domain Socket Server**: Compatible with Android's property service protocol
- **⚡ High Performance**: Non-blocking property operations with concurrent client handling
- **🛡️ Robust Error Handling**: Comprehensive error handling with graceful degradation
- **📂 File-Based Configuration**: Supports property contexts and build.prop file loading
- **🔧 Configurable**: Flexible directory and socket path configuration

## Architecture

The service consists of two main components running as separate actors.
Both are internal: the public API is `run`, `ServiceOptions` and the
`ServiceHandle` that `run` returns, with the types they use:
`ControlMessage` (the first argument of the control handler: `action`,
`target`, and the sender's `pid`, `uid`, `gid`), `ControlReply` (its
second argument, which answers the client that sent the message; with
`ControlReplyProbe` for tests of a handler), `PowerctlMessage` (the
argument of the powerctl handler: `command`, `pid`, `uid`, `gid`),
`PowerctlOutcome` (what the powerctl handler returns), `StartupError` /
`StartupStage` (a failed `run`) and `ServiceError` (a failure after
startup).

### Properties service
- Manages the actual property storage and retrieval
- Loads property contexts from files
- Processes build.prop files
- Handles property addition, updates, and lookups
- Maintains system property state

### Socket service
- Provides Unix domain socket interface
- Handles client connections and commands
- Implements Android-compatible property service protocol
- Handles the SETPROP2 (V2) command for property setting; the legacy SETPROP (V1) command is refused
- Manages concurrent client sessions

## Installation

This crate is not published on crates.io; depend on it via git, and take
`rsproperties` (used for `PropertyConfig` in the examples below) from the same
git source. A crates.io `rsproperties` would be a separate crate whose
`PropertyConfig` `run` does not accept:

```toml
[dependencies]
rsproperties = { git = "https://github.com/hiking90/rsproperties" }
rsproperties-service = { git = "https://github.com/hiking90/rsproperties" }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
```

`rsproperties-service` has no Cargo features. It always enables
`rsproperties/builder`.

Supported platform: Linux only. On any other target (Android and macOS
included) the crate fails to compile with a `compile_error!` that names
the reason.

## Quick Start

### Basic Service Setup

```rust,no_run
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // `run` creates a missing properties dir (last component only, 0711) and
    // socket dir (0755, parents included), both minus the umask's bits;
    // create them yourself beforehand to choose owner and mode. These
    // paths suit a single-user dev box only: under `/tmp` any local user
    // can create the directories first, and `run` uses an existing
    // directory as is (see "Socket dir contract").

    // Create configuration
    let config = rsproperties::PropertyConfig::new()
        .properties_dir(PathBuf::from("/tmp/properties"))
        .socket_dir(PathBuf::from("/tmp/sockets"));

    // Start the service (no input files, no persistence). `run` returns
    // once the service is ready for clients.
    let mut service =
        rsproperties_service::run(config, rsproperties_service::ServiceOptions::new()).await?;

    println!("Service started successfully!");

    // Run until Ctrl+C, or until the service ends by itself.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("Shutdown signal received");
        }
        // `shutdown` below returns the same result.
        _ = service.join() => {}
    }

    // Stops the service and waits until the sockets are unlinked and the
    // properties dir's writer lock is released.
    service.shutdown().await?;

    Ok(())
}
```

### Advanced Configuration with Files

```rust,no_run
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Linux host paths laid out like an Android rootfs; the socket dir must
    // be writable only by the service's user and root.
    let properties_dir = PathBuf::from("/system/etc/properties");
    let socket_dir = PathBuf::from("/dev/socket");

    // Configure property context files
    let property_contexts = vec![
        PathBuf::from("/system/etc/selinux/property_contexts"),
        PathBuf::from("/vendor/etc/selinux/vendor_property_contexts"),
    ];

    // Configure build.prop files. A missing or unreadable file, or one
    // whose expanded values total more than 4 MiB, fails startup; a file
    // writable by group or others is logged at `error`
    // and not loaded, and the service starts without its properties.
    let build_props = vec![
        PathBuf::from("/system/build.prop"),
        PathBuf::from("/vendor/build.prop"),
        PathBuf::from("/product/build.prop"),
    ];

    let config = rsproperties::PropertyConfig::new()
        .properties_dir(properties_dir)
        .socket_dir(socket_dir);

    // Start services with configuration files. `persist.*` properties are
    // kept in an AOSP-format persistent_properties file (its directory must
    // exist); `ctl.*` and `sys.powerctl` sets go to the handlers instead
    // of being stored.
    let options = rsproperties_service::ServiceOptions::new()
        .property_contexts_files(property_contexts)
        .build_prop_files(build_props)
        .persistent_properties_file("/data/property/persistent_properties")
        .control_handler(|msg, reply| {
            eprintln!(
                "ctl.{} {} from pid {:?}, uid {:?}",
                msg.action, msg.target, msg.pid, msg.uid
            );
            // No services to control: report failure. A host that has
            // some passes `msg` and `reply` on to the code that starts
            // and stops them, which answers when it is done.
            reply.send(false);
        })
        .powerctl_handler(|msg| {
            // The client already has its reply; hand the command to the
            // code that reboots or powers off, and return what was done
            // with it. See "Answering as init does" below.
            eprintln!("sys.powerctl {} from pid {:?}", msg.command, msg.pid);
            rsproperties_service::PowerctlOutcome::Ignored
        });
    let mut service = rsproperties_service::run(config, options).await?;

    // The service runs with the given files. Nothing ends it in normal
    // operation, so `join` returns only if it fails.
    service.join().await?;

    Ok(())
}
```

## Service Components

### ServiceHandle

`run` returns a `ServiceHandle`. The service runs on the tokio runtime
that called `run` until the handle stops it:

| Call | Effect |
|---|---|
| `service.join().await` | Waits until the service has ended by itself (a failure after startup). Takes `&mut self` and is cancel-safe, so it can be an arm of `tokio::select!`. By the time it returns, both socket files are unlinked, the writer lock is released and no control or powerctl handler call is running (as for `shutdown`). |
| `service.stop_handler_calls()` | Ends the control and powerctl handler calls while the service keeps running; init's `StopSendingMessages`, which init calls when it begins a shutdown or reboot. A request read afterwards reaches no handler and is answered as init answers it during shutdown: `ctl.stop` with `PROP_SUCCESS` (also without a control handler), every other `ctl.*` action with `HANDLE_CONTROL_MESSAGE`, `sys.powerctl` with `PROP_SUCCESS` once it has passed the checks. Other sets are applied as before. Cannot be undone. A powerctl handler that returns `PowerctlOutcome::ShuttingDown` has the same effect, so call this only when the host goes down without a `sys.powerctl` set. The call that is running is not affected and at most one more begins (the one the service was about to make); `ctl.*` requests that were waiting for their call get the replies above without one. A `ControlReply` the host already holds is the host's to answer; one it drops from then on gets the replies above as well. |
| `service.shutdown().await` | Stops the socket service, then the properties service, and waits for both. Sets already queued are applied first. A `ctl.*` request read from an open connection afterwards (also one the client sent earlier) is refused (`HANDLE_CONTROL_MESSAGE` to a V2 client), and so is a `sys.powerctl` request (`SET_FAILED`); after `stop_handler_calls` the `ctl.*` requests get the replies of that row instead, and a `sys.powerctl` request gets them only until the properties service has ended (`SET_FAILED` afterwards, as for every set). `ctl.*` requests that were still waiting for their handler call are refused in the same way, without a call. Then waits for the handler call that is running, and for the powerctl handler call of a `sys.powerctl` set that was answered before that: when `shutdown` returns, no handler call is running, none begins afterwards, and the service has dropped both handlers. A `ControlReply` that the host got from a call and has not answered is not waited for: it keeps its client's connection and can be answered or dropped after `shutdown` has returned. There is no timeout, so a handler call that does not return keeps `shutdown` pending (the sockets are unlinked and the lock is released by then), and a handler must not wait for `shutdown` or `join`. Returns the failure the service had ended with, if any. |
| dropping the handle | Stops the service without waiting: the sockets are unlinked and the lock is released later, by tasks on the runtime, and a control or powerctl handler call that is running keeps running. A `ControlReply` the host holds stays usable. Call `shutdown` when the next step depends on them (starting the service again on the same directories, for example). |

A `run` that fails to start waits the same way before it returns its
error: the socket service accepts requests while the properties service
is still starting, so a control handler call can begin before `run`
returns, and a failed `run` returns only after that call has
returned, without a timeout. A handler must not wait for `run` to
return either. (A powerctl handler call begins only after the
properties service has checked the request, so none begins in a `run`
whose properties service failed to start.)

A powerctl handler that ends the service must not do it inside the
call: `shutdown` and `join` wait for the call to return. Send the
command to the task that owns the `ServiceHandle` (a channel) and
return `PowerctlOutcome::ShuttingDown`. That task stops the processes
of the host, calls `shutdown` and then reboots or powers off. From the
return of the handler until `shutdown`, a client that stops a process
with `ctl.stop` gets `PROP_SUCCESS`, as it does from init during
shutdown, instead of reaching a control handler whose targets are
already being stopped. The `PROP_SUCCESS` is sent without anything
having been stopped, so return `ShuttingDown` (or call
`stop_handler_calls`) only when the host is stopping those processes.

Once `join`, `shutdown` or a failed `run` has returned, the service
holds no clone of either handler: one task owns both handlers and makes
every call, and the three wait for that task to end. Unless the caller
kept a clone of the `ServiceOptions`, the closures and what they
captured (a channel sender, a clone of an `Arc`) have been dropped by
then. The `ControlReply` values the host has not answered are not part
of this: each keeps the connection of its client until it is answered
or dropped.

### Answering as init does

The service makes the handler calls in the order in which init's main
thread handles the same requests, one call at a time: `ctl.*` requests
in the order they were read, and a `sys.powerctl` command before the
next `ctl.*` request. A call that blocks delays every call behind it;
init has the same property, since its control functions run on its main
thread.

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
the two limits agree while a control message is in its call. An unanswered
request holds one fd (the connection of its client) and none of the
connection permits under [Backpressure](#hardening-highlights), so
other sets are served meanwhile.

What handling a message means is the host's part. A host that
is to be indistinguishable from init for the clients of `ctl.*` and
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
time (an `rsproperties` client outside Android gives up after 2 s with
`TimedOut`), which init does not.

The host sets `init.svc.<name>` itself, with `rsproperties::set` from a
thread that may block (the handler runs on tokio's blocking pool, so the
handler may do it); the service gives the name no special treatment and
any client that can connect can set it as well. The values init uses
are `running`, `stopping`, `stopped` and `restarting`. With `running`
init also sets `ro.boottime.<name>` (once) and
`init.svc_debug_pid.<name>`.

What the service cannot reproduce: init checks with SELinux whether the
sender may set the `ctl.*` name for that target
(`CheckControlPropertyPerms`) and answers `HANDLE_CONTROL_MESSAGE`
before the message is queued. Here every sender that can connect
reaches the handler, which can refuse by the sender's `uid`, `gid` or
`pid` (it has no SELinux context to check) with `reply.send(false)`;
its client then gets the same `HANDLE_CONTROL_MESSAGE`. Such a request
counts toward the 102 until it is answered, where init refuses it
before its queue.

`join` and `shutdown` return `Result<(), ServiceError>`. `ServiceError`
displays as `<part> failed: <cause>` (`socket service` or
`properties service`), and `source()` is the cause. Neither part returns
an error once it has started, so in this version the cause is a panic in
that part's task.

The property area files and `property_info` stay in the properties dir
after the service has ended; readers that mapped them keep working and
see no further changes.

### No in-process actor API

The two actors (socket service, properties service), their argument
types and their messages are crate-private; `ServiceHandle` exposes no
rsactor or tokio type. `run` is the only way to start them: it takes the
properties dir's writer lock before either actor exists, so a second
instance fails before it replaces a running instance's sockets. External
callers drive property updates by **connecting to the Unix socket and
sending an AOSP `SETPROP2` frame**, exactly as the Android client does.
See [Protocol Compatibility](#protocol-compatibility) below.

If you need to read or set properties from the same process that owns
the service, use the `rsproperties` crate directly
(`rsproperties::get_or`, `rsproperties::set`, etc.) — it talks to the
same socket the external clients use. `set` is blocking socket I/O and
waits up to 2 s for the reply: called from a task on the runtime that
runs the service (e.g. a `current_thread` runtime), it keeps the service
from accepting, returns a timeout error, and the already-sent value may still be
applied once the runtime resumes. Run the service on its own runtime/thread, or
call `set` through `tokio::task::spawn_blocking` on a multi-thread
runtime.

## Protocol Compatibility

The socket service implements the Android property service protocol:

- **Socket Names**: Uses standard Android socket names (`property_service`, `property_service_for_system`)
- **Commands**: `PROP_MSG_SETPROP2` (0x00020001) only
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
  the property area yet
- **Response Codes**: `PROP_SUCCESS` (0) or init's error codes
  (`rsproperties::wire::PROP_ERROR_*`): `READ_CMD` when the command word
  cannot be read, `READ_DATA` for an unreadable
  frame or a name/value length over the wire caps (see Size limits),
  `INVALID_CMD`, `INVALID_NAME`, `INVALID_VALUE` (`PROP_VALUE_MAX` bytes
  or more for a non-`ro.` name, an interior NUL, not UTF-8, or not
  matching the `property_contexts` type),
  `READ_ONLY_PROPERTY`,
  `HANDLE_CONTROL_MESSAGE` (`ctl.*` refused or failed), `SET_FAILED`.
  Both V2 fields are read before either is checked, and the name is
  checked first, as in init
- **`sys.powerctl`**: never stored, as in init, so a `get` finds no
  value. `reboot,userspace` is answered with `INVALID_VALUE`, as in
  init (Android 17). Any other value that passes the checks every set
  gets (name, length, UTF-8, `property_contexts` type) is answered with
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
  has stopped gets `SET_FAILED`. The value is not logged (init logs it)
- **`selinux.restorecon_recursive`**: stored like any property. init
  runs a recursive restorecon on the path and stores the path when that
  is done, so a client that waits for the property to equal the path it
  set sees the same result; no restorecon runs here
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
  with a `ControlReply`, or refused without a handler. The handler is
  called for one request at a time, in the order the requests were
  read, as init handles control messages on its main thread, and the
  client is answered when the host answers the reply (in the call or
  after it) or drops it. A request that finds 102 others unanswered
  gets `HANDLE_CONTROL_MESSAGE`; init refuses a message when 101 are
  queued, whether or not one is being handled (the limits agree while a
  control message is in its call). The service sets no time limit for
  the reply, as init sets none for its control functions. Once the
  handler calls are stopped (a powerctl handler
  returned `PowerctlOutcome::ShuttingDown`, or
  `ServiceHandle::stop_handler_calls`) no handler is called: `ctl.stop`
  is answered with `PROP_SUCCESS` and every other action
  (`ctl.interface_stop` included) with `HANDLE_CONTROL_MESSAGE`, as in
  init during shutdown. **Not mirrored**: init leaves the messages
  still in its queue unanswered from then on, so their clients wait
  until the device goes down; here they get the same replies as a
  request read after the stop, and so does a request whose reply the
  host drops afterwards. init handles the messages in the order it
  read them; here that holds for the handler calls, and a host that
  passes the messages on to code of its own decides the order in which
  they are handled. init also checks the sender with
  SELinux before it queues a message, which is left to the handler
  here; see [Answering as init does](#answering-as-init-does)
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
  dropped instead and the service sets `2`
- **Message Format**: Compatible with Android's binary protocol

## Error Handling

`run` returns a `StartupError`: `stage()` names the step that failed
(`StartupStage::Config`, `WriterLock`, `PropertiesService`,
`SocketService`) and `source()` is the cause — `std::io::Error` from the
properties service, `rsproperties::Error` from `rsproperties::init`, the
writer lock or the socket service.

```rust,no_run
use rsproperties_service::{ServiceOptions, StartupStage};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = rsproperties::PropertyConfig::new()
        .properties_dir("/tmp/properties")
        .socket_dir("/tmp/sockets");

    match rsproperties_service::run(config, ServiceOptions::new()).await {
        Ok(service) => {
            println!("Service running");
            // Keep `service`: dropping the handle stops the service.
            service.shutdown().await?;
        }
        Err(e) => {
            // Displays as "<stage> failed to start: <cause>".
            eprintln!("{e}");
            let cause = std::error::Error::source(&e);
            if e.stage() == StartupStage::WriterLock
                && matches!(
                    cause.and_then(|c| c.downcast_ref::<rsproperties::Error>()),
                    Some(rsproperties::Error::Lock(_))
                )
            {
                eprintln!("another instance owns the properties dir");
            }
            // Walk the rest of the chain, e.g. down to the OS error.
            let mut next = cause.and_then(|c| c.source());
            while let Some(c) = next {
                eprintln!("  caused by: {c}");
                next = c.source();
            }
        }
    }
    Ok(())
}
```

## Performance Features

- **Concurrent Connections**: Each client connection handled in separate tasks
- **Non-blocking I/O**: All operations use async/await for optimal performance
- **Memory Efficient**: Property data shared between services using actor references
- **Fast Lookups**: Optimized property storage for quick access

## Directory Structure

The service expects this directory layout:

```text
properties_dir/
├── .writer_lock           # flock held by the running writer
├── property_info          # Property metadata (generated, replaced by rename)
├── properties_serial      # Property versioning
└── u:object_r:*:s0       # SELinux context files

socket_dir/
├── property_service                    # Main property socket
└── property_service_for_system        # System property socket
```

### File ownership: a release build runs as root

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

A debug build without `strict-file-validation` skips the ownership
check and logs one warning ("root-ownership check on property files is
disabled"), so a non-root service starts there. The check first applies
in the release build.

## Security & Hardening

### Hardening highlights

- **Socket permissions**: each socket's mode is set to `0o660` on the
  socket fd before `bind`, so the file exists only with `0o660` minus the
  umask's bits; if the umask cleared any of those bits, the mode is
  raised to exactly `0o660` through an `O_PATH|O_NOFOLLOW` descriptor
  (needs `/proc`: `/proc/thread-self/fd`, or `/proc/self/fd` on kernels
  before 3.17). Only then does the socket `listen` — a `connect`
  before that fails with `ECONNREFUSED`. Both sockets are bound as
  `.<name>.tmp-<pid>` and renamed onto their final names only after both
  listen. Stricter than AOSP init, which creates `property_service` 0666
  (relying on SELinux) and only `property_service_for_system` 0660.
- **Backpressure**: `accept()` does not wait for a permit. Each accepted
  connection must take a waiting-room permit with `try_acquire`
  (`MAX_WAITING_CLIENTS` = 256); when the waiting room is full the
  connection is closed immediately. The spawned task then waits up to
  `CLIENT_TIMEOUT` (10 s) for one of `MAX_CONCURRENT_CLIENTS` (64)
  handler permits and drops the connection on timeout. The accept loop
  never parks, and accepted fds are capped at 64 + 256. A `ctl.*`
  request gives its handler permit back once it is queued for the
  control handler: its connection then belongs to the `ControlReply`,
  and at
  most 102 of those exist (see
  [Answering as init does](#answering-as-init-does)), so the service
  holds at most 64 + 256 + 102 client fds.
- **`TimedOut` has an unknown outcome**: like AOSP init, the service
  applies every request it received in full, also when the client has
  already stopped waiting. The Linux client waits at most 2 s for the
  reply, so a `set` that returns `TimedOut` while waiting for the reply
  may have been applied (a `TimedOut` while connecting or sending means
  the request was not delivered). Retrying with the same value is
  harmless except for `ro.*`, where the retry gets
  `rsproperties::Error::SetRejected` (`SetRejection::ReadOnlyProperty`)
  if the first attempt was applied, and `ctl.*`, whose handler is
  called once per request also when the client has stopped waiting (a
  request stays queued after the client's `TimedOut`, and one the host
  holds stays with the host, so a retry can run the action twice). The
  service does not time a `ctl.*` reply out, so for `ctl.*` the 2 s are
  the time the host has to answer such a client; an answer sent later
  is discarded.
- **`accept()` back-off**: a 100 ms sleep is inserted after an `accept()`
  error so an `EMFILE`/`ENFILE` storm cannot spin the worker and flood
  the log.
- **Value masking**: `PropertyMessage::value` is masked in `Debug`
  output and service logs (`<N bytes>` placeholder). Property values
  may carry tokens or device identifiers; names are still logged in
  full.
- **Init errors propagate**: `run()` calls `rsproperties::init`, so a
  directory already set to a different path surfaces as a `Result`
  (`StartupStage::Config`) rather than silently leaving the service bound
  to whatever paths a previous instance committed.
- **Deterministic `build.prop` apply order**: entries are collected
  into a `BTreeMap` before apply, so the order in which they are added
  to the area (and thus the trie layout) no longer varies per run with
  `HashMap`'s hash seed. Which file wins a key conflict does not depend
  on this: `load_properties_from_file_with` overwrites in call order, so
  the later file wins (an entry the type check drops overwrites nothing).

### Size limits

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

### Other security features

- **Access control**: the socket file mode (`0o660`) plus the socket
  dir's permissions are the only access control. Any peer that can
  connect may set any non-`ro.` property, and any `ro.` property that
  has no value yet (the first writer fixes it), and send any `ctl.*`
  control message to the configured control handler and any
  `sys.powerctl` command to the configured powerctl handler. A handler
  that reboots the host must check the sender (`uid`, `gid`, `pid` from
  `SO_PEERCRED`) or the command itself.
- **Socket dir contract**: the service opens the socket dir once at
  startup and changes only entries directly in it named
  `property_service`, `property_service_for_system`, or
  `.<either name>.tmp-<pid>`, relative to that descriptor, never following
  a symlink at those names. `bind` itself takes a path: if the socket
  dir's path is swapped after startup opened it, the temp socket is
  created at the new location, startup fails, and that socket is left
  there. Any uid other than the service's and root that
  can write to the socket dir or one of its ancestors can replace the
  sockets with its own and so impersonate the service; that is out of
  scope. Keep the socket dir and its ancestors writable only by the
  service's user and root. A world-writable socket dir without the sticky
  bit is reported with a warning at startup; this is not an access check.
  A default ACL on the socket dir gives its named users and groups access
  beyond the mode bits; the service does not inspect or remove ACLs.
- **Single instance**: `run` takes the properties dir's writer lock before
  starting either actor, so a second `run` on the same properties dir
  fails (`StartupStage::WriterLock`) without touching `property_info` or
  the sockets. Run one instance per socket dir: two instances with
  different properties dirs must not share a socket dir.
- **property_contexts**: maps property names to contexts and types
  (values are type-checked); no per-property (SELinux or `SO_PEERCRED`)
  authorization is performed.
- **Permission handling**: respects file system permissions

## Logging

The service provides detailed logging at multiple levels:

```rust
// Enable logging
env_logger::Builder::from_env(
    env_logger::Env::default().default_filter_or("info")
).init();
```

Log levels:
- **ERROR**: Service failures, connection errors
- **WARN**: Killed (non-graceful) shutdowns, configuration issues
- **INFO**: Service lifecycle (including graceful shutdowns), property operations
- **DEBUG**: Client connections, message details
- **TRACE**: Protocol-level details, fine-grained operations

## Examples

### Running the Example Service

From the workspace root:

```bash
cargo run -p rsproperties-service --example example_service -- \
    --properties-dir /tmp/test_properties \
    --socket-dir /tmp/test_sockets
```

This is a debug build, which any user can run. With `--release` the
service has to run as root; see
[File ownership](#file-ownership-a-release-build-runs-as-root).

### Testing with netcat

```bash
# Connect to the property service socket
nc -U /tmp/test_sockets/property_service
```

## Testing

Run the comprehensive test suite:

```bash
# Run all tests
cargo test

# Run with logging
RUST_LOG=debug cargo test -- --nocapture

# Run a single integration-test binary (file under `tests/`)
cargo test --test integration_tests
cargo test --test performance_tests
```

`cargo test --release` and `cargo test --all-features` enforce the
root-ownership check (see
[File ownership](#file-ownership-a-release-build-runs-as-root)), so as a
non-root user every test that creates property files fails there. Build
as yourself and start the test binaries as root through cargo's runner,
as CI does:

```bash
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test --release
```

## Dependencies

- **tokio**: Async runtime and I/O
- **tokio-stream**: The listeners as streams in the accept loop
- **rustix**: Socket, file-mode and `flock` system calls
- **rsactor**: Actor framework for message passing
- **rsproperties**: Core property system implementation
- **log**: Logging framework
- **thiserror**: Error handling

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Contributing

Contributions are welcome! Please see the main [rsproperties](../README.md) project for contribution guidelines.

## Related Projects

- **[rsproperties](../rsproperties/)**: Core property system library
- **Android Property System**: Original implementation this project emulates
