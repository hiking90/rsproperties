// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Async property socket service implementation for Android system properties
//!
//! This crate provides a tokio-based async implementation of the Android property
//! socket service, allowing for non-blocking property value reception and parsing.

use std::path::PathBuf;

use rsactor::{Actor, ActorRef, ActorResult};

pub mod properties_service;
pub mod socket_service;

pub use socket_service::{SocketService, SocketServiceArgs};

pub use properties_service::PropertiesService;

pub(crate) struct ReadyMessage;

#[derive(Clone)]
pub(crate) struct PropertyMessage {
    pub name: String,
    pub value: String,
}

// Mask `value` in `Debug` output so log-level captures don't spill
// property contents. Property names are public knowledge (they cross the
// AOSP wire by name and appear in `getprop` output), but values may be
// sensitive — persisted tokens, device identifiers, configuration knobs.
// Logging `value.len()` is enough for diagnostics.
impl std::fmt::Debug for PropertyMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PropertyMessage")
            .field("name", &self.name)
            .field("value", &format_args!("<{} bytes>", self.value.len()))
            .finish()
    }
}

pub struct ServiceContext<T: Actor> {
    pub actor_ref: ActorRef<T>,
    pub join_handle: tokio::task::JoinHandle<ActorResult<T>>,
}

/// Runs the property socket service with the given configuration.
///
/// # Requirements
/// All folders specified in the PropertyConfig must be valid and accessible
/// for the function to execute successfully.
///
/// # Failure semantics
/// `rsproperties::try_init` commits process-global, first-write-wins
/// state. If a later startup step fails, that state stays committed (a
/// `OnceLock` cannot be un-set), so retrying `run` in the same process
/// with a *different* config will fail with "already initialized" —
/// treat a startup failure as fatal for the process.
///
/// The error type is `Send + Sync` so the returned future can be
/// `tokio::spawn`ed and the error converted into `anyhow::Error`.
pub async fn run(
    config: rsproperties::PropertyConfig,
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
) -> Result<
    (
        ServiceContext<SocketService>,
        ServiceContext<PropertiesService>,
    ),
    Box<dyn std::error::Error + Send + Sync>,
> {
    // Use `try_init` rather than `init`: if the global properties_dir /
    // socket_dir cells were already committed (e.g. earlier service
    // instance, double-init, hostile race), the silent `init` swallow
    // would let the service start with the *previous* directories while
    // the caller believes their new config took effect. `?`-propagating
    // surfaces that drift at startup instead of producing a service bound
    // to wrong paths.
    rsproperties::try_init(config)?;

    let properties_service = properties_service::run(property_contexts_files, build_prop_files);

    // Initialize the socket service
    let socket_service = socket_service::run(SocketServiceArgs {
        socket_dir: rsproperties::socket_dir().to_path_buf(),
        properties_service: properties_service.actor_ref.clone(),
    });

    // Sequential readiness checks (not an eagerly-evaluated pair): if the
    // socket service already failed, waiting for the properties service's
    // potentially slow init before reporting would only delay the failure.
    // On failure, stop both actors explicitly instead of leaving them to
    // the implicit drop of the returned contexts — the caller never sees
    // the contexts on the error path.
    if let Err(e) = socket_service.actor_ref.ask(ReadyMessage).await {
        let _ = socket_service.actor_ref.stop().await;
        let _ = properties_service.actor_ref.stop().await;
        return Err(format!("Failed to start socket service: {e}").into());
    }
    if let Err(e) = properties_service.actor_ref.ask(ReadyMessage).await {
        let _ = socket_service.actor_ref.stop().await;
        let _ = properties_service.actor_ref.stop().await;
        return Err(format!("Failed to start properties service: {e}").into());
    }

    Ok((socket_service, properties_service))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_property_message() {
        let msg = PropertyMessage {
            name: "test.key".to_string(),
            value: "test.value".to_string(),
        };
        assert_eq!(msg.name, "test.key");
        assert_eq!(msg.value, "test.value");
    }
}
