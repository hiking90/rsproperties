use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use rsactor::{Actor, ActorRef, ActorWeak};
use rsproperties::{build_trie, load_properties_from_file, PropertyInfoEntry, SystemProperties};

pub struct PropertiesServiceArgs {
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
}

impl PropertiesServiceArgs {
    /// Public constructor: the type is exposed through `Actor::Args`, so
    /// downstream users spawning the actor directly (instead of via
    /// `run`) need a way to build it.
    pub fn new(property_contexts_files: Vec<PathBuf>, build_prop_files: Vec<PathBuf>) -> Self {
        Self {
            property_contexts_files,
            build_prop_files,
        }
    }
}

pub struct PropertiesService {
    system_properties: SystemProperties,
}

/// Wrap any error implementing the standard `Error` trait into an
/// `io::Error` preserving the source chain. The previous `e.to_string()`
/// flattening lost `Error::source()` and made anyhow/backtrace useless.
fn io_other<E>(e: E) -> std::io::Error
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    std::io::Error::other(e)
}

/// Synchronous initialisation: parses property_contexts files, writes the
/// trie to `property_info`, loads build.prop files, and applies them to a
/// freshly-mapped `SystemProperties` area.
///
/// Kept synchronous on purpose — every step is blocking I/O against the
/// filesystem and we don't want to scatter `spawn_blocking` calls through
/// the loop body. Callers invoke this once via `spawn_blocking` so the
/// tokio worker isn't held for the duration of init.
///
/// Build-prop entries are collected into a `BTreeMap` so the order in
/// which they are applied to the area (and thus the physical layout of
/// the trie) is deterministic across runs. Which *file* wins a key
/// conflict is already deterministic — `load_properties_from_file`
/// overwrites in call order — the map only fixes the apply order.
fn init_system_properties_sync(
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
    dir: &Path,
) -> std::io::Result<SystemProperties> {
    let mut property_infos = Vec::new();
    for file in property_contexts_files {
        let (mut property_info, errors) =
            PropertyInfoEntry::parse_from_file(&file, false).map_err(io_other)?;
        if !errors.is_empty() {
            log::error!("{errors:?}");
        }
        property_infos.append(&mut property_info);
    }

    let data: Vec<u8> =
        build_trie(&property_infos, "u:object_r:build_prop:s0", "string").map_err(io_other)?;

    File::create(dir.join("property_info"))?.write_all(&data)?;

    // `load_properties_from_file` only accepts `&mut HashMap` (other
    // callers depend on that signature). Re-collect into a `BTreeMap`
    // before the apply loop so the iteration order is fully determined
    // by the keys, not by HashMap's randomised hash seed.
    let mut properties_unordered: HashMap<String, String> = HashMap::new();
    for file in build_prop_files {
        load_properties_from_file(&file, None, "u:r:init:s0", &mut properties_unordered)
            .map_err(io_other)?;
    }
    let properties: BTreeMap<String, String> = properties_unordered.into_iter().collect();

    let mut system_properties = SystemProperties::new_area(dir).map_err(io_other)?;
    // `new_area` starts from a freshly-recreated, empty area and the
    // BTreeMap keys are unique, so every key is new — `add` alone covers
    // the loop. (The previous `find → update` branch was unreachable; had
    // it ever been reached, `update` would have rejected the `ro.` keys
    // that dominate build.prop files and killed the whole init.)
    for (key, value) in properties.iter() {
        system_properties
            .add(key.as_str(), value.as_str())
            .map_err(io_other)?;
    }
    Ok(system_properties)
}

impl Actor for PropertiesService {
    type Args = PropertiesServiceArgs;
    type Error = std::io::Error;
    // This actor does no periodic / event-driven idle work, so the idle event
    // type is unit and `on_idle` is left at its default no-op. (0.16 requires
    // the associated type even when unused; manual impls must spell it out.)
    type IdleEvent = ();

    async fn on_start(
        args: Self::Args,
        _actor_ref: &rsactor::ActorRef<Self>,
    ) -> std::result::Result<Self, Self::Error> {
        let dir = rsproperties::properties_dir().to_path_buf();
        // Filesystem + mmap + trie build all block. Run them on a blocking
        // task so the tokio worker that polls this actor is free to drive
        // other tasks (notably the sibling SocketService) while
        // initialisation runs.
        let system_properties = tokio::task::spawn_blocking(move || {
            init_system_properties_sync(args.property_contexts_files, args.build_prop_files, &dir)
        })
        .await
        .map_err(|e| std::io::Error::other(format!("init join failed: {e}")))??;

        Ok(PropertiesService { system_properties })
    }

    async fn on_stop(
        &mut self,
        _actor_weak: &ActorWeak<Self>,
        killed: bool,
    ) -> std::result::Result<(), Self::Error> {
        // Routine shutdown logs at `info!`; only a kill is `warn!`-worthy.
        if killed {
            log::warn!("PropertiesService killed — cleaning up resources");
        } else {
            log::info!("PropertiesService stopping gracefully");
        }
        Ok(())
    }
}

impl rsactor::Message<crate::ReadyMessage> for PropertiesService {
    type Reply = ();

    async fn handle(
        &mut self,
        _message: crate::ReadyMessage,
        _actor_ref: &ActorRef<Self>,
    ) -> Self::Reply {
    }
}

use rsproperties::wire::{validate_property_name, validate_value_len};

impl rsactor::Message<crate::PropertyMessage> for PropertiesService {
    type Reply = bool;

    async fn handle(
        &mut self,
        message: crate::PropertyMessage,
        _actor_ref: &ActorRef<Self>,
    ) -> Self::Reply {
        log::debug!("Handling property message: {message:?}");
        let name = message.name;
        let value = message.value;

        // Single source-of-truth for name + length policy — client and
        // server use the same `rsproperties::wire` functions so policy
        // drift (e.g. `>` vs `>=`) cannot reappear.
        if let Err(e) = validate_property_name(&name) {
            log::error!("Rejected setprop: {e}");
            return false;
        }
        if let Err(e) = validate_value_len(&name, &value) {
            log::error!("Rejected setprop: {e}");
            return false;
        }

        // Delegate to `set`, which already encapsulates the find →
        // update-or-add sequence (plus the `ro.` rejection) — duplicating
        // that logic here invited policy drift between the two copies.
        match self.system_properties.set(&name, &value) {
            Ok(()) => {
                // Mask the value (same policy as `PropertyMessage`'s Debug
                // impl and the socket layer): values may carry sensitive
                // payloads, and logging them here would defeat the masking
                // everywhere upstream.
                log::info!("Set property: {name} (<{} bytes>)", value.len());
                true
            }
            Err(e) => {
                log::error!("Failed to set property '{name}': {e}");
                false
            }
        }
    }
}

pub fn run(
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
) -> crate::ServiceContext<PropertiesService> {
    let args = PropertiesServiceArgs {
        property_contexts_files,
        build_prop_files,
    };

    let (actor_ref, join_handle) = rsactor::spawn(args);
    crate::ServiceContext {
        actor_ref,
        join_handle,
    }
}
