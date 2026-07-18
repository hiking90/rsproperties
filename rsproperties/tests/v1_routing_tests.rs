// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Regression tests for V1 (`PROP_MSG_SETPROP`) client-side socket routing.
//!
//! bionic's `PropertyServiceConnection` is constructed from the *property
//! name* and routes `sys.powerctl` to the `property_service_for_system`
//! socket. The V1 arm previously passed the literal socket name instead of
//! the property name, so `sys.powerctl` never reached the for_system
//! socket. These tests run fake listeners and assert where the frame lands.
//!
//! One #[test] fn with sequential phases: the socket dir and protocol
//! version latch process-wide, and the phases share the two socket paths.

#![cfg(not(target_os = "android"))]

use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use rsproperties::wire::{PROP_MSG_SETPROP, PROP_NAME_MAX, PROP_VALUE_MAX};
use rsproperties::{
    PropertyConfig, PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME, PROPERTY_SERVICE_SOCKET_NAME,
};

const V1_FRAME_LEN: usize = 4 + PROP_NAME_MAX + PROP_VALUE_MAX;

/// Accepts one connection, reads a full V1 frame, and closes the stream
/// (the close is the V1 ack). Non-blocking accept with a deadline so a
/// routing bug fails the test instead of hanging it.
fn serve_one_v1_frame(listener: UnixListener) -> std::thread::JoinHandle<Vec<u8>> {
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream: UnixStream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "no client connected to this socket within 5s"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut frame = vec![0u8; V1_FRAME_LEN];
        stream.read_exact(&mut frame).expect("read full V1 frame");
        frame // dropping the stream closes it — the V1 implicit ack
    })
}

fn frame_cmd(frame: &[u8]) -> u32 {
    u32::from_ne_bytes(frame[..4].try_into().unwrap())
}

fn frame_cstr(field: &[u8]) -> &str {
    let nul = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    std::str::from_utf8(&field[..nul]).unwrap()
}

fn assert_no_pending_client(listener: &UnixListener, label: &str) {
    listener.set_nonblocking(true).unwrap();
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("unexpected client connected to the {label} socket"),
        Err(e) => panic!("accept failed on {label}: {e}"),
    }
}

#[test]
fn test_v1_sys_powerctl_routes_to_for_system_socket() {
    // Force V1 before anything can latch the protocol version. The
    // property store is never initialized in this process, so the version
    // is re-derived from the env on every `set`.
    std::env::set_var("PROPERTY_SERVICE_VERSION", "1");

    let dir = std::env::temp_dir().join(format!("rsprops_v1route_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    rsproperties::init(PropertyConfig::with_socket_dir(&dir));

    let plain_path = dir.join(PROPERTY_SERVICE_SOCKET_NAME);
    let system_path = dir.join(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME);

    // Phase 1 — with both sockets present, `sys.powerctl` must go to the
    // for_system socket and ONLY there.
    let plain = UnixListener::bind(&plain_path).unwrap();
    let system = UnixListener::bind(&system_path).unwrap();
    let server = serve_one_v1_frame(system);

    rsproperties::set("sys.powerctl", "reboot").expect("V1 set over fake service");

    let frame = server.join().expect("server thread panicked");
    assert_eq!(frame_cmd(&frame), PROP_MSG_SETPROP);
    assert_eq!(frame_cstr(&frame[4..4 + PROP_NAME_MAX]), "sys.powerctl");
    assert_eq!(frame_cstr(&frame[4 + PROP_NAME_MAX..]), "reboot");
    assert_no_pending_client(&plain, "plain property_service");

    // Phase 2 — with the for_system socket gone, `sys.powerctl` must fall
    // back to the plain socket (old devices without the dedicated socket).
    remove_socket(&system_path);
    let server = serve_one_v1_frame(plain);
    rsproperties::set("sys.powerctl", "shutdown").expect("V1 fallback set");
    let frame = server.join().expect("server thread panicked");
    assert_eq!(frame_cstr(&frame[4..4 + PROP_NAME_MAX]), "sys.powerctl");
    assert_eq!(frame_cstr(&frame[4 + PROP_NAME_MAX..]), "shutdown");

    // Phase 3 — an ordinary property must use the plain socket even while
    // the for_system socket exists. (The phase-2 listener is gone but its
    // socket file remains — unlink before rebinding.)
    remove_socket(&plain_path);
    let plain = UnixListener::bind(&plain_path).unwrap();
    let system = UnixListener::bind(&system_path).unwrap();
    let server = serve_one_v1_frame(plain);
    rsproperties::set("test.v1.other", "v").expect("V1 ordinary set");
    let frame = server.join().expect("server thread panicked");
    assert_eq!(frame_cstr(&frame[4..4 + PROP_NAME_MAX]), "test.v1.other");
    assert_no_pending_client(&system, "for_system");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Unlinks a socket path so a later bind can reclaim it.
fn remove_socket(path: &Path) {
    std::fs::remove_file(path).expect("remove socket file");
}
