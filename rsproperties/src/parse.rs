// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Property value parsers that follow AOSP libbase's rules, so a value
//! reads the same here as through `android::base::GetBoolProperty` /
//! `GetIntProperty` / `GetUintProperty`.
//!
//! [`crate::get`], [`crate::get_or`] and [`crate::get_or_else`] apply
//! these parsers to `bool` and the integer types (see
//! [`crate::FromProperty`]). Call them directly on a value that came from
//! somewhere else — [`crate::SystemProperties::read_with`], a
//! [`crate::FromProperty`] implementation of your own — or where Rust's
//! `FromStr` would differ: `FromStr` rejects leading whitespace and a `0x`
//! prefix, and `bool` accepts only `true`/`false`.
//!
//! libcutils (`property_get_int32` / `property_get_int64`) follows other
//! rules — octal (`010` is 8) and trailing characters (`12abc` is 12) are
//! accepted — and is not mirrored here: libbase is the newer API, and the
//! libcutils source itself calls accepting octal unwise.

/// C `isspace` in the "C" locale: what libbase's parsers skip before a
/// number, and what AOSP's build.prop loader trims.
pub(crate) const fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

/// libbase `ParseBool`: `1`, `y`, `yes`, `on`, `true` are `true`; `0`, `n`,
/// `no`, `off`, `false` are `false`; anything else — including the empty
/// string, other letter case, and surrounding whitespace — is `None`.
///
/// ```
/// use rsproperties::parse::parse_bool;
///
/// assert_eq!(parse_bool("on"), Some(true));
/// assert_eq!(parse_bool("0"), Some(false));
/// assert_eq!(parse_bool("True"), None);
/// ```
pub fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "y" | "yes" | "on" | "true" => Some(true),
        "0" | "n" | "no" | "off" | "false" => Some(false),
        _ => None,
    }
}

mod sealed {
    pub trait Sealed: Sized {
        /// Signed types follow libbase `ParseInt` (`strtoll` range),
        /// unsigned types `ParseUint` (`strtoull` range, no `-`).
        const SIGNED: bool;
        fn from_i128(value: i128) -> Option<Self>;
    }
}

/// The integer types [`parse_int`] parses: the types libbase instantiates
/// `GetIntProperty` / `GetUintProperty` for (`i8`–`i64`, `u8`–`u64`) plus
/// `isize` and `usize`. Sealed: it cannot be implemented outside this
/// crate.
pub trait Integer: sealed::Sealed {}

macro_rules! impl_integer {
    ($signed:expr; $($t:ty),*) => {$(
        impl sealed::Sealed for $t {
            const SIGNED: bool = $signed;
            fn from_i128(value: i128) -> Option<Self> {
                <$t>::try_from(value).ok()
            }
        }
        impl Integer for $t {}
    )*};
}

impl_integer!(true; i8, i16, i32, i64, isize);
impl_integer!(false; u8, u16, u32, u64, usize);

/// libbase `ParseInt` (signed `T`) / `ParseUint` (unsigned `T`) without
/// range arguments: `None` unless the whole string is one integer in
/// `T`'s range.
///
/// - Leading C whitespace is skipped; trailing characters, whitespace
///   included, fail.
/// - A string that starts with `0x` or `0X` (after the whitespace) is
///   hexadecimal; anything else is decimal with an optional sign. A sign
///   before `0x` therefore fails (`-0x10`), as in libbase, which picks
///   base 16 only for an unsigned `0x` prefix.
/// - Unsigned types reject a leading `-`, even `-0`.
/// - A value outside `T` — or outside `i64` / `u64`, the `strtoll` /
///   `strtoull` range libbase parses into first — fails.
///
/// ```
/// use rsproperties::parse::parse_int;
///
/// assert_eq!(parse_int::<i32>(" 42"), Some(42));
/// assert_eq!(parse_int::<u32>("0x10"), Some(16));
/// assert_eq!(parse_int::<i32>("010"), Some(10));
/// assert_eq!(parse_int::<i32>("12abc"), None);
/// assert_eq!(parse_int::<u8>("256"), None);
/// ```
pub fn parse_int<T: Integer>(s: &str) -> Option<T> {
    let s = s.trim_start_matches(is_c_space);
    let (negative, digits, radix) =
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            (false, hex, 16)
        } else if let Some(rest) = s.strip_prefix('-') {
            if !T::SIGNED {
                return None;
            }
            (true, rest, 10)
        } else {
            (false, s.strip_prefix('+').unwrap_or(s), 10)
        };
    // Checked here, not left to `from_str_radix`: that function accepts a
    // leading `+`, which would let `0x+5` and `+-5`-style inputs through.
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    // More digits than `u128` holds is out of every target's range.
    let magnitude = i128::try_from(u128::from_str_radix(digits, radix).ok()?).ok()?;
    let value = if negative { -magnitude } else { magnitude };
    let in_strto_range = if T::SIGNED {
        (i128::from(i64::MIN)..=i128::from(i64::MAX)).contains(&value)
    } else {
        value <= i128::from(u64::MAX)
    };
    if !in_strto_range {
        return None;
    }
    T::from_i128(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_accepts_exactly_libbase_spellings() {
        for s in ["1", "y", "yes", "on", "true"] {
            assert_eq!(parse_bool(s), Some(true), "{s:?}");
        }
        for s in ["0", "n", "no", "off", "false"] {
            assert_eq!(parse_bool(s), Some(false), "{s:?}");
        }
        for s in ["", "TRUE", "Yes", " 1", "1 ", "2", "enabled"] {
            assert_eq!(parse_bool(s), None, "{s:?}");
        }
    }

    #[test]
    fn int_whitespace_and_signs() {
        assert_eq!(parse_int::<i32>("\t\n\x0b\x0c\r 7"), Some(7));
        assert_eq!(parse_int::<i32>("-7"), Some(-7));
        assert_eq!(parse_int::<i32>("+7"), Some(7));
        assert_eq!(parse_int::<u32>("+7"), Some(7));
        assert_eq!(parse_int::<i32>("7 "), None);
        assert_eq!(parse_int::<i32>(""), None);
        assert_eq!(parse_int::<i32>("   "), None);
        assert_eq!(parse_int::<i32>("-"), None);
        assert_eq!(parse_int::<i32>("+-7"), None);
        assert_eq!(parse_int::<i32>("- 7"), None);
    }

    #[test]
    fn int_hex_only_without_sign() {
        assert_eq!(parse_int::<i32>("0x1f"), Some(31));
        assert_eq!(parse_int::<i32>("0X1F"), Some(31));
        assert_eq!(parse_int::<u64>("0xffffffffffffffff"), Some(u64::MAX));
        assert_eq!(parse_int::<i32>("-0x10"), None);
        assert_eq!(parse_int::<i32>("+0x10"), None);
        assert_eq!(parse_int::<i32>("0x"), None);
        assert_eq!(parse_int::<i32>("0x+5"), None);
        assert_eq!(parse_int::<i32>("0xg"), None);
        assert_eq!(parse_int::<i32>("0x0x5"), None);
    }

    #[test]
    fn int_is_decimal_not_octal() {
        assert_eq!(parse_int::<i32>("010"), Some(10));
        assert_eq!(parse_int::<i32>("12abc"), None);
    }

    #[test]
    fn unsigned_rejects_minus() {
        assert_eq!(parse_int::<u32>("-0"), None);
        assert_eq!(parse_int::<u32>("-1"), None);
        assert_eq!(parse_int::<u32>(" -1"), None);
        assert_eq!(parse_int::<i32>("-0"), Some(0));
    }

    #[test]
    fn int_ranges() {
        assert_eq!(parse_int::<i8>("127"), Some(127));
        assert_eq!(parse_int::<i8>("128"), None);
        assert_eq!(parse_int::<i8>("-128"), Some(-128));
        assert_eq!(parse_int::<u8>("255"), Some(255));
        assert_eq!(parse_int::<u8>("256"), None);
        assert_eq!(parse_int::<i64>("-9223372036854775808"), Some(i64::MIN));
        assert_eq!(parse_int::<i64>("9223372036854775808"), None);
        assert_eq!(parse_int::<u64>("18446744073709551616"), None);
        assert_eq!(parse_int::<i64>(&"9".repeat(60)), None);
    }
}
