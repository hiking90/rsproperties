// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use log::{info, warn};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::errors::*;
use crate::trie_builder::*;
use crate::trie_serializer::*;

/// Whether [`PropertyInfoEntry::parse_from_file`] checks the match
/// operation of a `property_contexts` line — its third token, `prefix` or
/// `exact`. AOSP's `ParsePropertyInfoFile` takes this as
/// `bool require_prefix_or_exact`.
///
/// In both modes a line without a third token (the two-token
/// `<property> <context>` form of older files) is a prefix match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKeyword {
    /// A line whose third token is neither `prefix` nor `exact` is
    /// rejected (`require_prefix_or_exact = true`; AOSP init passes this
    /// for a vendor image of API level 30 or later).
    Checked,
    /// A third token other than `exact` makes the line a prefix match,
    /// whatever the token is (`require_prefix_or_exact = false`).
    Unchecked,
}

/// One `property_contexts` line: a property name (or name prefix), the
/// SELinux context it maps to, its value type, and whether the name is an
/// exact match or a prefix. [`build_trie`] turns a list of entries into
/// the `property_info` file's content.
///
/// Two entries are equal when all four are equal; the type is compared in
/// its stored form, so `"enum  a b"` and `"enum a b"` give equal entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyInfoEntry {
    name: String,
    context: String,
    type_str: String,
    exact_match: bool,
}

impl PropertyInfoEntry {
    /// An entry that matches the property named `name` only — the `exact`
    /// match operation of a `property_contexts` line (the file-based path
    /// is [`Self::parse_from_file`]).
    ///
    /// `name` and `context` are stored as given, so a `String` is moved in
    /// and a `&str` is copied. `type_str` is the space-separated type
    /// (`"string"`, `"enum a b"`, or `""` for none). It is borrowed: only
    /// its whitespace-normalized copy is stored.
    ///
    /// ```
    /// use rsproperties::PropertyInfoEntry;
    ///
    /// # fn main() -> rsproperties::Result<()> {
    /// let entry = PropertyInfoEntry::exact("ro.build.host", "u:object_r:build_prop:s0", "string")?;
    /// assert!(entry.exact_match());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a `type_str` the parser would
    /// reject.
    pub fn exact(
        name: impl Into<String>,
        context: impl Into<String>,
        type_str: &str,
    ) -> Result<Self> {
        Self::with_match(name.into(), context.into(), type_str, true)
    }

    /// An entry that matches every property whose name starts with `name`
    /// — the `prefix` match operation. Arguments and errors as for
    /// [`Self::exact`].
    pub fn prefix(
        name: impl Into<String>,
        context: impl Into<String>,
        type_str: &str,
    ) -> Result<Self> {
        Self::with_match(name.into(), context.into(), type_str, false)
    }

    fn with_match(
        name: String,
        context: String,
        type_str: &str,
        exact_match: bool,
    ) -> Result<Self> {
        // Store the whitespace-normalized form (`join(" ")`), matching
        // `parse_from_line` — otherwise the same logical type could
        // serialize as different bytes depending on which constructor
        // produced the entry.
        //
        // The guard checks the *token list*, not `type_str.is_empty()`:
        // a whitespace-only `type_str` normalizes to the same empty type
        // as `""` and must pass identically (`parse_from_line` already
        // treats them alike).
        //
        // Same separator set as `parse_from_line` (C `isspace`), so one
        // `type_str` yields the same tokens through either path.
        let type_strings: Vec<&str> = type_str
            .split(crate::parse::is_c_space)
            .filter(|token| !token.is_empty())
            .collect();
        if !type_strings.is_empty() && !Self::is_type_valid(&type_strings) {
            return Err(Error::InvalidArgument(format!(
                "Type {type_str:?} is not valid."
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
    // If require_prefix_or_exact is true, a match operation that is present
    // must be one of the two; a line without one is a prefix match.
    // Example:
    //     ro.build.host u:object_r:build_prop:s0 exact string
    fn parse_from_line(line: &str, require_prefix_or_exact: bool) -> Result<PropertyInfoEntry> {
        // AOSP `SpaceTokenizer` splits on C `isspace` — the six ASCII
        // bytes of `is_c_space`. `split_whitespace` would also split on
        // Unicode White_Space (U+00A0, U+3000, ...), which AOSP keeps
        // inside a token.
        let mut tokenizer = line
            .split(crate::parse::is_c_space)
            .filter(|token| !token.is_empty());

        let property = tokenizer
            .next()
            .ok_or_else(|| Error::Parse(format!("Did not find a property entry in {line:?}")))?;

        let context = tokenizer
            .next()
            .ok_or_else(|| Error::Parse(format!("Did not find a context entry in {line:?}")))?;

        let match_operation = tokenizer.next();

        // Borrow from `line` — the only owned copy is the final `join`.
        let type_strings: Vec<&str> = tokenizer.collect();

        let mut exact_match = false;

        if match_operation == Some("exact") {
            exact_match = true;
        } else if let Some(op) = match_operation.filter(|&op| op != "prefix") {
            // AOSP `ParsePropertyInfoLine` rejects on
            // `match_operation != "prefix" && match_operation != "" &&
            // require_prefix_or_exact`: a *missing* operation (its
            // tokenizer yields "" there) is legal with the flag too —
            // "older files will not contain them" — and the two-token
            // `<property> <context>` line is a prefix match. Only a token
            // that is present and neither 'prefix' nor 'exact' is an error.
            // No log here: the only caller (`parse_from_file`) already
            // warns with line context — logging both duplicated every
            // parse failure.
            if require_prefix_or_exact {
                return Err(Error::Parse(format!(
                    "Match operation {op:?} is not valid. Must be 'prefix' or 'exact'"
                )));
            }
        }

        if !type_strings.is_empty() && !Self::is_type_valid(&type_strings) {
            return Err(Error::Parse(format!(
                "Type {:?} is not valid.",
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

    /// Parses a `property_contexts` file. Each line is
    /// `<property> <context> [<prefix|exact> [<type> ...]]`, for example
    /// `ro.build.host u:object_r:build_prop:s0 exact string`; empty lines
    /// and lines starting with `#` are skipped.
    ///
    /// A line without a third token is a prefix match. With
    /// [`MatchKeyword::Checked`], a third token that is neither `prefix`
    /// nor `exact` is an error (AOSP `ParsePropertyInfoLine` with
    /// `require_prefix_or_exact`); with [`MatchKeyword::Unchecked`], such
    /// a line is a prefix match too.
    ///
    /// Returns the parsed entries and one [`Error::Parse`] per rejected
    /// line (naming the line number and the file): a bad line does not
    /// stop the parse, so check the second vector when every line must be
    /// valid.
    ///
    /// # Errors
    ///
    /// Only when the file cannot be opened or read.
    pub fn parse_from_file(
        filename: &Path,
        match_keyword: MatchKeyword,
    ) -> Result<(Vec<PropertyInfoEntry>, Vec<Error>)> {
        let require_prefix_or_exact = match_keyword == MatchKeyword::Checked;
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
                // libbase `Trim` strips C `isspace` bytes only.
                Ok(line) => line.trim_matches(crate::parse::is_c_space),
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
                    warn!("Line {line_count}: Failed to parse line {line:?}: {err}");
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

/// Serializes `property_info` entries into the content of a
/// `property_info` file, in the format bionic maps
/// (`property_info_parser`). `default_context` and `default_type` apply to
/// a name that no entry matches.
///
/// Write the result with `AreaWriterLock::publish_property_info`, which
/// replaces the file by rename; a file rewritten in place can fault a
/// reader that has it mapped.
///
/// # Errors
///
/// - [`Error::InvalidArgument`] when `default_context` or `default_type`
///   contains a NUL byte.
/// - [`Error::Parse`] for an entry whose name has an empty segment
///   (`a..b`) or too many segments.
/// - [`Error::FileValidation`] for two entries with the same name and
///   match kind, and when the trie cannot be serialized.
pub fn build_trie(
    property_info: &[PropertyInfoEntry],
    default_context: &str,
    default_type: &str,
) -> Result<Vec<u8>> {
    info!(
        "Building trie from {} property info entries (default_context={:?}, default_type={:?})",
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

    /// Names reach `build_trie` from `property_contexts` lines and from
    /// `PropertyInfoEntry::exact`/`prefix`; its errors print them
    /// `Debug`-escaped.
    #[test]
    fn build_trie_error_escapes_the_name() {
        let entry =
            PropertyInfoEntry::prefix("a..b\u{1b}[31m", "u:object_r:x:s0", "string").unwrap();
        let msg = build_trie(&[entry], "u:object_r:default_prop:s0", "string")
            .unwrap_err()
            .to_string();
        assert!(!msg.contains('\u{1b}'), "raw ESC in {msg:?}");
        assert!(msg.contains(r#""a..b\u{1b}[31m""#), "{msg:?}");
    }

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

        // AOSP parity: with require_prefix_or_exact, a MISSING operation
        // is still a prefix match (`match_operation != ""` in
        // `ParsePropertyInfoLine`); only a present-but-invalid one is an
        // error.
        let entry =
            PropertyInfoEntry::parse_from_line("ro.build.host u:object_r:build_prop:s0", true)
                .unwrap();
        assert_eq!(entry.type_str, "");
        assert!(!entry.exact_match);
        assert!(PropertyInfoEntry::parse_from_line(
            "ro.build.host u:object_r:build_prop:s0 bogus string",
            true
        )
        .is_err());
        // Without the flag the invalid token is taken as a prefix match
        // and the rest as the type.
        let entry = PropertyInfoEntry::parse_from_line(
            "ro.build.host u:object_r:build_prop:s0 bogus string",
            false,
        )
        .unwrap();
        assert_eq!(entry.type_str, "string");
        assert!(!entry.exact_match);

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
    fn constructors_validate_type_and_set_match() {
        let ctx = || String::from("u:object_r:build_prop:s0");
        let exact = PropertyInfoEntry::exact("ro.a", ctx(), "string").unwrap();
        assert!(exact.exact_match());
        let prefix = PropertyInfoEntry::prefix("ro.a", ctx(), "").unwrap();
        assert!(!prefix.exact_match());
        assert_eq!(prefix.type_str(), "");
        // Whitespace-only normalizes to the same empty type as "" and must
        // behave identically.
        let blank = PropertyInfoEntry::prefix("ro.a", ctx(), "   ").unwrap();
        assert_eq!(blank.type_str(), "");
        let spaced = PropertyInfoEntry::exact("ro.a", ctx(), " enum  a   b ").unwrap();
        assert_eq!(spaced.type_str(), "enum a b");
        for bad in [
            PropertyInfoEntry::exact("ro.a", ctx(), "not_a_type"),
            PropertyInfoEntry::prefix("ro.a", ctx(), "not_a_type"),
        ] {
            assert!(matches!(bad, Err(Error::InvalidArgument(_))), "{bad:?}");
        }
    }

    /// A constructed entry equals the one parsed from the same line, and
    /// the match operation, the context and the type each break equality.
    #[test]
    fn entries_compare_by_all_four_fields() {
        let ctx = "u:object_r:build_prop:s0";
        let parsed =
            PropertyInfoEntry::parse_from_line(&format!("ro.a {ctx} exact enum  a b"), true)
                .unwrap();
        // `&str` and `String` arguments, and an un-normalized type.
        let built = PropertyInfoEntry::exact("ro.a", String::from(ctx), "enum a   b").unwrap();
        assert_eq!(parsed, built);

        for other in [
            PropertyInfoEntry::prefix("ro.a", ctx, "enum a b").unwrap(),
            PropertyInfoEntry::exact("ro.b", ctx, "enum a b").unwrap(),
            PropertyInfoEntry::exact("ro.a", "u:object_r:other:s0", "enum a b").unwrap(),
            PropertyInfoEntry::exact("ro.a", ctx, "string").unwrap(),
        ] {
            assert_ne!(built, other);
        }
    }

    /// `MatchKeyword` selects what `parse_from_file` does with a third
    /// token that is neither `prefix` nor `exact`; a two-token line is a
    /// prefix match in both modes.
    #[test]
    fn parse_from_file_match_keyword() {
        let path = std::env::temp_dir().join(format!(
            "rsprops_match_keyword_{}_property_contexts",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "ro.a u:object_r:a:s0 exact string\n\
             ro.b. u:object_r:b:s0\n\
             ro.c. u:object_r:c:s0 bogus string\n",
        )
        .unwrap();
        let unchecked = PropertyInfoEntry::parse_from_file(&path, MatchKeyword::Unchecked);
        let checked = PropertyInfoEntry::parse_from_file(&path, MatchKeyword::Checked);
        std::fs::remove_file(&path).unwrap();

        let (entries, errors) = unchecked.unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(entries.len(), 3);
        assert!(entries[0].exact_match());
        assert_eq!(entries[1].name(), "ro.b.");
        assert!(!entries[1].exact_match());
        assert_eq!(entries[2].name(), "ro.c.");
        assert!(!entries[2].exact_match());
        assert_eq!(entries[2].type_str(), "string");

        let (entries, errors) = checked.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name(), "ro.a");
        assert_eq!(entries[1].name(), "ro.b.");
        assert!(!entries[1].exact_match());
        assert_eq!(errors.len(), 1);
        let msg = errors[0].to_string();
        assert!(msg.contains("line 3"), "{msg}");
    }

    /// AOSP splits a line with `SpaceTokenizer` and trims it with libbase
    /// `Trim`, both on C `isspace`: space, `\t`, `\n`, `\x0b`, `\x0c`,
    /// `\r`. U+00A0 and U+3000 are token bytes there, so they are here.
    #[test]
    fn only_c_isspace_separates_tokens() {
        // Each of the six bytes separates tokens.
        let entry = PropertyInfoEntry::parse_from_line(
            "ro.a\tu:object_r:a:s0\x0bexact\x0cenum\ra\nb  c",
            true,
        )
        .unwrap();
        assert_eq!(entry.name, "ro.a");
        assert_eq!(entry.context, "u:object_r:a:s0");
        assert_eq!(entry.type_str, "enum a b c");
        assert!(entry.exact_match);

        // U+00A0 joins what looks like two tokens: the name runs up to the
        // next C space, `exact` is read as the context and `string` as the
        // match operation.
        let line = "ro.foo\u{a0}u:object_r:foo_prop:s0 exact string";
        let entry = PropertyInfoEntry::parse_from_line(line, false).unwrap();
        assert_eq!(entry.name, "ro.foo\u{a0}u:object_r:foo_prop:s0");
        assert_eq!(entry.context, "exact");
        assert_eq!(entry.type_str, "");
        assert!(!entry.exact_match);
        let msg = PropertyInfoEntry::parse_from_line(line, true)
            .unwrap_err()
            .to_string();
        assert!(msg.contains(r#"Match operation "string""#), "{msg}");

        // The constructors tokenize `type_str` the same way.
        let ctx = || String::from("u:object_r:a:s0");
        let entry = PropertyInfoEntry::exact("ro.a", ctx(), "enum\ta\x0bb").unwrap();
        assert_eq!(entry.type_str(), "enum a b");
        let entry = PropertyInfoEntry::exact("ro.a", ctx(), "enum a\u{a0}b").unwrap();
        assert_eq!(entry.type_str(), "enum a\u{a0}b");
        assert!(matches!(
            PropertyInfoEntry::exact("ro.a", ctx(), "\u{3000}"),
            Err(Error::InvalidArgument(_))
        ));

        // In a file: a line is blank only when it holds nothing but C
        // spaces, and `#` starts a comment only as the first byte after
        // those are trimmed.
        let path = std::env::temp_dir().join(format!(
            "rsprops_c_isspace_{}_property_contexts",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "\u{a0}# not a comment\n\
             \u{3000}\n\
             \x0b\x0c \t\r\n\
             ro.c u:object_r:c:s0 exact string\u{a0}\n\
             \t ro.d u:object_r:d:s0 exact string \x0c\n",
        )
        .unwrap();
        let parsed = PropertyInfoEntry::parse_from_file(&path, MatchKeyword::Checked);
        std::fs::remove_file(&path).unwrap();
        let (entries, errors) = parsed.unwrap();

        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].name(), "ro.d");
        assert_eq!(entries[0].type_str(), "string");
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].contains("line 1"), "{errors:?}");
        assert!(errors[1].contains("line 2"), "{errors:?}");
        assert!(
            errors[1].contains("Did not find a context entry"),
            "{errors:?}"
        );
        assert!(errors[2].contains("line 4"), "{errors:?}");
    }

    /// Every third-token class against both modes, as AOSP
    /// `ParsePropertyInfoLine` decides it:
    ///
    /// ```text
    /// if (match_operation == "exact") exact_match = true;
    /// else if (match_operation != "prefix" && match_operation != "" &&
    ///          require_prefix_or_exact) -> error
    /// ```
    ///
    /// then the type check on the tokens after the third one. `Ok` holds
    /// `(exact_match, type_str)`.
    #[test]
    fn match_operation_table_follows_aosp() {
        type Expected = std::result::Result<(bool, &'static str), ()>;
        // (rest of the line after `<property> <context>`, unchecked, checked)
        let table: &[(&str, Expected, Expected)] = &[
            // No third token: a prefix match in both modes.
            ("", Ok((false, "")), Ok((false, ""))),
            (" \t ", Ok((false, "")), Ok((false, ""))),
            (" prefix", Ok((false, "")), Ok((false, ""))),
            (
                " prefix string",
                Ok((false, "string")),
                Ok((false, "string")),
            ),
            (" exact", Ok((true, "")), Ok((true, ""))),
            (" exact string", Ok((true, "string")), Ok((true, "string"))),
            (
                " exact  enum  a b",
                Ok((true, "enum a b")),
                Ok((true, "enum a b")),
            ),
            // Any other third token: a prefix match unchecked, an error
            // checked. The comparison is byte-exact, so `Exact` is one.
            (" bogus", Ok((false, "")), Err(())),
            (" bogus string", Ok((false, "string")), Err(())),
            (" Exact string", Ok((false, "string")), Err(())),
            // A type where the match operation belongs is the match
            // operation, not the type.
            (" string", Ok((false, "")), Err(())),
            // The type check does not depend on the mode.
            (" prefix not_a_type", Err(()), Err(())),
            (" exact enum", Err(()), Err(())),
        ];
        for (rest, unchecked, checked) in table {
            let line = format!("ro.x u:object_r:x_prop:s0{rest}");
            for (require_prefix_or_exact, want) in [(false, unchecked), (true, checked)] {
                let got = PropertyInfoEntry::parse_from_line(&line, require_prefix_or_exact);
                let got_summary = got
                    .as_ref()
                    .map(|e| (e.exact_match, e.type_str.as_str()))
                    .map_err(|_| ());
                assert_eq!(
                    &got_summary, want,
                    "line {line:?}, require_prefix_or_exact = {require_prefix_or_exact}: {got:?}"
                );
                if let Ok(entry) = got {
                    assert_eq!(entry.name, "ro.x");
                    assert_eq!(entry.context, "u:object_r:x_prop:s0");
                }
            }
        }
    }

    /// A two-token line read with `MatchKeyword::Checked` reaches the
    /// serialized trie as a prefix entry: names under it resolve to its
    /// context, and it does not match as an exact entry would.
    #[test]
    fn checked_two_token_line_resolves_as_prefix() {
        use crate::property_info_parser::PropertyInfoArea;
        use zerocopy::IntoBytes;

        const DEFAULT: &str = "u:object_r:default_prop:s0";
        const TWO: &str = "u:object_r:two_prop:s0";
        const TYPED: &str = "u:object_r:typed_prop:s0";
        const EXACT: &str = "u:object_r:exact_prop:s0";

        let path = std::env::temp_dir().join(format!(
            "rsprops_two_token_trie_{}_property_contexts",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!(
                "ro.two. {TWO}\n\
                 ro.two.typed. {TYPED} prefix int\n\
                 ro.exact.name {EXACT} exact string\n"
            ),
        )
        .unwrap();
        let parsed = PropertyInfoEntry::parse_from_file(&path, MatchKeyword::Checked);
        std::fs::remove_file(&path).unwrap();
        let (entries, errors) = parsed.unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(entries.len(), 3);

        let bytes = build_trie(&entries, DEFAULT, "string").unwrap();
        // The area reads `u32`s in place; a `Vec<u8>` is not aligned for it.
        let mut words = vec![0u32; bytes.len().div_ceil(4)];
        words.as_mut_bytes()[..bytes.len()].copy_from_slice(&bytes);
        let area = PropertyInfoArea::new(&words.as_bytes()[..bytes.len()]);
        let context_of = |name: &str| {
            let (index, _) = area.get_property_info_indexes(name);
            let offset = area.context_offset(index as usize).unwrap();
            area.cstr(offset).unwrap().to_str().unwrap()
        };

        assert_eq!(context_of("ro.two.a"), TWO);
        assert_eq!(context_of("ro.two.a.b"), TWO);
        // The longer prefix wins under it.
        assert_eq!(context_of("ro.two.typed.a"), TYPED);
        // `ro.two.` is a prefix with its dot: `ro.two` is outside it.
        assert_eq!(context_of("ro.two"), DEFAULT);
        // The exact entry matches its own name only.
        assert_eq!(context_of("ro.exact.name"), EXACT);
        assert_eq!(context_of("ro.exact.name.a"), DEFAULT);
        assert_eq!(context_of("ro.other"), DEFAULT);
    }
}
