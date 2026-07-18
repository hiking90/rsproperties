// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Wire-protocol constants and shared validators.
//!
//! This module is the single source-of-truth for values that cross the
//! property-service Unix-socket boundary. Both the client (`system_property_set`)
//! and the server (`rsproperties-service`) must agree on these — duplicating
//! them in each crate would let "client rejects, server accepts" drift sneak in.

use crate::errors::{Error, Result};

/// Size of the in-memory property-value buffer **including** the trailing
/// NUL — matches the historical bionic `PROP_VALUE_MAX = 92` definition.
/// User content is therefore capped at `PROP_VALUE_MAX - 1 = 91` bytes,
/// enforced via `validate_value_len`'s `>= PROP_VALUE_MAX` check (bionic
/// `__system_property_set` does the same `strlen(value) >= PROP_VALUE_MAX`).
/// Long `ro.` properties bypass this via the long-property out-of-line
/// path in `property_info`.
pub const PROP_VALUE_MAX: usize = 92;

/// Hard cap on property-name byte length in the **V1 wire protocol** only
/// (V1's message buffer is a fixed `[u8; PROP_NAME_MAX]`). The V2 protocol
/// is length-prefixed and does not impose this limit at the wire layer —
/// AOSP `init/property_service.cpp::IsLegalPropertyName` likewise doesn't
/// enforce a length on V2.
pub const PROP_NAME_MAX: usize = 32;

/// V1 SETPROP wire command id.
pub const PROP_MSG_SETPROP: u32 = 1;
/// V2 SETPROP wire command id (length-prefixed name/value).
pub const PROP_MSG_SETPROP2: u32 = 0x00020001;

/// V2 success response code.
pub const PROP_SUCCESS: i32 = 0;
/// V2 generic error response code.
pub const PROP_ERROR: i32 = -1;

/// Sanity cap on a V2 wire property-name length. The wire format is
/// length-prefixed, so this only exists to bound the server's upfront
/// allocation against a hostile peer; `validate_property_name` rejects
/// anything actually malformed. Lives here (not in the server crate) so
/// the client can pre-check the same limit — "client accepts, server
/// rejects" drift is exactly what this module exists to prevent.
pub const MAX_WIRE_NAME_LEN: usize = 1024;

/// Sanity cap on a V2 wire property-value length. Long `ro.` values may
/// legitimately exceed `PROP_VALUE_MAX`, so the cap is generous; see
/// `MAX_WIRE_NAME_LEN` for why it lives in this module.
pub const MAX_WIRE_VALUE_LEN: usize = 8192;

/// Decides whether a property value is storable: length policy plus a
/// NUL-byte check.
///
/// Single policy used by both `system_property_set` (client), the service
/// handler, and `SystemProperties::{add, update}`. Pre-change those sites
/// disagreed on whether the comparison was `>` or `>=` — a value of
/// exactly [`PROP_VALUE_MAX`] bytes could be sent by the client and then
/// rejected by the server (or vice versa).
///
/// Names starting with `ro.` are allowed to exceed the limit (the server
/// stores them as long properties).
///
/// Passing an **empty `name`** deliberately disables the `ro.` exemption:
/// in-place update paths (`SystemProperties::update`) cannot promote a
/// value to the out-of-line long-property representation, so they pass
/// `""` to enforce the short-value cap regardless of the real name. Every
/// other caller must pass the actual property name.
///
/// Interior NUL bytes are rejected because the storage format treats
/// values as C strings: the length recorded in the entry's serial word is
/// the full byte length, while every scan (value slot, dirty backup,
/// long value) stops at the first NUL. A value like `"a\0bc"` would
/// record length 4 but store/back-up only 1 byte — a seqlock reader on
/// the dirty path would then copy `serial >> 24` = 4 bytes, including
/// stale bytes from an earlier update of a *different* property. bionic
/// cannot even express such a value (its API takes C strings).
pub fn validate_value_len(name: &str, value: &str) -> Result<()> {
    if value.as_bytes().contains(&0) {
        return Err(Error::InvalidArgument(
            "value must not contain NUL bytes".into(),
        ));
    }
    if value.len() >= PROP_VALUE_MAX && !name.starts_with("ro.") {
        return Err(Error::InvalidArgument(format!(
            "value too long: {} bytes (max {} for non-'ro.' properties)",
            value.len(),
            PROP_VALUE_MAX - 1
        )));
    }
    Ok(())
}

/// Rejects interior NUL bytes in a property-metadata string (name, SELinux
/// context, or type). Everything downstream — the shared-memory trie, the
/// serialized `property_info` string table — stores these as C strings, so
/// an interior NUL desyncs the recorded byte length from what NUL-scanning
/// readers see: truncated ghost names, unreachable entries, and misleading
/// "string table" build errors. `kind` names the field for the error
/// message (e.g. `"property name"`).
///
/// `pub(crate)`: this guards internal storage invariants (trie/string
/// table), not the wire protocol — exporting it would freeze an
/// implementation detail into the public API. Builder-gated because every
/// caller (trie builder, serializer, `PropertyArea::add`) is.
#[cfg(feature = "builder")]
pub(crate) fn validate_no_interior_nul(kind: &str, s: &str) -> Result<()> {
    if s.as_bytes().contains(&0) {
        return Err(Error::InvalidArgument(format!(
            "{kind} must not contain NUL bytes: \"{}\"",
            s.escape_default()
        )));
    }
    Ok(())
}

/// AOSP `init/util.cpp::IsLegalPropertyName` parity (V2 path).
///
/// - non-empty
/// - cannot start with `.` (AOSP only rejects a leading dot — `-foo`,
///   `@foo`, `:foo` are legal names for bionic clients, so rejecting them
///   here would refuse writes that Android's own property service accepts)
/// - cannot end with `.`
/// - no consecutive `.`
/// - allowed chars: ASCII alphanumeric, `_`, `.`, `-`, `@`, `:`
///
/// The V1 wire protocol additionally enforces `name.len() <= PROP_NAME_MAX`
/// at the message-encoding layer; V2 has no length cap here.
pub fn validate_property_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidArgument("name is empty".into()));
    }
    if name.starts_with('.') {
        return Err(Error::InvalidArgument(format!(
            "name cannot start with '.': {name}"
        )));
    }
    if name.ends_with('.') {
        return Err(Error::InvalidArgument(format!(
            "name cannot end with '.': {name}"
        )));
    }
    let mut prev_dot = false;
    for c in name.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@' | ':');
        if !ok {
            return Err(Error::InvalidArgument(format!(
                "invalid char {c:?} in name: {name}"
            )));
        }
        if c == '.' && prev_dot {
            return Err(Error::InvalidArgument(format!(
                "consecutive '.' in name: {name}"
            )));
        }
        prev_dot = c == '.';
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_len_short_ok() {
        assert!(validate_value_len("foo", "x".repeat(PROP_VALUE_MAX - 1).as_str()).is_ok());
    }

    #[test]
    fn value_len_at_cap_rejected_for_non_ro() {
        assert!(validate_value_len("foo", "x".repeat(PROP_VALUE_MAX).as_str()).is_err());
    }

    #[test]
    fn value_len_at_cap_ok_for_ro() {
        assert!(validate_value_len("ro.foo", "x".repeat(PROP_VALUE_MAX * 10).as_str()).is_ok());
    }

    #[test]
    fn value_rejects_interior_nul() {
        // Interior NUL would desync the serial length from the NUL-scanned
        // storage length and leak stale backup bytes to seqlock readers.
        assert!(validate_value_len("foo", "a\0bc").is_err());
        assert!(validate_value_len("ro.foo", "a\0bc").is_err());
        assert!(validate_value_len("foo", "\0").is_err());
    }

    #[test]
    fn name_basic_ok() {
        assert!(validate_property_name("ro.build.version.sdk").is_ok());
        assert!(validate_property_name("_internal.flag").is_ok());
        assert!(validate_property_name("a").is_ok());
    }

    #[test]
    fn name_rejects_empty() {
        assert!(validate_property_name("").is_err());
    }

    #[test]
    fn name_rejects_leading_dot() {
        assert!(validate_property_name(".leading.dot").is_err());
    }

    #[test]
    fn name_allows_leading_symbols_like_aosp() {
        // AOSP `IsLegalPropertyName` only rejects a *leading dot*; these are
        // all legal names for bionic clients and must stay accepted.
        assert!(validate_property_name("-leading.dash").is_ok());
        assert!(validate_property_name("@leading.at").is_ok());
        assert!(validate_property_name(":leading.colon").is_ok());
        assert!(validate_property_name("_internal").is_ok());
    }

    #[test]
    fn name_rejects_trailing_dot() {
        assert!(validate_property_name("trailing.dot.").is_err());
    }

    #[test]
    fn name_rejects_consecutive_dots() {
        assert!(validate_property_name("double..dot").is_err());
    }

    #[test]
    fn name_rejects_invalid_chars() {
        assert!(validate_property_name("has space").is_err());
        assert!(validate_property_name("has/slash").is_err());
    }
}
