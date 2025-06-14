// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use log::{error, warn};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::errors::*;
use rustix::process::{Gid, Pid, Uid};

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
    // TODO: Implement proper permission checking
    Ok(())
}

pub fn load_properties_from_file(
    filename: &Path,
    filter: Option<&str>,
    context: &str,
    properties: &mut HashMap<String, String>,
) -> Result<()> {
    let file =
        File::open(filename).context_with_location(format!("Failed to open to {filename:?}"))?;
    let reader = BufReader::new(file);
    let has_filter = match filter {
        Some(filter) => !filter.is_empty(),
        None => false,
    };

    let mut line_count = 0;
    let mut _processed_properties = 0;
    let mut _skipped_lines = 0;

    for line in reader.lines() {
        line_count += 1;
        let line = line.map_err(Error::from)?;
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            _skipped_lines += 1;
            continue;
        }

        if !has_filter && line.starts_with("import ") {
            warn!(
                "Line {}: Import statements not implemented: {}",
                line_count, line
            );
            // let line = line[7..].trim();
            unimplemented!("import")
        } else {
            let (key, value) = match line.find('=') {
                Some(pos) => (&line[..pos], line[pos + 1..].trim()),
                None => {
                    _skipped_lines += 1;
                    continue;
                }
            };

            if has_filter {
                let filter = filter.expect("filter must be valid.");
                if filter.ends_with('*') {
                    if let Some(prefix) = filter.strip_suffix('*') {
                        if !key.starts_with(prefix) {
                            continue;
                        }
                    }
                } else if line != filter {
                    continue;
                }
            }

            if key.starts_with("ctl.") || key == "sys.powerctl" || key == RESTORECON_PROPERTY {
                error!("Line {}: Ignoring disallowed property '{}' with special meaning in prop file '{:?}'", line_count, key, filename);
                continue;
            }

            // Create UCred with safe initialization
            let cr = UCred {
                pid: Pid::from_raw(1).expect("Valid PID for init process"),
                uid: Uid::from_raw(0),
                gid: Gid::from_raw(0),
            };

            match check_permissions(key, value, context, &cr) {
                Ok(_) => {
                    if let Some(old_value) = properties.insert(key.to_string(), value.to_string()) {
                        warn!(
                            "Line {}: Overriding previous property '{}':'{}' with new value '{}'",
                            line_count, key, old_value, value
                        );
                    }
                    _processed_properties += 1;
                }
                Err(e) => {
                    error!(
                        "Line {}: Failed to check permissions for '{}': {}",
                        line_count, key, e
                    );
                    continue;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "android"))]
    use super::*;

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_load_properties_from_file() {
        let mut properties = HashMap::new();
        load_properties_from_file(
            Path::new("tests/android/system_build.prop"),
            None,
            "u:r:init:s0",
            &mut properties,
        )
        .unwrap();
        assert_eq!(
            properties.get("persist.sys.usb.config"),
            Some(&"adb".to_string())
        );
    }
}
