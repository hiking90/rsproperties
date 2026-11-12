// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! # Android System Properties for Linux and Android both.
//!
//! This crate provides a way to access system properties on Linux and Android.
//!
//! ## Features
//!
//! - Get system properties.
//! - Set system properties.
//! - Wait for system properties.
//! - Serialize/deserialize the property-info trie (requires the `builder`
//!   feature).
//!
//! ## Usage
//!
//! ```rust,no_run
//! // Get a value of the property.
//! let value: String = rsproperties::get_or("ro.build.version.sdk", "0".to_owned());
//! println!("ro.build.version.sdk: {}", value);
//!
//! // Set a value of the property - use string literals for compatibility
//! // with values other Android components write (e.g. "1", not "true").
//! rsproperties::set("test.property", "test.value").unwrap();
//! ```

// Forward-compat with Rust 2024 edition: `unsafe fn` bodies must wrap
// individual unsafe ops in their own `unsafe {}` blocks rather than
// inheriting the function's effect. Enabling this lint as a warning today
// keeps the codebase ready and surfaces regressions in PRs.
#![warn(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
// docs.rs builds with `--cfg docsrs` (see `[package.metadata.docs.rs]`):
// items behind a `cfg` are then labelled with the feature they need.
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// The directories [`init`] sets. A directory left out is not touched: it
/// keeps whatever it has, or latches its default on first use.
///
/// ```
/// let config = rsproperties::PropertyConfig::new()
///     .properties_dir("/custom/properties")
///     .socket_dir("/custom/socket");
/// ```
///
/// Paths are not checked here: a missing directory is reported at its
/// first use (first property read, first `set`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropertyConfig {
    properties_dir: Option<PathBuf>,
    socket_dir: Option<PathBuf>,
}

impl PropertyConfig {
    /// No directories: [`init`] with it changes nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// The directory the property areas are read from (default
    /// [`PROP_DIRNAME`], `/dev/__properties__`).
    pub fn properties_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.properties_dir = Some(dir.into());
        self
    }

    /// The directory of the property service sockets (default
    /// `/dev/socket`, or `PROPERTY_SERVICE_SOCKET_DIR`; see [`socket_dir`]).
    pub fn socket_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.socket_dir = Some(dir.into());
        self
    }
}

mod errors;
mod from_property;
pub mod parse;
pub mod wire;
pub use errors::{Error, Result};
pub use from_property::FromProperty;

#[cfg(feature = "builder")]
mod build_property_parser;
mod context_node;
mod contexts_serialized;
mod file_validation;
mod property_area;
mod property_info;
mod property_info_parser;
#[cfg(feature = "builder")]
mod property_info_serializer;
mod system_properties;
mod system_property_set;
#[cfg(feature = "builder")]
mod trie_builder;
#[cfg(feature = "builder")]
mod trie_node_arena;
#[cfg(feature = "builder")]
mod trie_serializer;

/// Unit-test fixtures shared by `tests` below and `build_property_parser`.
#[cfg(all(test, feature = "builder", not(target_os = "android")))]
pub(crate) mod test_support {
    use std::fs::{File, OpenOptions, Permissions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    /// `temp_dir()/<tag>_<pid>`, removed on drop (panics included). The
    /// guard holds the path before the dir is created, so nothing created
    /// after `new` outlives a failing test. Tests of one binary share the
    /// pid: `tag` must differ between tests that can run concurrently.
    pub(crate) struct FixtureDir(PathBuf);

    impl FixtureDir {
        pub(crate) fn new(tag: &str) -> Self {
            let guard = Self(std::env::temp_dir().join(format!("{tag}_{}", std::process::id())));
            std::fs::create_dir_all(&guard.0).unwrap();
            guard
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FixtureDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Removes every `temp_dir()/<prefix><pid>` whose process is gone
    /// (`kill(pid, 0)` fails with `ESRCH`).
    ///
    /// For a dir that a `static` owns until the process exits, where no
    /// guard can remove it: the next run of the test removes it instead.
    /// The dir of a live process — another `cargo test` running the same
    /// test — is kept. A reused pid keeps a dead dir: that leaks one
    /// directory but never deletes a live one.
    pub(crate) fn sweep_dead_dirs(prefix: &str) {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let pid = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix(prefix))
                .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|p| p.parse::<i32>().ok())
                .and_then(rustix::process::Pid::from_raw);
            let Some(pid) = pid else { continue };
            if rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// Copies `tests/android/<name>` into `into` with mode 0644 and returns
    /// the copy's path.
    ///
    /// The loader skips group/other-writable prop files like AOSP init
    /// (`open_prop_file`), and git checks the fixtures out as
    /// `0666 & ~umask` — 0664 under umask 002 — so tests load copies, not
    /// the checkout.
    pub(crate) fn copy_android_fixture(name: &str, into: &Path) -> PathBuf {
        copy_fixture(&Path::new("tests/android").join(name), into)
    }

    fn copy_fixture(src: &Path, into: &Path) -> PathBuf {
        let dst = into.join(src.file_name().expect("fixture path has a file name"));
        // Overwrite rather than `create_new`: a copy left at `dst` (pid
        // reuse, an earlier caller) must not fail with EEXIST. `mode` only
        // applies when the file is created, so `fchmod` re-asserts 0644 on
        // an existing one.
        let mut out = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&dst)
            .unwrap_or_else(|e| panic!("create {dst:?}: {e}"));
        out.set_permissions(Permissions::from_mode(0o644))
            .unwrap_or_else(|e| panic!("fchmod {dst:?}: {e}"));
        let mut input = File::open(src).unwrap_or_else(|e| panic!("open {src:?}: {e}"));
        std::io::copy(&mut input, &mut out).unwrap_or_else(|e| panic!("copy {src:?}: {e}"));
        dst
    }

    #[test]
    fn copy_fixture_is_0644_over_writable_source_and_stale_copy() {
        use std::collections::HashMap;
        use std::fs::{create_dir, metadata, set_permissions, write};

        let dir = FixtureDir::new("rsprops_copy_fixture");
        let (src_dir, out_dir) = (dir.path().join("src"), dir.path().join("out"));
        create_dir(&src_dir).unwrap();
        create_dir(&out_dir).unwrap();

        // Source as a umask 002 checkout leaves it.
        let src = src_dir.join("a.prop");
        write(&src, "ro.x=1\n").unwrap();
        set_permissions(&src, Permissions::from_mode(0o664)).unwrap();
        // A longer, world-writable copy already at the destination.
        let stale = out_dir.join("a.prop");
        write(&stale, "#######\nro.y=stale\n").unwrap();
        set_permissions(&stale, Permissions::from_mode(0o666)).unwrap();

        let copy = copy_fixture(&src, &out_dir);
        assert_eq!(copy, stale);
        assert_eq!(
            metadata(&copy).unwrap().permissions().mode() & 0o7777,
            0o644
        );

        let mut properties = HashMap::new();
        crate::load_properties_from_file(&copy, None, &mut properties).unwrap();
        assert_eq!(properties.get("ro.x").map(String::as_str), Some("1"));
        assert!(
            !properties.contains_key("ro.y"),
            "stale bytes left: {properties:?}"
        );
    }
}

// Explicit re-export lists (not globs) so the public API surface is
// visible here and additions to the modules don't silently become public.
#[cfg(feature = "builder")]
pub use build_property_parser::load_properties_from_file;
#[cfg(feature = "builder")]
pub use contexts_serialized::AreaWriterLock;
#[cfg(feature = "builder")]
pub use property_info_serializer::{build_trie, MatchKeyword, PropertyInfoEntry};
pub use system_properties::{PropertyIndex, SystemProperties};
pub use system_property_set::socket_dir;

pub use system_property_set::{
    PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME, PROPERTY_SERVICE_SOCKET_NAME,
};

/// The default properties directory, used unless [`init`] names another
/// one (see [`properties_dir`]). On Android this is where init creates the
/// property areas.
pub const PROP_DIRNAME: &str = "/dev/__properties__";

// System properties directory.
static SYSTEM_PROPERTIES_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Serializes every commit to the first-write-wins directory cells
/// (`SYSTEM_PROPERTIES_DIR` here and `SOCKET_DIR` in `system_property_set`).
/// `init` must make its pre-check + set atomic against both concurrent
/// inits and the implicit env/default latch performed by the first call to
/// `properties_dir()` / `socket_dir()` — otherwise a lost race after the
/// pre-check leaves the globals half-applied with no way to roll back a
/// committed `OnceLock`. Read fast paths (`OnceLock::get`) stay lock-free.
///
/// Private on purpose: all access goes through [`lock_global_dirs`] so no
/// call site can bypass its poison recovery.
static GLOBAL_DIRS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire `GLOBAL_DIRS_LOCK`, recovering from poison: the lock only guards
/// the check-then-set ordering of `OnceLock` cells, each of which is
/// internally consistent even if a holder panicked mid-sequence.
pub(crate) fn lock_global_dirs() -> std::sync::MutexGuard<'static, ()> {
    GLOBAL_DIRS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
/// The global reader, once created. Only a success is stored: a failed
/// creation leaves the cell empty and the next access tries again, the
/// way area files are reopened (`ContextNode::property_area`) — this
/// crate's service can create the store after a reader started.
/// `OnceLock::get_or_try_init` would express this but is unstable (Rust
/// issue 109737); [`SYSTEM_PROPERTIES_INIT`] provides its serialization.
static SYSTEM_PROPERTIES: OnceLock<system_properties::SystemProperties> = OnceLock::new();

/// Serializes creating [`SYSTEM_PROPERTIES`], so concurrent first readers
/// open and map the store once. Not `GLOBAL_DIRS_LOCK`: creation calls
/// `properties_dir()`, which takes that lock, and `Mutex` is not
/// reentrant. Lock order: this one, then `GLOBAL_DIRS_LOCK`.
static SYSTEM_PROPERTIES_INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Whether a creation failure was logged at `error` yet. Later failures
/// log at `debug`, so a caller that keeps reading during an outage does
/// not write an error line per read.
static INIT_FAILURE_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Sets the directories `config` names. Call it before the first property
/// access in the process.
///
/// Each directory is set once per process: by an earlier `init`, or by
/// its first use, which latches the default (the properties directory on
/// the first property read, the socket directory on the first `set()` or
/// [`socket_dir`] call). So `init` returns `Ok(())` when every directory
/// it names is unset or already holds that same path, and
/// `Error::AlreadyInitialized` when one holds a different path — then
/// neither directory is changed. Paths are compared with [`Path::eq`]
/// (component-wise, not canonicalized): a symlink to the set directory is
/// a different path. A directory `config` leaves out is not touched.
///
/// ```rust,no_run
/// # fn main() -> rsproperties::Result<()> {
/// use rsproperties::PropertyConfig;
///
/// rsproperties::init(
///     PropertyConfig::new()
///         .properties_dir("/custom/properties")
///         .socket_dir("/custom/socket"),
/// )?;
/// # Ok(())
/// # }
/// ```
pub fn init(config: PropertyConfig) -> Result<()> {
    // Pre-check both directories before setting either, so a rejected call
    // changes nothing. The lock makes pre-check + set atomic against
    // concurrent `init`s and the implicit latches in `properties_dir()` /
    // `socket_dir()`.
    let _guard = lock_global_dirs();
    let PropertyConfig {
        properties_dir,
        socket_dir,
    } = config;
    if let (Some(want), Some(have)) = (&properties_dir, SYSTEM_PROPERTIES_DIR.get()) {
        if want != have {
            return Err(dir_conflict("system properties directory", have, want));
        }
    }
    if let (Some(want), Some(have)) = (&socket_dir, system_property_set::socket_dir_if_set()) {
        if want != have {
            return Err(dir_conflict("socket directory", have, want));
        }
    }

    if let Some(dir) = properties_dir {
        if SYSTEM_PROPERTIES_DIR.get().is_none() {
            log::info!("Setting system properties directory to: {dir:?}");
            // Unset under the lock above, so this `set` succeeds.
            let _ = SYSTEM_PROPERTIES_DIR.set(dir);
        }
    }
    if let Some(dir) = socket_dir {
        if system_property_set::socket_dir_if_set().is_none() {
            log::info!("Setting socket directory to: {dir:?}");
            let set = system_property_set::set_socket_dir(dir);
            debug_assert!(set, "socket dir was unset under GLOBAL_DIRS_LOCK");
        }
    }
    Ok(())
}

fn dir_conflict(what: &str, have: &Path, want: &Path) -> Error {
    Error::AlreadyInitialized(format!(
        "{what} is already {have:?} (set by an earlier init() or latched by its first use), \
         not {want:?}"
    ))
}

/// Get the system properties directory.
/// Returns the configured directory if init() was called,
/// otherwise returns the default PROP_DIRNAME (/dev/__properties__).
///
/// Unlike [`socket_dir`] (which honors `PROPERTY_SERVICE_SOCKET_DIR`),
/// there is deliberately no environment-variable override here: the
/// properties directory decides which mmap'd files this process trusts,
/// so it is configured only through code ([`init`]).
pub fn properties_dir() -> &'static Path {
    // Lock-free once initialized; the first call takes `GLOBAL_DIRS_LOCK` so
    // the default-latch cannot slip between `init`'s pre-check and set.
    if let Some(dir) = SYSTEM_PROPERTIES_DIR.get() {
        return dir.as_path();
    }
    let _guard = lock_global_dirs();
    SYSTEM_PROPERTIES_DIR
        .get_or_init(|| {
            log::info!("Using default properties directory: {PROP_DIRNAME}");
            PathBuf::from(PROP_DIRNAME)
        })
        .as_path()
}

/// The global instance, or `None` until it has been created. Never
/// *triggers* creation — used by call sites (e.g. the wire-protocol
/// version probe in `system_property_set`) that must not latch the default
/// properties directory as a side effect.
pub(crate) fn system_properties_if_initialized(
) -> Option<&'static system_properties::SystemProperties> {
    SYSTEM_PROPERTIES.get()
}

/// The process-wide reader of the properties directory, created on the
/// first call.
///
/// Call [`init`] first when the properties directory is not the default:
/// the first call here latches the directory (`/dev/__properties__` unless
/// `init` set one), and a later `init` naming a different one fails with
/// `AlreadyInitialized`. The first successful
/// creation is kept for the process lifetime. A failure is not kept: each
/// call opens the store again until one succeeds, so a store that appears
/// after this process started (this crate's service started later) is
/// picked up. While the store is missing, each call costs the failed open
/// (one `open` of `property_info`, or more when it exists but the areas do
/// not yet); the properties directory itself stays the one latched by the
/// first call. The failure is `Error::Init`, with the cause as `source()`.
pub fn system_properties() -> Result<&'static system_properties::SystemProperties> {
    if let Some(props) = SYSTEM_PROPERTIES.get() {
        return Ok(props);
    }
    // Poison carries no state: the guarded cell is set only on success.
    let _guard = SYSTEM_PROPERTIES_INIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Created by the thread this one waited for.
    if let Some(props) = SYSTEM_PROPERTIES.get() {
        return Ok(props);
    }
    let dir = properties_dir();
    log::debug!("Initializing global SystemProperties instance from: {dir:?}");
    match system_properties::SystemProperties::open(dir) {
        // Only a holder of `SYSTEM_PROPERTIES_INIT` sets the cell, and it
        // was empty above, so `props` is the value stored.
        Ok(props) => Ok(SYSTEM_PROPERTIES.get_or_init(|| props)),
        Err(e) => {
            if INIT_FAILURE_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::debug!("SystemProperties from {dir:?} still unavailable: {e}");
            } else {
                log::error!(
                    "Failed to initialize SystemProperties from {dir:?}: {e} \
                     (retried on the next access)"
                );
            }
            Err(Error::Init(Box::new(e)))
        }
    }
}

/// Aligns `value` *up* to the given alignment (bionic style). The
/// align-up contract — the result is never less than `value` — is
/// load-bearing for allocation size computations, so overflow panics
/// instead of saturating: a saturating add here would align *down* near
/// `usize::MAX` and hand callers an under-sized allocation.
///
/// # Panics
/// - if `alignment` is not a power of 2
/// - if `value + alignment - 1` overflows `usize`
pub(crate) fn bionic_align(value: usize, alignment: usize) -> usize {
    assert!(
        alignment.is_power_of_two(),
        "Alignment must be a power of 2"
    );

    // (value + alignment - 1) & !(alignment - 1)
    value
        .checked_add(alignment - 1)
        .expect("bionic_align: value + alignment overflows usize")
        & !(alignment - 1)
}

/// Reads the property `name` as a `T`.
///
/// `T` is a [`FromProperty`] type: `String`, `bool`, an integer, or a
/// type of the caller's. `bool` and the integers follow AOSP libbase
/// (`"1"` is `true`, `"0x10"` is `16`); the table on [`FromProperty`] has
/// the rules.
///
/// Fails with `Error::NotFound` for a missing property, `Error::Parse`
/// for a value that is not a `T`, and with the read's own error otherwise
/// (a non-UTF-8 value is `Error::Utf8`; a store that cannot be opened is
/// `Error::Init`).
///
/// Note: unlike [`get_or`], an *empty* property value is not special-cased
/// here — `get::<String>` returns `Ok("")` for a set-but-empty property,
/// while `get_or` follows the Android convention (empty = unset) and
/// falls back to the default.
///
/// The conversion runs while the property area's internal read lock is
/// held (see [`SystemProperties::read_with`]). This matters only for a
/// [`FromProperty`] implementation of the caller's: keep it short and do
/// not call this crate's property API from it.
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::get;
///
/// let sdk_version: i32 = get("ro.build.version.sdk").unwrap();
/// let version: String = get("ro.build.version.release").unwrap();
///
/// // Boolean properties are written as "0"/"1" (`ro.debuggable`) or
/// // "true"/"false" (`ro.apex.updatable`); both read as a `bool`.
/// let is_debuggable: bool = get("ro.debuggable").unwrap();
///
/// // With fallback
/// let sdk_version: i32 = get("ro.build.version.sdk").unwrap_or(0);
/// let version: String = get("ro.build.version.release").unwrap_or_default();
/// ```
pub fn get<T: FromProperty>(name: &str) -> Result<T> {
    // Route through `read_with` so the convert-and-discard path never
    // allocates a `String` — the value bytes are handed to
    // `from_property` as `&str` borrowed from the seqlock buffer (short
    // variant) or the mmap (long variant).
    system_properties()?.read_with(name, |value| {
        // The raw value is not formatted here (the error's Display reaches
        // logs), same policy as the set path's error log.
        T::from_property(value).ok_or_else(|| {
            Error::Parse(format!(
                "property {name:?} (<{} bytes>) is not a {}",
                value.len(),
                std::any::type_name::<T>()
            ))
        })
    })?
}

/// Reads the property `name` as a `T`, or returns `default`.
///
/// Never fails: it falls back on the same conditions as [`get_or_else`]
/// (missing, empty, not a `T`, or a failed read). `T` is a
/// [`FromProperty`] type, as for [`get`].
///
/// The default is constructed eagerly; when it allocates (a `String`),
/// prefer [`get_or_else`], which constructs it only on the fallback path.
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::get_or;
///
/// let sdk_version: i32 = get_or("ro.build.version.sdk", 0);
/// let version: String = get_or("ro.build.version.release", "unknown".to_owned());
///
/// // "1" and "true" are both `true`, as with libbase `GetBoolProperty`.
/// let is_debuggable = get_or("ro.debuggable", false);
/// ```
pub fn get_or<T: FromProperty>(name: &str, default: T) -> T {
    get_or_else(name, move || default)
}

/// Like [`get_or`], but the default is produced lazily: the closure runs
/// when the property is missing or empty, when its value is not a `T`, or
/// when the read fails for any other reason — a non-UTF-8 value
/// (`Error::Utf8`), a corrupt property area, or a failed global
/// initialization (see [`system_properties()`]; the next call tries
/// again).
/// Use [`get`] to tell these cases apart. A property that is found and
/// converts does not construct the default.
///
/// The conversion runs under the property area's read lock — see the
/// caution on [`get`]. The `default` closure runs after the lock is
/// released.
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::get_or_else;
///
/// let version: String = get_or_else("ro.build.version.release", || "unknown".to_owned());
/// ```
pub fn get_or_else<T, F>(name: &str, default: F) -> T
where
    T: FromProperty,
    F: FnOnce() -> T,
{
    let Ok(props) = system_properties() else {
        return default();
    };
    // `None` from the callback means "use the default". The default is
    // produced below, after `read_with` released the read lock, so the
    // callback never owns it.
    match props.read_with(name, |value| {
        if value.is_empty() {
            return None;
        }
        T::from_property(value)
    }) {
        Ok(Some(v)) => v,
        _ => default(),
    }
}

/// Set a value of the property with any Display type.
///
/// **Important**: All values are converted to strings using the `Display` trait before being stored.
/// This means that when reading properties set by other applications or systems, you should be aware
/// of potential format differences. For example:
/// - Boolean values are stored as "true"/"false" (Rust format)
/// - Numbers may have different precision or formatting
/// - Different applications may use different string representations for the same logical value
///
/// For maximum compatibility with existing Android properties, consider using string literals
/// when setting well-known system properties that may be read by other applications.
///
/// If an error occurs, it returns Err.
/// The V1 protocol has no status reply: a server-side rejection is not
/// observable and returns `Ok(())`; only V2 reports `Error::ServiceError`.
/// V2 names and `ro.` values are limited to 65535 bytes on Android
/// (`wire::AOSP_MAX_WIRE_STRING_LEN`, AOSP init's limit) and to
/// [`wire::MAX_WIRE_NAME_LEN`] / [`wire::MAX_WIRE_VALUE_LEN`] elsewhere
/// (this crate's service).
/// It uses socket communication to set the property. Because it is designed for client applications.
///
/// On Linux a V2 `set` waits at most 2 s for the service's reply. An
/// `Error::Io` of kind `TimedOut` from that wait does not mean the
/// property was not set: the service applies a fully received request
/// even after the client stopped waiting. A `TimedOut` while connecting
/// or sending means the request was not delivered. On Android `set`
/// waits for init's reply without a timeout, like bionic; connecting and
/// sending keep the 2 s bound there too, where bionic waits without one.
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::set;
///
/// // Setting various types (all converted to strings)
/// set("test.int.property", 42).unwrap();           // Stored as "42"
/// set("test.bool.property", true).unwrap();        // Stored as "true"
/// set("test.float.property", 3.14).unwrap();       // Stored as "3.14"
/// set("test.string.property", "hello").unwrap();   // Stored as "hello"
///
/// // A reference to a `Display` value is itself `Display`, so a borrowed
/// // value needs no copy.
/// let owned = String::from("world");
/// set("test.borrowed.property", &owned).unwrap();
///
/// // For well-known system properties, prefer string literals so the
/// // stored form matches what other Android components expect ("1", not
/// // "true"). Note `ro.*` properties are write-once: the property service
/// // rejects setting one that already has a value.
/// set("persist.sys.timezone", "Asia/Seoul").unwrap();
/// ```
///
/// # Compatibility Notes
/// - Android system properties typically use "0"/"1" for boolean values, not "true"/"false"
/// - Numeric properties may have specific formatting requirements
/// - Always test compatibility when setting properties that will be read by other applications
pub fn set(name: &str, value: impl std::fmt::Display) -> Result<()> {
    system_property_set::set(name, &value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "android")]
    use android_system_properties::AndroidSystemProperties;
    // Used only by the builder-only host tests below; cfg-scope them
    // instead of blanket-allowing unused_imports for every configuration.
    #[cfg(all(feature = "builder", not(target_os = "android")))]
    use std::collections::HashMap;
    #[cfg(all(feature = "builder", not(target_os = "android")))]
    use std::fs::remove_dir_all;
    #[cfg(all(feature = "builder", not(target_os = "android")))]
    use std::path::Path;
    #[cfg(all(feature = "builder", not(target_os = "android")))]
    use std::sync::{Mutex, MutexGuard};

    /// The store of `system_properties_area` is
    /// `temp_dir()/<this prefix><pid>`. The pid keeps two processes running
    /// these tests at once (two `cargo test` runs in one checkout, or a
    /// runner that starts one process per test) off each other's store: a
    /// path shared between them made the second `AreaWriterLock::acquire`
    /// fail with `Error::Lock`.
    #[cfg(all(feature = "builder", not(target_os = "android")))]
    const TEST_STORE_PREFIX: &str = "rsprops_lib_store_";

    #[cfg(any(feature = "builder", target_os = "android"))]
    fn enable_logger() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[cfg(target_os = "android")]
    #[test]
    fn test_get() {
        const PROPERTIES: [&str; 40] = [
            "ro.build.version.sdk",
            "ro.build.version.release",
            "ro.product.model",
            "ro.product.manufacturer",
            "ro.product.name",
            "ro.serialno",
            "ro.bootloader",
            "ro.hardware",
            "ro.revision",
            "ro.kernel.qemu",
            "dalvik.vm.heapsize",
            "dalvik.vm.heapgrowthlimit",
            "dalvik.vm.heapstartsize",
            "dalvik.vm.heaptargetutilization",
            "dalvik.vm.heapminfree",
            "dalvik.vm.heapmaxfree",
            "net.bt.name",
            "net.change",
            "net.dns1",
            "net.dns2",
            "net.hostname",
            "net.tcp.default_init_rwnd",
            "persist.sys.timezone",
            "persist.sys.locale",
            "persist.sys.dalvik.vm.lib.2",
            "persist.sys.profiler_ms",
            "persist.sys.usb.config",
            "persist.service.acm.enable",
            "ril.ecclist",
            "ril.subscription.types",
            "service.adb.tcp.port",
            "service.bootanim.exit",
            "service.camera.running",
            "service.media.powersnd",
            "sys.boot_completed",
            "sys.usb.config",
            "sys.usb.state",
            "vold.post_fs_data_done",
            "wifi.interface",
            "wifi.supplicant_scan_interval",
        ];

        enable_logger();
        for prop in PROPERTIES.iter() {
            let value1: String = get_or(prop, "".to_owned());
            let value2 = AndroidSystemProperties::new().get(prop).unwrap_or_default();

            println!("{}: [{}], [{}]", prop, value1, value2);
            assert_eq!(value1, value2);
        }
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    fn load_properties(tag: &str) -> HashMap<String, String> {
        let build_prop_files = [
            "product_build.prop",
            "system_build.prop",
            "system_dlkm_build.prop",
            "system_ext_build.prop",
            "vendor_build.prop",
            "vendor_dlkm_build.prop",
            "vendor_odm_build.prop",
            "vendor_odm_dlkm_build.prop",
        ];

        // Outside the properties dir: creating `<dir>/android/` would
        // create `<dir>` ahead of `AreaWriterLock::acquire`, which sets
        // mode 0711 only on a dir it creates.
        let fixtures = crate::test_support::FixtureDir::new(tag);
        let mut properties = HashMap::new();
        for name in build_prop_files {
            let file = crate::test_support::copy_android_fixture(name, fixtures.path());
            load_properties_from_file(&file, None, &mut properties).unwrap();
        }
        // A group/other-writable prop file (git checkout under umask 002)
        // is skipped with `Ok`, which would leave callers looping over
        // nothing.
        assert!(
            properties.contains_key("ro.build.version.sdk"),
            "tests/android/*.prop not loaded (group/other-writable?)"
        );

        properties
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    fn system_properties_area() -> MutexGuard<'static, Option<SystemProperties>> {
        static SYSTEM_PROPERTIES: Mutex<Option<SystemProperties>> = Mutex::new(None);
        let mut system_properties_guard = SYSTEM_PROPERTIES.lock().unwrap();

        if system_properties_guard.is_none() {
            // The `static` above owns the writer until the process exits,
            // so nothing removes the store in this run; the next run does.
            crate::test_support::sweep_dead_dirs(TEST_STORE_PREFIX);
            let dir =
                std::env::temp_dir().join(format!("{TEST_STORE_PREFIX}{}", std::process::id()));
            *system_properties_guard = Some(build_property_dir(&dir));
        }
        system_properties_guard
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    fn build_property_dir(dir: &Path) -> SystemProperties {
        // `init` fails if another test in this binary latched a *different*
        // directory first; the `properties_dir()` cleanup below would then
        // delete that directory instead of ours.
        crate::init(PropertyConfig::new().properties_dir(dir))
            .expect("another test latched a different properties dir before this one");

        let property_contexts_files = vec![
            "tests/android/plat_property_contexts",
            "tests/android/system_ext_property_contexts",
            "tests/android/vendor_property_contexts",
        ];

        let mut property_infos = Vec::new();
        for file in property_contexts_files {
            let (mut property_info, errors) =
                PropertyInfoEntry::parse_from_file(Path::new(file), MatchKeyword::Unchecked)
                    .unwrap();
            if !errors.is_empty() {
                log::error!("{errors:?}");
            }
            property_infos.append(&mut property_info);
        }

        let data: Vec<u8> =
            build_trie(&property_infos, "u:object_r:build_prop:s0", "string").unwrap();

        let dir = properties_dir();
        // Missing dir is fine on first run; any other failure (permissions)
        // surfaces at `acquire` below, which recreates the dir.
        let _ = remove_dir_all(dir);

        let properties = load_properties("rsprops_lib_build");

        // The documented writer sequence: lock, publish, create the areas.
        let lock = AreaWriterLock::acquire(dir).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            // `acquire` created the dir (0711 minus umask); nothing above
            // created it first with a group/other-readable mode.
            let mode = std::fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o066, 0, "{dir:?} mode {:o}", mode & 0o7777);
        }
        lock.publish_property_info(&data).unwrap();
        let mut system_properties = SystemProperties::new_area_locked(lock)
            .unwrap_or_else(|e| panic!("Cannot create system properties in {dir:?}: {e}"));
        for (key, value) in properties.iter() {
            match system_properties.find(key.as_str()).unwrap() {
                Some(prop_ref) => {
                    system_properties.update(prop_ref, value.as_str()).unwrap();
                }
                None => {
                    system_properties.add(key.as_str(), value.as_str()).unwrap();
                }
            }
        }

        system_properties
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    #[test]
    fn test_property_info() {
        enable_logger();

        let _guard = system_properties_area();

        let system_properties = system_properties().unwrap();

        let properties = load_properties("rsprops_lib_expect");

        for (key, value) in properties.iter() {
            let prop_value = system_properties.get(key.as_str()).unwrap_or_default();
            assert_eq!(prop_value, value.as_str());
        }
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    #[test]
    fn test_wait() {
        enable_logger();

        let mut guard = system_properties_area();

        let system_properties_area = guard.as_mut().unwrap();

        let test_prop = "test.property";

        // Deterministic (no sleeps): sample the serial *before* spawning
        // the waiter and pass it as `old_serial` — the documented contract
        // closes the lost-wakeup window, so it does not matter whether the
        // waiter enters futex_wait before or after the change lands. The
        // old sleep-based sync hung forever (not failed) when the waiter
        // lost the race.
        let reader = system_properties().unwrap();
        let wait_any_from = |old: u32| {
            std::thread::spawn(move || {
                let changed = reader.wait(None, old, None).unwrap();
                assert!(changed.is_some(), "no timeout was given");
            })
        };

        let old = reader.area_serial();
        let handle = wait_any_from(old);

        system_properties_area.add(test_prop, "true").unwrap();
        handle.join().unwrap();

        let index = reader.find(test_prop).unwrap().expect("just added");
        let old_prop_serial = reader.serial(index).expect("index valid");
        let handle = std::thread::spawn(move || {
            let changed = reader.wait(Some(index), old_prop_serial, None).unwrap();
            assert!(changed.is_some(), "no timeout was given");
        });

        let handle_any = wait_any_from(reader.area_serial());

        let index = system_properties_area.find(test_prop).unwrap();
        system_properties_area
            .update(index.unwrap(), "false")
            .unwrap();

        handle.join().unwrap();
        handle_any.join().unwrap();
    }

    #[test]
    fn test_bionic_align_normal() {
        // Test normal alignment
        assert_eq!(bionic_align(0, 4), 0);
        assert_eq!(bionic_align(1, 4), 4);
        assert_eq!(bionic_align(4, 4), 4);
        assert_eq!(bionic_align(5, 4), 8);
        assert_eq!(bionic_align(7, 4), 8);
        assert_eq!(bionic_align(8, 4), 8);

        // Test with 8-byte alignment
        assert_eq!(bionic_align(0, 8), 0);
        assert_eq!(bionic_align(1, 8), 8);
        assert_eq!(bionic_align(8, 8), 8);
        assert_eq!(bionic_align(9, 8), 16);
    }

    #[test]
    #[should_panic(expected = "bionic_align")]
    fn test_bionic_align_overflow_panics() {
        // Align-up cannot represent the result near usize::MAX; returning a
        // value *smaller* than the input (the old saturating behavior)
        // would under-size allocation computations, so overflow must panic.
        bionic_align(usize::MAX - 10, 16);
    }

    #[test]
    fn test_bionic_align_upholds_align_up_contract() {
        // The result is always >= the input and aligned.
        for value in [0usize, 1, 3, 91, 92, 4095] {
            for align in [1usize, 4, 16, 4096] {
                let r = bionic_align(value, align);
                assert!(r >= value);
                assert_eq!(r % align, 0);
            }
        }
    }

    #[test]
    #[should_panic(expected = "Alignment must be a power of 2")]
    fn test_bionic_align_invalid_alignment() {
        // Test that non-power-of-2 alignment panics
        bionic_align(100, 3);
    }

    #[test]
    fn test_bionic_align_edge_cases() {
        // Test with alignment of 1 (trivial case)
        assert_eq!(bionic_align(5, 1), 5);
        assert_eq!(bionic_align(0, 1), 0);

        // Test with larger alignment
        assert_eq!(bionic_align(100, 64), 128);
        assert_eq!(bionic_align(64, 64), 64);
        assert_eq!(bionic_align(65, 64), 128);
    }
}
