// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::fs::File;
use std::io::{BufReader, BufRead};
use log::{info, warn, error};

use crate::errors::*;
use crate::trie_builder::*;
use crate::trie_serializer::*;

pub struct PropertyInfoEntry {
    name: String,
    context: String,
    type_str: String,
    exact_match: bool,
}

impl PropertyInfoEntry {
    fn is_type_valid(type_strings: &[String]) -> bool {
        if type_strings.is_empty() {
            return false;
        }

        if type_strings[0] == "enum" {
            return type_strings.len() > 1;
        }

        if type_strings.len() > 1 {
            return false;
        }

        const NO_PARAMETER_TYPES: &[&str] = &["string", "int", "bool", "uint", "double", "size"];

        for no_parameter_type in NO_PARAMETER_TYPES {
            if type_strings[0] == *no_parameter_type {
                return true;
            }
        }

        false
    }

    // Parse a line from the property info file.
    // The line should be in the format:
    // <property> <context> <match operation> <type> [<type> ...]
    // where <match operation> is either "prefix" or "exact".
    // If require_prefix_or_exact is true, the match operation must be specified.
    // Example:
    //     ro.build.host u:object_r:build_prop:s0 exact string
    fn parse_from_line(line: &str, require_prefix_or_exact: bool) -> Result<PropertyInfoEntry> {
        let mut tokenizer = line.split_whitespace();

        let property = tokenizer.next().ok_or_else(||
            Error::new_parse(format!("Did not find a property entry in '{line}'")))?;

        let context = tokenizer.next().ok_or_else(||
            Error::new_parse(format!("Did not find a context entry in '{line}'")))?;

        let match_operation = tokenizer.next();

        let mut type_strings = Vec::new();
        for type_str in tokenizer {
            type_strings.push(type_str.to_owned());
        }

        let mut exact_match = false;

        if match_operation == Some("exact") {
            exact_match = true;
        } else if match_operation != Some("prefix") && require_prefix_or_exact {
            error!("Invalid match operation '{:?}' - must be 'prefix' or 'exact'", match_operation);
            return Err(Error::new_parse(format!("Match operation '{match_operation:?}' is not valid. Must be 'prefix' or 'exact'")).into());
        }

        if !type_strings.is_empty() && !Self::is_type_valid(&type_strings) {
            error!("Invalid type specification: '{}'", type_strings.join(" "));
            return Err(Error::new_parse(format!("Type '{}' is not valid.", type_strings.join(" "))).into());
        }

        let entry = Self {
            name: property.to_owned(),
            context: context.to_owned(),
            type_str: type_strings.join(" "),
            exact_match,
        };

        Ok(entry)
    }

    pub fn parse_from_file(filename: &Path, require_prefix_or_exact: bool) -> Result<(Vec<PropertyInfoEntry>, Vec<Error>)> {
        info!("Parsing property info file: {:?} (require_prefix_or_exact={})", filename, require_prefix_or_exact);

        let file = File::open(filename)
            .map_err(|e| Error::new_io(e))?;
        let reader = BufReader::new(file);

        let mut errors = Vec::new();
        let mut entries = Vec::new();
        let mut line_count = 0;
        let mut skipped_lines = 0;

        for line in reader.lines() {
            line_count += 1;
            let line = line.context_with_location("Failed to read line")?;
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') {
                skipped_lines += 1;
                continue;
            }

            match PropertyInfoEntry::parse_from_line(line, require_prefix_or_exact) {
                Ok(entry) => {
                    entries.push(entry);
                }
                Err(err) => {
                    warn!("Line {}: Failed to parse line '{}': {}", line_count, line, err);
                    errors.push(err);
                }
            }
        }

        info!("Finished parsing property info file: {} total lines, {} entries parsed, {} lines skipped, {} errors",
              line_count, entries.len(), skipped_lines, errors.len());

        Ok((entries, errors))
    }
}

pub fn build_trie(property_info: &Vec<PropertyInfoEntry>, default_context: &str, default_type: &str) -> Result<Vec<u8>> {
    info!("Building trie from {} property info entries (default_context='{}', default_type='{}')",
          property_info.len(), default_context, default_type);

    let mut trie = TrieBuilder::new(default_context, default_type);

    for entry in property_info {
        trie.add_to_trie(
            entry.name.as_str(),
            entry.context.as_str(),
            entry.type_str.as_str(),
            entry.exact_match)?;
    }

    let mut serializer = TrieSerializer::new(&trie);
    let data = serializer.take_data();

    info!("Trie built and serialized successfully: {} bytes", data.len());
    Ok(data)
}

#[cfg(test)]
mod tests {
    // use std::ffi::CString;
    use super::*;
    // use crate::property_info_parser::*;

    #[test]
    fn test_parse_from_line() {
        let entry = PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0 exact string", true).unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "string");
        assert!(entry.exact_match);

        let entry = PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0 prefix string", true).unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "string");
        assert!(!entry.exact_match);

        let entry = PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0", false).unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "");
        assert!(!entry.exact_match);

        let entry = PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0 exact enum string int", true).unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "enum string int");
        assert!(entry.exact_match);
    }

    // #[test]
    // fn test_parse_from_file() {
    //     let entries = PropertyInfoEntry::parse_from_file(Path::new("tests/android/plat_property_contexts"), false).unwrap();
    //     assert_eq!(entries.1.len(), 0);
    //     assert_eq!(entries.0[0].name, "net.rmnet");
    //     assert_eq!(entries.0[entries.0.len() - 1].name, "ro.quick_start.device_id");

    //     let data: Vec<u8> = build_trie(&entries.0, "u:object_r:build_prop:s0", "string").unwrap();

    //     let property_info = PropertyInfoArea::new(&data);
    //     let index = property_info.get_property_info("ro.unknown.unknown");
    //     assert_eq!(index, (Some(CString::new("u:object_r:build_prop:s0").unwrap()).as_deref(), Some(CString::new("string").unwrap()).as_deref()));
    //     let index = property_info.get_property_info("net.rmnet");
    //     assert_eq!(index, (Some(CString::new("u:object_r:net_radio_prop:s0").unwrap()).as_deref(), Some(CString::new("string").unwrap()).as_deref()));
    //     let index = property_info.get_property_info("ro.quick_start.device_id");
    //     assert_eq!(index, (Some(CString::new("u:object_r:quick_start_prop:s0").unwrap()).as_deref(), Some(CString::new("string").unwrap()).as_deref()));
    // }
}