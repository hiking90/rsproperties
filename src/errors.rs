// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{fmt, error, panic::Location};

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
        source: nix::errno::Errno,
        location: &'static Location<'static>,
    },

    #[error("Invalid data: at {location} - {message}")]
    InvalidData {
        message: &'static str,
        location: &'static Location<'static>,
    },
}

impl Error {
    #[track_caller]
    pub fn new_io(source: std::io::Error) -> Error {
        Error::Io { source, location: Location::caller() }
    }

    #[track_caller]
    pub fn new_errno(source: nix::errno::Errno) -> Error {
        Error::Errno { source, location: Location::caller() }
    }

    #[track_caller]
    pub fn new_invalid_data(message: &'static str) -> Error {
        Error::InvalidData { message, location: Location::caller() }
    }
}
