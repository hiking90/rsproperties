// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use log::{error, warn};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::errors::*;

const RESTORECON_PROPERTY: &str = "selinux.restorecon_recursive";

/// Placeholder for future per-property SELinux permission enforcement.
/// Currently a no-op; see TODO in caller.
pub fn check_permissions(_key: &str, _value: &str, _context: &str) {
    // TODO: Implement proper permission checking
}

pub fn load_properties_from_file(
    filename: &Path,
    filter: Option<&str>,
    context: &str,
    properties: &mut HashMap<String, String>,
) -> Result<()> {
    let file =
        File::open(filename).context_with_location(format!("Failed to open {filename:?}"))?;
    let reader = BufReader::new(file);
    let filter = filter.filter(|s| !s.is_empty());

    for (line_count, line) in reader.lines().enumerate() {
        let line_count = line_count + 1;
        // Lazy context: unlike the open above, this runs per line — the
        // closure only allocates on the error path.
        let line = line.with_context_location(|| {
            format!("Failed to read line {line_count} of {filename:?}")
        })?;
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if filter.is_none() && line.starts_with("import ") {
            // Pre-change: `unimplemented!()` panic. A silent skip would
            // drop dependent properties without the caller noticing, so
            // escalate to a hard error. Callers that intentionally want
            // to ignore imports can pass a non-empty filter.
            error!("Line {line_count} in {filename:?}: 'import' not supported: {line}");
            return Err(Error::Parse(format!(
                "import statement is not supported (line {line_count} of {filename:?})"
            )));
        }

        let (key, value) = match line.find('=') {
            Some(pos) => (line[..pos].trim_end(), line[pos + 1..].trim()),
            None => continue,
        };

        if let Some(filter) = filter {
            if let Some(prefix) = filter.strip_suffix('*') {
                if !key.starts_with(prefix) {
                    continue;
                }
            } else if key != filter {
                continue;
            }
        }

        if key.starts_with("ctl.") || key == "sys.powerctl" || key == RESTORECON_PROPERTY {
            error!("Line {line_count}: Ignoring disallowed property '{key}' with special meaning in prop file '{filename:?}'");
            continue;
        }

        check_permissions(key, value, context);
        if let Some(old_value) = properties.insert(key.to_string(), value.to_string()) {
            warn!(
                "Line {line_count}: Overriding previous property '{key}':'{old_value}' with new value '{value}'"
            );
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
