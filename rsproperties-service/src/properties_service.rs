use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rsactor::{Actor, ActorRef, ActorWeak};
use rsproperties::{
    build_trie, load_properties_from_file, AreaWriterLock, PropertyInfoEntry, SystemProperties,
};

pub struct PropertiesServiceArgs {
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
    writer_lock: Option<AreaWriterLock>,
}

impl PropertiesServiceArgs {
    /// Public constructor: the type is exposed through `Actor::Args`, so
    /// downstream users spawning the actor directly (instead of via
    /// `run`) need a way to build it.
    ///
    /// Without [`with_writer_lock`](Self::with_writer_lock), the actor's
    /// `on_start` reads the `property_contexts` and build.prop files and
    /// then takes the writer lock of `rsproperties::properties_dir()`
    /// itself, before it writes `property_info` or any area file. A
    /// [`SocketService`] spawned
    /// next to it must not start before that lock is held: spawned earlier,
    /// a second instance renames its sockets over a running instance's
    /// sockets before its properties service fails on the lock. Take the
    /// lock first ([`AreaWriterLock::acquire`]) and pass it here, as
    /// `rsproperties_service::run` does.
    ///
    /// [`SocketService`]: crate::SocketService
    pub fn new(property_contexts_files: Vec<PathBuf>, build_prop_files: Vec<PathBuf>) -> Self {
        Self {
            property_contexts_files,
            build_prop_files,
            writer_lock: None,
        }
    }

    /// Hands the actor a writer lock the caller already holds. The lock
    /// must be for `rsproperties::properties_dir()` (compared as paths with
    /// [`Path::eq`]); otherwise `on_start` fails with
    /// `std::io::ErrorKind::InvalidInput` before it writes anything. The
    /// service keeps the lock until it is dropped.
    pub fn with_writer_lock(mut self, lock: AreaWriterLock) -> Self {
        self.writer_lock = Some(lock);
        self
    }
}

pub struct PropertiesService {
    system_properties: SystemProperties,
}

/// Wrap any error implementing the standard `Error` trait into an
/// `io::Error` preserving the source chain. The previous `e.to_string()`
/// flattening lost `Error::source()` and made anyhow/backtrace useless.
fn io_other<E>(e: E) -> std::io::Error
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    std::io::Error::other(e)
}

/// Synchronous initialisation: parses property_contexts files, builds the
/// trie, loads build.prop files, takes the writer lock (unless `lock` is
/// given), publishes the trie as `property_info`, and applies the build.prop
/// entries to freshly created areas.
///
/// Every input file is read before `property_info` or any area file in
/// `dir` is written, and the writer lock is held before `property_info` is
/// replaced: a failed input
/// file or a second instance (which fails on the lock) leaves the running
/// instance's `property_info` untouched.
///
/// Kept synchronous on purpose — every step is blocking I/O against the
/// filesystem and we don't want to scatter `spawn_blocking` calls through
/// the loop body. Callers invoke this once via `spawn_blocking` so the
/// tokio worker isn't held for the duration of init.
///
/// Build-prop entries are collected into a `BTreeMap` so the order in
/// which they are applied to the area (and thus the physical layout of
/// the trie) is deterministic across runs. Which *file* wins a key
/// conflict is already deterministic — `load_properties_from_file`
/// overwrites in call order — the map only fixes the apply order.
fn init_system_properties_sync(
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
    dir: &Path,
    lock: Option<AreaWriterLock>,
) -> std::io::Result<SystemProperties> {
    let mut property_infos = Vec::new();
    for file in property_contexts_files {
        let (mut property_info, errors) =
            PropertyInfoEntry::parse_from_file(&file, false).map_err(io_other)?;
        if !errors.is_empty() {
            log::error!("{errors:?}");
        }
        property_infos.append(&mut property_info);
    }

    let data: Vec<u8> =
        build_trie(&property_infos, "u:object_r:build_prop:s0", "string").map_err(io_other)?;

    // `load_properties_from_file` only accepts `&mut HashMap` (other
    // callers depend on that signature). Re-collect into a `BTreeMap`
    // before the apply loop so the iteration order is fully determined
    // by the keys, not by HashMap's randomised hash seed.
    let mut properties_unordered: HashMap<String, String> = HashMap::new();
    for file in build_prop_files {
        load_properties_from_file(&file, None, "u:r:init:s0", &mut properties_unordered)
            .map_err(io_other)?;
    }
    let properties: BTreeMap<String, String> = properties_unordered.into_iter().collect();

    // First write to `dir` (unless the caller already took the lock, which
    // created `.writer_lock`): everything above only read files. The lock
    // comes before `publish_property_info`, so a losing second instance
    // fails here without replacing the running instance's `property_info`.
    let lock = match lock {
        Some(lock) if lock.dir() == dir => lock,
        Some(lock) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "writer lock is for {:?}, properties dir is {dir:?}",
                    lock.dir()
                ),
            ))
        }
        None => AreaWriterLock::acquire(dir).map_err(io_other)?,
    };
    lock.publish_property_info(&data).map_err(io_other)?;
    let mut system_properties = SystemProperties::new_area_locked(lock).map_err(io_other)?;
    // `new_area_locked` starts from a freshly-created, empty area and the
    // BTreeMap keys are unique, so every key is new — `add` alone covers
    // the loop. (The previous `find → update` branch was unreachable; had
    // it ever been reached, `update` would have rejected the `ro.` keys
    // that dominate build.prop files and killed the whole init.)
    //
    // A rejected entry (over-long non-`ro.` value, NUL, full area) is
    // logged and skipped, like AOSP init's .prop apply loop — one bad
    // vendor line must not keep the whole service from starting. The value
    // is not logged (value-masking policy, see `PropertyMessage`'s Debug).
    for (key, value) in properties.iter() {
        if let Err(e) = system_properties.add(key.as_str(), value.as_str()) {
            log::error!("Skipping build.prop entry {key:?}: {e}");
        }
    }
    Ok(system_properties)
}

impl Actor for PropertiesService {
    type Args = PropertiesServiceArgs;
    type Error = std::io::Error;
    // This actor does no periodic / event-driven idle work, so the idle event
    // type is unit and `on_idle` is left at its default no-op. (0.16 requires
    // the associated type even when unused; manual impls must spell it out.)
    type IdleEvent = ();

    async fn on_start(
        args: Self::Args,
        _actor_ref: &rsactor::ActorRef<Self>,
    ) -> std::result::Result<Self, Self::Error> {
        let dir = rsproperties::properties_dir().to_path_buf();
        // Filesystem + mmap + trie build all block. Run them on a blocking
        // task so the tokio worker that polls this actor is free to drive
        // other tasks (notably the sibling SocketService) while
        // initialisation runs.
        let system_properties = tokio::task::spawn_blocking(move || {
            init_system_properties_sync(
                args.property_contexts_files,
                args.build_prop_files,
                &dir,
                args.writer_lock,
            )
        })
        .await
        .map_err(|e| std::io::Error::other(format!("init join failed: {e}")))??;

        Ok(PropertiesService { system_properties })
    }

    async fn on_stop(
        &mut self,
        _actor_weak: &ActorWeak<Self>,
        killed: bool,
    ) -> std::result::Result<(), Self::Error> {
        // Routine shutdown logs at `info!`; only a kill is `warn!`-worthy.
        if killed {
            log::warn!("PropertiesService killed — cleaning up resources");
        } else {
            log::info!("PropertiesService stopping gracefully");
        }
        Ok(())
    }
}

impl rsactor::Message<crate::ReadyMessage> for PropertiesService {
    type Reply = ();

    async fn handle(
        &mut self,
        _message: crate::ReadyMessage,
        _actor_ref: &ActorRef<Self>,
    ) -> Self::Reply {
    }
}

use rsproperties::wire::{validate_property_name, validate_value_len};

impl rsactor::Message<crate::PropertyMessage> for PropertiesService {
    type Reply = bool;

    async fn handle(
        &mut self,
        message: crate::PropertyMessage,
        _actor_ref: &ActorRef<Self>,
    ) -> Self::Reply {
        log::debug!("Handling property message: {message:?}");
        let name = message.name;
        let value = message.value;

        // Single source-of-truth for name + length policy — client and
        // server use the same `rsproperties::wire` functions so policy
        // drift (e.g. `>` vs `>=`) cannot reappear.
        if let Err(e) = validate_property_name(&name) {
            log::error!("Rejected setprop: {e}");
            return false;
        }
        if let Err(e) = validate_value_len(&name, &value) {
            log::error!("Rejected setprop: {e}");
            return false;
        }

        // Delegate to `set`, which already encapsulates the find →
        // update-or-add sequence (plus the `ro.` rejection) — duplicating
        // that logic here invited policy drift between the two copies.
        match self.system_properties.set(&name, &value) {
            Ok(()) => {
                // Mask the value (same policy as `PropertyMessage`'s Debug
                // impl and the socket layer): values may carry sensitive
                // payloads, and logging them here would defeat the masking
                // everywhere upstream.
                log::info!("Set property: {name:?} (<{} bytes>)", value.len());
                true
            }
            Err(e) => {
                log::error!("Failed to set property {name:?}: {e}");
                false
            }
        }
    }
}

/// Spawns the properties service actor with `args`. See
/// [`PropertiesServiceArgs::new`] for when the writer lock is taken.
pub fn spawn(args: PropertiesServiceArgs) -> crate::ServiceContext<PropertiesService> {
    let (actor_ref, join_handle) = rsactor::spawn(args);
    crate::ServiceContext {
        actor_ref,
        join_handle,
    }
}

/// `spawn(PropertiesServiceArgs::new(property_contexts_files,
/// build_prop_files))`: the actor takes the writer lock itself (see
/// [`PropertiesServiceArgs::new`]).
pub fn run(
    property_contexts_files: Vec<PathBuf>,
    build_prop_files: Vec<PathBuf>,
) -> crate::ServiceContext<PropertiesService> {
    spawn(PropertiesServiceArgs::new(
        property_contexts_files,
        build_prop_files,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// Mode of the published `property_info`, matching AOSP init.
    const PROPERTY_INFO_MODE: u32 = 0o444;

    /// Per-test directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("{label}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A reader that already holds the previous `property_info` must keep
    /// seeing its bytes: the new trie is published on a new inode.
    #[test]
    fn property_info_republish_keeps_old_inode_intact() {
        let tmp = TempDir::new("rsprops_svc_info_inode");
        let info_path = tmp.0.join("property_info");
        let old_bytes = b"previous trie bytes".to_vec();
        std::fs::write(&info_path, &old_bytes).unwrap();
        // 0644, not 0444: the pre-fix in-place rewrite must be able to
        // open the file as a non-root user, so the test reaches the inode
        // assertion instead of failing on EACCES.
        std::fs::set_permissions(&info_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut old_reader = std::fs::File::open(&info_path).unwrap();
        let old_ino = old_reader.metadata().unwrap().ino();

        drop(init_system_properties_sync(vec![], vec![], &tmp.0, None).unwrap());

        let new_meta = std::fs::metadata(&info_path).unwrap();
        assert_ne!(new_meta.ino(), old_ino);
        assert_eq!(new_meta.mode() & 0o777, PROPERTY_INFO_MODE);
        let mut seen = Vec::new();
        old_reader.rewind().unwrap();
        old_reader.read_to_end(&mut seen).unwrap();
        assert_eq!(seen, old_bytes);
    }

    /// A leftover group-writable `property_info` (e.g. from a run under
    /// umask 0002) must not make the service reject its own file.
    #[test]
    fn property_info_mode_does_not_depend_on_previous_file() {
        let tmp = TempDir::new("rsprops_svc_info_mode");
        let info_path = tmp.0.join("property_info");
        std::fs::write(&info_path, b"stale").unwrap();
        std::fs::set_permissions(&info_path, std::fs::Permissions::from_mode(0o664)).unwrap();

        drop(init_system_properties_sync(vec![], vec![], &tmp.0, None).unwrap());

        let mode = std::fs::metadata(&info_path).unwrap().mode() & 0o777;
        assert_eq!(mode, PROPERTY_INFO_MODE);
    }

    /// A temp left by a crashed run under another pid is removed.
    #[test]
    fn stale_property_info_temp_of_other_pid_is_removed() {
        let tmp = TempDir::new("rsprops_svc_info_stale_tmp");
        let stale = tmp.0.join(".property_info.tmp-4294967295");
        std::fs::write(&stale, b"half-written trie").unwrap();

        drop(init_system_properties_sync(vec![], vec![], &tmp.0, None).unwrap());

        assert!(!stale.exists());
    }

    /// One build.prop entry the area rejects (non-`ro.` value of
    /// `PROP_VALUE_MAX` bytes or more) is skipped, not fatal.
    #[test]
    fn rejected_build_prop_entry_is_skipped() {
        let tmp = TempDir::new("rsprops_svc_bad_entry");
        let build_prop = tmp.0.join("build.prop");
        {
            let mut f = std::fs::File::create(&build_prop).unwrap();
            writeln!(f, "persist.vendor.too_long={}", "x".repeat(100)).unwrap();
            writeln!(f, "ro.ok=1").unwrap();
        }
        // Explicit 0644: under umask 002 `File::create` gives 0664, and a
        // group-writable prop file is skipped like AOSP init does.
        std::fs::set_permissions(&build_prop, std::fs::Permissions::from_mode(0o644)).unwrap();

        let props = init_system_properties_sync(vec![], vec![build_prop], &tmp.0, None).unwrap();
        assert_eq!(props.get_with_result("ro.ok").unwrap(), "1");
        assert!(props.get_with_result("persist.vendor.too_long").is_err());
    }

    /// Writes a one-entry `property_contexts` file mapping `prefix` to
    /// `context`, outside the properties dir.
    fn contexts_file(tmp: &TempDir, name: &str, prefix: &str, context: &str) -> PathBuf {
        let inputs = tmp.0.join("inputs");
        std::fs::create_dir_all(&inputs).unwrap();
        let path = inputs.join(name);
        std::fs::write(&path, format!("{prefix} {context}\n")).unwrap();
        path
    }

    /// Whether `e` wraps `rsproperties::Error::Lock`.
    fn is_lock_error(e: &std::io::Error) -> bool {
        matches!(
            e.get_ref()
                .and_then(|inner| inner.downcast_ref::<rsproperties::Error>()),
            Some(rsproperties::Error::Lock(_))
        )
    }

    /// A second init on a properties dir whose writer lock is held fails
    /// on the lock before it replaces the running instance's
    /// `property_info`.
    #[test]
    fn second_init_fails_before_replacing_property_info() {
        let tmp = TempDir::new("rsprops_svc_second_init");
        let props = tmp.0.join("props");
        let info = props.join("property_info");
        let contexts_a = contexts_file(&tmp, "a", "debug.a.", "u:object_r:debug_a_prop:s0");
        let contexts_b = contexts_file(&tmp, "b", "debug.b.", "u:object_r:debug_b_prop:s0");

        let first = init_system_properties_sync(vec![contexts_a], vec![], &props, None).unwrap();
        let before = std::fs::read(&info).unwrap();

        let err = init_system_properties_sync(vec![contexts_b], vec![], &props, None)
            .err()
            .expect("the writer lock is held by `first`");
        assert!(is_lock_error(&err), "expected Error::Lock, got: {err}");
        assert_eq!(
            std::fs::read(&info).unwrap(),
            before,
            "the losing instance replaced property_info"
        );
        assert!(!props.join("u:object_r:debug_b_prop:s0").exists());
        drop(first);
    }

    /// A build.prop file that cannot be read fails init before
    /// `property_info` is replaced.
    #[test]
    fn build_prop_error_leaves_property_info_untouched() {
        let tmp = TempDir::new("rsprops_svc_bad_build_prop");
        let props = tmp.0.join("props");
        let info = props.join("property_info");
        drop(init_system_properties_sync(vec![], vec![], &props, None).unwrap());
        let before = std::fs::read(&info).unwrap();
        let before_ino = std::fs::metadata(&info).unwrap().ino();

        let contexts = contexts_file(&tmp, "c", "debug.c.", "u:object_r:debug_c_prop:s0");
        let missing = tmp.0.join("inputs").join("missing_build.prop");
        let err = init_system_properties_sync(vec![contexts], vec![missing], &props, None)
            .err()
            .expect("a missing build.prop must fail init");
        assert!(err.to_string().contains("missing_build.prop"), "{err}");
        assert_eq!(std::fs::metadata(&info).unwrap().ino(), before_ino);
        assert_eq!(std::fs::read(&info).unwrap(), before);
    }

    /// A writer lock for another directory is refused before anything is
    /// written to either directory.
    #[test]
    fn writer_lock_for_another_dir_is_rejected() {
        let tmp = TempDir::new("rsprops_svc_lock_mismatch");
        let props = tmp.0.join("props");
        let other = tmp.0.join("other");
        std::fs::create_dir_all(&props).unwrap();
        let lock = AreaWriterLock::acquire(&other).unwrap();

        let err = init_system_properties_sync(vec![], vec![], &props, Some(lock))
            .err()
            .expect("a lock for another dir must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
        assert!(!props.join("property_info").exists());
        assert!(!other.join("property_info").exists());
    }
}
