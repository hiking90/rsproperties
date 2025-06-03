// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::fs::File;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use log::{trace, debug, info, warn, error};

use rustix::process::{Pid, Uid, Gid};
use crate::errors::*;

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
    trace!("Checking permissions for key '{}' with context '{}' (UCred: pid={:?}, uid={:?}, gid={:?})",
           _key, _context, _cr.pid, _cr.uid, _cr.gid);
    // TODO: Implement proper permission checking
    Ok(())
}

pub fn load_properties_from_file(filename: &Path, filter: Option<&str>, context: &str, properties: &mut HashMap<String, String>) -> Result<()> {
    info!("Loading properties from file: {:?} (filter={:?}, context={})", filename, filter, context);

    let file = File::open(filename)
        .context_with_location(format!("Failed to open to {filename:?}"))?;
    let reader = BufReader::new(file);
    let has_filter = match filter {
        Some(filter) => !filter.is_empty(),
        None => false,
    };

    debug!("Using filter: has_filter={}, filter={:?}", has_filter, filter);

    let mut line_count = 0;
    let mut processed_properties = 0;
    let mut skipped_lines = 0;

    for line in reader.lines() {
        line_count += 1;
        let line = line.map_err(Error::from)?;
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            skipped_lines += 1;
            continue;
        }

        if !has_filter && line.starts_with("import ") {
            warn!("Line {}: Import statements not implemented: {}", line_count, line);
            // let line = line[7..].trim();
            unimplemented!("import")
        } else {
            let (key, value) = match line.find('=') {
                Some(pos) => (&line[..pos], line[pos + 1..].trim()),
                None => {
                    trace!("Line {}: Skipping line without '=' delimiter: {}", line_count, line);
                    skipped_lines += 1;
                    continue;
                }
            };

            trace!("Line {}: Found property candidate: '{}' = '{}'", line_count, key, value);

            if has_filter {
                let filter = filter.expect("filter must be valid.");
                if filter.ends_with('*') {
                    if !key.starts_with(&filter[..filter.len() - 1]) {
                        trace!("Line {}: Property '{}' filtered out by prefix filter '{}'", line_count, key, filter);
                        continue;
                    }
                } else if line != filter {
                    trace!("Line {}: Property '{}' filtered out by exact filter '{}'", line_count, key, filter);
                    continue;
                }
                debug!("Line {}: Property '{}' passed filter '{}'", line_count, key, filter);
            }

            if key.starts_with("ctl.") || key == "sys.powerctl" || key == RESTORECON_PROPERTY {
                error!("Line {}: Ignoring disallowed property '{}' with special meaning in prop file '{:?}'", line_count, key, filename);
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
                        warn!("Line {}: Overriding previous property '{}':'{}' with new value '{}'", line_count, key, old_value, value);
                    } else {
                        trace!("Line {}: Added new property '{}' = '{}'", line_count, key, value);
                    }
                    processed_properties += 1;
                }
                Err(e) => {
                    error!("Line {}: Failed to check permissions for '{}': {}", line_count, key, e);
                    continue;
                }
            }
        }
    }

    info!("Finished loading properties from {:?}: {} total lines, {} properties loaded, {} lines skipped",
          filename, line_count, processed_properties, skipped_lines);

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