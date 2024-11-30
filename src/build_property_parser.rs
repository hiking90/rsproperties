// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::fs::File;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};

use rustix::process::{Pid, Uid, Gid};
use anyhow::Error;
use rserror::*;

#[cfg(any(target_os = "android", target_os = "linux"))]
use rustix::net::UCred;
#[cfg(target_os = "macos")]
pub struct UCred {
    pub pid: Pid,
    pub uid: Uid,
    pub gid: Gid,
}

const RESTORECON_PROPERTY: &str = "selinux.restorecon_recursive";
// const INIT_CONTEXT: &str = "u:r:init:s0";

pub fn check_permissions(_key: &str, _value: &str, _context: &str, _cr: &UCred) -> Result<()> {
    Ok(())
}

pub fn load_properties_from_file(filename: &Path, filter: Option<&str>, context: &str, properties: &mut HashMap<String, String>) -> Result<()> {
    let file = File::open(filename)
        .context_with_location(format!("Failed to open to {filename:?}"))?;
    let reader = BufReader::new(file);
    let has_filter = match filter {
        Some(filter) => !filter.is_empty(),
        None => false,
    };

    for line in reader.lines() {
        let line = line.map_err(Error::from)?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if !has_filter && line.starts_with("import ") {
            // let line = line[7..].trim();
            unimplemented!("import")
        } else {
            let (key, value) = match line.find('=') {
                Some(pos) => (&line[..pos], line[pos + 1..].trim()),
                None => continue,
            };

            if has_filter {
                let filter = filter.expect("filter must be valid.");
                if filter.ends_with('*') {
                    if !key.starts_with(&filter[..filter.len() - 1]) {
                        continue;
                    }
                } else if line != filter {
                    continue;
                }
            }

            if key.starts_with("ctl.") || key == "sys.powerctl" || key == RESTORECON_PROPERTY {
                log::error!("Ignoring disallowed property '{key}' with special meaning in prop file '{filename:?}'");
                continue;
            }

            let cr = unsafe {
                UCred {
                    pid: Pid::from_raw_unchecked(1),
                    uid: Uid::from_raw(0),
                    gid: Gid::from_raw(0),
                }
            };

            match check_permissions(key, value, context, &cr) {
                Ok(_) => {
                    if let Some(old_value) = properties.insert(key.to_string(), value.to_string()) {
                        log::warn!("Overriding previous property '{key}':'{old_value}' with new value '{value}'");
                    }
                }
                Err(e) => {
                    log::error!("Failed to check permissions for '{key}': {e}");
                    continue;
                }
            }

        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_properties_from_file() {
        let mut properties = HashMap::new();
        load_properties_from_file(Path::new("tests/android/system_build.prop"), None, "u:r:init:s0", &mut properties).unwrap();
        assert_eq!(properties.get("persist.sys.usb.config"), Some(&"adb".to_string()));
    }
}