// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::num::ParseIntError;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(std::io::Error),

    #[error("System error: {0}")]
    Errno(rustix::io::Errno),

    #[error("Property not found: {0}")]
    NotFound(String),

    #[error("Encoding error: {0}")]
    Encoding(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("File validation error: {0}")]
    FileValidation(String),

    #[error("Conversion error: {0}")]
    Conversion(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("File size error: {0}")]
    FileSize(String),

    #[error("File ownership error: {0}")]
    FileOwnership(String),
}

impl Error {
    pub fn new_not_found(key: String) -> Error {
        Error::NotFound(key)
    }

    pub fn new_encoding(msg: String) -> Error {
        Error::Encoding(msg)
    }

    pub fn new_parse(msg: String) -> Error {
        Error::Parse(msg)
    }

    pub fn new_file_validation(msg: String) -> Error {
        Error::FileValidation(msg)
    }

    pub fn new_conversion(msg: String) -> Error {
        Error::Conversion(msg)
    }

    pub fn new_permission_denied(msg: String) -> Error {
        Error::PermissionDenied(msg)
    }

    pub fn new_file_size(msg: String) -> Error {
        Error::FileSize(msg)
    }

    pub fn new_file_ownership(msg: String) -> Error {
        Error::FileOwnership(msg)
    }

    pub fn new_io(io_error: std::io::Error) -> Error {
        let error = Error::Io(io_error);
        log::error!("I/O error: {}", error);
        error
    }

    pub fn new_errno(errno: rustix::io::Errno) -> Error {
        let error = Error::Errno(errno);
        log::error!("System error: {}", error);
        error
    }
}

impl From<rustix::io::Errno> for Error {
    fn from(source: rustix::io::Errno) -> Self {
        let error = Error::Errno(source);
        log::error!("Converting errno to Error: {}", source);
        error
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        let error = Error::Io(source);
        log::error!("Converting I/O error to Error: {}", error);
        error
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(source: std::str::Utf8Error) -> Self {
        let error_msg = format!("UTF-8 conversion error: {}", source);
        log::error!("{}", error_msg);
        Error::Encoding(error_msg)
    }
}

impl From<std::ffi::OsString> for Error {
    fn from(source: std::ffi::OsString) -> Self {
        let error_msg = format!("OsString conversion error: {:?}", source);
        log::error!("{}", error_msg);
        Error::Conversion(error_msg)
    }
}

impl From<&str> for Error {
    fn from(source: &str) -> Self {
        log::error!("String error: {}", source);
        Error::Parse(source.to_owned())
    }
}

impl From<ParseIntError> for Error {
    fn from(source: ParseIntError) -> Self {
        let error_msg = format!("Parse integer error: {}", source);
        log::error!("{}", error_msg);
        Error::Parse(error_msg)
    }
}

pub trait ContextWithLocation<T> {
    fn context_with_location(self, msg: impl Into<String>) -> Result<T>;
}

impl<T, E> ContextWithLocation<T> for std::result::Result<T, E>
where
    E: Into<Error>,
{
    fn context_with_location(self, msg: impl Into<String>) -> Result<T> {
        self.map_err(|e| e.into())
            .map_err(|_| Error::new_file_validation(msg.into()))
    }
}

/// Validates file metadata for system property files.
///
/// In test and debug modes, only checks file permissions and size.
/// In production mode, also enforces that the file is owned by root (uid=0, gid=0).
pub fn validate_file_metadata(
    metadata: &std::fs::Metadata,
    path: &std::path::Path,
    min_size: u64,
) -> Result<()> {
    // Platform-specific MetadataExt imports
    #[cfg(target_os = "android")]
    use std::os::android::fs::MetadataExt;
    #[cfg(target_os = "linux")]
    use std::os::linux::fs::MetadataExt;
    #[cfg(target_os = "macos")]
    use std::os::macos::fs::MetadataExt;

    use rustix::fs;

    // Check file size first (applies to all modes)
    if metadata.st_size() < min_size {
        let error_msg = format!(
            "File too small: size={}, min_size={} for {:?}",
            metadata.st_size(),
            min_size,
            path
        );
        log::error!("{}", error_msg);
        return Err(Error::new_file_size(error_msg));
    }

    // Check write permissions (applies to all modes)
    if metadata.st_mode() & (fs::Mode::WGRP.bits() | fs::Mode::WOTH.bits()) as u32 != 0 {
        let error_msg = format!(
            "File has group or other write permissions: mode={:#o} for {:?}",
            metadata.st_mode(),
            path
        );
        log::error!("{}", error_msg);
        return Err(Error::new_permission_denied(error_msg));
    }

    // In production mode, also check ownership
    // Skip ownership checks only in test/development environments:
    // 1. When compiled with debug assertions (development builds)
    // 2. When compiled in test configuration
    // This is compile-time only and cannot be bypassed at runtime
    let skip_ownership_check = cfg!(debug_assertions) || cfg!(test);

    if !skip_ownership_check {
        if metadata.st_uid() != 0 || metadata.st_gid() != 0 {
            let error_msg = format!(
                "File not owned by root: uid={}, gid={} for {:?}",
                metadata.st_uid(),
                metadata.st_gid(),
                path
            );
            log::error!("{}", error_msg);
            return Err(Error::new_file_ownership(error_msg));
        }
    }

    Ok(())
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
            })
            .unwrap_err();
        std::fs::File::open("non-existent-file")
            .context("Failed to open file")
            .unwrap_err();
    }
}
