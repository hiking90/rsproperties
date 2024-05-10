use std::path::Path;
use std::fs::File;
use std::io::{BufReader, BufRead};

use log;

use crate::errors::{self, *};

struct PropertyInfoEntry {
    name: String,
    context: String,
    type_str: String,
    exact_match: bool,
}

impl PropertyInfoEntry {


    fn is_type_valid(type_strings: &Vec<String>) -> bool {
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
            Error::new_custom(format!("Did not find a property entry in '{line}'")))?;

        let context = tokenizer.next().ok_or_else(||
            Error::new_custom(format!("Did not find a context entry in '{line}'")))?;

        let match_operation = tokenizer.next();

        let mut type_strings = Vec::new();
        for type_str in tokenizer {
            type_strings.push(type_str.to_owned());
        }

        let mut exact_match = false;

        if match_operation == Some("exact") {
            exact_match = true;
        } else if match_operation != Some("prefix") && require_prefix_or_exact == true {
            return Err(Error::new_custom(format!("Match operation '{match_operation:?}' is not valid. Must be 'prefix' or 'exact'")));
        }

        if type_strings.is_empty() == false && Self::is_type_valid(&type_strings) == false {
            return Err(Error::new_custom(format!("Type '{}' is not valid.", type_strings.join(" "))));
        }

        Ok(Self {
            name: property.to_owned(),
            context: context.to_owned(),
            type_str: type_strings.join(" "),
            exact_match,
        })
    }

    pub fn parse_from_file(filename: &Path, require_prefix_or_exact: bool) -> Result<(Vec<PropertyInfoEntry>, Vec<Error>)> {
        let file = File::open(filename).map_err(Error::new_io)?;
        let reader = BufReader::new(file);

        let mut errors = Vec::new();
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(Error::new_io)?;
            let line = line.trim();
            if line.is_empty() || line.starts_with("#") {
                continue;
            }
            match PropertyInfoEntry::parse_from_line(line, require_prefix_or_exact) {
                Ok(entry) => entries.push(entry),
                Err(err) => {
                    errors.push(err);
                }
            }
        }

        Ok((entries, errors))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_from_line() {
        let entry = PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0 exact string", true).unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "string");
        assert_eq!(entry.exact_match, true);

        let entry = PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0 prefix string", true).unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "string");
        assert_eq!(entry.exact_match, false);

        let entry = PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0", false).unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "");
        assert_eq!(entry.exact_match, false);

        let entry = PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0 exact enum string int", true).unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "enum string int");
        assert_eq!(entry.exact_match, true);
    }

    #[test]
    fn test_parse_from_file() {
        let entries = PropertyInfoEntry::parse_from_file(Path::new("tests/plat_property_contexts"), false).unwrap();
        assert_eq!(entries.1.len(), 0);
        assert_eq!(entries.0[0].name, "net.rmnet");
        assert_eq!(entries.0[entries.0.len() - 1].name, "ro.quick_start.device_id");
    }
}