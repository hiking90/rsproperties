// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `rsproperties::asynchronous` against a running service: `set` stores
//! what `get` reads back, a refusal is the sync `set`'s `SetRejected`, and
//! a spawned `set` wakes an async wait on another task.

mod common;

use std::time::Duration;

use common::init_test;
use rsproperties::{Error, SetRejection};

#[tokio::test]
async fn async_set_then_get() -> anyhow::Result<()> {
    init_test().await;
    rsproperties::asynchronous::set("test.async_set.basic", "value").await?;
    assert_eq!(
        rsproperties::get::<String>("test.async_set.basic")?,
        "value"
    );
    rsproperties::asynchronous::set("test.async_set.number", 42).await?;
    assert_eq!(rsproperties::get::<i32>("test.async_set.number")?, 42);
    Ok(())
}

#[tokio::test]
async fn async_set_refusal_matches_sync() -> anyhow::Result<()> {
    init_test().await;
    rsproperties::asynchronous::set("ro.test.async_set.once", "1").await?;
    let err = rsproperties::asynchronous::set("ro.test.async_set.once", "2")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::SetRejected {
                reason: SetRejection::ReadOnlyProperty,
                ..
            }
        ),
        "{err}"
    );
    let sync = rsproperties::set("ro.test.async_set.once", "2").unwrap_err();
    assert_eq!(err.to_string(), sync.to_string());
    assert_eq!(rsproperties::get::<String>("ro.test.async_set.once")?, "1");
    Ok(())
}

/// Both futures go to `tokio::spawn` (`Send + 'static`), on a runtime with
/// several workers: the set's reply and the wait's wake both arrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_set_wakes_an_async_wait() -> anyhow::Result<()> {
    init_test().await;
    let props = rsproperties::asynchronous::system_properties()?;
    let waiting = tokio::spawn(props.wait_for_value("test.async_set.spawned", "done"));
    // Let the wait block first, so the wake comes from the waiter thread.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished());

    tokio::spawn(rsproperties::asynchronous::set(
        "test.async_set.spawned",
        "done",
    ))
    .await??;
    tokio::time::timeout(Duration::from_secs(5), waiting).await???;
    Ok(())
}
