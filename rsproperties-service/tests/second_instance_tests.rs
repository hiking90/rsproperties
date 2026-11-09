// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A second `run` on a properties dir that a running service owns must fail
//! on the writer lock without touching the running service's
//! `property_info` or sockets.
//!
//! `run` calls the process-global `try_init`, so the second instance runs in
//! a child process: this test binary re-executed with
//! [`CHILD_ENV`] set, running only this test.

mod common;

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rsproperties_service::StartupStage;

/// Set in the child process started by the parent half of the test.
const CHILD_ENV: &str = "RSPROPS_SECOND_INSTANCE_CHILD";

const TEST_NAME: &str = "second_instance_leaves_running_service_intact";

#[tokio::test]
async fn second_instance_leaves_running_service_intact() {
    if std::env::var_os(CHILD_ENV).is_some() {
        second_instance().await;
        return;
    }

    common::init_test().await;
    rsproperties::set("test.second.before", "1").unwrap();

    let dir = Path::new(common::TEST_PROPERTIES_DIR);
    let info = dir.join("property_info");
    let socket = dir
        .join("sockets")
        .join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
    let info_before = std::fs::read(&info).unwrap();
    let socket_ino_before = std::fs::symlink_metadata(&socket).unwrap().ino();

    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--test-threads=1", "--nocapture"])
        .env(CHILD_ENV, "1")
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
/// It must not call `common::init_test()`, which deletes and recreates
/// `__properties__`. The `Error::Lock` check relies on
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
async fn second_instance() {
    let config = rsproperties::PropertyConfig::with_both_dirs(
        common::TEST_PROPERTIES_DIR,
        Path::new(common::TEST_PROPERTIES_DIR).join("sockets"),
    );
    let err = match rsproperties_service::run(config, vec![], vec![]).await {
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
