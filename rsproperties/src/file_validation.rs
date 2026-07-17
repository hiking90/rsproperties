// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared on-disk metadata validation for the mmap'd property files
//! (`property_info`, per-context areas, `properties_serial`).

use crate::errors::{Error, Result};

/// Validates file metadata for system property files.
///
/// In test and debug modes, only checks file permissions and size.
/// In production mode, also enforces that the file is owned by root (uid=0, gid=0).
pub(crate) fn validate_file_metadata(
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
        log::error!("{error_msg}");
        return Err(Error::FileSize(error_msg));
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    let check_permissions = metadata.st_mode() & (fs::Mode::WGRP.bits() | fs::Mode::WOTH.bits());

    #[cfg(target_os = "macos")]
    let check_permissions =
        metadata.st_mode() & (fs::Mode::WGRP.bits() | fs::Mode::WOTH.bits()) as u32;

    // Check write permissions (applies to all modes)
    if check_permissions != 0 {
        let error_msg = format!(
            "File has group or other write permissions: mode={:#o} for {:?}",
            metadata.st_mode(),
            path
        );
        log::error!("{error_msg}");
        return Err(Error::PermissionDenied(error_msg));
    }

    // In production (release) builds, also check ownership.
    //
    // `cfg!(test)` is intentionally NOT part of this condition: it is only
    // true while compiling this crate's own `--test` harness, so it never
    // covers integration tests, doctests, or downstream crates — relying
    // on it made `cargo test` and `cargo test --release` behave
    // differently for the same fixture files. `debug_assertions` alone
    // draws the line uniformly at dev-vs-release.
    let skip_ownership_check = cfg!(debug_assertions);

    if !skip_ownership_check && (metadata.st_uid() != 0 || metadata.st_gid() != 0) {
        let error_msg = format!(
            "File not owned by root: uid={}, gid={} for {:?}",
            metadata.st_uid(),
            metadata.st_gid(),
            path
        );
        log::error!("{error_msg}");
        return Err(Error::FileOwnership(error_msg));
    }

    Ok(())
}
