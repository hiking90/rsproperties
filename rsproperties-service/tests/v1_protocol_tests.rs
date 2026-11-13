// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! End-to-end test for the legacy V1 (`PROP_MSG_SETPROP`) wire protocol,
//! which the service does not support.
//!
//! V1 is a fixed-size frame — cmd word + `PROP_NAME_MAX` name bytes +
//! `PROP_VALUE_MAX` value bytes, NUL-padded. The service answers it like an
//! unknown command (`PROP_ERROR_INVALID_CMD`) and sets nothing. AOSP init
//! applies a V1 set and closes the connection without a reply.

mod common;
use common::init_test;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use rsproperties::wire::{PROP_ERROR_INVALID_CMD, PROP_MSG_SETPROP, PROP_NAME_MAX, PROP_VALUE_MAX};

fn fixed<const N: usize>(s: &str) -> [u8; N] {
    let mut buf = [0u8; N];
    buf[..s.len()].copy_from_slice(s.as_bytes());
    buf
}

#[tokio::test]
async fn test_v1_setprop_is_refused() {
    let _ = init_test().await;

    let socket_path = rsproperties::socket_dir().join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect to property_service socket");

    let name = "test.v1.property";
    let value = "v1_value";

    let mut msg = Vec::with_capacity(4 + PROP_NAME_MAX + PROP_VALUE_MAX);
    msg.extend_from_slice(&PROP_MSG_SETPROP.to_ne_bytes());
    msg.extend_from_slice(&fixed::<PROP_NAME_MAX>(name));
    msg.extend_from_slice(&fixed::<PROP_VALUE_MAX>(value));
    stream.write_all(&msg).await.expect("send V1 frame");

    // Only the 4-byte reply is read. The service closes the connection
    // without reading the name and value, so a read past the reply can
    // fail with `ECONNRESET` instead of returning EOF.
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await.expect("read the reply");
    assert_eq!(i32::from_ne_bytes(reply), PROP_ERROR_INVALID_CMD);

    // The service sends its reply after it has decided what to do with
    // the request, so a set made for this frame would be visible now.
    assert!(
        rsproperties::get::<String>(name).is_err(),
        "a V1 frame must not set {name}"
    );
}
