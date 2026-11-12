use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rsactor::{Actor, ActorRef, ActorWeak};
use rsproperties::wire::{
    validate_property_name, validate_value_len, PROP_ERROR_HANDLE_CONTROL_MESSAGE,
    PROP_ERROR_INVALID_NAME, PROP_ERROR_INVALID_VALUE, PROP_ERROR_READ_ONLY_PROPERTY,
    PROP_ERROR_SET_FAILED, PROP_SUCCESS,
};
use rsproperties::{
    build_trie, load_properties_from_file, AreaWriterLock, MatchKeyword, PropertyInfoEntry,
    SystemProperties,
};

use crate::persistent_properties::{is_persistent_name, PersistentStore};
use crate::property_type::check_type;
use crate::SetProtocol;

/// What [`spawn`] starts the properties service with.
pub(crate) struct PropertiesServiceArgs {
    pub(crate) property_contexts_files: Vec<PathBuf>,
    pub(crate) build_prop_files: Vec<PathBuf>,
    /// The writer lock of `rsproperties::properties_dir()`, taken by `run`
    /// before either actor is spawned: the socket service must not bind
    /// while another instance holds the lock, or it renames its sockets
    /// over that instance's sockets. A lock for another directory
    /// (compared as paths with [`Path::eq`]) makes `on_start` fail with
    /// `std::io::ErrorKind::InvalidInput` before it writes anything. The
    /// service keeps the lock until it is dropped.
    pub(crate) writer_lock: AreaWriterLock,
    /// Stores `persist.*` / `next_boot.*` properties in this file; see
    /// [`crate::ServiceOptions::persistent_properties_file`].
    pub(crate) persistent_properties_file: Option<PathBuf>,
}

pub(crate) struct PropertiesService {
    system_properties: SystemProperties,
    persistent: Option<PersistentStore>,
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
/// trie, loads build.prop files, publishes the trie as `property_info`
/// under `lock`, and applies the build.prop entries to freshly created
/// areas.
///
/// Every input file is read before `property_info` or any area file in
/// `dir` is written, so a failed input file leaves the previous
/// `property_info` untouched. `lock` must be the writer lock of `dir`.
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
    lock: AreaWriterLock,
) -> std::io::Result<SystemProperties> {
    let mut property_infos = Vec::new();
    for file in property_contexts_files {
        let (mut property_info, errors) =
            PropertyInfoEntry::parse_from_file(&file, MatchKeyword::Unchecked).map_err(io_other)?;
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
        load_properties_from_file(&file, None, &mut properties_unordered).map_err(io_other)?;
    }
    let properties: BTreeMap<String, String> = properties_unordered.into_iter().collect();

    // First write to `dir` since the lock was taken (which created
    // `.writer_lock`): everything above only read files.
    if lock.dir() != dir {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "writer lock is for {:?}, properties dir is {dir:?}",
                lock.dir()
            ),
        ));
    }
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

/// The properties AOSP init sets after the build.prop files, in its order:
/// `ro.property_service.version` (`StartPropertyService`), the persistent
/// properties (`load_persist_props`, each through the same checks as a
/// client set, without writing the file back), then
/// `ro.persistent_properties.ready`. A failed set is logged and skipped,
/// as init's `InitPropertySet` does.
///
/// `store` is loaded by the caller before `property_info` is published
/// ([`PersistentStore::load`]), so a missing directory or a lock held by
/// another service fails startup before the properties dir is changed.
fn apply_startup_properties(
    system_properties: &mut SystemProperties,
    store: Option<PersistentStore>,
) -> Option<PersistentStore> {
    init_set(system_properties, "ro.property_service.version", "2");
    let store = match store {
        Some(mut store) => {
            // Staged values for non-persistent names come last, where init
            // appends the staged records it has not matched.
            let unpersisted = store.take_unpersisted();
            for (name, value) in store.records().iter().chain(&unpersisted) {
                match (std::str::from_utf8(name), std::str::from_utf8(value)) {
                    (Ok(name), Ok(value)) => init_set(system_properties, name, value),
                    // init rejects such a value in `IsLegalPropertyValue`;
                    // the record stays in the file.
                    _ => log::error!(
                        "Skipping persistent property {:?}: not UTF-8",
                        String::from_utf8_lossy(name)
                    ),
                }
            }
            log::info!(
                "Loaded {} persistent properties from {:?}",
                store.records().len(),
                store.path()
            );
            Some(store)
        }
        None => None,
    };
    init_set(system_properties, "ro.persistent_properties.ready", "true");
    store
}

/// init `InitPropertySet`: [`apply_set`], logging a failure.
fn init_set(system_properties: &mut SystemProperties, name: &str, value: &str) {
    let code = apply_set(system_properties, name, value);
    if code != PROP_SUCCESS {
        log::error!("Cannot set {name:?} at startup: error code {code:#x}");
    }
}

/// Checks and stores one set, returning the V2 response code AOSP init
/// sends for it, in init's order (`CheckPermissions`, then `PropertySet`):
///
/// 1. illegal name → `PROP_ERROR_INVALID_NAME`;
/// 2. `ctl.*` → `PROP_ERROR_HANDLE_CONTROL_MESSAGE` (the socket service
///    handles control messages before they reach this actor; they are
///    never stored);
/// 3. value not matching the `property_contexts` type
///    ([`check_type`]) → `PROP_ERROR_INVALID_VALUE`;
/// 4. value too long or containing NUL → `PROP_ERROR_INVALID_VALUE`;
/// 5. `ro.` property that already has a value →
///    `PROP_ERROR_READ_ONLY_PROPERTY`;
/// 6. any other storage failure (e.g. a full area) →
///    `PROP_ERROR_SET_FAILED`.
///
/// No per-property access check: see the access model on the socket
/// service.
fn apply_set(system_properties: &mut SystemProperties, name: &str, value: &str) -> i32 {
    if let Err(e) = validate_property_name(name) {
        log::error!("Rejected setprop: {e}");
        return PROP_ERROR_INVALID_NAME;
    }
    if name.starts_with("ctl.") {
        log::error!("Rejected setprop {name:?}: control messages are not stored");
        return PROP_ERROR_HANDLE_CONTROL_MESSAGE;
    }
    match system_properties.property_type(name) {
        Ok(Some(property_type)) if !check_type(property_type, value) => {
            log::error!("Rejected setprop {name:?}: value does not match type {property_type:?}");
            return PROP_ERROR_INVALID_VALUE;
        }
        Ok(_) => {}
        Err(e) => {
            log::error!("Rejected setprop {name:?}: cannot read its type: {e}");
            return PROP_ERROR_SET_FAILED;
        }
    }
    if let Err(e) = validate_value_len(name, value) {
        log::error!("Rejected setprop: {e}");
        return PROP_ERROR_INVALID_VALUE;
    }
    // `set` encapsulates find → update-or-add, including the `ro.`
    // rejection (`ReadOnlyProperty`). Every other failure, including
    // `PermissionDenied` (a file mode or a read-only mapping), is a
    // storage failure.
    match system_properties.set(name, value) {
        // Mask the value (same policy as `PropertyMessage`'s Debug impl
        // and the socket layer).
        Ok(()) => {
            log::info!("Set property: {name:?} (<{} bytes>)", value.len());
            PROP_SUCCESS
        }
        Err(rsproperties::Error::ReadOnlyProperty(_)) => PROP_ERROR_READ_ONLY_PROPERTY,
        Err(e) => {
            log::error!("Failed to set property {name:?}: {e}");
            PROP_ERROR_SET_FAILED
        }
    }
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
        let dir = rsproperties::properties_dir();
        // Filesystem + mmap + trie build all block. Run them on a blocking
        // task so the tokio worker that polls this actor is free to drive
        // other tasks (notably the sibling SocketService) while
        // initialisation runs.
        let (system_properties, persistent) = tokio::task::spawn_blocking(move || {
            // Before `property_info` and the areas are written: a missing
            // directory or another service's lock must fail startup while
            // the properties dir is still as it was.
            let persistent = args
                .persistent_properties_file
                .as_deref()
                .map(PersistentStore::load)
                .transpose()?;
            let mut system_properties = init_system_properties_sync(
                args.property_contexts_files,
                args.build_prop_files,
                dir,
                args.writer_lock,
            )?;
            let persistent = apply_startup_properties(&mut system_properties, persistent);
            Ok::<_, std::io::Error>((system_properties, persistent))
        })
        .await
        .map_err(io_other)??;

        Ok(PropertiesService {
            system_properties,
            persistent,
        })
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

impl rsactor::Message<crate::PropertyMessage> for PropertiesService {
    /// The V2 response code: `PROP_SUCCESS` or one of init's
    /// `PROP_ERROR_*` codes.
    type Reply = i32;

    async fn handle(
        &mut self,
        message: crate::PropertyMessage,
        _actor_ref: &ActorRef<Self>,
    ) -> Self::Reply {
        log::debug!("Handling property message: {message:?}");
        self.client_set(&message.name, message.value, message.protocol)
            .await
    }
}

impl PropertiesService {
    /// One set from a client: [`apply_set`], then, for a V2 set, the
    /// persistent file.
    async fn client_set(&mut self, name: &str, value: String, protocol: SetProtocol) -> i32 {
        let code = apply_set(&mut self.system_properties, name, &value);
        if code != PROP_SUCCESS || !is_persistent_name(name.as_bytes()) {
            return code;
        }
        let Some(store) = self.persistent.as_mut() else {
            return code;
        };
        // init `PropertySet` writes the file only when the set came with a
        // socket to answer on (`socket && persistent_properties_loaded &&
        // need_persist`), and the V1 command calls it without one
        // (`HandlePropertySetNoSocket`). The store's records are left as
        // they are too: init's next write starts from the file, which does
        // not hold this value (docs/aosp-parity.md S1).
        if protocol == SetProtocol::V1 {
            log::warn!(
                "{name:?} was set with the V1 command (PROP_MSG_SETPROP): the value is in the \
                 property area only and is not written to {:?}, as in init; it is lost when the \
                 service restarts. Set it with PROP_MSG_SETPROP2 to store it",
                store.path()
            );
            return code;
        }
        // A V2 set of a `persist.` / `next_boot.` name is written to the
        // file before the reply. The write runs on the blocking pool;
        // awaiting it keeps this actor's sets, and so the file writes, in
        // order.
        if store.reload_pending() {
            let path = store.shared_path();
            match tokio::task::spawn_blocking(move || {
                crate::persistent_properties::read_file(&path)
            })
            .await
            {
                Ok(result) => store.finish_reload(result),
                Err(e) => log::error!("Persistent property read task failed: {e}"),
            }
        }
        if let Some(bytes) = store.update(name, value) {
            if store.reload_pending() {
                // The file still could not be read: writing now would
                // replace records this run never saw. The set is kept in
                // memory and written with a later one.
                store.set_synced(false);
                return code;
            }
            let path = store.shared_path();
            match tokio::task::spawn_blocking(move || {
                crate::persistent_properties::write_file(&path, &bytes).map_err(|e| (path, e))
            })
            .await
            {
                Ok(Ok(())) => store.set_synced(true),
                // Logged, and the set still succeeds — init does the same.
                Ok(Err((path, e))) => {
                    log::error!("Could not store persistent property {name:?} in {path:?}: {e}");
                    store.set_synced(false);
                }
                Err(e) => {
                    log::error!("Persistent property write task failed: {e}");
                    store.set_synced(false);
                }
            }
        }
        code
    }
}

/// Spawns the properties service actor with `args`.
pub(crate) fn spawn(
    args: PropertiesServiceArgs,
) -> (
    ActorRef<PropertiesService>,
    tokio::task::JoinHandle<rsactor::ActorResult<PropertiesService>>,
) {
    rsactor::spawn(args)
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

    /// [`init_system_properties_sync`] with the writer lock of `dir`, taken
    /// first as `run` does.
    fn init_locked(
        property_contexts_files: Vec<PathBuf>,
        build_prop_files: Vec<PathBuf>,
        dir: &Path,
    ) -> std::io::Result<SystemProperties> {
        let lock = AreaWriterLock::acquire(dir).map_err(io_other)?;
        init_system_properties_sync(property_contexts_files, build_prop_files, dir, lock)
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

        drop(init_locked(vec![], vec![], &tmp.0).unwrap());

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

        drop(init_locked(vec![], vec![], &tmp.0).unwrap());

        let mode = std::fs::metadata(&info_path).unwrap().mode() & 0o777;
        assert_eq!(mode, PROPERTY_INFO_MODE);
    }

    /// A temp left by a crashed run under another pid is removed.
    #[test]
    fn stale_property_info_temp_of_other_pid_is_removed() {
        let tmp = TempDir::new("rsprops_svc_info_stale_tmp");
        let stale = tmp.0.join(".property_info.tmp-4294967295");
        std::fs::write(&stale, b"half-written trie").unwrap();

        drop(init_locked(vec![], vec![], &tmp.0).unwrap());

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

        let props = init_locked(vec![], vec![build_prop], &tmp.0).unwrap();
        assert_eq!(props.get("ro.ok").unwrap(), "1");
        assert!(props.get("persist.vendor.too_long").is_err());
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

    /// The service holds the writer lock it was given for as long as its
    /// `SystemProperties` lives, so a second instance's `acquire` (the
    /// first step of `run`) fails until then.
    #[test]
    fn writer_lock_is_held_while_properties_live() {
        let tmp = TempDir::new("rsprops_svc_second_init");
        let props = tmp.0.join("props");
        let contexts = contexts_file(&tmp, "a", "debug.a.", "u:object_r:debug_a_prop:s0");

        let first = init_locked(vec![contexts], vec![], &props).unwrap();
        assert!(matches!(
            AreaWriterLock::acquire(&props),
            Err(rsproperties::Error::Lock(_))
        ));
        drop(first);
        // At once, with no retry: other tests of this binary start child
        // processes, and a child between its clone and its `exec` holds a
        // copy of the lock's descriptor. `AreaWriterLock`'s `Drop` unlocks
        // instead of relying on the last close.
        if let Err(e) = AreaWriterLock::acquire(&props) {
            panic!("the lock must be released with the properties: {e}");
        }
    }

    /// A build.prop file that cannot be read fails init before
    /// `property_info` is replaced.
    #[test]
    fn build_prop_error_leaves_property_info_untouched() {
        let tmp = TempDir::new("rsprops_svc_bad_build_prop");
        let props = tmp.0.join("props");
        let info = props.join("property_info");
        drop(init_locked(vec![], vec![], &props).unwrap());
        let before = std::fs::read(&info).unwrap();
        let before_ino = std::fs::metadata(&info).unwrap().ino();

        let contexts = contexts_file(&tmp, "c", "debug.c.", "u:object_r:debug_c_prop:s0");
        let missing = tmp.0.join("inputs").join("missing_build.prop");
        let err = init_locked(vec![contexts], vec![missing], &props)
            .expect_err("a missing build.prop must fail init");
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

        let err = init_system_properties_sync(vec![], vec![], &props, lock)
            .expect_err("a lock for another dir must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
        assert!(!props.join("property_info").exists());
        assert!(!other.join("property_info").exists());
    }

    /// `on_start`'s persistent steps: load the store at `path` (if any),
    /// then apply the startup properties.
    fn startup_at(
        sp: &mut SystemProperties,
        path: Option<&Path>,
    ) -> std::io::Result<Option<PersistentStore>> {
        let store = path.map(PersistentStore::load).transpose()?;
        Ok(apply_startup_properties(sp, store))
    }

    /// A properties dir whose `property_contexts` gives some names a type.
    fn typed_properties(tmp: &TempDir) -> SystemProperties {
        let contexts = tmp.0.join("typed_property_contexts");
        std::fs::write(
            &contexts,
            "debug. u:object_r:debug_prop:s0 prefix string\n\
             debug.int u:object_r:debug_prop:s0 exact int\n\
             debug.enum u:object_r:debug_prop:s0 exact enum a b\n\
             persist. u:object_r:persist_prop:s0 prefix string\n\
             persist.int u:object_r:persist_prop:s0 exact int\n\
             next_boot. u:object_r:persist_prop:s0 prefix string\n\
             ro. u:object_r:ro_prop:s0 prefix string\n",
        )
        .unwrap();
        init_locked(vec![contexts], vec![], &tmp.0.join("props")).unwrap()
    }

    /// init's codes, in init's order of checks.
    #[test]
    fn apply_set_returns_init_codes() {
        let tmp = TempDir::new("rsprops_svc_apply_codes");
        let mut sp = typed_properties(&tmp);
        let cases: &[(&str, &str, i32)] = &[
            ("debug.int", "12", PROP_SUCCESS),
            ("debug.int", "x", PROP_ERROR_INVALID_VALUE),
            ("debug.int", "", PROP_SUCCESS),
            ("debug.enum", "b", PROP_SUCCESS),
            ("debug.enum", "c", PROP_ERROR_INVALID_VALUE),
            (".bad", "1", PROP_ERROR_INVALID_NAME),
            ("ctl.start", "svc", PROP_ERROR_HANDLE_CONTROL_MESSAGE),
            ("debug.long", &"x".repeat(100), PROP_ERROR_INVALID_VALUE),
            ("ro.once", "1", PROP_SUCCESS),
            ("ro.once", "2", PROP_ERROR_READ_ONLY_PROPERTY),
        ];
        for &(name, value, code) in cases {
            assert_eq!(apply_set(&mut sp, name, value), code, "{name:?}");
        }
        assert_eq!(sp.get("debug.int").unwrap(), "");
        assert_eq!(sp.get("debug.enum").unwrap(), "b");
        assert_eq!(sp.get("ro.once").unwrap(), "1");
        assert!(
            sp.find("ctl.start").unwrap().is_none(),
            "ctl.* is never stored"
        );
    }

    #[test]
    fn startup_sets_version_and_ready_without_persistence() {
        let tmp = TempDir::new("rsprops_svc_startup_plain");
        let mut sp = typed_properties(&tmp);
        assert!(startup_at(&mut sp, None).unwrap().is_none());
        assert_eq!(sp.get("ro.property_service.version").unwrap(), "2");
        assert_eq!(sp.get("ro.persistent_properties.ready").unwrap(), "true");
    }

    /// init `load_persist_props`: staged values replace their targets, and
    /// each record is checked like a client set.
    #[test]
    fn startup_loads_persistent_properties() {
        use crate::persistent_properties::{decode, encode, write_file};
        let tmp = TempDir::new("rsprops_svc_startup_persist");
        let mut sp = typed_properties(&tmp);
        let path = tmp.0.join("persistent_properties");
        let rec = |n: &str, v: &str| (n.as_bytes().to_vec(), v.as_bytes().to_vec());
        write_file(
            &path,
            &encode(&[
                rec("persist.a", "1"),
                rec("persist.int", "not-int"),
                rec("persist.b", "old"),
                rec("next_boot.persist.b", "new"),
            ]),
        )
        .unwrap();

        let store = startup_at(&mut sp, Some(&path)).unwrap().expect("store");
        assert_eq!(sp.get("persist.a").unwrap(), "1");
        assert_eq!(sp.get("persist.b").unwrap(), "new");
        assert!(
            sp.find("persist.int").unwrap().is_none(),
            "type check applies"
        );
        assert_eq!(sp.get("ro.persistent_properties.ready").unwrap(), "true");
        // The rejected record stays in the file, as in init.
        let on_disk = decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk, store.records());
        assert!(on_disk.contains(&rec("persist.int", "not-int")));
        assert!(!on_disk.iter().any(|(n, _)| n.starts_with(b"next_boot.")));
    }

    /// A staged record whose target is not a persistent name is applied
    /// once and not written back, so the next start still loads the file
    /// instead of deleting it.
    #[test]
    fn staged_non_persistent_target_does_not_delete_file() {
        use crate::persistent_properties::{decode, encode, write_file};
        let tmp = TempDir::new("rsprops_svc_startup_staged_foreign");
        let path = tmp.0.join("persistent_properties");
        let rec = |n: &str, v: &str| (n.as_bytes().to_vec(), v.as_bytes().to_vec());
        write_file(
            &path,
            &encode(&[rec("persist.a", "1"), rec("next_boot.debug.s", "v")]),
        )
        .unwrap();

        let mut sp = typed_properties(&tmp);
        drop(startup_at(&mut sp, Some(&path)).unwrap());
        assert_eq!(sp.get("debug.s").unwrap(), "v");
        assert_eq!(
            decode(&std::fs::read(&path).unwrap()).unwrap(),
            vec![rec("persist.a", "1")]
        );

        let tmp2 = TempDir::new("rsprops_svc_startup_staged_foreign_2");
        let mut sp = typed_properties(&tmp2);
        startup_at(&mut sp, Some(&path)).unwrap();
        assert_eq!(sp.get("persist.a").unwrap(), "1");
        assert!(sp.find("debug.s").unwrap().is_none());
    }

    #[test]
    fn startup_fails_when_persistent_dir_is_missing() {
        let tmp = TempDir::new("rsprops_svc_startup_nodir");
        let mut sp = typed_properties(&tmp);
        let path = tmp.0.join("missing").join("persistent_properties");
        assert!(startup_at(&mut sp, Some(&path)).is_err());
    }

    impl PropertiesService {
        /// A set that arrived as `PROP_MSG_SETPROP2`.
        async fn set_v2(&mut self, name: &str, value: &str) -> i32 {
            self.client_set(name, value.into(), SetProtocol::V2).await
        }
    }

    /// A successful V2 client set of a `persist.` / `next_boot.` name
    /// rewrites the file; a rejected set or another name does not.
    #[tokio::test]
    async fn client_set_writes_persistent_file() {
        use crate::persistent_properties::decode;
        let tmp = TempDir::new("rsprops_svc_client_persist");
        let mut sp = typed_properties(&tmp);
        let path = tmp.0.join("persistent_properties");
        let persistent = startup_at(&mut sp, Some(&path)).unwrap();
        let mut service = PropertiesService {
            system_properties: sp,
            persistent,
        };
        let on_disk = || decode(&std::fs::read(&path).unwrap_or_default()).unwrap();
        let rec = |n: &str, v: &str| (n.as_bytes().to_vec(), v.as_bytes().to_vec());

        assert_eq!(service.set_v2("persist.x", "1").await, PROP_SUCCESS);
        assert_eq!(on_disk(), vec![rec("persist.x", "1")]);
        assert_eq!(service.set_v2("debug.y", "1").await, PROP_SUCCESS);
        assert_eq!(
            service.set_v2("persist.int", "bad").await,
            PROP_ERROR_INVALID_VALUE
        );
        assert_eq!(service.set_v2("persist.x", "2").await, PROP_SUCCESS);
        assert_eq!(
            service.set_v2("next_boot.persist.z", "9").await,
            PROP_SUCCESS
        );
        assert_eq!(
            on_disk(),
            vec![rec("persist.x", "2"), rec("next_boot.persist.z", "9")]
        );

        // The next start applies the staged value.
        drop(service);
        let tmp2 = TempDir::new("rsprops_svc_client_persist_restart");
        let mut sp = typed_properties(&tmp2);
        startup_at(&mut sp, Some(&path)).unwrap();
        assert_eq!(sp.get("persist.x").unwrap(), "2");
        assert_eq!(sp.get("persist.z").unwrap(), "9");
        assert_eq!(
            on_disk(),
            vec![rec("persist.x", "2"), rec("persist.z", "9")]
        );
    }

    /// init `PropertySet` writes the file for a V2 set only (`socket &&`):
    /// a V1 set of a `persist.` / `next_boot.` name changes the area and
    /// neither the file nor the records a later V2 write starts from.
    #[tokio::test]
    async fn v1_set_is_not_written_to_persistent_file() {
        use crate::persistent_properties::decode;
        let tmp = TempDir::new("rsprops_svc_client_persist_v1");
        let mut sp = typed_properties(&tmp);
        let path = tmp.0.join("persistent_properties");
        let persistent = startup_at(&mut sp, Some(&path)).unwrap();
        let mut service = PropertiesService {
            system_properties: sp,
            persistent,
        };
        let on_disk = || decode(&std::fs::read(&path).unwrap_or_default()).unwrap();
        let rec = |n: &str, v: &str| (n.as_bytes().to_vec(), v.as_bytes().to_vec());

        assert_eq!(service.set_v2("persist.x", "1").await, PROP_SUCCESS);
        for (name, value) in [
            ("persist.x", "2"),
            ("persist.v1only", "a"),
            ("next_boot.persist.s", "5"),
        ] {
            assert_eq!(
                service
                    .client_set(name, value.into(), SetProtocol::V1)
                    .await,
                PROP_SUCCESS,
                "{name}"
            );
            assert_eq!(service.system_properties.get(name).unwrap(), value);
        }
        assert_eq!(on_disk(), vec![rec("persist.x", "1")]);
        // A type mismatch is rejected for V1 as for V2.
        assert_eq!(
            service
                .client_set("persist.int", "bad".into(), SetProtocol::V1)
                .await,
            PROP_ERROR_INVALID_VALUE
        );

        // A V2 write of another name does not carry the V1 values along.
        assert_eq!(service.set_v2("persist.y", "1").await, PROP_SUCCESS);
        assert_eq!(
            on_disk(),
            vec![rec("persist.x", "1"), rec("persist.y", "1")]
        );
        // A V2 set of the value the area already has (from V1) is written:
        // the file's record, not the area, decides whether it changed.
        assert_eq!(service.set_v2("persist.x", "2").await, PROP_SUCCESS);
        assert_eq!(
            on_disk(),
            vec![rec("persist.x", "2"), rec("persist.y", "1")]
        );

        // The V1-only values are gone after a restart.
        drop(service);
        let tmp2 = TempDir::new("rsprops_svc_client_persist_v1_restart");
        let mut sp = typed_properties(&tmp2);
        startup_at(&mut sp, Some(&path)).unwrap();
        assert_eq!(sp.get("persist.x").unwrap(), "2");
        assert!(sp.find("persist.v1only").unwrap().is_none());
        assert!(sp.find("persist.s").unwrap().is_none());
        assert!(sp.find("next_boot.persist.s").unwrap().is_none());
    }

    /// After `load` hit a transient read error, the first persistent set
    /// reads the file again and keeps its records instead of replacing the
    /// file with this run's set alone.
    #[tokio::test]
    async fn first_set_after_transient_load_failure_keeps_file_records() {
        use crate::persistent_properties::{decode, encode, write_file};
        let tmp = TempDir::new("rsprops_svc_reload");
        let mut sp = typed_properties(&tmp);
        let path = tmp.0.join("persistent_properties");
        let rec = |n: &str, v: &str| (n.as_bytes().to_vec(), v.as_bytes().to_vec());
        write_file(
            &path,
            &encode(&[rec("persist.a", "1"), rec("persist.b", "2")]),
        )
        .unwrap();
        let mut store = startup_at(&mut sp, Some(&path)).unwrap().unwrap();
        store.assume_transient_load_failure();
        let mut service = PropertiesService {
            system_properties: sp,
            persistent: Some(store),
        };

        assert_eq!(service.set_v2("persist.b", "new").await, PROP_SUCCESS);
        assert_eq!(service.set_v2("persist.c", "3").await, PROP_SUCCESS);
        assert_eq!(
            decode(&std::fs::read(&path).unwrap()).unwrap(),
            vec![
                rec("persist.a", "1"),
                rec("persist.b", "new"),
                rec("persist.c", "3")
            ]
        );
    }
}
