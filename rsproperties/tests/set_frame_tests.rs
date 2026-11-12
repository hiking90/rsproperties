// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The V2 (`PROP_MSG_SETPROP2`) frames `set` sends, checked byte for byte
//! against a fake service.
//!
//! Scope: only what the client writes to the socket and how it maps the
//! status reply. The fake service does not store anything, so a `set` →
//! `get` round trip is not checked here; the rsproperties-service
//! integration tests run the real service for that. `sys.powerctl`
//! routing is covered by `v1_routing_tests.rs`.
//!
//! One `#[test]` fn with sequential phases: the socket dir latches
//! process-wide. The property store is never initialized in this process,
//! so the protocol version comes from `PROPERTY_SERVICE_VERSION` (removed
//! below) and defaults to V2.

#![cfg(not(target_os = "android"))]

mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use rsproperties::wire::{
    PROP_ERROR, PROP_ERROR_INVALID_VALUE, PROP_MSG_SETPROP2, PROP_SUCCESS, PROP_VALUE_MAX,
};
use rsproperties::{Error, PropertyConfig, SetRejection, PROPERTY_SERVICE_SOCKET_NAME};

/// What the fake service read from one connection.
#[derive(Debug)]
struct Frame {
    cmd: u32,
    name: Vec<u8>,
    value: Vec<u8>,
    /// Bytes the client sent after the value, up to its close.
    trailing: Vec<u8>,
}

fn read_u32(stream: &mut impl Read) -> u32 {
    let mut word = [0u8; 4];
    stream.read_exact(&mut word).expect("read u32");
    u32::from_ne_bytes(word)
}

fn read_bytes(stream: &mut impl Read, len: u32) -> Vec<u8> {
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).expect("read string payload");
    buf
}

/// Accepts one connection, reads one V2 frame (`u32 cmd`, then name and
/// value, each as `u32 len` + bytes), replies `reply`, and reads to EOF.
/// Accept and reads have a 5 s deadline so a client bug fails the test
/// instead of hanging it.
fn serve_one_v2_frame(listener: &UnixListener, reply: i32) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "no client connected within 5s");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("accept failed: {e}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let cmd = read_u32(&mut stream);
    let name_len = read_u32(&mut stream);
    let name = read_bytes(&mut stream, name_len);
    let value_len = read_u32(&mut stream);
    let value = read_bytes(&mut stream, value_len);
    stream.write_all(&reply.to_ne_bytes()).expect("write reply");
    let mut trailing = Vec::new();
    stream
        .read_to_end(&mut trailing)
        .expect("read until the client closes");
    Frame {
        cmd,
        name,
        value,
        trailing,
    }
}

/// Runs `set(name, value)` against a fake service that replies `reply`.
fn set_against(
    listener: &UnixListener,
    name: &str,
    value: &str,
    reply: i32,
) -> (Frame, Result<(), Error>) {
    std::thread::scope(|s| {
        let server = s.spawn(|| serve_one_v2_frame(listener, reply));
        let result = rsproperties::set(name, value);
        (server.join().expect("fake service panicked"), result)
    })
}

#[test]
fn test_v2_set_frames() {
    // V2 is the default when neither the store nor the env names a
    // version; make sure an inherited env does not force V1.
    std::env::remove_var("PROPERTY_SERVICE_VERSION");

    let dir = common::socket_test_dir("rsprops_setframe", 1 + PROPERTY_SERVICE_SOCKET_NAME.len());
    std::fs::create_dir_all(dir.path()).unwrap();
    rsproperties::init(PropertyConfig::new().socket_dir(dir.path())).expect("init");

    let listener = UnixListener::bind(dir.path().join(PROPERTY_SERVICE_SOCKET_NAME)).unwrap();
    listener.set_nonblocking(true).unwrap();

    // Phase 1 — accepted inputs reach the service unchanged.
    let long_ro = "r".repeat(1000);
    let max_short = "x".repeat(PROP_VALUE_MAX - 1);
    let ro_at_max = "y".repeat(PROP_VALUE_MAX);
    let cases: Vec<(&str, &str)> = vec![
        ("edge.value.empty", ""),
        ("edge.value.space", " "),
        ("edge.value.spaces", "  "),
        ("edge.value.tab", "\t"),
        ("edge.value.newline", "\n"),
        ("edge.value.cr", "\r"),
        ("edge.value.crlf", "\r\n"),
        ("edge.value.inner_spaces", "value with spaces"),
        ("edge.value.inner_tabs", "value\twith\ttabs"),
        ("edge.value.dquote", "\"quoted value\""),
        ("edge.value.squote", "'single quoted'"),
        ("edge.value.equals", "value=with=equals"),
        ("edge.value.colons", "value:with:colons"),
        ("edge.value.semicolons", "value;with;semicolons"),
        ("edge.value.slashes", "value/with/slashes"),
        ("edge.value.backslashes", "value\\with\\backslashes"),
        ("edge.value.unicode", "value with unicode: 🚀🌟⭐"),
        ("edge.value.latin1", "üñíçødé"),
        ("edge.value.zero", "0"),
        ("edge.value.false", "false"),
        ("edge.value.true", "true"),
        ("edge.value.negative", "-1"),
        ("edge.value.decimal", "3.14159"),
        ("edge.value.scientific", "1e10"),
        ("edge.value.control", "value\x01with\x02control\x03chars"),
        ("edge.value.del", "value\x7Fwith\x7Fdel"),
        ("edge.value.escape", "value\x1Bwith\x1Bescape"),
        // Longest non-`ro.` value.
        ("edge.maxlen", max_short.as_str()),
        // `ro.` values may be longer than PROP_VALUE_MAX - 1.
        ("ro.edge.long92", ro_at_max.as_str()),
        ("ro.edge.long1000", long_ro.as_str()),
        // `@` and `:` are legal name characters (HIDL-style names).
        ("vendor.hw@1.0:x", "v"),
    ];
    for (name, value) in cases {
        let (frame, result) = set_against(&listener, name, value, PROP_SUCCESS);
        result.unwrap_or_else(|e| panic!("set({name:?}) failed: {e}"));
        assert_eq!(frame.cmd, PROP_MSG_SETPROP2, "{name}: cmd");
        assert_eq!(frame.name, name.as_bytes(), "{name}: name bytes");
        assert_eq!(frame.value, value.as_bytes(), "{name}: value bytes");
        assert!(
            frame.trailing.is_empty(),
            "{name}: bytes after the frame: {:?}",
            frame.trailing
        );
    }

    // Phase 2 — a non-success status becomes `SetRejected` with that
    // code: one with a variant, the 0.6.0 service's -1, and one without.
    for (status, expected) in [
        (PROP_ERROR_INVALID_VALUE, SetRejection::InvalidValue),
        (PROP_ERROR, SetRejection::Unspecified),
        (0x7abc, SetRejection::Unknown(0x7abc)),
    ] {
        let (frame, result) = set_against(&listener, "edge.rejected", "v", status);
        assert_eq!(frame.name, b"edge.rejected");
        match result {
            Err(Error::SetRejected { name, reason }) => {
                assert_eq!(name, "edge.rejected");
                assert_eq!(reason, expected);
                assert_eq!(reason.code(), status);
            }
            other => panic!("expected SetRejected, got {other:?}"),
        }
    }

    drop(listener);
}
