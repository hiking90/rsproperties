// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Value check against a property's `property_contexts` type: AOSP init's
//! `CheckType` (`init/property_type.cpp`).

use rsproperties::parse::parse_int;

/// Whether `value` is valid for `type_string`, the type `property_info`
/// records for the property (`SystemProperties::property_type`). Same
/// rules as init's `CheckType`:
///
/// - an empty value is always valid (clearing a property);
/// - `string`: anything; `bool`: `true`, `false`, `1`, `0`;
/// - `int` / `uint`: libbase `ParseInt` / `ParseUint` into 64 bits;
/// - `double`: libbase `ParseDouble` (`strtod`, whole string, no
///   overflow or underflow to zero);
/// - `size`: one or more digits followed by exactly one of `g`, `k`, `m`;
/// - `enum a b c`: one of the listed words;
/// - any other type, or an empty type string, is invalid.
///
/// `double` differs from init in two inputs `strtod` accepts and Rust's
/// float parser does not: hexadecimal floats (`0x1p3`) and `nan(...)`
/// payloads. Both are rejected here.
pub(crate) fn check_type(type_string: &str, value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    // init splits on single spaces; the serializer stores type strings
    // joined by single spaces, so `split(' ')` sees the same tokens.
    let mut tokens = type_string.split(' ');
    let kind = tokens.next().unwrap_or("");
    match kind {
        "string" => true,
        "bool" => matches!(value, "true" | "false" | "1" | "0"),
        "int" => parse_int::<i64>(value).is_some(),
        // init checks the leading '-' itself before `ParseUint`, which
        // also rejects it after whitespace.
        "uint" => parse_int::<u64>(value).is_some(),
        "double" => parse_double(value),
        "size" => is_size(value),
        "enum" => tokens.any(|t| t == value),
        _ => false,
    }
}

/// libbase `ParseDouble` with default bounds: `strtod` must consume the
/// whole string without `ERANGE`.
fn parse_double(value: &str) -> bool {
    // `strtod` skips leading whitespace; trailing whitespace fails the
    // whole-string check.
    let s = value.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let Ok(parsed) = s.parse::<f64>() else {
        return false;
    };
    let unsigned = s.trim_start_matches(['+', '-']).to_ascii_lowercase();
    let is_literal = unsigned.starts_with("inf") || unsigned.starts_with("nan");
    // Overflow: `strtod` returns ±HUGE_VAL with ERANGE; Rust rounds to ±inf.
    if parsed.is_infinite() && !is_literal {
        return false;
    }
    // Underflow to zero: ERANGE as well. A zero result from a mantissa
    // with a non-zero digit is such an underflow.
    if parsed == 0.0 {
        let mantissa = unsigned.split(['e', 'E']).next().unwrap_or("");
        if mantissa.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
            return false;
        }
    }
    true
}

/// init's `size` type: `[0-9]+[gkm]`.
fn is_size(value: &str) -> bool {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &value.as_bytes()[digits..];
    digits > 0 && matches!(rest, [b'g' | b'k' | b'm'])
}

#[cfg(test)]
mod tests {
    use super::check_type;

    #[test]
    fn empty_value_is_always_valid() {
        for t in [
            "string", "bool", "int", "uint", "double", "size", "enum a", "bogus", "",
        ] {
            assert!(check_type(t, ""), "{t:?}");
        }
    }

    #[test]
    fn unknown_or_empty_type_rejects_values() {
        assert!(!check_type("", "x"));
        assert!(!check_type("bogus", "x"));
    }

    #[test]
    fn bool_is_narrower_than_parse_bool() {
        for v in ["true", "false", "1", "0"] {
            assert!(check_type("bool", v), "{v}");
        }
        for v in ["yes", "on", "y", "TRUE", " 1"] {
            assert!(!check_type("bool", v), "{v}");
        }
    }

    #[test]
    fn int_and_uint() {
        assert!(check_type("int", "-5"));
        assert!(check_type("int", " 0x10"));
        assert!(check_type("int", "-9223372036854775808"));
        assert!(!check_type("int", "9223372036854775808"));
        assert!(!check_type("int", "1.5"));
        assert!(check_type("uint", "18446744073709551615"));
        assert!(!check_type("uint", "-1"));
        assert!(!check_type("uint", " -1"));
    }

    #[test]
    fn double() {
        for v in [
            "1.5",
            " -2e3",
            ".5",
            "5.",
            "inf",
            "-Infinity",
            "nan",
            "0",
            "0.0e10",
        ] {
            assert!(check_type("double", v), "{v}");
        }
        for v in ["1.5 ", "abc", "1e999", "-1e999", "1e-999", "0x1p3"] {
            assert!(!check_type("double", v), "{v}");
        }
    }

    #[test]
    fn size() {
        for v in ["1k", "512m", "4g", "0k"] {
            assert!(check_type("size", v), "{v}");
        }
        for v in ["k", "1", "1K", "1kb", "1.5m", "-1k"] {
            assert!(!check_type("size", v), "{v}");
        }
    }

    #[test]
    fn enum_values() {
        assert!(check_type("enum a bb c", "bb"));
        assert!(!check_type("enum a bb c", "b"));
        assert!(!check_type("enum a bb c", "enum"));
        assert!(!check_type("enum", "x"));
    }
}
