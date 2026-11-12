//! Shared setup for the integration tests that talk to a running service.
//!
//! Each test binary runs one service on its own directories,
//! `<base>/rsprops_svcit_<test>_<pid>` and `sockets/` below it. The pid
//! keeps two processes off each other's directories: with one path shared
//! by every binary, a second `cargo test` in the same checkout deleted the
//! directories of a running service and then failed on its writer lock.

use std::fs::{create_dir_all, remove_dir_all};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tokio::sync::OnceCell;

use rsproperties::PropertyConfig;

use rsproperties_service::ServiceHandle;

static SERVICE: OnceCell<()> = OnceCell::const_new();

/// The properties dir of this process's service.
///
/// A child process that must reach the same service (`second_instance_tests`)
/// gets this path from its parent: calling this in the child names a
/// different directory, because the pid differs.
pub fn properties_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(fixture_dir)
}

/// The socket dir of this process's service.
pub fn socket_dir() -> PathBuf {
    properties_dir().join("sockets")
}

/// `sockaddr_un.sun_path` is 108 bytes and std's `UnixListener::bind` /
/// `UnixStream::connect` need a NUL in it, so a socket path has at most
/// 107 bytes.
const SUN_PATH_LEN: usize = 108;

/// Picks `<base>/rsprops_svcit_<test>_<pid>` and removes the directories
/// that dead processes of the same test left behind.
///
/// `<test>` is the executable stem without cargo's `-<hash>` suffix, so a
/// run also removes what the same test built with other features left.
/// `base` is `temp_dir()` when the socket paths fit in `sun_path` there,
/// otherwise `/tmp`; the test fails with the reason when neither fits.
fn fixture_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("test executable name is UTF-8");
    let test = stem.rsplit_once('-').map_or(stem, |(test, _hash)| test);
    let prefix = format!("rsprops_svcit_{test}_");
    let pid = std::process::id().to_string();
    let name = format!("{prefix}{pid}");

    // The longest path the service creates below the properties dir: the
    // temp socket it binds before the rename,
    // `sockets/.property_service_for_system.tmp-<pid>`.
    let suffix_len = "/sockets/.".len()
        + rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME.len()
        + ".tmp-".len()
        + pid.len();
    let fits = |base: &Path| base.as_os_str().len() + 1 + name.len() + suffix_len < SUN_PATH_LEN;

    let preferred = std::env::temp_dir();
    let base = if fits(&preferred) {
        preferred
    } else if fits(Path::new("/tmp")) {
        PathBuf::from("/tmp")
    } else {
        panic!(
            "{name:?} plus {suffix_len} bytes of socket path does not fit in the \
             {SUN_PATH_LEN}-byte sun_path under {preferred:?} or /tmp"
        );
    };

    sweep_dead_fixtures(&base, &prefix);
    base.join(name)
}

/// Removes every `<base>/<prefix><pid>` whose process is gone (`kill(pid,
/// 0)` fails with `ESRCH`).
///
/// The service runs until the process exits, so nothing removes its
/// directories in the same run; the next run of the test does. The
/// directories of a live process — another `cargo test` running the same
/// test — are kept. A reused pid keeps a dead directory: that leaks one
/// directory but never deletes a live one.
fn sweep_dead_fixtures(base: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let pid = entry
            .file_name()
            .to_str()
            .and_then(|n| n.strip_prefix(prefix))
            .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|p| p.parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw);
        let Some(pid) = pid else { continue };
        if rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH) {
            let _ = remove_dir_all(entry.path());
        }
    }
}

async fn start_service() -> ServiceHandle {
    let _ = env_logger::builder().is_test(true).try_init();

    let properties_dir = properties_dir();
    let socket_dir = socket_dir();

    // A leftover at our own path: a dead process that had this pid.
    remove_dir_all(properties_dir).unwrap_or_default();
    create_dir_all(properties_dir).expect("Failed to create properties directory");
    create_dir_all(&socket_dir).expect("Failed to create socket directory");

    let config = PropertyConfig::new()
        .properties_dir(properties_dir)
        .socket_dir(socket_dir);

    rsproperties_service::run(config, rsproperties_service::ServiceOptions::new())
        .await
        .expect("Failed to start services")
}

/// Starts the service once per test binary, on its own thread and
/// runtime, and returns when it is ready. The service runs until the
/// process exits.
pub async fn init_test() {
    SERVICE
        .get_or_init(|| async {
            let (sender, receiver) = tokio::sync::oneshot::channel::<()>();

            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create Tokio runtime");

                runtime.block_on(async {
                    let mut service = start_service().await;
                    let _ = sender.send(());

                    if let Err(e) = service.join().await {
                        eprintln!("Property service error: {e}");
                    }
                });
            });

            receiver.await.expect("Failed to receive services")
        })
        .await;
}
