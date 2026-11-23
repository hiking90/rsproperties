// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A service with an authorizer (`ServiceOptions::authorizer`) and a
//! `property_service` socket open to every uid: what the authorizer is
//! given for each kind of request, and what a client gets when it denies.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

use rsproperties::{Error, SetRejection};
use rsproperties_service::{Decision, Listener, ServiceOptions, SocketAccess};

/// One authorizer call: name, value, context, listener, (pid, uid, gid).
type Call = (String, String, String, Listener, (i32, u32, u32));

static CALLS: Mutex<Vec<Call>> = Mutex::new(Vec::new());
/// The `ctl.*` requests that reached the control handler: action, target.
static CONTROL: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// The context of a name when the service has no `property_contexts`
/// file: the default it builds `property_info` with.
const DEFAULT_CONTEXT: &str = "u:object_r:build_prop:s0";

fn options() -> ServiceOptions {
    ServiceOptions::new()
        // Refused at startup without an authorizer.
        .socket_access(Listener::Property, SocketAccess::new(0o666))
        .control_handler(|message, reply| {
            CONTROL
                .lock()
                .unwrap()
                .push((message.action, message.target));
            reply.send(true);
        })
        .authorizer(|request| {
            let peer = request.peer();
            CALLS.lock().unwrap().push((
                request.name().to_owned(),
                request.value().to_owned(),
                request.context().to_owned(),
                request.listener(),
                (peer.pid(), peer.uid(), peer.gid()),
            ));
            let allowed = match request.name() {
                "ctl.start" => request.value() == "allowed_service",
                name => name.starts_with("test.authz.allowed."),
            };
            if allowed {
                Decision::Allow
            } else {
                Decision::Deny
            }
        })
}

/// The calls made for `name`, removed from the record.
fn take_calls(name: &str) -> Vec<Call> {
    let mut calls = CALLS.lock().unwrap();
    let (taken, kept) = std::mem::take(&mut *calls)
        .into_iter()
        .partition(|call| call.0 == name);
    *calls = kept;
    taken
}

fn rejection(result: rsproperties::Result<()>) -> SetRejection {
    match result {
        Err(Error::SetRejected { reason, .. }) => reason,
        other => panic!("expected SetRejected, got {other:?}"),
    }
}

fn own_credentials() -> (i32, u32, u32) {
    (
        std::process::id() as i32,
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_mode_for_others_is_applied() {
    common::init_test_with(options).await;
    let dir = common::socket_dir();
    for (name, mode) in [
        (rsproperties::PROPERTY_SERVICE_SOCKET_NAME, 0o666),
        (rsproperties::PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME, 0o660),
    ] {
        let found = std::fs::metadata(dir.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(found, mode, "{name}: {found:#o}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allowed_set_is_stored_and_denied_set_is_not() -> anyhow::Result<()> {
    common::init_test_with(options).await;

    rsproperties::set("test.authz.allowed.x", "value")?;
    assert_eq!(
        rsproperties::get::<String>("test.authz.allowed.x")?,
        "value"
    );
    assert_eq!(
        take_calls("test.authz.allowed.x"),
        [(
            "test.authz.allowed.x".to_owned(),
            "value".to_owned(),
            DEFAULT_CONTEXT.to_owned(),
            Listener::Property,
            own_credentials(),
        )]
    );

    assert_eq!(
        rejection(rsproperties::set("test.authz.denied.x", "value")),
        SetRejection::PermissionDenied
    );
    assert!(rsproperties::get::<String>("test.authz.denied.x").is_err());
    assert_eq!(take_calls("test.authz.denied.x").len(), 1);

    // `ro.` is write-once for clients unless the policy denies it.
    assert_eq!(
        rejection(rsproperties::set("ro.test.authz.x", "1")),
        SetRejection::PermissionDenied
    );
    assert!(rsproperties::get::<String>("ro.test.authz.x").is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_request_is_authorized_before_it_is_queued() -> anyhow::Result<()> {
    common::init_test_with(options).await;

    rsproperties::set("ctl.start", "allowed_service")?;
    assert_eq!(
        rejection(rsproperties::set("ctl.start", "other_service")),
        SetRejection::ControlMessage
    );
    assert_eq!(
        rejection(rsproperties::set("ctl.stop", "allowed_service")),
        SetRejection::ControlMessage
    );
    assert_eq!(
        *CONTROL.lock().unwrap(),
        [("start".to_owned(), "allowed_service".to_owned())]
    );

    let calls = take_calls("ctl.start");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, "allowed_service");
    assert_eq!(calls[0].2, DEFAULT_CONTEXT);
    assert_eq!(calls[0].4, own_credentials());
    assert_eq!(take_calls("ctl.stop").len(), 1);
    Ok(())
}

/// The client sends `sys.powerctl` to `property_service_for_system`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn powerctl_is_authorized_with_its_listener() {
    common::init_test_with(options).await;

    assert_eq!(
        rejection(rsproperties::set("sys.powerctl", "reboot,test")),
        SetRejection::PermissionDenied
    );
    let calls = take_calls("sys.powerctl");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "reboot,test");
    assert_eq!(calls[0].3, Listener::System);
}
