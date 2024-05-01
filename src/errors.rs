// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::panic::Location;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error at {location} - {source}")]
    Io {
        source: std::io::Error,
        location: &'static Location<'static>,
    },

    #[error("Nix errorno at {location} - {source}")]
    Errno {
        source: rustix::io::Errno,
        location: &'static Location<'static>,
    },

    #[error("Invalid data: at {location} - {message}")]
    InvalidData {
        message: String,
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
    pub fn new_invalid_data(message: String) -> Error {
        Error::InvalidData { message, location: Location::caller() }
    }

    #[track_caller]
    pub fn new_utf8(source: std::str::Utf8Error) -> Error {
        Error::InvalidData { message: format!("{}", source), location: Location::caller() }
    }
}
