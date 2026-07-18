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
    // The portable-across-unix trait (uniform `u32`/`u64` accessors), not
    // the per-OS `st_*` variants — those needed three cfg'd imports plus a
    // macOS-only integer cast, and broke the build on every other unix.
    use std::os::unix::fs::MetadataExt;

    // Only regular files are acceptable mmap targets. The metadata comes
    // from an already-open fd (fstat), so this check is race-free; without
    // it a directory fails later at mmap with a confusing ENODEV, and a
    // device file could be mapped successfully despite not being a
    // trustworthy property file.
    if !metadata.is_file() {
        let error_msg = format!("Not a regular file: {path:?}");
        log::error!("{error_msg}");
        return Err(Error::FileValidation(error_msg));
    }

    // Check file size first (applies to all modes)
    if metadata.size() < min_size {
        let error_msg = format!(
            "File too small: size={}, min_size={} for {:?}",
            metadata.size(),
            min_size,
            path
        );
        log::error!("{error_msg}");
        return Err(Error::FileSize(error_msg));
    }

    // Check write permissions (applies to all modes). 0o022 = the
    // group-write | other-write bits (S_IWGRP | S_IWOTH).
    let writable_by_others = metadata.mode() & 0o022;
    if writable_by_others != 0 {
        let error_msg = format!(
            "File has group or other write permissions: mode={:#o} for {:?}",
            metadata.mode(),
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
    //
    // `debug-assertions` is not a security switch, though: a release
    // profile with `debug-assertions = true` (commonly enabled for
    // overflow checks — this workspace's bench profile does exactly that)
    // would silently disable the check as a side effect. Deployments in
    // that situation opt back in explicitly with the
    // `strict-file-validation` feature, which enforces ownership
    // regardless of the profile. The remaining skip is logged so it is
    // observable either way.
    let skip_ownership_check = cfg!(debug_assertions) && !cfg!(feature = "strict-file-validation");

    if skip_ownership_check {
        // AtomicBool, not `Once`: a logger backed by property reads would
        // re-enter this function from inside its own log call, and
        // re-entering an in-progress `call_once` on the same thread is a
        // deadlock by contract. A lost race here just means one extra warn.
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::warn!(
                "root-ownership check on property files is disabled \
                 (debug-assertions build)"
            );
        }
    }

    if !skip_ownership_check && (metadata.uid() != 0 || metadata.gid() != 0) {
        let error_msg = format!(
            "File not owned by root: uid={}, gid={} for {:?}",
            metadata.uid(),
            metadata.gid(),
            path
        );
        log::error!("{error_msg}");
        return Err(Error::FileOwnership(error_msg));
    }

    Ok(())
}
