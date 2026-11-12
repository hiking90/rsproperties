// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A second `run` on a properties dir that a running service owns must fail
//! on the writer lock without touching the running service's
//! `property_info` or sockets.
//!
//! `run` calls the process-global `rsproperties::init`, so the second
//! instance (with its own directories) runs in
//! a child process: this test binary re-executed with
//! [`CHILD_ENV`] set to the parent's properties dir, running only this test.

mod common;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rsproperties_service::StartupStage;

/// Set in the child process started by the parent half of the test, to
/// the properties dir of the parent's service. The dir's name contains the
/// parent's pid, so the child cannot derive it.
const CHILD_ENV: &str = "RSPROPS_SECOND_INSTANCE_CHILD";

const TEST_NAME: &str = "second_instance_leaves_running_service_intact";

#[tokio::test]
async fn second_instance_leaves_running_service_intact() {
    if let Some(parent_dir) = std::env::var_os(CHILD_ENV) {
        second_instance(&PathBuf::from(parent_dir)).await;
        return;
    }

    common::init_test().await;
    rsproperties::set("test.second.before", "1").unwrap();

    let dir = common::properties_dir();
    let info = dir.join("property_info");
    let socket = common::socket_dir().join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
    let info_before = std::fs::read(&info).unwrap();
    let socket_ino_before = std::fs::symlink_metadata(&socket).unwrap().ino();

    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--test-threads=1", "--nocapture"])
        .env(CHILD_ENV, dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "second instance child failed ({}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        std::fs::read(&info).unwrap(),
        info_before,
        "the second instance replaced property_info"
    );
    let socket_ino_after = std::fs::symlink_metadata(&socket)
        .expect("the second instance removed the running service's socket")
        .ino();
    assert_eq!(
        socket_ino_after, socket_ino_before,
        "the second instance replaced the running service's socket"
    );
    rsproperties::set("test.second.after", "2").unwrap();
    assert_eq!(
        rsproperties::get::<String>("test.second.after").unwrap(),
        "2"
    );
    assert_eq!(
        rsproperties::get::<String>("test.second.before").unwrap(),
        "1"
    );
}

/// The child half: `run` on the dirs the parent's service owns.
///
/// It must not call `common::init_test()` or `common::properties_dir()`:
/// in the child they name a new directory (the child's pid), where a `run`
/// succeeds. The `Error::Lock` check relies on
/// `AreaWriterLock::acquire` returning that error unwrapped (not inside
/// `Error::Context`).
///
/// Why the check is deterministic: `run` takes the writer lock before it
/// spawns either actor, so without the lock no socket would be touched;
/// were the lock missing from `run`, `run` would wait for the socket
/// service to become ready, which renames the child's sockets over the
/// parent's, before it handled the properties service's lock failure (and
/// stopping the child's socket service then unlinks them). If `run` stops
/// waiting for the socket service first, this test can stop detecting that
/// regression.
async fn second_instance(parent_dir: &Path) {
    // The same layout as `common::socket_dir`.
    let config = rsproperties::PropertyConfig::new()
        .properties_dir(parent_dir)
        .socket_dir(parent_dir.join("sockets"));
    let err = match rsproperties_service::run(config, rsproperties_service::ServiceOptions::new())
        .await
    {
        Ok(_) => panic!("a second instance on the same properties dir must fail"),
        Err(e) => e,
    };
    assert_eq!(err.stage(), StartupStage::WriterLock, "{err}");
    let source = std::error::Error::source(&err).expect("StartupError has a source");
    assert!(
        matches!(
            source.downcast_ref::<rsproperties::Error>(),
            Some(rsproperties::Error::Lock(_))
        ),
        "expected rsproperties::Error::Lock, got: {source}"
    );
}
