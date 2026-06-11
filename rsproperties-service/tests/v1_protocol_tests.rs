// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! End-to-end test for the legacy V1 (`PROP_MSG_SETPROP`) wire protocol.
//!
//! V1 is a fixed-size frame — cmd word + `PROP_NAME_MAX` name bytes +
//! `PROP_VALUE_MAX` value bytes, NUL-padded — and the client treats the
//! server closing the connection as the ack (no status reply). The server
//! previously only understood SETPROP2, so a V1 client appeared to succeed
//! while the set was silently dropped.

mod common;
use common::init_test;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use rsproperties::wire::{PROP_MSG_SETPROP, PROP_NAME_MAX, PROP_VALUE_MAX};

fn fixed<const N: usize>(s: &str) -> [u8; N] {
    let mut buf = [0u8; N];
    buf[..s.len()].copy_from_slice(s.as_bytes());
    buf
}

#[tokio::test]
async fn test_v1_setprop_roundtrip() {
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
    stream.shutdown().await.expect("half-close write side");

    // V1 ack is the server closing the connection after processing.
    let mut sink = Vec::new();
    stream.read_to_end(&mut sink).await.expect("await close");

    // `handle_client` forwards to the properties service before returning,
    // so once the socket is closed the property must be visible.
    let read: String = rsproperties::get(name).expect("property set via V1");
    assert_eq!(read, value);
}
