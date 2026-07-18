// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use log::{info, warn};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::errors::*;
use crate::trie_builder::*;
use crate::trie_serializer::*;

#[derive(Debug, Clone)]
pub struct PropertyInfoEntry {
    name: String,
    context: String,
    type_str: String,
    exact_match: bool,
}

impl PropertyInfoEntry {
    /// Constructs an entry programmatically (the file-based path is
    /// [`Self::parse_from_file`]). Validates `type_str` with the same rule
    /// as the parser; AOSP's `PropertyInfoEntry` likewise exposes a public
    /// constructor.
    ///
    /// `type_str` is borrowed: only its whitespace-normalized copy is
    /// stored, so taking ownership would force callers to allocate a
    /// `String` that is immediately discarded.
    pub fn new(name: String, context: String, type_str: &str, exact_match: bool) -> Result<Self> {
        // Store the whitespace-normalized form (`join(" ")`), matching
        // `parse_from_line` — otherwise the same logical type could
        // serialize as different bytes depending on which constructor
        // produced the entry.
        //
        // The guard checks the *token list*, not `type_str.is_empty()`:
        // a whitespace-only `type_str` normalizes to the same empty type
        // as `""` and must pass identically (`parse_from_line` already
        // treats them alike).
        let type_strings: Vec<&str> = type_str.split_whitespace().collect();
        if !type_strings.is_empty() && !Self::is_type_valid(&type_strings) {
            return Err(Error::InvalidArgument(format!(
                "Type '{type_str}' is not valid."
            )));
        }
        Ok(Self {
            name,
            context,
            type_str: type_strings.join(" "),
            exact_match,
        })
    }

    /// Property name (e.g. `ro.build.host`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// SELinux context this property maps to.
    pub fn context(&self) -> &str {
        &self.context
    }

    /// Space-joined type specification (e.g. `"string"`, `"enum a b"`).
    pub fn type_str(&self) -> &str {
        &self.type_str
    }

    /// Whether the entry is an exact match (vs a prefix match).
    pub fn exact_match(&self) -> bool {
        self.exact_match
    }

    fn is_type_valid(type_strings: &[&str]) -> bool {
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

        NO_PARAMETER_TYPES.contains(&type_strings[0])
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

        let property = tokenizer
            .next()
            .ok_or_else(|| Error::Parse(format!("Did not find a property entry in '{line}'")))?;

        let context = tokenizer
            .next()
            .ok_or_else(|| Error::Parse(format!("Did not find a context entry in '{line}'")))?;

        let match_operation = tokenizer.next();

        // Borrow from `line` — the only owned copy is the final `join`.
        let type_strings: Vec<&str> = tokenizer.collect();

        let mut exact_match = false;

        if match_operation == Some("exact") {
            exact_match = true;
        } else if let Some(op) = match_operation.filter(|&op| op != "prefix") {
            // AOSP parity (`ParsePropertyInfoLine`): a *missing* operation
            // is legal even with `require_prefix_or_exact` — legacy
            // two-token `<property> <context>` lines default to prefix
            // match. Only a token that is present but neither
            // 'prefix'/'exact' is an error.
            // No log here: the only caller (`parse_from_file`) already
            // warns with line context — logging both duplicated every
            // parse failure.
            if require_prefix_or_exact {
                return Err(Error::Parse(format!(
                    "Match operation '{op}' is not valid. Must be 'prefix' or 'exact'"
                )));
            }
        }

        if !type_strings.is_empty() && !Self::is_type_valid(&type_strings) {
            return Err(Error::Parse(format!(
                "Type '{}' is not valid.",
                type_strings.join(" ")
            )));
        }

        let entry = Self {
            name: property.to_owned(),
            context: context.to_owned(),
            type_str: type_strings.join(" "),
            exact_match,
        };

        Ok(entry)
    }

    pub fn parse_from_file(
        filename: &Path,
        require_prefix_or_exact: bool,
    ) -> Result<(Vec<PropertyInfoEntry>, Vec<Error>)> {
        info!(
            "Parsing property info file: {filename:?} (require_prefix_or_exact={require_prefix_or_exact})"
        );

        let file = File::open(filename)
            .context_with_location(format!("Failed to open property info file {filename:?}"))?;
        let mut reader = BufReader::new(file);

        let mut errors = Vec::new();
        let mut entries = Vec::new();
        let mut line_count = 0;
        let mut skipped_lines = 0;

        // Raw bytes per line instead of `lines()`: this function's contract
        // is per-line error *collection*, but `lines()` reports a non-UTF-8
        // byte as an `InvalidData` I/O error, which would abort the whole
        // parse and discard everything gathered so far. Decode failures are
        // collected into `errors` like any other bad line (same pattern as
        // `build_property_parser`).
        let mut raw_line = Vec::new();
        loop {
            // Bounded like `build_property_parser`'s loop: same crafted-
            // input threat model, same fix.
            let (read, truncated) =
                crate::build_property_parser::read_bounded_line(&mut reader, &mut raw_line)
                    .with_context_location(|| {
                        format!("Failed to read line {} of {filename:?}", line_count + 1)
                    })?;
            if read == 0 {
                break;
            }
            line_count += 1;
            if truncated {
                warn!("Line {line_count}: skipping over-long line");
                errors.push(Error::Parse(format!(
                    "line {line_count} of {filename:?}: line longer than {} bytes",
                    crate::build_property_parser::MAX_LINE_LEN
                )));
                continue;
            }

            let line = match std::str::from_utf8(&raw_line) {
                Ok(line) => line.trim(),
                Err(e) => {
                    warn!("Line {line_count}: skipping non-UTF-8 line: {e}");
                    // Collected entries must be self-describing: callers
                    // log the returned Vec, not the warn above, and a bare
                    // `Utf8Error` only carries an intra-line byte offset.
                    errors.push(Error::Parse(format!(
                        "line {line_count} of {filename:?}: non-UTF-8 line: {e}"
                    )));
                    continue;
                }
            };

            if line.is_empty() || line.starts_with('#') {
                skipped_lines += 1;
                continue;
            }

            match PropertyInfoEntry::parse_from_line(line, require_prefix_or_exact) {
                Ok(entry) => {
                    entries.push(entry);
                }
                Err(err) => {
                    warn!("Line {line_count}: Failed to parse line '{line}': {err}");
                    // Same self-describing contract as the UTF-8 arm above:
                    // callers consume the returned Vec, so the position
                    // must live in the error itself, not only in the warn.
                    // Unwrap the inner `Parse` payload — re-wrapping the
                    // whole error would render as "Parse error: line N …:
                    // Parse error: …".
                    let msg = match err {
                        Error::Parse(m) => m,
                        other => other.to_string(),
                    };
                    errors.push(Error::Parse(format!(
                        "line {line_count} of {filename:?}: {msg}"
                    )));
                }
            }
        }

        info!("Finished parsing property info file: {} total lines, {} entries parsed, {} lines skipped, {} errors",
              line_count, entries.len(), skipped_lines, errors.len());

        Ok((entries, errors))
    }
}

pub fn build_trie(
    property_info: &[PropertyInfoEntry],
    default_context: &str,
    default_type: &str,
) -> Result<Vec<u8>> {
    info!(
        "Building trie from {} property info entries (default_context='{}', default_type='{}')",
        property_info.len(),
        default_context,
        default_type
    );

    // The defaults bypass `add_to_trie` (they are interned directly by
    // `TrieBuilder::new`), so they need the same interior-NUL gate the
    // per-entry path applies — without it a NUL default reaches the string
    // table and later fails as a misleading "serializer invariant
    // violation" instead of an input error.
    crate::wire::validate_no_interior_nul("default context", default_context)?;
    crate::wire::validate_no_interior_nul("default type", default_type)?;

    let mut trie = TrieBuilder::new(default_context, default_type);

    for entry in property_info {
        trie.add_to_trie(
            entry.name.as_str(),
            entry.context.as_str(),
            entry.type_str.as_str(),
            entry.exact_match,
        )?;
    }

    let serializer = TrieSerializer::new(&trie)?;
    let data = serializer.into_data();

    info!(
        "Trie built and serialized successfully: {} bytes",
        data.len()
    );
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_from_line() {
        let entry = PropertyInfoEntry::parse_from_line(
            "ro.build.host u:object_r:build_prop:s0 exact string",
            true,
        )
        .unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "string");
        assert!(entry.exact_match);

        let entry = PropertyInfoEntry::parse_from_line(
            "ro.build.host u:object_r:build_prop:s0 prefix string",
            true,
        )
        .unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "string");
        assert!(!entry.exact_match);

        let entry =
            PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0", false)
                .unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "");
        assert!(!entry.exact_match);

        // AOSP parity: a legacy two-token line is legal even when
        // require_prefix_or_exact is true — a MISSING operation defaults
        // to prefix match; only a present-but-invalid one is an error.
        let entry =
            PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0", true)
                .unwrap();
        assert!(!entry.exact_match);
        assert!(PropertyInfoEntry::parse_from_line(
            "ro.build.host u:object_r:build_prop:s0 bogus string",
            true
        )
        .is_err());

        let entry = PropertyInfoEntry::parse_from_line(
            "ro.build.host u:object_r:build_prop:s0 exact enum string int",
            true,
        )
        .unwrap();
        assert_eq!(entry.name, "ro.build.host");
        assert_eq!(entry.context, "u:object_r:build_prop:s0");
        assert_eq!(entry.type_str, "enum string int");
        assert!(entry.exact_match);
    }

    #[test]
    fn test_new_validates_type() {
        assert!(PropertyInfoEntry::new(
            "ro.a".into(),
            "u:object_r:build_prop:s0".into(),
            "string",
            true
        )
        .is_ok());
        assert!(PropertyInfoEntry::new(
            "ro.a".into(),
            "u:object_r:build_prop:s0".into(),
            "",
            false
        )
        .is_ok());
        // Whitespace-only normalizes to the same empty type as "" and must
        // behave identically.
        assert!(PropertyInfoEntry::new(
            "ro.a".into(),
            "u:object_r:build_prop:s0".into(),
            "   ",
            false
        )
        .is_ok());
        assert!(PropertyInfoEntry::new(
            "ro.a".into(),
            "u:object_r:build_prop:s0".into(),
            "not_a_type",
            true
        )
        .is_err());
    }
}
