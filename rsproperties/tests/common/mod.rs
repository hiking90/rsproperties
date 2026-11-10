//! Shared setup for the integration tests that read the property store.
//!
//! On a host each test binary builds its own store under
//! `temp_dir()/rsprops_it_<test>_<pid>` from `tests/android` and loads it
//! through the public API. A binary must not depend on a store another
//! binary left behind: run alone on a clean tree, a missing store used to
//! latch an `Err`, and every "default returned" / `is_err()` assertion
//! passed without reading anything. On Android the real
//! `/dev/__properties__` is used and `init_test` does nothing.
//!
//! Building the store needs the `builder` feature, so on a host without it
//! `init_test` (and every test that calls it) is compiled out.
//!
//! Every item is `allow(dead_code)`: each test binary that declares
//! `mod common;` uses a different subset.

use std::fs::{File, OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A test directory, `<base>/<tag>_<pid>`, removed when the guard drops —
/// also when the test panics, because the guard exists before anything is
/// created in it.
///
/// The tests of one binary run as threads of one process and share the
/// pid, and `new` removes whatever is at the path, so `tag` must differ
/// between the tests of a binary.
#[allow(dead_code)]
pub struct TempDir(PathBuf);

#[allow(dead_code)]
impl TempDir {
    /// `temp_dir()/<tag>_<pid>`. Removes a leftover there; does not create
    /// the directory.
    pub fn new(tag: &str) -> Self {
        Self::under(&std::env::temp_dir(), tag)
    }

    fn under(base: &Path, tag: &str) -> Self {
        let path = base.join(format!("{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `sockaddr_un.sun_path` is 108 bytes and std's `UnixListener::bind` /
/// `UnixStream::connect` need a NUL in it, so a socket path has at most
/// 107 bytes.
const SUN_PATH_LEN: usize = 108;

/// A [`TempDir`] for Unix sockets whose paths are the directory plus
/// `suffix_len` bytes (for example `"/property_service".len()`).
///
/// Uses `temp_dir()` when the socket paths fit in `sun_path`. A long
/// `TMPDIR` falls back to `/tmp`, chosen only if the paths fit there and
/// `create_dir` succeeds; otherwise the test fails and says why. The
/// directory may already exist when this returns (the fallback is probed
/// by creating it).
#[allow(dead_code)]
pub fn socket_test_dir(tag: &str, suffix_len: usize) -> TempDir {
    let fits = |dir: &TempDir| dir.path().as_os_str().len() + suffix_len < SUN_PATH_LEN;

    let preferred = TempDir::new(tag);
    if fits(&preferred) {
        return preferred;
    }
    let fallback = TempDir::under(Path::new("/tmp"), tag);
    let why = if !fits(&fallback) {
        "its socket paths do not fit either".to_string()
    } else {
        match std::fs::create_dir(fallback.path()) {
            Ok(()) => return fallback,
            Err(e) => format!("create_dir failed: {e}"),
        }
    };
    panic!(
        "socket dir {:?} plus {suffix_len} bytes of socket name is not shorter than \
         {SUN_PATH_LEN} bytes (TMPDIR too long), and the fallback {:?} is unusable: {why}",
        preferred.path(),
        fallback.path(),
    );
}

/// Copies the fixture `src` into `into` (created if missing) with mode
/// 0644 and returns the copy's path.
///
/// The `.prop` loader skips a group- or world-writable file like init
/// does, and git checks files out as `0666 & !umask` (0664 under umask
/// 002). An existing copy is overwritten, and the mode is set on the open
/// fd because `mode()` applies only to a newly created file, minus the
/// umask.
#[allow(dead_code)]
pub fn copy_fixture_0644(src: &Path, into: &Path) -> PathBuf {
    std::fs::create_dir_all(into).unwrap_or_else(|e| panic!("create {into:?}: {e}"));
    let dst = into.join(src.file_name().expect("fixture path names a file"));
    let mut from = File::open(src).unwrap_or_else(|e| panic!("open {src:?}: {e}"));
    let mut to = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&dst)
        .unwrap_or_else(|e| panic!("create {dst:?}: {e}"));
    to.set_permissions(Permissions::from_mode(0o644))
        .unwrap_or_else(|e| panic!("chmod {dst:?}: {e}"));
    std::io::copy(&mut from, &mut to).unwrap_or_else(|e| panic!("copy {src:?}: {e}"));
    dst
}

/// Points the global reader at this binary's store and asserts that the
/// store loads.
#[cfg(any(feature = "builder", target_os = "android"))]
#[allow(dead_code)]
pub fn init_test() {
    #[cfg(not(target_os = "android"))]
    host::init();
}

#[cfg(all(feature = "builder", not(target_os = "android")))]
mod host {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Once;

    use rsproperties::{
        build_trie, load_properties_from_file, AreaWriterLock, PropertyConfig, PropertyInfoEntry,
        SystemProperties,
    };

    pub(super) fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let prefix = fixture_prefix();
            let dir = std::env::temp_dir().join(format!("{prefix}{}", std::process::id()));
            sweep_dead_fixtures(&prefix, &dir);
            build_store(&dir);
            rsproperties::init(
                PropertyConfig::new()
                    .properties_dir(&dir)
                    .socket_dir(dir.join("sockets")),
            )
            .expect("init_test: init");
            rsproperties::try_system_properties().expect("init_test: the store must load");
        });
    }

    /// `rsprops_it_<test>_`, where `<test>` is the executable stem without
    /// cargo's `-<hash>` suffix, so a run also sweeps dead fixtures of the
    /// same test built with other features.
    fn fixture_prefix() -> String {
        let exe = std::env::current_exe().expect("current_exe");
        let stem = exe
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("test executable name is UTF-8");
        let test = stem.rsplit_once('-').map_or(stem, |(test, _hash)| test);
        format!("rsprops_it_{test}_")
    }

    /// Removes a leftover at our own path and every fixture of this test
    /// whose process is gone (`kill(pid, 0)` fails with `ESRCH`). The
    /// fixture of a live process — another `cargo test` running the same
    /// test — is kept. A reused pid keeps a dead fixture: that leaks one
    /// directory but never deletes a live one.
    fn sweep_dead_fixtures(prefix: &str, own: &Path) {
        let _ = std::fs::remove_dir_all(own);

        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let pid = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix(prefix))
                // The store (`<prefix><pid>`) and its fixture-copy sibling
                // (`<prefix>fixtures_<pid>`, `build_store`), which a
                // SIGINT/SIGKILL during `build_store` leaves behind.
                .map(|p| p.strip_prefix("fixtures_").unwrap_or(p))
                .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|p| p.parse::<i32>().ok())
                .and_then(rustix::process::Pid::from_raw);
            let Some(pid) = pid else { continue };
            if rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// The documented writer sequence (lock, publish `property_info`,
    /// create the areas) with the same inputs as the lib unit test fixture.
    /// The writer is dropped at the end: the files stay, the lock is
    /// released.
    fn build_store(dir: &Path) {
        let android: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/android");

        let mut entries = Vec::new();
        for file in [
            "plat_property_contexts",
            "system_ext_property_contexts",
            "vendor_property_contexts",
        ] {
            let (mut parsed, errors) =
                PropertyInfoEntry::parse_from_file(&android.join(file), false)
                    .unwrap_or_else(|e| panic!("parse {file}: {e}"));
            assert!(errors.is_empty(), "{file}: {errors:?}");
            entries.append(&mut parsed);
        }
        let trie = build_trie(&entries, "u:object_r:build_prop:s0", "string").expect("build_trie");

        // Load 0644 copies (see `copy_fixture_0644`). They go to a sibling
        // of the store, not into it: creating a subdir first would create
        // the store dir with `0777 & !umask` instead of the 0711 that
        // `AreaWriterLock::acquire` gives it. The guard removes the
        // sibling (`<prefix>fixtures_<pid>`); if the process is killed
        // first, `sweep_dead_fixtures` of a later run removes it.
        let copies = super::TempDir::new(&format!("{}fixtures", fixture_prefix()));
        let mut properties = HashMap::new();
        for file in [
            "product_build.prop",
            "system_build.prop",
            "system_dlkm_build.prop",
            "system_ext_build.prop",
            "vendor_build.prop",
            "vendor_dlkm_build.prop",
            "vendor_odm_build.prop",
            "vendor_odm_dlkm_build.prop",
        ] {
            let copy = super::copy_fixture_0644(&android.join(file), copies.path());
            load_properties_from_file(&copy, None, "u:r:init:s0", &mut properties)
                .unwrap_or_else(|e| panic!("load {file}: {e}"));
        }
        // The loader skips a group- or world-writable file with only a
        // warning; fail here instead of in each value assertion.
        assert!(
            properties.contains_key("ro.build.version.sdk"),
            "tests/android/*.prop were not loaded (group- or world-writable?)"
        );

        let lock = AreaWriterLock::acquire(dir).expect("AreaWriterLock::acquire");
        lock.publish_property_info(&trie)
            .expect("publish_property_info");
        let mut writer = SystemProperties::new_area_locked(lock).expect("new_area_locked");
        for (name, value) in &properties {
            writer
                .add(name, value)
                .unwrap_or_else(|e| panic!("add {name}: {e}"));
        }
    }
}
