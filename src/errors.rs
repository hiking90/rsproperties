// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{num::ParseIntError, panic::Location};
use anyhow::Context;

pub type Result<T> = std::result::Result<T, anyhow::Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error - {0} at {1}")]
    Io(std::io::Error, &'static Location<'static>),

    #[error("Errno error - {0} at {1}")]
    Errno(rustix::io::Errno, &'static Location<'static>),

    #[error("NotFound error - Key: {0} at {1}")]
    NotFound(String, &'static Location<'static>),

    #[error("Context error - {0} at {1}")]
    Context(String, &'static Location<'static>),
}

impl Error {
    #[track_caller]
    pub fn new_not_found(key: String) -> Error {
        Error::NotFound(key, Location::caller())
    }

    #[track_caller]
    pub fn new_context(context: String) -> Error {
        Error::Context(context, Location::caller())
    }
}

impl From<rustix::io::Errno> for Error {
    #[track_caller]
    fn from(source: rustix::io::Errno) -> Self {
        Error::Errno(source, Location::caller())
    }
}

impl From<std::io::Error> for Error {
    #[track_caller]
    fn from(source: std::io::Error) -> Self {
        Error::Io(source, Location::caller())
    }
}

impl From<std::str::Utf8Error> for Error {
    #[track_caller]
    fn from(source: std::str::Utf8Error) -> Self {
        Error::Context(format!("{}", source), Location::caller())
    }
}

impl From<std::ffi::OsString> for Error {
    #[track_caller]
    fn from(source: std::ffi::OsString) -> Self {
        Error::Context(format!("{:?}", source), Location::caller())
    }
}

impl From<&str> for Error {
    #[track_caller]
    fn from(source: &str) -> Self {
        Error::Context(source.to_owned(), Location::caller())
    }
}

impl From<ParseIntError> for Error {
    #[track_caller]
    fn from(source: ParseIntError) -> Self {
        Error::Context(format!("{}", source), Location::caller())
    }
}

pub trait ContextWithLocation<T> {
    fn context_with_location(self, msg: impl Into<String>) -> Result<T>;
}

impl<T, E> ContextWithLocation<T> for std::result::Result<T, E>
where
    E: Into<anyhow::Error>,
{
    #[track_caller]
    fn context_with_location(self, msg: impl Into<String>) -> Result<T> {
        self.map_err(|e| e.into()).context(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn try_open_file() -> Result<()> {
        std::fs::File::open("non-existent-file")?;
        Ok(())
    }

    #[test]
    fn test_error_location() {
        try_open_file()
            .map_err(|e| {
                println!("Error: {}", e);
                e
            }).unwrap_err();
        std::fs::File::open("non-existent-file")
            .context("Failed to open file")
            .unwrap_err();
    }
}
