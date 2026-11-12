// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `persist.*` / `next_boot.*` properties stored across service restarts,
//! in AOSP init's `persistent_properties` file format
//! (`init/persistent_properties.{proto,cpp}`), so a file copied from a
//! device's `/data/property/persistent_properties` loads here and back.
//!
//! The format is proto2:
//!
//! ```text
//! message PersistentProperties {
//!     message PersistentPropertyRecord {
//!         optional string name = 1;
//!         optional string value = 2;
//!     }
//!     repeated PersistentPropertyRecord properties = 1;
//! }
//! ```
//!
//! Only this message is needed, so the codec is written here instead of
//! adding a protobuf dependency. The encoder writes what init's
//! `SerializeToString` writes for records whose name and value are both
//! set (init always sets both): field 1 then field 2, each
//! length-delimited.
//!
//! Differences from init:
//! - The legacy layout (one `/data/property/persist.*` file per property,
//!   Android 8 and earlier) is not read.
//! - Records are kept in memory after the load and every write serializes
//!   that copy; init re-reads the file before every write, to keep less
//!   memory in PID 1 (its source comment says so). Keeping the copy means
//!   a change whose write failed is still in the next write; init's
//!   re-read drops it from the file. A file edited by another program
//!   while the service runs is overwritten on the next persistent set —
//!   do not edit it then.
//! - One service owns the file: [`PersistentStore::load`] takes an
//!   exclusive `flock` on `<path>.lock` and holds it for the store's
//!   lifetime, so two services given the same file do not overwrite each
//!   other's records. Dropping the store unlocks it with `LOCK_UN`, so a
//!   child process that has not yet `exec`ed and still holds a copy of
//!   the fd does not keep the file locked. A store dropped in a `fork`
//!   child does not unlock: the lock stays with the process that loaded
//!   it. init has no lock; it is the only writer on a device.
//! - Duplicate names are merged at load, keeping the last value (the one
//!   loading leaves in the property area). init's file never has
//!   duplicates; one from another source would otherwise bring an old
//!   value back on the next start, because an update changes only the
//!   first record of a name.
//! - A staged `next_boot.<name>` whose `<name>` is not itself persistent
//!   is applied once at load and not written back. init writes it, and
//!   its next load then deletes the whole file over that name.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::{error, info, warn};

/// One record: name and value bytes as stored (proto2 `string` fields are
/// not validated as UTF-8 by init's C++ parser either).
pub(crate) type Record = (Vec<u8>, Vec<u8>);

const PERSIST_PREFIX: &[u8] = b"persist.";
const NEXT_BOOT_PREFIX: &[u8] = b"next_boot.";

/// Whether `name` belongs in the file: `persist.` and `next_boot.` names.
/// One rule for three uses that must agree: a successful set of such a
/// name is written (init `PropertySet`), `decode` refuses a file holding
/// any other name (init `ParsePersistentPropertyFile`), and `apply_staged`
/// keeps only staged targets that pass, so a kept record never makes the
/// next load delete the file.
pub(crate) fn is_persistent_name(name: &[u8]) -> bool {
    name.starts_with(PERSIST_PREFIX) || name.starts_with(NEXT_BOOT_PREFIX)
}

/// The loaded file plus the records the service writes back. Names are
/// unique in `records`.
pub(crate) struct PersistentStore {
    /// `Arc` so each blocking read or write task gets the path without a
    /// copy ([`shared_path`](Self::shared_path)).
    path: Arc<Path>,
    records: Vec<Record>,
    /// See [`take_unpersisted`](Self::take_unpersisted).
    unpersisted: Vec<Record>,
    /// The last write of `records` failed, so the file may differ from
    /// them. See [`set_synced`](Self::set_synced).
    unsynced: bool,
    /// `load` could not read the file because of a transient error
    /// ([`is_transient`]), so `records` may lack what the file holds. See
    /// [`finish_reload`](Self::finish_reload).
    reload_pending: bool,
    /// `<path>.lock`, `flock`ed exclusively; unlocked in `Drop`.
    lock: File,
    /// Process that called `load`. `Drop` unlocks only there.
    owner_pid: u32,
}

impl PersistentStore {
    /// init `LoadPersistentProperties`, without the legacy directory:
    ///
    /// 1. Takes the file's lock (`<path>.lock`, see the module doc).
    /// 2. A leftover `<path>.tmp` (a write interrupted before its rename)
    ///    is removed.
    /// 3. A file that cannot be read — missing, unreadable, a symlink, or
    ///    group- or world-writable (init `ReadFile`: `O_NOFOLLOW`, then
    ///    the mode check) — gives an empty set and is left as it is. After
    ///    a transient read error ([`is_transient`]) the file is read again
    ///    before the first write ([`finish_reload`](Self::finish_reload)),
    ///    so that write does not replace records it never saw.
    /// 4. A file that does not parse, or holds a name that starts with
    ///    neither `persist.` nor `next_boot.`, is deleted and gives an empty
    ///    set, so later writes succeed (init does the same).
    /// 5. Records with the same name are merged into the first one's
    ///    position with the last one's value.
    /// 6. Staged `next_boot.<name>` records replace `<name>`'s value (or
    ///    are appended when `<name>` has no record) and are dropped. A
    ///    `<name>` that is neither `persist.` nor `next_boot.` is not
    ///    appended but kept for [`take_unpersisted`](Self::take_unpersisted).
    ///
    /// When step 5 or 6 changed anything, the result is written back.
    ///
    /// Fails when the parent directory of `path` does not exist (every
    /// later write would fail, so the misconfiguration is reported at
    /// startup instead), and when another process holds the lock
    /// (`ErrorKind::WouldBlock`).
    pub(crate) fn load(path: &Path) -> std::io::Result<Self> {
        let parent = parent_dir(path);
        if !parent.is_dir() {
            return Err(std::io::Error::new(
                ErrorKind::NotFound,
                format!("directory of the persistent properties file {path:?} does not exist"),
            ));
        }
        // Before the temp file is removed: a `.tmp` may belong to the
        // service that holds the lock.
        let lock = lock_file(path)?;
        let tmp = tmp_path(path);
        match std::fs::remove_file(&tmp) {
            Ok(()) => {
                info!("Removed {tmp:?}: a previous persistent property write may have failed")
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => warn!("Cannot remove {tmp:?}: {e}"),
        }

        let mut reload_pending = false;
        let records = match read_file(path) {
            Ok(bytes) => match decode(&bytes) {
                Ok(records) => records,
                Err(e) => {
                    error!("Unable to parse persistent property file {path:?}: {e}; deleting it");
                    if let Err(e) = std::fs::remove_file(path) {
                        error!("Cannot delete {path:?}: {e}");
                    }
                    Vec::new()
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                error!("Unable to read persistent property file {path:?}: {e}");
                reload_pending = is_transient(&e);
                Vec::new()
            }
        };

        let mut store = Self {
            path: Arc::from(path),
            records,
            unpersisted: Vec::new(),
            unsynced: false,
            reload_pending,
            lock,
            owner_pid: std::process::id(),
        };
        // Both steps run; `|` does not short-circuit.
        if store.merge_duplicates() | store.apply_staged() {
            if let Err(e) = write_file(&store.path, &encode(&store.records)) {
                error!(
                    "Could not store persistent properties in {:?}: {e}",
                    store.path
                );
                store.unsynced = true;
            }
        }
        Ok(store)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The path for a `spawn_blocking` task, which needs an owned value.
    pub(crate) fn shared_path(&self) -> Arc<Path> {
        Arc::clone(&self.path)
    }

    pub(crate) fn records(&self) -> &[Record] {
        &self.records
    }

    /// Merges records with the same name into the first one's position,
    /// with the last one's value — the value loading leaves in the area.
    /// Returns whether any duplicate existed.
    fn merge_duplicates(&mut self) -> bool {
        let mut merged: Vec<Record> = Vec::with_capacity(self.records.len());
        let mut changed = false;
        for (name, value) in self.records.drain(..) {
            match merged.iter_mut().find(|(n, _)| *n == name) {
                Some(entry) => {
                    entry.1 = value;
                    changed = true;
                }
                None => merged.push((name, value)),
            }
        }
        if changed {
            warn!("Merged duplicate records in {:?}", self.path);
        }
        self.records = merged;
        changed
    }

    /// The `next_boot.` step of init `LoadPersistentProperties`. Returns
    /// whether any staged record existed.
    fn apply_staged(&mut self) -> bool {
        // One pass moves each record out: staged ones lose their prefix,
        // the rest keep their order in `kept`.
        let mut staged: Vec<Record> = Vec::new();
        let mut kept: Vec<Record> = Vec::with_capacity(self.records.len());
        for (mut name, value) in self.records.drain(..) {
            if name.starts_with(NEXT_BOOT_PREFIX) {
                name.drain(..NEXT_BOOT_PREFIX.len());
                // Last staged value wins, like init's map assignment.
                match staged.iter_mut().find(|(n, _)| *n == name) {
                    Some(entry) => entry.1 = value,
                    None => staged.push((name, value)),
                }
            } else {
                kept.push((name, value));
            }
        }
        if staged.is_empty() {
            self.records = kept;
            return false;
        }
        let mut updated = Vec::with_capacity(kept.len() + staged.len());
        for (name, value) in kept {
            match staged.iter().position(|(n, _)| *n == name) {
                Some(i) => {
                    let (_, staged_value) = staged.remove(i);
                    updated.push((name, staged_value));
                }
                None => updated.push((name, value)),
            }
        }
        // init appends the remaining staged records in `unordered_map`
        // order (unspecified); here in the order they appeared. A target
        // that is not a persistent name (`next_boot.foo` → `foo`) is
        // applied at startup but not kept: `decode` rejects such a name,
        // so writing it would delete the whole file on the next load.
        // init writes it and deletes the file on the boot after.
        let (persistent, unpersisted): (Vec<Record>, Vec<Record>) =
            staged.into_iter().partition(|(n, _)| is_persistent_name(n));
        for (name, _) in &unpersisted {
            warn!(
                "Staged property {:?} is applied once and not kept in {:?}: \
                 its name starts with neither 'persist.' nor 'next_boot.'",
                String::from_utf8_lossy(name),
                self.path
            );
        }
        updated.extend(persistent);
        self.records = updated;
        self.unpersisted = unpersisted;
        true
    }

    /// Staged records whose target is not a persistent name: applied at
    /// startup after [`records`](Self::records), never written to the file.
    pub(crate) fn take_unpersisted(&mut self) -> Vec<Record> {
        std::mem::take(&mut self.unpersisted)
    }

    /// init `WritePersistentProperty`, in memory: updates the record named
    /// `name` (names are unique after `load`), or appends one. Returns the
    /// encoded file to write, or `None` when the record already has
    /// `value` and the last write succeeded (init skips the write when the
    /// file already has the value). The memory copy changes even if the
    /// caller's write then fails, so the next write carries this change as
    /// well; the caller reports the result with
    /// [`set_synced`](Self::set_synced).
    pub(crate) fn update(&mut self, name: &str, value: String) -> Option<Vec<u8>> {
        match self.records.iter_mut().find(|(n, _)| n == name.as_bytes()) {
            Some((_, stored)) if stored == value.as_bytes() && !self.unsynced => return None,
            Some((_, stored)) => *stored = value.into_bytes(),
            None => self
                .records
                .push((name.as_bytes().to_vec(), value.into_bytes())),
        }
        Some(encode(&self.records))
    }

    /// Records whether the write of the bytes [`update`](Self::update)
    /// returned succeeded. After a failure, `update` returns the file to
    /// write even for an unchanged value, so a retried set rewrites it.
    pub(crate) fn set_synced(&mut self, synced: bool) {
        self.unsynced = !synced;
    }

    /// Whether the file must be read again ([`read_file`]) and passed to
    /// [`finish_reload`](Self::finish_reload) before the next write.
    pub(crate) fn reload_pending(&self) -> bool {
        self.reload_pending
    }

    /// The state `load` leaves after a transient read error, which a test
    /// cannot provoke.
    #[cfg(test)]
    pub(crate) fn assume_transient_load_failure(&mut self) {
        self.records.clear();
        self.reload_pending = true;
    }

    /// Takes the result of reading the file again after `load` hit a
    /// transient error. init reads the file before every write
    /// (`WritePersistentProperty`), so the same cases end the same way:
    ///
    /// - read and parsed: the file's records (duplicates merged as in
    ///   `load`) with this run's sets over them, which are newer. The
    ///   file's records are not applied to the property area — init does
    ///   not apply them either; they are only kept in the file.
    /// - not parsable, missing, or a non-transient error: this run's
    ///   records replace the file on the next write. init deletes an
    ///   unparsable file and rebuilds it from the area's `persist.*`
    ///   values ("Recovering persistent properties from memory").
    /// - transient again: still pending; the caller does not write, so the
    ///   records stay unsynced and the next set tries again.
    pub(crate) fn finish_reload(&mut self, result: std::io::Result<Vec<u8>>) {
        match result {
            Ok(bytes) => {
                match decode(&bytes) {
                    Ok(file_records) => {
                        let ours = std::mem::replace(&mut self.records, file_records);
                        self.merge_duplicates();
                        for (name, value) in ours {
                            match self.records.iter_mut().find(|(n, _)| *n == name) {
                                Some(entry) => entry.1 = value,
                                None => self.records.push((name, value)),
                            }
                        }
                        info!("Read {:?} again and merged its records", self.path);
                    }
                    Err(e) => error!(
                        "Unable to parse persistent property file {:?}: {e}; \
                         the next write replaces it",
                        self.path
                    ),
                }
                self.reload_pending = false;
            }
            Err(e) if is_transient(&e) => warn!(
                "Still cannot read persistent property file {:?}: {e}; not writing it yet",
                self.path
            ),
            Err(e) => {
                if e.kind() != ErrorKind::NotFound {
                    error!(
                        "Unable to read persistent property file {:?}: {e}; \
                         the next write replaces it",
                        self.path
                    );
                }
                self.reload_pending = false;
            }
        }
    }
}

/// A read error that may not happen again: `EINTR`, `EIO`, `EAGAIN`,
/// `ENOMEM`, `ENOBUFS`, `EMFILE`, `ENFILE`. Anything else (`EACCES`, a
/// symlink's `ELOOP`, the mode check) fails the same way on every read.
fn is_transient(e: &std::io::Error) -> bool {
    use rustix::io::Errno;
    e.kind() == ErrorKind::Interrupted
        || e.raw_os_error().is_some_and(|code| {
            [
                Errno::IO,
                Errno::AGAIN,
                Errno::NOMEM,
                Errno::NOBUFS,
                Errno::MFILE,
                Errno::NFILE,
            ]
            .iter()
            .any(|errno| errno.raw_os_error() == code)
        })
}

impl Drop for PersistentStore {
    /// Unlocks before the fd is closed. Closing releases a `flock` only
    /// when no other fd refers to the open file description, and a child
    /// this process is spawning has a copy of every fd until its `exec`
    /// closes the `O_CLOEXEC` ones; `LOCK_UN` releases it regardless.
    ///
    /// Not in a `fork` child (pid differs from the loading process), as in
    /// `rsproperties::AreaWriterLock`: `LOCK_UN` there would unlock the
    /// file while the parent still writes to it.
    fn drop(&mut self) {
        if std::process::id() != self.owner_pid {
            return;
        }
        if let Err(e) = rustix::fs::flock(&self.lock, rustix::fs::FlockOperation::Unlock) {
            // The close that follows still releases it, once every copy of
            // the fd is closed.
            warn!("Failed to unlock the lock file of {:?}: {e}", self.path);
        }
    }
}

/// Opens `<path>.lock` and takes an exclusive, non-blocking `flock` on it,
/// like `rsproperties::AreaWriterLock`: `O_NOFOLLOW` so a planted symlink
/// cannot move the lock to another inode, and mode 0600 (re-applied with
/// `fchmod` for a file left by an older run) because `flock` works on a
/// read-only fd and any user able to open the file could hold the lock.
fn lock_file(path: &Path) -> std::io::Result<File> {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as _)
        .open(&lock_path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("cannot open {lock_path:?}: {e}")))?;
    rustix::fs::fchmod(&file, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR).map_err(|e| {
        std::io::Error::new(
            std::io::Error::from(e).kind(),
            format!("cannot chmod {lock_path:?}: {e}"),
        )
    })?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
        |e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                std::io::Error::new(
                    ErrorKind::WouldBlock,
                    format!(
                        "persistent properties file {path:?} is used by another service \
                         ({lock_path:?} is locked): {e}"
                    ),
                )
            } else {
                std::io::Error::new(
                    std::io::Error::from(e).kind(),
                    format!("cannot lock {lock_path:?}: {e}"),
                )
            }
        },
    )?;
    Ok(file)
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// init `ReadFile` (`init/util.cpp`): opened with `O_NOFOLLOW`, and a
/// group- or world-writable file is refused ("Skipping insecure file").
/// rsproperties' `build_property_parser::open_prop_file` applies the same
/// rule to prop files; change both together. Not shared: it would be a
/// public rsproperties API with this crate as its only user.
pub(crate) fn read_file(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::PermissionsExt;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as _)
        .open(path)?;
    if file.metadata()?.permissions().mode() & 0o022 != 0 {
        return Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "group- or world-writable file skipped (init ReadFile)",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// init `WritePersistentPropertyFile`: write `<path>.tmp` (mode 0600,
/// `O_NOFOLLOW`, truncated), fsync it, rename it over `path`, then fsync
/// the directory so the rename reaches storage. A failed rename removes
/// the temp file.
pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = tmp_path(path);
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as _)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    File::open(parent_dir(path))?.sync_all()
}

/// Serializes `records` as `PersistentProperties`.
pub(crate) fn encode(records: &[Record]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut record = Vec::new();
    for (name, value) in records {
        record.clear();
        put_bytes_field(&mut record, 1, name);
        put_bytes_field(&mut record, 2, value);
        put_bytes_field(&mut out, 1, &record);
    }
    out
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A length-delimited (wire type 2) field.
fn put_bytes_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_varint(out, (field << 3) | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Parses `PersistentProperties` and checks every name's prefix, as init's
/// `ParsePersistentPropertyFile` does. Unknown fields are skipped, a
/// repeated `name` / `value` keeps its last occurrence, and a missing one
/// is empty (proto2 semantics).
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<Record>, String> {
    let mut reader = Reader { buf: bytes };
    let mut records = Vec::new();
    while !reader.buf.is_empty() {
        let (field, wire_type) = reader.key()?;
        if field == 1 && wire_type == 2 {
            records.push(decode_record(reader.bytes()?)?);
        } else {
            reader.skip(field, wire_type, 0)?;
        }
    }
    for (name, _) in &records {
        if !is_persistent_name(name) {
            return Err(format!(
                "property {:?} doesn't start with 'persist.' or 'next_boot.'",
                String::from_utf8_lossy(name)
            ));
        }
    }
    Ok(records)
}

fn decode_record(bytes: &[u8]) -> Result<Record, String> {
    let mut reader = Reader { buf: bytes };
    let (mut name, mut value) = (Vec::new(), Vec::new());
    while !reader.buf.is_empty() {
        match reader.key()? {
            (1, 2) => name = reader.bytes()?.to_vec(),
            (2, 2) => value = reader.bytes()?.to_vec(),
            (field, wire_type) => reader.skip(field, wire_type, 0)?,
        }
    }
    Ok((name, value))
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for i in 0..10 {
            let (&b, rest) = self.buf.split_first().ok_or("truncated varint")?;
            self.buf = rest;
            if i == 9 && b > 1 {
                return Err("varint overflows 64 bits".into());
            }
            value |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint longer than 10 bytes".into())
    }

    /// Field number and wire type of the next key.
    fn key(&mut self) -> Result<(u64, u8), String> {
        let key = self.varint()?;
        let field = key >> 3;
        if field == 0 || field > u64::from(u32::MAX >> 3) {
            return Err(format!("invalid field number {field}"));
        }
        Ok((field, (key & 7) as u8))
    }

    fn take(&mut self, len: u64) -> Result<&'a [u8], String> {
        let len = usize::try_from(len).map_err(|_| "length overflows usize")?;
        if len > self.buf.len() {
            return Err("truncated field".into());
        }
        let (head, rest) = self.buf.split_at(len);
        self.buf = rest;
        Ok(head)
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let len = self.varint()?;
        self.take(len)
    }

    /// Skips one field of `wire_type`; a group (3) is skipped up to its
    /// matching end-group key.
    fn skip(&mut self, field: u64, wire_type: u8, depth: u32) -> Result<(), String> {
        match wire_type {
            0 => self.varint().map(drop),
            1 => self.take(8).map(drop),
            2 => self.bytes().map(drop),
            5 => self.take(4).map(drop),
            3 => {
                if depth >= 64 {
                    return Err("groups nested too deeply".into());
                }
                loop {
                    match self.key()? {
                        (f, 4) if f == field => return Ok(()),
                        (_, 4) => return Err("mismatched end-group".into()),
                        (f, wt) => self.skip(f, wt, depth + 1)?,
                    }
                }
            }
            4 => Err("unexpected end-group".into()),
            other => Err(format!("invalid wire type {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, value: &str) -> Record {
        (name.as_bytes().to_vec(), value.as_bytes().to_vec())
    }

    /// Bytes `protoc --encode` produces for two records — what init writes.
    #[test]
    fn encoding_matches_protobuf_wire_format() {
        let bytes = encode(&[rec("persist.a", "1"), rec("persist.b", "")]);
        let mut expected = vec![0x0a, 14, 0x0a, 9];
        expected.extend_from_slice(b"persist.a");
        expected.extend_from_slice(&[0x12, 1, b'1']);
        expected.extend_from_slice(&[0x0a, 13, 0x0a, 9]);
        expected.extend_from_slice(b"persist.b");
        expected.extend_from_slice(&[0x12, 0]);
        assert_eq!(bytes, expected);
        assert_eq!(
            decode(&bytes).unwrap(),
            vec![rec("persist.a", "1"), rec("persist.b", "")]
        );
    }

    #[test]
    fn long_values_use_multibyte_lengths() {
        let value = "v".repeat(300);
        let records = vec![rec("persist.long", &value)];
        assert_eq!(decode(&encode(&records)).unwrap(), records);
    }

    #[test]
    fn decode_skips_unknown_fields_and_keeps_last_value() {
        let mut inner = Vec::new();
        put_bytes_field(&mut inner, 1, b"persist.x");
        put_bytes_field(&mut inner, 2, b"old");
        put_varint(&mut inner, 3 << 3); // field 3, varint
        put_varint(&mut inner, 300);
        put_bytes_field(&mut inner, 2, b"new");
        let mut bytes = Vec::new();
        put_varint(&mut bytes, (7 << 3) | 5); // unknown fixed32 at top level
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        // An unknown group with a nested field.
        put_varint(&mut bytes, (9 << 3) | 3);
        put_varint(&mut bytes, 1 << 3);
        put_varint(&mut bytes, 5);
        put_varint(&mut bytes, (9 << 3) | 4);
        put_bytes_field(&mut bytes, 1, &inner);
        assert_eq!(decode(&bytes).unwrap(), vec![rec("persist.x", "new")]);
    }

    #[test]
    fn decode_rejects_malformed_input_and_foreign_names() {
        for bad in [
            &[0x0a, 5, 0x0a][..], // truncated record
            &[0x0a][..],          // truncated length
            &[0x00][..],          // field 0
            &[0x0e][..],          // wire type 6
            &[0x0c][..],          // end-group without start
            &[0xff; 11][..],      // overlong varint
        ] {
            assert!(decode(bad).is_err(), "{bad:?}");
        }
        assert!(decode(&encode(&[rec("ro.x", "1")])).is_err());
        assert!(decode(&encode(&[rec("", "1")])).is_err());
        assert!(decode(&encode(&[rec("next_boot.persist.a", "1")])).is_ok());
        assert_eq!(decode(&[]).unwrap(), Vec::<Record>::new());
    }

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

    #[test]
    fn load_applies_staged_next_boot_records() {
        let tmp = TempDir::new("rsprops_persist_staged");
        let path = tmp.0.join("persistent_properties");
        write_file(
            &path,
            &encode(&[
                rec("persist.a", "1"),
                rec("next_boot.persist.a", "2"),
                rec("persist.b", "x"),
                rec("next_boot.persist.c", "new"),
            ]),
        )
        .unwrap();
        let store = PersistentStore::load(&path).unwrap();
        let expected = vec![
            rec("persist.a", "2"),
            rec("persist.b", "x"),
            rec("persist.c", "new"),
        ];
        assert_eq!(store.records(), expected.as_slice());
        // Written back without the staged records.
        assert_eq!(decode(&std::fs::read(&path).unwrap()).unwrap(), expected);
    }

    #[test]
    fn load_deletes_unparsable_file_and_removes_tmp() {
        let tmp = TempDir::new("rsprops_persist_corrupt");
        let path = tmp.0.join("persistent_properties");
        write_file(&path, &[0x0a, 50, 1]).unwrap();
        std::fs::write(tmp_path(&path), b"partial").unwrap();
        let store = PersistentStore::load(&path).unwrap();
        assert!(store.records().is_empty());
        assert!(!path.exists());
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn load_keeps_unreadable_file_and_fails_without_parent() {
        let tmp = TempDir::new("rsprops_persist_missing");
        let path = tmp.0.join("persistent_properties");
        assert!(PersistentStore::load(&path).unwrap().records().is_empty());
        assert!(!path.exists(), "a missing file is not created by load");

        let target = tmp.0.join("target");
        write_file(&target, &encode(&[rec("persist.a", "1")])).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(PersistentStore::load(&path).unwrap().records().is_empty());
        assert!(
            path.symlink_metadata().is_ok(),
            "a symlink is left in place"
        );

        assert!(PersistentStore::load(&tmp.0.join("no_dir/persistent_properties")).is_err());
    }

    /// init `ReadFile` skips a group- or world-writable file; it is not
    /// deleted.
    #[test]
    fn load_skips_group_or_world_writable_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("rsprops_persist_insecure");
        let path = tmp.0.join("persistent_properties");
        write_file(&path, &encode(&[rec("persist.a", "1")])).unwrap();
        for mode in [0o620, 0o602, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let store = PersistentStore::load(&path).unwrap();
            assert!(store.records().is_empty(), "{mode:o}");
            assert!(path.exists(), "{mode:o}");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            PersistentStore::load(&path).unwrap().records(),
            [rec("persist.a", "1")].as_slice()
        );
    }

    #[test]
    fn update_skips_unchanged_values() {
        let tmp = TempDir::new("rsprops_persist_update");
        let mut store = PersistentStore::load(&tmp.0.join("persistent_properties")).unwrap();
        let first = store.update("persist.a", "1".into()).expect("new record");
        assert_eq!(decode(&first).unwrap(), vec![rec("persist.a", "1")]);
        assert!(store.update("persist.a", "1".into()).is_none());
        let second = store.update("persist.a", "2".into()).expect("changed");
        assert_eq!(decode(&second).unwrap(), vec![rec("persist.a", "2")]);
    }

    /// After a failed write, setting the same value again is written.
    #[test]
    fn update_rewrites_unchanged_value_after_failed_write() {
        let tmp = TempDir::new("rsprops_persist_unsynced");
        let mut store = PersistentStore::load(&tmp.0.join("persistent_properties")).unwrap();
        store.update("persist.a", "1".into()).expect("new record");
        store.set_synced(false);
        let retry = store
            .update("persist.a", "1".into())
            .expect("previous write failed");
        assert_eq!(decode(&retry).unwrap(), vec![rec("persist.a", "1")]);
        store.set_synced(true);
        assert!(store.update("persist.a", "1".into()).is_none());
    }

    /// One store per file: a second load fails while the first store
    /// lives (also inside one process: `flock` belongs to the open file
    /// description), and succeeds once it is dropped.
    #[test]
    fn second_store_on_same_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("rsprops_persist_lock");
        let path = tmp.0.join("persistent_properties");
        let first = PersistentStore::load(&path).unwrap();
        let err = PersistentStore::load(&path)
            .err()
            .expect("second load must fail");
        assert_eq!(err.kind(), ErrorKind::WouldBlock, "{err}");
        let lock_mode = std::fs::metadata(tmp.0.join("persistent_properties.lock"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(lock_mode & 0o777, 0o600);
        drop(first);
        assert!(PersistentStore::load(&path).is_ok());
    }

    /// A copy of the lock fd that outlives the store, like the one a child
    /// holds between its clone and its `exec`, does not keep the file
    /// locked.
    #[test]
    fn dropped_store_unlocks_while_fd_copy_is_open() {
        let tmp = TempDir::new("rsprops_persist_lock_copy");
        let path = tmp.0.join("persistent_properties");
        let store = PersistentStore::load(&path).unwrap();
        // Same open file description, as in a child's fd table.
        let copy = store.lock.try_clone().unwrap();
        drop(store);
        let reloaded = PersistentStore::load(&path);
        drop(copy);
        if let Err(err) = reloaded {
            panic!("load after drop failed: {err}");
        }
    }

    /// A store dropped in a `fork` child leaves the lock with the parent,
    /// whose descriptor shares the open file description.
    #[test]
    fn drop_in_another_process_does_not_unlock() {
        let tmp = TempDir::new("rsprops_persist_lock_fork");
        let path = tmp.0.join("persistent_properties");
        let mut store = PersistentStore::load(&path).unwrap();
        // The parent's descriptor, still open after the child's drop.
        let parent = store.lock.try_clone().unwrap();
        // What the child sees: `load` ran under another pid.
        store.owner_pid = store.owner_pid.wrapping_add(1);
        drop(store);
        let while_parent_holds = PersistentStore::load(&path);
        drop(parent);
        match while_parent_holds {
            Err(err) => assert_eq!(err.kind(), ErrorKind::WouldBlock, "{err}"),
            Ok(_) => panic!("the child's drop must not unlock"),
        }
    }

    /// A repeated name keeps its first position and its last value, the
    /// file is rewritten without the duplicate, and a later update is not
    /// shadowed by a stale copy on the next load.
    #[test]
    fn duplicate_names_are_merged_at_load() {
        let tmp = TempDir::new("rsprops_persist_dup");
        let path = tmp.0.join("persistent_properties");
        write_file(
            &path,
            &encode(&[
                rec("persist.a", "1"),
                rec("persist.b", "x"),
                rec("persist.a", "2"),
            ]),
        )
        .unwrap();
        let mut store = PersistentStore::load(&path).unwrap();
        let merged = vec![rec("persist.a", "2"), rec("persist.b", "x")];
        assert_eq!(store.records(), merged.as_slice());
        assert_eq!(decode(&std::fs::read(&path).unwrap()).unwrap(), merged);

        let bytes = store.update("persist.a", "3".into()).unwrap();
        write_file(&path, &bytes).unwrap();
        drop(store);
        let reloaded = PersistentStore::load(&path).unwrap();
        assert_eq!(
            reloaded.records(),
            [rec("persist.a", "3"), rec("persist.b", "x")].as_slice()
        );
    }

    #[test]
    fn write_file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("rsprops_persist_mode");
        let path = tmp.0.join("persistent_properties");
        write_file(&path, b"").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & !0o600, 0, "{mode:o}");
        assert!(!tmp_path(&path).exists());
    }
}
