// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What the service does with a client that goes away mid-request, the
//! service half of a dropped `asynchronous::set` (docs/async-wait-set.md
//! 3.6). The client half — a dropped exchange either sent no byte or the
//! whole frame — is tested in `tokio_transport`.
//!
//! - The whole frame, then the client closes without reading the reply:
//!   the set is applied.
//! - Part of the frame, then the client closes: nothing is applied.

mod common;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use common::init_test;
use rsproperties::wire::{PROP_ERROR_READ_DATA, PROP_MSG_SETPROP2};

/// The `PROP_MSG_SETPROP2` frame, as `rsproperties::set` builds it.
fn frame(name: &str, value: &str) -> Vec<u8> {
    let mut frame = PROP_MSG_SETPROP2.to_ne_bytes().to_vec();
    for s in [name, value] {
        frame.extend_from_slice(&(s.len() as u32).to_ne_bytes());
        frame.extend_from_slice(s.as_bytes());
    }
    frame
}

async fn connect() -> UnixStream {
    let path = rsproperties::socket_dir().join(rsproperties::PROPERTY_SERVICE_SOCKET_NAME);
    UnixStream::connect(&path)
        .await
        .expect("connect to property_service socket")
}

/// A client gone after the whole frame — a set dropped while it waited
/// for the reply — still has its set applied.
#[tokio::test]
async fn whole_frame_then_close_is_applied() -> anyhow::Result<()> {
    init_test().await;
    let (name, value) = ("test.cancel.after_frame", "applied");
    let mut stream = connect().await;
    stream.write_all(&frame(name, value)).await?;
    drop(stream);

    let props = rsproperties::asynchronous::system_properties()?;
    tokio::time::timeout(Duration::from_secs(5), props.wait_for_value(name, value)).await??;
    Ok(())
}

/// A client gone partway through the frame applies nothing. The write
/// half is shut down rather than the socket closed, so the reply and the
/// service's close can be read: once the read ends, the service is done
/// with the request.
#[tokio::test]
async fn partial_frame_then_close_is_not_applied() -> anyhow::Result<()> {
    init_test().await;
    let (name, value) = ("test.cancel.partial_frame", "never");
    let full = frame(name, value);
    // Everything but the last byte of the value.
    let partial = &full[..full.len() - 1];
    let mut stream = connect().await;
    stream.write_all(partial).await?;
    stream.shutdown().await?;

    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut reply)).await??;
    assert_eq!(reply, PROP_ERROR_READ_DATA.to_ne_bytes());
    assert!(
        rsproperties::get::<String>(name).is_err(),
        "a partial frame must not set {name}"
    );
    Ok(())
}
