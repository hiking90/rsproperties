// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Linux emulation of the Android system property service.
//!
//! Serves the `property_service` / `property_service_for_system` socket protocol
//! (SETPROP2 and legacy SETPROP) and maintains the property area files on a Linux
//! host, so `rsproperties` clients can `set`, `get` and `wait` without an Android
//! device. It is not usable on Android, where init owns the property service; the
//! crate compiles only for `target_os = "linux"` (on other targets the first error
//! names the reason).

#[cfg(not(target_os = "linux"))]
compile_error!(
    "rsproperties-service emulates the Android property service on Linux and builds only for \
     target_os = \"linux\". It is not usable on Android, where init owns the property service; \
     use the `rsproperties` client crate there."
);

use std::path::PathBuf;
use std::sync::Arc;

use rsactor::{Actor, ActorRef, ActorResult};

mod persistent_properties;
pub mod properties_service;
mod property_type;
pub mod socket_service;

pub use socket_service::{SocketService, SocketServiceArgs};

pub use properties_service::PropertiesService;

use properties_service::PropertiesServiceArgs;

/// A `ctl.*` set, which AOSP init handles as a control message instead of
/// storing it: `ctl.<action>` = `<target>`, e.g. `ctl.start` =
/// `my_service` (init `HandleControlMessage`). The service passes it to the
/// [`ControlHandler`] given in [`ServiceOptions::control_handler`] and
/// never writes it to a property area.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ControlMessage {
    /// The name after `ctl.`: `start`, `stop`, `restart`,
    /// `interface_start`, `sigstop_on`, ... (not checked against a list).
    pub action: String,
    /// The value: the service (or interface) name to act on.
    pub target: String,
    /// The sender's pid from `SO_PEERCRED`, when available.
    pub pid: Option<i32>,
}

/// Handles a [`ControlMessage`]; `true` reports success to the client
/// (`PROP_SUCCESS`), `false` reports `PROP_ERROR_HANDLE_CONTROL_MESSAGE`,
/// as init does when the service is unknown or the action fails.
///
/// Runs on tokio's blocking pool (`spawn_blocking`), one call per request,
/// so it may block, and calls can run concurrently. A panic is reported to
/// the client as a failure.
pub type ControlHandler = Arc<dyn Fn(&ControlMessage) -> bool + Send + Sync + 'static>;

/// What [`run`] serves besides the directories in its
/// `rsproperties::PropertyConfig`.
///
/// ```no_run
/// use rsproperties_service::ServiceOptions;
///
/// let options = ServiceOptions::new()
///     .property_contexts_files(["/etc/props/property_contexts"])
///     .build_prop_files(["/etc/props/build.prop"])
///     .persistent_properties_file("/var/lib/props/persistent_properties")
///     .control_handler(|msg| {
///         println!("ctl.{} {}", msg.action, msg.target);
///         true
///     });
/// ```
#[derive(Clone, Default)]
pub struct ServiceOptions {
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
    persistent_properties_file: Option<PathBuf>,
    control_handler: Option<ControlHandler>,
}

impl std::fmt::Debug for ServiceOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceOptions")
            .field("property_contexts_files", &self.property_contexts_files)
            .field("build_prop_files", &self.build_prop_files)
            .field(
                "persistent_properties_file",
                &self.persistent_properties_file,
            )
            .field("control_handler", &self.control_handler.is_some())
            .finish()
    }
}

impl ServiceOptions {
    /// No input files, no persistence, no control handler.
    pub fn new() -> Self {
        Self::default()
    }

    /// `property_contexts` files, parsed in order into `property_info`.
    pub fn property_contexts_files<I, P>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.property_contexts_files = files.into_iter().map(Into::into).collect();
        self
    }

    /// build.prop files, loaded in order (a later file overrides an
    /// earlier one's key).
    pub fn build_prop_files<I, P>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.build_prop_files = files.into_iter().map(Into::into).collect();
        self
    }

    /// Stores `persist.*` and `next_boot.*` properties in `path`, in AOSP
    /// init's `persistent_properties` format, and loads them at startup
    /// after the build.prop files (init's `load_persist_props`).
    ///
    /// - The directory of `path` must exist; startup fails otherwise.
    /// - Every successful set of such a name rewrites the file before the
    ///   client is answered: a temp file `<path>.tmp` (mode 0600) is
    ///   fsynced and renamed over `path`, then the directory is fsynced.
    ///   A failed write is logged and the set still succeeds, as in init.
    /// - A file that does not parse is deleted at startup, as init does.
    ///
    /// Without this option nothing is stored, and `persist.*` values last
    /// only as long as the property areas.
    pub fn persistent_properties_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.persistent_properties_file = Some(path.into());
        self
    }

    /// Handles `ctl.*` sets (see [`ControlHandler`]). Without a handler
    /// every `ctl.*` set is refused with
    /// `PROP_ERROR_HANDLE_CONTROL_MESSAGE`. Either way, `ctl.*` names are
    /// never stored.
    pub fn control_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(&ControlMessage) -> bool + Send + Sync + 'static,
    {
        self.control_handler = Some(Arc::new(handler));
        self
    }
}

pub(crate) struct ReadyMessage;

#[derive(Clone)]
pub(crate) struct PropertyMessage {
    pub name: String,
    pub value: String,
}

// Mask `value` in `Debug` output so log-level captures don't spill
// property contents. Property names are public knowledge (they cross the
// AOSP wire by name and appear in `getprop` output), but values may be
// sensitive — persisted tokens, device identifiers, configuration knobs.
// Logging `value.len()` is enough for diagnostics.
impl std::fmt::Debug for PropertyMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PropertyMessage")
            .field("name", &self.name)
            .field("value", &format_args!("<{} bytes>", self.value.len()))
            .finish()
    }
}

pub struct ServiceContext<T: Actor> {
    pub actor_ref: ActorRef<T>,
    pub join_handle: tokio::task::JoinHandle<ActorResult<T>>,
}

/// The startup step of [`run`] that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartupStage {
    /// `rsproperties::try_init` rejected the config (for example, the
    /// process-global directories were already set).
    Config,
    /// Taking the properties dir's writer lock
    /// ([`rsproperties::AreaWriterLock::acquire`]) failed. A second
    /// instance on the same properties dir fails here, with
    /// `rsproperties::Error::Lock` as the source.
    WriterLock,
    /// The properties service failed to start: reading the
    /// `property_contexts` or build.prop files, publishing `property_info`
    /// or creating the property areas. The source is a `std::io::Error`.
    PropertiesService,
    /// The socket service failed to start (socket dir or socket files).
    /// The source is an `rsproperties::Error`.
    SocketService,
}

impl std::fmt::Display for StartupStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Config => "configuration (try_init)",
            Self::WriterLock => "properties dir writer lock",
            Self::PropertiesService => "properties service",
            Self::SocketService => "socket service",
        })
    }
}

/// Error returned by [`run`]: the step that failed ([`stage`](Self::stage))
/// and its cause ([`source`](std::error::Error::source)).
///
/// The cause is the failed actor's own `on_start` error when it has one,
/// so `source()` can be downcast to it (`std::io::Error` for
/// [`StartupStage::PropertiesService`], `rsproperties::Error` for the other
/// stages). When an actor stopped without an error, the cause is a message
/// saying so.
///
/// `Display` prints `<stage> failed to start: <cause>`.
#[derive(Debug)]
pub struct StartupError {
    stage: StartupStage,
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl StartupError {
    pub(crate) fn new(
        stage: StartupStage,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self {
            stage,
            source: source.into(),
        }
    }

    /// The startup step that failed.
    pub fn stage(&self) -> StartupStage {
        self.stage
    }

    /// The cause, by value.
    pub fn into_source(self) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self.source
    }
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed to start: {}", self.stage, self.source)
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Runs the property socket service with the given configuration.
///
/// # Startup order
///
/// 1. `rsproperties::try_init(config)`.
/// 2. The properties dir's writer lock
///    ([`rsproperties::AreaWriterLock::acquire`]); this creates a missing
///    properties dir (the last component only). A second instance on the
///    same properties dir fails here with [`StartupStage::WriterLock`]
///    and `rsproperties::Error::Lock` as the source, before it replaces
///    `property_info` or touches any socket.
/// 3. Both actors are spawned together; the properties service gets the
///    lock and holds it for its lifetime. It reads every
///    `property_contexts` and build.prop file before it writes
///    `property_info` or any area file.
/// 4. `run` waits for the socket service, then for the properties service,
///    and returns when both are ready. If either fails, both are stopped
///    and `run` returns only after both actors have ended (the socket
///    service's final names are unlinked by then).
///
/// One service instance per socket dir: two instances with different
/// properties dirs and the same socket dir are not supported (the writer
/// lock does not cover the socket dir).
///
/// # Requests that arrive before the property areas exist
///
/// The socket service can be accepting connections while the properties
/// service is still reading its input files and creating the areas, so a
/// client that does not wait for `run` to return can send a `set` in that
/// window. Such a request is not rejected: it waits (in the properties
/// service's mailbox) and is applied once the areas exist, after the
/// build.prop entries, and only then is the client answered. The socket
/// service closes the connection without a reply after 10 s (up to 20 s
/// when all 64 handler slots are taken and the connection first waits for
/// one). A request already queued in the mailbox then is still applied once
/// the areas exist, like one whose client gave up on its own reply timeout;
/// one that was not yet queued (mailbox full) is dropped. If the properties
/// service fails to start,
/// the request is not applied and `run` returns
/// [`StartupStage::PropertiesService`]. A SETPROP2 client whose connection
/// was already accepted gets an error reply; one that connected after the
/// socket service's last accept sees the connection reset or refused
/// instead. A `get` in that window opens whatever is in the properties dir
/// at that moment (nothing, or a previous run's files), and a process's
/// first `get` fixes its reader for the process lifetime: if
/// `property_info` or `properties_serial` does not exist yet, every later
/// `get` in that process fails (see `rsproperties::try_system_properties`).
///
/// # Requirements
/// The parent of the properties dir must exist: only the properties dir's
/// last component is created (mode 0711 minus the umask's bits). A missing
/// socket dir is created with any missing parent (mode 0755 minus the
/// umask's bits); see [`SocketService`].
///
/// # Failure semantics
/// `rsproperties::try_init` commits process-global, first-write-wins
/// state. If a later startup step fails, that state stays committed (a
/// `OnceLock` cannot be un-set), so retrying `run` in the same process
/// with a *different* config will fail with "already initialized" —
/// treat a startup failure as fatal for the process.
///
/// [`StartupError`] is `Send + Sync` so the returned future can be
/// `tokio::spawn`ed and the error converted into `anyhow::Error` or
/// `Box<dyn Error + Send + Sync>`.
///
/// # Properties set at startup
///
/// After the build.prop entries, as AOSP init does: `ro.property_service.version`
/// = `2` (unless a build.prop file already set it), then the records of
/// [`ServiceOptions::persistent_properties_file`] (each checked like a
/// client set), then `ro.persistent_properties.ready` = `true` — also
/// without a persistent properties file, since init always sets it.
pub async fn run(
    config: rsproperties::PropertyConfig,
    options: ServiceOptions,
) -> Result<
    (
        ServiceContext<SocketService>,
        ServiceContext<PropertiesService>,
    ),
    StartupError,
> {
    // Use `try_init` rather than `init`: if the global properties_dir /
    // socket_dir cells were already committed (e.g. earlier service
    // instance, double-init, hostile race), the silent `init` swallow
    // would let the service start with the *previous* directories while
    // the caller believes their new config took effect. `?`-propagating
    // surfaces that drift at startup instead of producing a service bound
    // to wrong paths.
    rsproperties::try_init(config).map_err(|e| StartupError::new(StartupStage::Config, e))?;

    // The writer lock is taken before either actor is spawned: a second
    // instance on the same properties dir fails here, before it can rename
    // its sockets over the running instance's sockets or replace
    // `property_info`. (Spawning the socket service only after the
    // properties service is ready would also protect the sockets, but
    // would change the 0.6.0 startup order; the lock is enough.)
    let dir = rsproperties::properties_dir().to_path_buf();
    let lock = tokio::task::spawn_blocking(move || rsproperties::AreaWriterLock::acquire(&dir))
        .await
        .map_err(|e| StartupError::new(StartupStage::WriterLock, e))?
        .map_err(|e| StartupError::new(StartupStage::WriterLock, e))?;

    let ServiceOptions {
        property_contexts_files,
        build_prop_files,
        persistent_properties_file,
        control_handler,
    } = options;
    let mut properties_args = PropertiesServiceArgs::new(property_contexts_files, build_prop_files)
        .with_writer_lock(lock);
    if let Some(path) = persistent_properties_file {
        properties_args = properties_args.with_persistent_properties_file(path);
    }
    let properties_service = properties_service::spawn(properties_args);

    // Initialize the socket service
    let socket_service = socket_service::run(SocketServiceArgs {
        socket_dir: rsproperties::socket_dir().to_path_buf(),
        properties_service: properties_service.actor_ref.clone(),
        control_handler,
    });

    // Sequential readiness checks: the socket service is checked first, so
    // when both fail the socket failure is the one reported. Either failure
    // branch waits for both actors to end, so a socket failure is reported
    // only after the properties service's `on_start` has returned.
    // On failure, stop both actors explicitly instead of leaving them to
    // the implicit drop of the returned contexts — the caller never sees
    // the contexts on the error path.
    //
    // A failed `ask` only carries rsactor's mailbox error; the `on_start`
    // error that actually stopped the actor is delivered solely through its
    // join handle (rsactor's own log line for it goes to `tracing`, which
    // is not bridged to `log` here). Await the handle to return that cause.
    if let Err(e) = socket_service.actor_ref.ask(ReadyMessage).await {
        let _ = socket_service.actor_ref.stop().await;
        let _ = properties_service.actor_ref.stop().await;
        let err = startup_error(StartupStage::SocketService, e, socket_service.join_handle).await;
        // `stop` only enqueues the signal; the properties service sees it
        // after its `on_start` (input files, `property_info`, areas) ends.
        // Wait for that, so nothing writes the properties dir after `run`
        // returned.
        let _ = properties_service.join_handle.await;
        return Err(err);
    }
    if let Err(e) = properties_service.actor_ref.ask(ReadyMessage).await {
        let _ = socket_service.actor_ref.stop().await;
        let _ = properties_service.actor_ref.stop().await;
        let err = startup_error(
            StartupStage::PropertiesService,
            e,
            properties_service.join_handle,
        )
        .await;
        // The socket service passed `on_start` and is listening on both
        // final names; it handles the stop on its next loop iteration, and
        // dropping its join result drops the actor, which unlinks both
        // names. Wait for that, so a retry on the same socket dir does not
        // race this instance's unlink.
        let _ = socket_service.join_handle.await;
        return Err(err);
    }

    Ok((socket_service, properties_service))
}

/// Builds the [`StartupError`] for a failed actor: the cause is the
/// actor's own failure (normally its `on_start` error) when the join
/// handle has one, the `JoinError` when the task panicked or was
/// cancelled, else a message built from the `ask` error.
async fn startup_error<T: Actor>(
    stage: StartupStage,
    ask_error: rsactor::Error,
    join_handle: tokio::task::JoinHandle<ActorResult<T>>,
) -> StartupError
where
    T::Error: std::error::Error + Send + Sync + 'static,
{
    match join_handle.await {
        Ok(result) => match result.into_error() {
            Some(cause) => StartupError::new(stage, cause),
            None => StartupError::new(
                stage,
                format!("actor stopped before replying Ready: {ask_error}"),
            ),
        },
        Err(join) => StartupError::new(stage, join),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::time::{Duration, Instant};

    use rsproperties::wire::{PROP_MSG_SETPROP2, PROP_SUCCESS};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    use crate::socket_service::tests::{socket_suffix_len, socket_test_dir};

    /// `Debug` must not print the value (value-masking policy).
    #[test]
    fn test_property_message() {
        let msg = PropertyMessage {
            name: "test.key".to_string(),
            value: "test.value".to_string(),
        };
        let debug = format!("{msg:?}");
        assert!(!debug.contains("test.value"), "value leaked: {debug}");
        assert!(debug.contains("<10 bytes>"), "length missing: {debug}");
        assert!(debug.contains("test.key"), "name missing: {debug}");
    }

    /// `run` must return the actor's own `on_start` error, not rsactor's
    /// generic mailbox error, tagged with the stage that failed. The only
    /// test that calls `run` in this process (`try_init` is
    /// process-global); the early-request tests below call it in a child
    /// process.
    #[tokio::test]
    async fn run_reports_on_start_cause() {
        let dir = socket_test_dir("rsprops_svc_run_err", socket_suffix_len("sockets"));
        let _ = std::fs::remove_dir_all(&dir);
        let socket_dir = dir.join("sockets");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let missing = dir.join("missing_property_contexts");

        let config = rsproperties::PropertyConfig::with_both_dirs(dir.clone(), socket_dir);
        let result = run(
            config,
            ServiceOptions::new().property_contexts_files([missing]),
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);

        let err = match result {
            Ok(_) => panic!("run must fail for a missing property_contexts file"),
            Err(e) => e,
        };
        assert_eq!(err.stage(), StartupStage::PropertiesService, "{err}");
        let source = std::error::Error::source(&err).expect("StartupError has a source");
        assert!(
            source.downcast_ref::<std::io::Error>().is_some(),
            "expected the on_start io::Error, got: {source}"
        );
        let shown = err.to_string();
        assert!(
            shown.starts_with("properties service failed to start: "),
            "stage missing: {shown}"
        );
        assert!(
            shown.contains("missing_property_contexts"),
            "cause must name the missing file: {shown}"
        );
    }

    /// Env var that makes a re-executed test binary run the child half of
    /// the test it names.
    const CHILD_ENV: &str = "RSPROPS_SVC_LIB_CHILD";

    /// Whether this process is the child started by [`run_in_child`] for
    /// `test`.
    fn is_child(test: &str) -> bool {
        std::env::var(CHILD_ENV).is_ok_and(|v| v == test)
    }

    /// Runs `test` (its full path in this binary) in a new process of this
    /// test binary, so it can call `run`, and thus `try_init`, with its own
    /// dirs. Panics unless the child ran exactly that test and it passed.
    fn run_in_child(test: &str) {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env(CHILD_ENV, test)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child {test} failed ({}):\n{stdout}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Runs `body` on a fresh runtime and shuts the runtime down without
    /// waiting for blocking tasks: a failed assertion can leave the
    /// properties service blocked in `open` on the FIFO, and dropping the
    /// runtime would wait for that task forever.
    fn block_on_child(body: impl std::future::Future<Output = ()>) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(body)));
        rt.shutdown_background();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    /// Temp dir removed on drop.
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    type RunResult = Result<
        (
            ServiceContext<SocketService>,
            ServiceContext<PropertiesService>,
        ),
        StartupError,
    >;

    /// A `run` whose properties service is blocked before it creates the
    /// areas, plus a client connection that already sent a SETPROP2
    /// request.
    struct EarlyRequest {
        dir: TempDir,
        fifo: PathBuf,
        run: tokio::task::JoinHandle<RunResult>,
        stream: UnixStream,
    }

    /// Starts `run` with a FIFO as its only `property_contexts` file: the
    /// properties service blocks in `open` until [`release_fifo`] opens
    /// the write end, so it cannot create the areas before that. Connects
    /// to `property_service` once the socket service has bound it, sends
    /// `debug.early.set=1`, and checks that no reply has arrived and that
    /// `property_info` does not exist yet.
    async fn send_request_while_init_blocked(
        label: &str,
        build_prop_files: Vec<PathBuf>,
    ) -> EarlyRequest {
        let dir = TempDir(socket_test_dir(
            &format!("rsprops_svc_{label}"),
            socket_suffix_len("sockets"),
        ));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        let fifo = dir.0.join("property_contexts.fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let socket_dir = dir.0.join("sockets");
        let config =
            rsproperties::PropertyConfig::with_both_dirs(dir.0.clone(), socket_dir.clone());
        let run_task = tokio::spawn(run(
            config,
            ServiceOptions::new()
                .property_contexts_files([fifo.clone()])
                .build_prop_files(build_prop_files),
        ));

        let socket = socket_dir.join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match UnixStream::connect(&socket).await {
                Ok(stream) => break stream,
                Err(e) => {
                    assert!(
                        Instant::now() < deadline,
                        "the socket service must accept connections while the properties \
                         service is still starting: {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        };

        let (name, value) = ("debug.early.set", "1");
        let mut frame = Vec::new();
        frame.extend_from_slice(&PROP_MSG_SETPROP2.to_ne_bytes());
        frame.extend_from_slice(&(name.len() as u32).to_ne_bytes());
        frame.extend_from_slice(name.as_bytes());
        frame.extend_from_slice(&(value.len() as u32).to_ne_bytes());
        frame.extend_from_slice(value.as_bytes());
        stream.write_all(&frame).await.unwrap();

        // The properties service is still blocked on the FIFO, so nothing
        // may answer yet. (Not a timing assumption: no reply can exist
        // before `release_fifo`; the 200 ms only bounds how long we look.)
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stream.readable())
                .await
                .is_err(),
            "a request sent before the areas exist must not be answered before they do"
        );
        assert!(
            !dir.0.join("property_info").exists(),
            "the properties service must still be blocked before it publishes property_info"
        );

        EarlyRequest {
            dir,
            fifo,
            run: run_task,
            stream,
        }
    }

    /// Opens and closes the FIFO's write end: the blocked reader then reads
    /// EOF, i.e. an empty `property_contexts` file. A non-blocking write
    /// open fails with `ENXIO` until the reader has the FIFO open, so this
    /// retries until it does.
    async fn release_fifo(fifo: &Path) {
        use std::os::unix::fs::OpenOptionsExt;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as _)
                .open(fifo)
            {
                Ok(_writer) => return,
                Err(e)
                    if e.raw_os_error() == Some(rustix::io::Errno::NXIO.raw_os_error())
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("cannot open the write end of {fifo:?}: {e}"),
            }
        }
    }

    /// Reads the 4-byte status reply; `None` when the connection closed
    /// without one.
    async fn read_reply(stream: &mut UnixStream) -> Option<i32> {
        let mut buf = [0u8; 4];
        match tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf)).await {
            Ok(Ok(_)) => Some(i32::from_ne_bytes(buf)),
            Ok(Err(_)) => None,
            Err(_) => panic!("no reply within 10 s after the properties service was released"),
        }
    }

    /// Pins the documented behavior for a `set` that arrives while the
    /// properties service is still creating the areas: it waits, is applied
    /// once the areas exist, and is answered with success. Whether the
    /// request waited in the properties service's mailbox or was still
    /// unread in the socket when init finished is not controlled; both give
    /// this client-visible result.
    #[test]
    fn set_before_areas_exist_is_applied_after_init() {
        const TEST: &str = "tests::set_before_areas_exist_is_applied_after_init";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let mut early = send_request_while_init_blocked("early_set_ok", vec![]).await;
            release_fifo(&early.fifo).await;
            assert_eq!(read_reply(&mut early.stream).await, Some(PROP_SUCCESS));

            let (socket_service, properties_service) = match early.run.await.unwrap() {
                Ok(services) => services,
                Err(e) => panic!("run failed: {e}"),
            };
            assert_eq!(
                rsproperties::get::<String>("debug.early.set").unwrap(),
                "1",
                "the early set must be applied to the areas"
            );
            let _ = socket_service.actor_ref.stop().await;
            let _ = properties_service.actor_ref.stop().await;
            drop(early.dir);
        });
    }

    /// Pins the behavior when the properties service fails after a `set`
    /// arrived: the set is not applied (the client gets `PROP_ERROR`, or a
    /// closed connection if the socket service is stopped before it
    /// accepts the connection) and `run` reports
    /// [`StartupStage::PropertiesService`].
    #[test]
    fn set_before_failed_init_is_not_applied() {
        const TEST: &str = "tests::set_before_failed_init_is_not_applied";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let missing = std::env::temp_dir().join(format!(
                "rsprops_svc_early_set_missing_{}.prop",
                std::process::id()
            ));
            let mut early = send_request_while_init_blocked("early_set_fail", vec![missing]).await;
            release_fifo(&early.fifo).await;
            let reply = read_reply(&mut early.stream).await;
            assert_ne!(
                reply,
                Some(PROP_SUCCESS),
                "a set must not succeed when init fails"
            );

            let err = match early.run.await.unwrap() {
                Ok(_) => panic!("run must fail for a missing build.prop file"),
                Err(e) => e,
            };
            assert_eq!(err.stage(), StartupStage::PropertiesService, "{err}");
            assert!(
                err.to_string().contains("rsprops_svc_early_set_missing_"),
                "cause must name the missing file: {err}"
            );
            drop(early.dir);
        });
    }

    /// When the properties service fails after the socket service is
    /// listening, `run` must not return before the socket service has
    /// stopped and unlinked its final names; otherwise a retry on the same
    /// socket dir races that unlink. `run` is awaited directly on a
    /// current-thread runtime, so no other task runs between its return
    /// and the checks.
    #[test]
    fn properties_failure_returns_after_sockets_are_unlinked() {
        const TEST: &str = "tests::properties_failure_returns_after_sockets_are_unlinked";
        if !is_child(TEST) {
            run_in_child(TEST);
            return;
        }
        block_on_child(async {
            let dir = TempDir(socket_test_dir(
                "rsprops_svc_prop_fail",
                socket_suffix_len("sockets"),
            ));
            let _ = std::fs::remove_dir_all(&dir.0);
            std::fs::create_dir_all(&dir.0).unwrap();
            let contexts = dir.0.join("property_contexts");
            std::fs::write(&contexts, "").unwrap();
            let socket_dir = dir.0.join("sockets");
            let config =
                rsproperties::PropertyConfig::with_both_dirs(dir.0.clone(), socket_dir.clone());

            let result = run(
                config,
                ServiceOptions::new()
                    .property_contexts_files([contexts])
                    .build_prop_files([dir.0.join("missing.prop")]),
            )
            .await;
            let err = match result {
                Ok(_) => panic!("run must fail for a missing build.prop file"),
                Err(e) => e,
            };
            assert_eq!(err.stage(), StartupStage::PropertiesService, "{err}");
            for name in [
                rsproperties::PROPERTY_SERVICE_SOCKET_NAME,
                rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME,
            ] {
                assert!(
                    !socket_dir.join(name).exists(),
                    "{name} must be unlinked before run returns"
                );
            }
        });
    }
}
