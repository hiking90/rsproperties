// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! [`FromProperty`]: the conversion [`crate::get`], [`crate::get_or`] and
//! [`crate::get_or_else`] apply to a property value.

use crate::parse;

/// A type a property value can be read as with [`crate::get`],
/// [`crate::get_or`] and [`crate::get_or_else`].
///
/// The implementations in this crate follow AOSP libbase, so a value reads
/// the same here as through `android::base::GetProperty` /
/// `GetBoolProperty` / `GetIntProperty` / `GetUintProperty`:
///
/// | Type | Rule |
/// |---|---|
/// | `String` | the value, unchanged |
/// | `bool` | [`parse::parse_bool`]: `1`/`y`/`yes`/`on`/`true`, `0`/`n`/`no`/`off`/`false` |
/// | `i8`–`i64`, `isize`, `u8`–`u64`, `usize` | [`parse::parse_int`]: leading whitespace skipped, `0x` for hexadecimal, no trailing characters |
///
/// There is one rule per type. `"1"` is `true` and `"0x10"` is `16`
/// whichever of the three functions reads it; with `FromStr`, which these
/// functions used through 0.6, both failed to parse and `get_or` returned
/// its default for them.
///
/// `f32` and `f64` have no implementation: libbase has no float getter
/// whose rules could be followed. Read a float — or any other `FromStr`
/// type this crate does not own — with
/// [`crate::SystemProperties::read_with`]:
///
/// ```rust,no_run
/// # fn main() -> rsproperties::Result<()> {
/// let scale: Option<f64> = rsproperties::system_properties()?
///     .read_with("debug.my_app.scale", |value| value.parse().ok())?;
/// # Ok(())
/// # }
/// ```
///
/// # Implementing it
///
/// Implement it for a type of your own to read that type with the same
/// three functions:
///
/// ```rust,no_run
/// enum LogLevel {
///     Verbose,
///     Info,
/// }
///
/// impl rsproperties::FromProperty for LogLevel {
///     fn from_property(value: &str) -> Option<Self> {
///         match value {
///             "verbose" => Some(Self::Verbose),
///             "info" => Some(Self::Info),
///             _ => None,
///         }
///     }
/// }
///
/// let level = rsproperties::get_or("debug.my_app.log_level", LogLevel::Info);
/// ```
///
/// [`Self::from_property`] runs while the property area's read lock is
/// held (see [`crate::SystemProperties::read_with`]): keep it short and do
/// not call this crate's property API from it.
pub trait FromProperty: Sized {
    /// The value as `Self`, or `None` when `value` is not one.
    ///
    /// On `None`, [`crate::get`] fails with `Error::Parse` and
    /// [`crate::get_or`] / [`crate::get_or_else`] return the default. The
    /// `get_or` functions do not call this for an empty value: empty
    /// counts as unset there.
    fn from_property(value: &str) -> Option<Self>;
}

impl FromProperty for String {
    fn from_property(value: &str) -> Option<Self> {
        Some(value.to_owned())
    }
}

impl FromProperty for bool {
    fn from_property(value: &str) -> Option<Self> {
        parse::parse_bool(value)
    }
}

macro_rules! impl_from_property_int {
    ($($t:ty),*) => {$(
        impl FromProperty for $t {
            fn from_property(value: &str) -> Option<Self> {
                parse::parse_int(value)
            }
        }
    )*};
}

impl_from_property_int!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_and_int_follow_libbase() {
        assert_eq!(bool::from_property("1"), Some(true));
        assert_eq!(bool::from_property("off"), Some(false));
        assert_eq!(bool::from_property("TRUE"), None);
        assert_eq!(u32::from_property("0x10"), Some(16));
        assert_eq!(i32::from_property(" 42"), Some(42));
        assert_eq!(i32::from_property("12abc"), None);
        assert_eq!(u8::from_property("256"), None);
    }

    #[test]
    fn string_keeps_the_value() {
        assert_eq!(String::from_property(" a b "), Some(" a b ".to_owned()));
        assert_eq!(String::from_property(""), Some(String::new()));
    }

    /// 0.6 parsed with `FromStr`. Every input `FromStr` accepts must still
    /// parse to the same value, so an upgrade changes a result only for
    /// inputs that used to fail.
    #[test]
    fn accepts_what_from_str_accepted() {
        let inputs = [
            "0",
            "7",
            "+7",
            "-7",
            "-0",
            "007",
            "127",
            "128",
            "-128",
            "-129",
            "255",
            "256",
            "65535",
            "2147483647",
            "2147483648",
            "-2147483648",
            "4294967295",
            "9223372036854775807",
            "-9223372036854775808",
            "18446744073709551615",
            "18446744073709551616",
            "",
            "+",
            "-",
            " 7",
            "7 ",
            "0x10",
            "1_000",
            "1.0",
            "true",
            "false",
        ];
        macro_rules! check {
            ($($t:ty),*) => {$(
                for input in inputs {
                    if let Ok(expected) = input.parse::<$t>() {
                        assert_eq!(
                            <$t>::from_property(input),
                            Some(expected),
                            "{} from {input:?}",
                            stringify!($t)
                        );
                    }
                }
            )*};
        }
        check!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, bool);
    }
}
