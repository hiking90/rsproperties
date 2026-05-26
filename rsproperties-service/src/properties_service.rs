use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use rsactor::{Actor, ActorRef, ActorWeak};
use rsproperties::{build_trie, load_properties_from_file, PropertyInfoEntry, SystemProperties};

pub struct PropertiesServiceArgs {
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
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

impl Actor for PropertiesService {
    type Args = PropertiesServiceArgs;
    type Error = std::io::Error;

    async fn on_start(
        args: Self::Args,
        _actor_ref: &rsactor::ActorRef<Self>,
    ) -> std::result::Result<Self, Self::Error> {
        let mut property_infos = Vec::new();
        for file in args.property_contexts_files {
            let (mut property_info, errors) =
                PropertyInfoEntry::parse_from_file(&file, false).map_err(io_other)?;
            if !errors.is_empty() {
                log::error!("{errors:?}");
            }
            property_infos.append(&mut property_info);
        }

        let data: Vec<u8> =
            build_trie(&property_infos, "u:object_r:build_prop:s0", "string").map_err(io_other)?;

        let dir = rsproperties::properties_dir();
        File::create(dir.join("property_info"))?.write_all(&data)?;

        let mut properties = HashMap::new();
        for file in args.build_prop_files {
            load_properties_from_file(&file, None, "u:r:init:s0", &mut properties)
                .map_err(io_other)?;
        }

        let mut system_properties = SystemProperties::new_area(dir).map_err(io_other)?;
        for (key, value) in properties.iter() {
            match system_properties.find(key.as_str()).map_err(io_other)? {
                Some(prop_ref) => {
                    system_properties
                        .update(&prop_ref, value.as_str())
                        .map_err(io_other)?;
                }
                None => {
                    system_properties
                        .add(key.as_str(), value.as_str())
                        .map_err(io_other)?;
                }
            }
        }

        Ok(PropertiesService { system_properties })
    }

    async fn on_stop(
        &mut self,
        _actor_weak: &ActorWeak<Self>,
        killed: bool,
    ) -> std::result::Result<(), Self::Error> {
        log::warn!("=====================================");
        log::warn!("    PROPERTIES SERVICE SHUTDOWN     ");
        log::warn!("=====================================");

        if killed {
            log::error!("*** FORCED TERMINATION *** PropertiesService is being killed, cleaning up resources.");
        } else {
            log::warn!("*** GRACEFUL SHUTDOWN *** PropertiesService is stopping gracefully.");
        }

        // Perform any necessary cleanup here
        // For example, you might want to save the current state or close any open files

        log::warn!("PropertiesService cleanup completed - SERVICE TERMINATED");
        log::warn!("=====================================");

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

        // Check if the property exists in the system properties
        match self.system_properties.find(&name) {
            Ok(Some(prop_ref)) => {
                // Update the existing property
                if let Err(e) = self.system_properties.update(&prop_ref, &value) {
                    log::error!("Failed to update property '{name}': {e}");
                    false
                } else {
                    log::info!("Updated property: {name} = {value}");
                    true
                }
            }
            Ok(None) => {
                // Property does not exist, add it
                if let Err(e) = self.system_properties.add(&name, &value) {
                    log::error!("Failed to add property '{name}': {e}");
                    false
                } else {
                    log::info!("Added property: {name} = {value}");
                    true
                }
            }
            Err(e) => {
                log::error!("Failed to find property '{name}': {e}");
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
