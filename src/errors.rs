// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{num::ParseIntError, panic::Location};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error at {location} - {source}")]
    Io {
        source: std::io::Error,
        location: &'static Location<'static>,
    },

    #[error("Errno at {location} - {source}")]
    Errno {
        source: rustix::io::Errno,
        location: &'static Location<'static>,
    },

    #[error("Error {context}: at {location}")]
    Context {
        context: String,
        location: &'static Location<'static>,
    },

    #[error("NotFound: {location} - Key: {key}")]
    NotFound {
        key: String,
        location: &'static Location<'static>,
    },
}

impl Error {
    #[track_caller]
    pub fn new_io(source: std::io::Error) -> Error {
        Error::Io { source, location: Location::caller() }
    }

    #[track_caller]
    pub fn new_errno(source: rustix::io::Errno) -> Error {
        Error::Errno { source, location: Location::caller() }
    }

    #[track_caller]
    pub fn new_context(context: String) -> Error {
        Error::Context { context, location: Location::caller() }
    }

    #[track_caller]
    pub fn new_not_found(key: String) -> Error {
        Error::NotFound { key, location: Location::caller() }
    }
}

impl From<std::io::Error> for Error {
    #[track_caller]
    fn from(source: std::io::Error) -> Self {
        Error::Io { source, location: Location::caller() }
    }
}

impl From<std::str::Utf8Error> for Error {
    #[track_caller]
    fn from(source: std::str::Utf8Error) -> Self {
        Error::Context { context: format!("{}", source), location: Location::caller() }
    }
}

impl From<std::ffi::OsString> for Error {
    #[track_caller]
    fn from(source: std::ffi::OsString) -> Self {
        Error::Context { context: format!("{:?}", source), location: Location::caller() }
    }
}

impl From<&str> for Error {
    #[track_caller]
    fn from(source: &str) -> Self {
        Error::Context { context: source.to_owned(), location: Location::caller() }
    }
}

impl From<ParseIntError> for Error {
    #[track_caller]
    fn from(source: ParseIntError) -> Self {
        Error::Context { context: format!("{}", source), location: Location::caller() }
    }
}

pub trait ResultContext<T,E> {
    fn context<C>(self, context: C) -> Result<T>
    where
        C: std::fmt::Display + Send + Sync + 'static;

    fn with_context<C,F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> C,
        C: std::fmt::Display + Send + Sync + 'static;
}

impl<T,E> ResultContext<T,E> for std::result::Result<T,E>
where
    E: std::fmt::Display + Send + Sync + 'static,
{
    #[track_caller]
    fn context<C>(self, context: C) -> Result<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
    {
        let location = Location::caller();
        self.map_err(|e| {
            let context = format!("{}: {}", e, context);
            Error::Context { context, location }
        })
    }

    #[track_caller]
    fn with_context<C,F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> C,
        C: std::fmt::Display + Send + Sync + 'static,
    {
        let location = Location::caller();
        self.map_err(|e| {
            let context = format!("{}: {}", e, f());
            Error::Context { context, location }
        })
    }
}

impl<T> ResultContext<T,std::convert::Infallible> for std::option::Option<T>
{
    #[track_caller]
    fn context<C>(self, context: C) -> Result<T>
    where
        C: std::fmt::Display,
    {
        let location = Location::caller();
        match self {
            Some(v) => Ok(v),
            None => Err(Error::Context { context: format!("{}", context), location }),
        }
    }

    #[track_caller]
    fn with_context<C,F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> C,
        C: std::fmt::Display,
    {
        let location = Location::caller();
        match self {
            Some(v) => Ok(v),
            None => Err(Error::Context { context: format!("{}", f()), location }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
