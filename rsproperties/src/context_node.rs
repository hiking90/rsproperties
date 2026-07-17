// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::ffi::CString;
use std::path::PathBuf;
#[cfg(feature = "builder")]
use std::sync::RwLockWriteGuard;
use std::sync::{RwLock, RwLockReadGuard};

use log::error;

use crate::errors::*;
use crate::property_area::PropertyAreaMap;

pub(crate) struct ContextNode {
    access_rw: bool,
    /// SELinux context this area belongs to (e.g.
    /// `u:object_r:system_prop:s0`). Applied as the `security.selinux`
    /// xattr when the area file is created read-write, mirroring bionic's
    /// `context_node::open` which labels each per-context file. `Some`
    /// only for writable nodes — read-only instances never label files,
    /// so they skip the allocation.
    context: Option<CString>,
    filename: PathBuf,
    /// Lazy-initialized property area. Once a writer puts `Some`, no code
    /// path ever resets it to `None` — this is the invariant that lets the
    /// `*Guard` types below skip the `expect()` runtime panic. The
    /// invariant is enforced by keeping the field private and only
    /// exposing it through this module's API.
    property_area: RwLock<Option<PropertyAreaMap>>,
}

impl ContextNode {
    pub(crate) fn new(access_rw: bool, context: Option<CString>, filename: PathBuf) -> Self {
        Self {
            access_rw,
            context,
            filename,
            property_area: RwLock::new(None),
        }
    }

    pub(crate) fn open(&self) -> Result<()> {
        if !self.access_rw {
            error!(
                "Attempted to open context node without write access: {:?}",
                self.filename
            );
            // A usage-contract violation, not a lock failure.
            return Err(Error::PermissionDenied(format!(
                "open() requires access_rw == true: {:?}",
                self.filename
            )));
        }

        let mut prop_area = self.property_area.write().map_err(lock_err("write"))?;
        if prop_area.as_ref().is_some_and(|pa| pa.is_writable()) {
            // Already opened read-write by a previous call.
            return Ok(());
        }
        if prop_area.is_some() {
            // A map exists but is read-only: the *read* path (`property_area`)
            // lazily creates RO maps regardless of `access_rw`, and if it ran
            // before this open() the slot holds one. Returning Ok here would
            // report "opened" while every subsequent write fails — surface
            // the ordering violation instead.
            error!(
                "open() called after the read path mapped {:?} read-only",
                self.filename
            );
            return Err(Error::PermissionDenied(format!(
                "property area already mapped read-only: {:?}",
                self.filename
            )));
        }

        *prop_area = Some(PropertyAreaMap::new_rw(
            self.filename.as_path(),
            self.context.as_deref(),
        )?);

        Ok(())
    }

    pub(crate) fn property_area(&self) -> Result<PropertyAreaGuard<'_>> {
        // The read path recovers from lock poison instead of failing: a
        // writer panicking under this RwLock cannot leave the protected
        // `Option` half-updated (`Some` is assigned whole and never reverts
        // to `None`), and mmap *content* consistency is the seqlock's job —
        // cross-process readers don't take this lock at all. Propagating
        // poison here would trade zero safety for permanently failing every
        // in-process read on this node. The builder-side paths (`open`,
        // `property_area_mut`) stay strict.
        use std::sync::PoisonError;
        // Fast path: already initialized.
        {
            let guard = self
                .property_area
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if guard.is_some() {
                return Ok(PropertyAreaGuard::from_initialized(guard));
            }
        }
        // Slow path: initialize under the write lock if still empty.
        {
            let mut guard = self
                .property_area
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if guard.is_none() {
                *guard = Some(PropertyAreaMap::new_ro(self.filename.as_path())?);
            }
        }
        // Re-acquire read lock for the typed guard.
        let guard = self
            .property_area
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(PropertyAreaGuard::from_initialized(guard))
    }

    #[cfg(feature = "builder")]
    pub(crate) fn property_area_mut(&self) -> Result<PropertyAreaMutGuard<'_>> {
        let guard = self.property_area.write().map_err(lock_err("write"))?;
        // Never lazily initialize here: the only mapping this path could
        // create is a read-only one (`new_ro`), and handing out a mut
        // guard over a PROT_READ mapping would SIGSEGV on first write.
        // Writable areas are opened eagerly by `ContextsSerialized::new`
        // via `open()`. Also reject a map that exists but is read-only
        // (e.g. lazily created by the *read* path before this call) — the
        // borrow-level guard must never wrap a PROT_READ mapping.
        let writable = guard.as_ref().is_some_and(|pa| pa.is_writable());
        if !writable {
            error!(
                "property_area_mut on an unopened or read-only area: {:?}",
                self.filename
            );
            return Err(Error::PermissionDenied(format!(
                "property area not opened read-write: {:?}",
                self.filename
            )));
        }
        Ok(PropertyAreaMutGuard::from_initialized(guard))
    }
}

fn lock_err<T>(kind: &'static str) -> impl Fn(std::sync::PoisonError<T>) -> Error {
    move |e| {
        Error::Lock(format!(
            "Failed to acquire {kind} lock on property area: {e}"
        ))
    }
}

/// Read-guard that only exists once the underlying `Option` has been
/// initialized. The `from_initialized` constructor is the single entry
/// point and is `pub(self)` so callers in this module cannot bypass the
/// `is_some()` check.
pub(crate) struct PropertyAreaGuard<'a> {
    guard: RwLockReadGuard<'a, Option<PropertyAreaMap>>,
}

impl<'a> PropertyAreaGuard<'a> {
    /// Caller MUST have verified `guard.is_some()`. The `debug_assert!`
    /// is a development-time tripwire; release builds rely on the
    /// type-level invariant documented on `ContextNode::property_area`.
    fn from_initialized(guard: RwLockReadGuard<'a, Option<PropertyAreaMap>>) -> Self {
        debug_assert!(
            guard.is_some(),
            "PropertyAreaGuard constructed from uninitialized Option — \
             this would have panicked in release at the access site"
        );
        Self { guard }
    }

    pub(crate) fn property_area(&self) -> &PropertyAreaMap {
        // SAFETY-level invariant: `from_initialized` is the only ctor; it
        // requires `Some(...)`. No code path in this module resets the
        // underlying `Option<PropertyAreaMap>` back to `None`.
        self.guard
            .as_ref()
            .expect("PropertyAreaGuard constructed only from Some(...); see ContextNode invariant")
    }
}

/// Write-guard counterpart of [`PropertyAreaGuard`]. Same invariant.
#[cfg(feature = "builder")]
pub(crate) struct PropertyAreaMutGuard<'a> {
    guard: RwLockWriteGuard<'a, Option<PropertyAreaMap>>,
}

#[cfg(feature = "builder")]
impl<'a> PropertyAreaMutGuard<'a> {
    fn from_initialized(guard: RwLockWriteGuard<'a, Option<PropertyAreaMap>>) -> Self {
        debug_assert!(
            guard.is_some(),
            "PropertyAreaMutGuard constructed from uninitialized Option — \
             this would have panicked in release at the access site"
        );
        Self { guard }
    }

    pub(crate) fn property_area_mut(&mut self) -> &mut PropertyAreaMap {
        self.guard.as_mut().expect(
            "PropertyAreaMutGuard constructed only from Some(...); see ContextNode invariant",
        )
    }
}
