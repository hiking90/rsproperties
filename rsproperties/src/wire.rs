// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Wire-protocol constants and shared validators.
//!
//! This module is the single source-of-truth for values that cross the
//! property-service Unix-socket boundary. Both the client (`system_property_set`)
//! and the server (`rsproperties-service`) must agree on these — duplicating
//! them in each crate would let "client rejects, server accepts" drift sneak in.

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

/// Decides whether a property value's byte length is acceptable.
///
/// Single policy used by both `system_property_set` (client) and the
/// service handler. Pre-change those two sites disagreed on whether the
/// comparison was `>` or `>=` — a value of exactly [`PROP_VALUE_MAX`]
/// bytes could be sent by the client and then rejected by the server (or
/// vice versa).
///
/// Names starting with `ro.` are allowed to exceed the limit (the server
/// stores them as long properties).
pub fn validate_value_len(name: &str, value: &str) -> Result<(), String> {
    if value.len() >= PROP_VALUE_MAX && !name.starts_with("ro.") {
        return Err(format!(
            "value too long: {} bytes (max {} for non-'ro.' properties)",
            value.len(),
            PROP_VALUE_MAX
        ));
    }
    Ok(())
}

/// AOSP `init/property_service.cpp::IsLegalPropertyName` parity (V2 path).
///
/// - non-empty
/// - first char is `_` or ASCII alphanumeric (rejects `-foo`, `@foo`, `:foo`)
/// - cannot end with `.`
/// - no consecutive `.`
/// - allowed chars: ASCII alphanumeric, `_`, `.`, `-`, `@`, `:`
///
/// The V1 wire protocol additionally enforces `name.len() <= PROP_NAME_MAX`
/// at the message-encoding layer; V2 has no length cap here.
pub fn validate_property_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name is empty".into());
    }
    let first = name.chars().next().expect("non-empty");
    if !(first.is_ascii_alphanumeric() || first == '_') {
        return Err(format!("name must start with alphanumeric or '_': {name}"));
    }
    if name.ends_with('.') {
        return Err(format!("name cannot end with '.': {name}"));
    }
    let mut prev_dot = false;
    for c in name.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@' | ':');
        if !ok {
            return Err(format!("invalid char {c:?} in name: {name}"));
        }
        if c == '.' && prev_dot {
            return Err(format!("consecutive '.' in name: {name}"));
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
    fn name_rejects_leading_non_alnum() {
        assert!(validate_property_name(".leading.dot").is_err());
        assert!(validate_property_name("-leading.dash").is_err());
        assert!(validate_property_name("@leading.at").is_err());
        assert!(validate_property_name(":leading.colon").is_err());
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
