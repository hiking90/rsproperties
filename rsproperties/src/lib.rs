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

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Configuration for initializing the property system
///
/// `#[non_exhaustive]`: construct via [`PropertyConfig::builder`],
/// [`PropertyConfig::with_both_dirs`], or `From` — never a struct literal,
/// which outside this crate is E0639 even with `..Default::default()` —
/// so adding a field is not semver-breaking (which is the reason the
/// builder exists at all).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PropertyConfig {
    /// Directory for reading system properties (default: "/dev/__properties__")
    pub properties_dir: Option<PathBuf>,
    /// Directory for property service sockets (default: "/dev/socket")
    pub socket_dir: Option<PathBuf>,
}

// Implement From traits for backward compatibility and convenience
impl From<PathBuf> for PropertyConfig {
    fn from(path: PathBuf) -> Self {
        Self {
            properties_dir: Some(path),
            socket_dir: None,
        }
    }
}

impl From<String> for PropertyConfig {
    fn from(path: String) -> Self {
        Self {
            properties_dir: Some(PathBuf::from(path)),
            socket_dir: None,
        }
    }
}

impl From<&str> for PropertyConfig {
    fn from(path: &str) -> Self {
        Self {
            properties_dir: Some(PathBuf::from(path)),
            socket_dir: None,
        }
    }
}

impl PropertyConfig {
    /// Create config with only properties directory
    pub fn with_properties_dir<P: Into<PathBuf>>(dir: P) -> Self {
        Self {
            properties_dir: Some(dir.into()),
            socket_dir: None,
        }
    }

    /// Create config with only socket directory
    pub fn with_socket_dir<P: Into<PathBuf>>(dir: P) -> Self {
        Self {
            properties_dir: None,
            socket_dir: Some(dir.into()),
        }
    }

    /// Create config with both directories
    pub fn with_both_dirs<P1: Into<PathBuf>, P2: Into<PathBuf>>(
        properties_dir: P1,
        socket_dir: P2,
    ) -> Self {
        Self {
            properties_dir: Some(properties_dir.into()),
            socket_dir: Some(socket_dir.into()),
        }
    }

    /// Create a new builder for PropertyConfig
    pub fn builder() -> PropertyConfigBuilder {
        PropertyConfigBuilder::default()
    }
}

/// Builder for [`PropertyConfig`]. Collects optional directories; `build()`
/// is infallible — and `try_init` only claims the first-write-wins slots.
/// Paths are actually validated at first *use* (first property access /
/// first socket connection), so a nonexistent directory is not reported
/// until then.
#[derive(Debug, Clone, Default)]
pub struct PropertyConfigBuilder {
    properties_dir: Option<PathBuf>,
    socket_dir: Option<PathBuf>,
}

impl PropertyConfigBuilder {
    /// Set the properties directory
    pub fn properties_dir<P: Into<PathBuf>>(mut self, dir: P) -> Self {
        self.properties_dir = Some(dir.into());
        self
    }

    /// Set the socket directory
    pub fn socket_dir<P: Into<PathBuf>>(mut self, dir: P) -> Self {
        self.socket_dir = Some(dir.into());
        self
    }

    /// Build the PropertyConfig
    pub fn build(self) -> PropertyConfig {
        PropertyConfig {
            properties_dir: self.properties_dir,
            socket_dir: self.socket_dir,
        }
    }
}

pub mod errors;
pub mod parse;
pub mod wire;
pub use errors::{ContextWithLocation, Error, Result};

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
        crate::load_properties_from_file(&copy, None, "u:r:init:s0", &mut properties).unwrap();
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
pub use property_info_serializer::{build_trie, PropertyInfoEntry};
pub use system_properties::SystemProperties;
pub use system_property_set::socket_dir;

/// Timeout type accepted by [`SystemProperties::wait`], re-exported so
/// callers don't need a direct dependency on the exact `rustix` version
/// this crate was built against.
pub use rustix::fs::Timespec;

pub use system_property_set::{
    PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME, PROPERTY_SERVICE_SOCKET_NAME,
};

// Re-export (not a second definition): `wire::PROP_VALUE_MAX` is the single
// source of truth — an independent constant here could drift and desync the
// seqlock read buffer size from the area's reserved slot size.
pub use wire::PROP_VALUE_MAX;
pub const PROP_DIRNAME: &str = "/dev/__properties__";

// System properties directory.
static SYSTEM_PROPERTIES_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Serializes every commit to the first-write-wins directory cells
/// (`SYSTEM_PROPERTIES_DIR` here and `SOCKET_DIR` in `system_property_set`).
/// `try_init` must make its pre-check + set atomic against both concurrent
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
// Global system properties. Stores Result so initialization failure does not
// poison the OnceLock and callers can observe the error. The error side is
// `Arc<Error>` because the cache can only hand out references while callers
// need an owned error — wrapping the shared original in `Error::Init`
// preserves both the variant and the `source()` chain.
static SYSTEM_PROPERTIES: OnceLock<
    std::result::Result<system_properties::SystemProperties, std::sync::Arc<Error>>,
> = OnceLock::new();

/// Initialize system properties with flexible configuration options.
///
/// # Arguments
/// * `config` - A [`PropertyConfig`]; only the directories present in it
///   are applied:
///   - `PropertyConfig::default()` - touch nothing (defaults latch on first use)
///   - `PropertyConfig::from(PathBuf)` - set only the properties directory
///   - a fully populated config - set both directories
///
/// # Examples
///
/// The two examples below are alternatives: call `init` once. A later call
/// naming a directory that is already set is rejected as a whole by
/// [`try_init`], and `init` only logs that rejection.
///
/// ```rust,no_run
/// use rsproperties::{init, PropertyConfig};
/// use std::path::PathBuf;
///
/// // Set only properties directory
/// init(PropertyConfig::from(PathBuf::from("/custom/properties")));
/// ```
///
/// ```rust,no_run
/// use rsproperties::{init, PropertyConfig};
///
/// // Full configuration
/// let config = PropertyConfig::with_both_dirs("/custom/properties", "/custom/socket");
/// init(config);
/// ```
pub fn init(config: PropertyConfig) {
    // The `Result` form (`try_init`) is preferred for new code; this wrapper
    // exists for backward compatibility and logs failures instead of returning
    // them.
    if let Err(e) = try_init(config) {
        log::warn!("init: {e}");
    }
}

/// Initialize system properties, returning an error when an option cannot be
/// applied — typically because it was already set, either explicitly by a
/// previous `init()`/`try_init()` or implicitly: the properties directory by
/// the first property read (`properties_dir()`), the socket directory by the
/// first `set()` or `socket_dir()` call.
///
/// Only the options present in `config` are touched: a socket-only config
/// leaves the properties directory unset (still overridable later), and
/// vice versa.
pub fn try_init(config: PropertyConfig) -> Result<()> {
    // Both `SYSTEM_PROPERTIES_DIR` and the socket-dir cell are first-write-
    // wins. Pre-check everything this call intends to set *before*
    // committing anything, so a failed init never leaves the global state
    // half-applied (e.g. properties_dir locked, socket_dir unset). The lock
    // makes pre-check + set atomic against concurrent `try_init`s and the
    // implicit latches in `properties_dir()` / `socket_dir()`.
    let _guard = lock_global_dirs();
    if config.properties_dir.is_some() && SYSTEM_PROPERTIES_DIR.get().is_some() {
        return Err(Error::AlreadyInitialized(
            "system properties directory \
             (explicitly via init() or implicitly by a prior property read)"
                .into(),
        ));
    }
    if config.socket_dir.is_some() && system_property_set::socket_dir_is_set() {
        return Err(Error::AlreadyInitialized("socket directory".into()));
    }

    if let Some(props_dir) = config.properties_dir {
        log::info!("Setting system properties directory to: {props_dir:?}");
        SYSTEM_PROPERTIES_DIR
            .set(props_dir)
            .map_err(|_| Error::AlreadyInitialized("system properties directory".into()))?;
    }

    if let Some(socket_dir) = config.socket_dir {
        if !system_property_set::set_socket_dir(&socket_dir) {
            // Unreachable while every committer honors `GLOBAL_DIRS_LOCK`
            // (pre-check and set are atomic under the guard above); kept as
            // defense in depth because a `OnceLock` cannot be un-set.
            return Err(Error::AlreadyInitialized(
                "socket directory (race after pre-check)".into(),
            ));
        }
        log::info!("Successfully set socket directory to: {socket_dir:?}");
    }
    Ok(())
}

/// Get the system properties directory.
/// Returns the configured directory if init() was called,
/// otherwise returns the default PROP_DIRNAME (/dev/__properties__).
///
/// Unlike [`socket_dir`] (which honors `PROPERTY_SERVICE_SOCKET_DIR`),
/// there is deliberately no environment-variable override here: the
/// properties directory decides which mmap'd files this process trusts,
/// so it is configured only through code (`init`/`try_init`).
pub fn properties_dir() -> &'static Path {
    // Lock-free once initialized; the first call takes `GLOBAL_DIRS_LOCK` so
    // the default-latch cannot slip between `try_init`'s pre-check and set.
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

/// The cached global instance, or `None` when it has not been initialized
/// yet or initialization failed. Never *triggers* initialization — used by
/// call sites (e.g. the wire-protocol version probe in
/// `system_property_set`) that must not latch the default properties
/// directory as a side effect.
pub(crate) fn system_properties_if_initialized(
) -> Option<&'static system_properties::SystemProperties> {
    SYSTEM_PROPERTIES.get().and_then(|r| r.as_ref().ok())
}

/// Get the system properties, returning an error if initialization fails.
///
/// This is the panic-free variant; `init()` should typically be called first
/// to choose the properties directory. The initialization is cached, so
/// subsequent calls reuse the same result — **including failure**: an error
/// is latched for the process lifetime, so a property store that becomes
/// available later (e.g. `/dev/__properties__` mounted after this process
/// started) is not picked up. Early-boot callers should defer their first
/// property access until the store is ready.
pub fn try_system_properties() -> Result<&'static system_properties::SystemProperties> {
    SYSTEM_PROPERTIES
        .get_or_init(|| {
            let dir = properties_dir();
            log::debug!("Initializing global SystemProperties instance from: {dir:?}");

            system_properties::SystemProperties::new(dir)
                .inspect_err(|e| {
                    log::error!("Failed to initialize SystemProperties from {dir:?}: {e}");
                })
                .map_err(std::sync::Arc::new)
        })
        .as_ref()
        // `Error::Init` shares the cached original, so both the original
        // variant (via `source()` downcast) and the full error chain stay
        // reachable — flattening to a Display string would lose both.
        .map_err(|e| Error::Init(std::sync::Arc::clone(e)))
}

/// Get the system properties.
///
/// Calling this without a prior `init()` does **not** panic by itself: the
/// default directory (`/dev/__properties__`) is latched on first use, and
/// a later `init()`/`try_init()` naming a properties directory will then
/// fail with `AlreadyInitialized`. The panic happens only when the
/// properties directory (configured or default) cannot be opened.
///
/// Prefer [`try_system_properties`] in code that must not panic.
pub fn system_properties() -> &'static system_properties::SystemProperties {
    match try_system_properties() {
        // `Error::Init`'s Display already starts with "SystemProperties
        // initialization failed" — no extra prefix here.
        Ok(props) => props,
        Err(e) => panic!("{e}"),
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

/// Get a property value parsed to specified type
/// Returns Err if property not found, system error, or parse error occurs
///
/// Note: unlike [`get_or`], an *empty* property value is not special-cased
/// here — `get::<String>` returns `Ok("")` for a set-but-empty property,
/// while `get_or` follows the Android convention (empty = unset) and
/// falls back to the default.
///
/// The `FromStr` parse runs while the property area's internal read lock
/// is held (see [`SystemProperties::read_with`]): keep it cheap and do not
/// re-enter this crate's property API from it — every ordinary `FromStr`
/// (ints, floats, `String`) is fine.
///
/// The parse is Rust's `FromStr`, not AOSP libbase's: `" 5"` and `"0x10"`
/// fail for integers, and `bool` accepts only `true`/`false`. To read a
/// value the way `android::base::GetBoolProperty` / `GetIntProperty` do,
/// use [`get_bool_or`] / [`get_int_or`] or the parsers in [`parse`].
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::get;
///
/// let sdk_version: i32 = get("ro.build.version.sdk").unwrap();
/// let version: String = get("ro.build.version.release").unwrap();
///
/// // Boolean properties are written either as "0"/"1" (`ro.debuggable`)
/// // or as "true"/"false" (`ro.apex.updatable`), depending on the
/// // property. `bool: FromStr` accepts only "true"/"false", so read a
/// // "0"/"1" property as an integer:
/// let is_debuggable: bool = get::<i32>("ro.debuggable").unwrap() != 0;
/// let apex_updatable: bool = get("ro.apex.updatable").unwrap_or(false);
///
/// // With fallback
/// let sdk_version: i32 = get("ro.build.version.sdk").unwrap_or(0);
/// let version: String = get("ro.build.version.release").unwrap_or_default();
/// ```
pub fn get<T>(name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    // Route through `read_with` so the parse-and-discard path never
    // allocates a `String` — the value bytes are handed to `FromStr` as
    // `&str` borrowed from the seqlock buffer (short variant) or the mmap
    // (long variant).
    try_system_properties()?.read_with(name, |value| {
        // The raw value is not formatted here (the error's Display reaches
        // logs), same policy as the set path's error log. `T::Err`'s Display
        // is included verbatim: std's numeric/bool/addr parse errors do not
        // echo their input, but a third-party `T` may.
        value.parse().map_err(|e| {
            Error::Parse(format!(
                "Failed to parse property {name:?} (<{} bytes>): {e}",
                value.len()
            ))
        })
    })?
}

/// Get a property value with default fallback
/// Never fails - always returns a valid value; falls back on the same
/// conditions as [`get_or_else`].
///
/// The default is constructed eagerly; when it is expensive to build
/// (e.g. an allocated `String`), prefer [`get_or_else`], which only
/// constructs it on the fallback path.
///
/// The `FromStr` parse runs under the property area's read lock — see the
/// caution on [`get`].
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::get_or;
///
/// let sdk_version: i32 = get_or("ro.build.version.sdk", 0);
/// let version: String = get_or("ro.build.version.release", "unknown".to_owned());
///
/// // Boolean properties are written either as "0"/"1" (`ro.debuggable`)
/// // or as "true"/"false" (`ro.apex.updatable`), depending on the
/// // property. `bool: FromStr` accepts only "true"/"false":
/// let is_debuggable: bool = get_or("ro.debuggable", 0) != 0;
/// let apex_updatable: bool = get_or("ro.apex.updatable", false);
/// ```
pub fn get_or<T>(name: &str, default: T) -> T
where
    T: std::str::FromStr,
{
    get_or_else(name, move || default)
}

/// Like [`get_or`], but the default is produced lazily: the closure runs
/// when the property is missing or empty, when its value fails to parse as
/// `T`, or when the read fails for any other reason — a non-UTF-8 value
/// (`Error::Utf8`), a corrupt property area, or a failed global
/// initialization (see [`try_system_properties`]: the failure is latched).
/// Use [`get`] to tell these cases apart. The found-and-parsed hot path
/// never pays for constructing the default.
///
/// The `FromStr` parse runs under the property area's read lock — see the
/// caution on [`get`]. (The `default` closure runs after the lock is
/// released.)
///
/// # Examples
/// ```rust,no_run
/// use rsproperties::get_or_else;
///
/// let version: String = get_or_else("ro.build.version.release", || "unknown".to_owned());
/// ```
pub fn get_or_else<T, F>(name: &str, default: F) -> T
where
    T: std::str::FromStr,
    F: FnOnce() -> T,
{
    let Ok(props) = try_system_properties() else {
        return default();
    };
    // Two-stage closure: the inner `Result<T, ()>` carries the parsed
    // value back out of `read_with` without ever allocating a `String`.
    // `Err(())` signals "use the default"; the default itself is produced
    // at the match below, so the `FnOnce` callback never needs to own it.
    match props.read_with(name, |value| {
        if value.is_empty() {
            return Err(());
        }
        value.parse::<T>().map_err(|_| ())
    }) {
        Ok(Ok(v)) => v,
        _ => default(),
    }
}

/// Reads a boolean property the way AOSP libbase `GetBoolProperty` does:
/// [`parse::parse_bool`] on the value (`1`/`y`/`yes`/`on`/`true`,
/// `0`/`n`/`no`/`off`/`false`), and `default` for any other value, an
/// empty or missing property, or a failed read — the same fallback
/// conditions as [`get_or_else`].
///
/// ```rust,no_run
/// let debuggable = rsproperties::get_bool_or("ro.debuggable", false);
/// ```
pub fn get_bool_or(name: &str, default: bool) -> bool {
    let Ok(props) = try_system_properties() else {
        return default;
    };
    props
        .read_with(name, parse::parse_bool)
        .ok()
        .flatten()
        .unwrap_or(default)
}

/// Reads an integer property the way AOSP libbase `GetIntProperty`
/// (signed `T`) / `GetUintProperty` (unsigned `T`) do without range
/// arguments: [`parse::parse_int`] on the value (leading whitespace
/// skipped, `0x` for hexadecimal, no trailing characters), and `default`
/// for a value that does not parse or is outside `T`, an empty or missing
/// property, or a failed read.
///
/// ```rust,no_run
/// let sdk: i32 = rsproperties::get_int_or("ro.build.version.sdk", 0);
/// ```
pub fn get_int_or<T: parse::Integer>(name: &str, default: T) -> T {
    let Ok(props) = try_system_properties() else {
        return default;
    };
    props
        .read_with(name, parse::parse_int::<T>)
        .ok()
        .flatten()
        .unwrap_or(default)
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
/// set("test.int.property", &42).unwrap();           // Stored as "42"
/// set("test.bool.property", &true).unwrap();        // Stored as "true"
/// set("test.float.property", &3.14).unwrap();       // Stored as "3.14"
/// set("test.string.property", &"hello").unwrap();   // Stored as "hello"
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
pub fn set<T: std::fmt::Display + ?Sized>(name: &str, value: &T) -> Result<()> {
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

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    const TEST_PROPERTY_DIR: &str = "__properties__";

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
            load_properties_from_file(&file, None, "u:r:init:s0", &mut properties).unwrap();
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
            *system_properties_guard = Some(build_property_dir(TEST_PROPERTY_DIR));
        }
        system_properties_guard
    }

    #[cfg(all(feature = "builder", not(target_os = "android")))]
    fn build_property_dir(dir: &str) -> SystemProperties {
        crate::init(PropertyConfig::from(PathBuf::from(dir)));
        // `init` is first-write-wins and swallows AlreadyInitialized with a
        // warn — if some other test in this binary latched a *different*
        // directory first, the `properties_dir()` cleanup below would
        // delete that directory instead of ours. Fail loudly instead.
        assert_eq!(
            properties_dir(),
            Path::new(dir),
            "another test latched a different properties dir before this one"
        );

        let property_contexts_files = vec![
            "tests/android/plat_property_contexts",
            "tests/android/system_ext_property_contexts",
            "tests/android/vendor_property_contexts",
        ];

        let mut property_infos = Vec::new();
        for file in property_contexts_files {
            let (mut property_info, errors) =
                PropertyInfoEntry::parse_from_file(Path::new(file), false).unwrap();
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
                    system_properties.update(&prop_ref, value.as_str()).unwrap();
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

        let system_properties = system_properties();

        let properties = load_properties("rsprops_lib_expect");

        for (key, value) in properties.iter() {
            let prop_value = system_properties
                .get_with_result(key.as_str())
                .unwrap_or_default();
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
        let wait_any_from = |old: u32| {
            std::thread::spawn(move || {
                let system_properties = system_properties();
                system_properties.wait(None, Some(old), None);
            })
        };

        let old = system_properties().context_serial();
        let handle = wait_any_from(old);

        system_properties_area.add(test_prop, "true").unwrap();
        handle.join().unwrap();

        let index = system_properties()
            .find(test_prop)
            .unwrap()
            .expect("just added");
        let old_prop_serial = system_properties().serial(&index).expect("index valid");
        let handle = std::thread::spawn(move || {
            let system_properties = system_properties();
            system_properties.wait(Some(&index), Some(old_prop_serial), None);
        });

        let handle_any = wait_any_from(system_properties().context_serial());

        let index = system_properties_area.find(test_prop).unwrap();
        system_properties_area
            .update(&index.unwrap(), "false")
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
