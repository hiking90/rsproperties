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
//! Differences from init, all for a host service rather than a device:
//! - The legacy layout (one `/data/property/persist.*` file per property,
//!   Android 8 and earlier) is not read.
//! - Records are kept in memory after the load; init re-reads the file
//!   before every write. A file edited by another program while the
//!   service runs is overwritten on the next persistent set.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use log::{error, info, warn};

/// One record: name and value bytes as stored (proto2 `string` fields are
/// not validated as UTF-8 by init's C++ parser either).
pub(crate) type Record = (Vec<u8>, Vec<u8>);

const PERSIST_PREFIX: &[u8] = b"persist.";
const NEXT_BOOT_PREFIX: &[u8] = b"next_boot.";

/// Whether a successful set of `name` is written to the file: init's
/// `PropertySet` persists `persist.` and `next_boot.` names.
pub(crate) fn is_persistent_name(name: &str) -> bool {
    let name = name.as_bytes();
    name.starts_with(PERSIST_PREFIX) || name.starts_with(NEXT_BOOT_PREFIX)
}

/// The loaded file plus the records the service writes back.
pub(crate) struct PersistentStore {
    path: PathBuf,
    records: Vec<Record>,
}

impl PersistentStore {
    /// init `LoadPersistentProperties`, without the legacy directory:
    ///
    /// 1. A leftover `<path>.tmp` (a write interrupted before its rename)
    ///    is removed.
    /// 2. A file that cannot be read — missing, unreadable, or a symlink
    ///    (opened with `O_NOFOLLOW`, as libbase `ReadFileToString` does) —
    ///    gives an empty set and is left as it is.
    /// 3. A file that does not parse, or holds a name that starts with
    ///    neither `persist.` nor `next_boot.`, is deleted and gives an empty
    ///    set, so later writes succeed (init does the same).
    /// 4. Staged `next_boot.<name>` records replace `<name>`'s value (or
    ///    are appended when `<name>` has no record) and are dropped; the
    ///    result is written back.
    ///
    /// Fails only when the parent directory of `path` does not exist: every
    /// later write would fail, so the misconfiguration is reported at
    /// startup instead.
    pub(crate) fn load(path: &Path) -> std::io::Result<Self> {
        let parent = parent_dir(path);
        if !parent.is_dir() {
            return Err(std::io::Error::new(
                ErrorKind::NotFound,
                format!("directory of the persistent properties file {path:?} does not exist"),
            ));
        }
        let tmp = tmp_path(path);
        match std::fs::remove_file(&tmp) {
            Ok(()) => {
                info!("Removed {tmp:?}: a previous persistent property write may have failed")
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => warn!("Cannot remove {tmp:?}: {e}"),
        }

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
                Vec::new()
            }
        };

        let mut store = Self {
            path: path.to_path_buf(),
            records,
        };
        if store.apply_staged() {
            if let Err(e) = write_file(&store.path, &encode(&store.records)) {
                error!(
                    "Could not store persistent properties in {:?}: {e}",
                    store.path
                );
            }
        }
        Ok(store)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn records(&self) -> &[Record] {
        &self.records
    }

    /// The `next_boot.` step of init `LoadPersistentProperties`. Returns
    /// whether any staged record existed.
    fn apply_staged(&mut self) -> bool {
        let mut staged: Vec<Record> = Vec::new();
        for (name, value) in &self.records {
            if let Some(target) = name.strip_prefix(NEXT_BOOT_PREFIX) {
                // Last staged value wins, like init's map assignment.
                match staged.iter_mut().find(|(n, _)| n == target) {
                    Some(entry) => entry.1 = value.clone(),
                    None => staged.push((target.to_vec(), value.clone())),
                }
            }
        }
        if staged.is_empty() {
            return false;
        }
        let mut updated = Vec::with_capacity(self.records.len());
        for (name, value) in self.records.drain(..) {
            if name.starts_with(NEXT_BOOT_PREFIX) {
                continue;
            }
            match staged.iter().position(|(n, _)| *n == name) {
                Some(i) => {
                    let (_, staged_value) = staged.remove(i);
                    updated.push((name, staged_value));
                }
                None => updated.push((name, value)),
            }
        }
        // init appends the remaining staged records in `unordered_map`
        // order (unspecified); here in the order they appeared.
        updated.extend(staged);
        self.records = updated;
        true
    }

    /// init `WritePersistentProperty`, in memory: updates the first record
    /// named `name`, or appends one. Returns the encoded file to write, or
    /// `None` when the record already has `value` (init skips the write
    /// then too).
    pub(crate) fn update(&mut self, name: &str, value: &str) -> Option<Vec<u8>> {
        match self.records.iter_mut().find(|(n, _)| n == name.as_bytes()) {
            Some((_, stored)) if stored == value.as_bytes() => return None,
            Some((_, stored)) => *stored = value.as_bytes().to_vec(),
            None => self
                .records
                .push((name.as_bytes().to_vec(), value.as_bytes().to_vec())),
        }
        Some(encode(&self.records))
    }
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

fn read_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as _)
        .open(path)?;
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
        if !name.starts_with(PERSIST_PREFIX) && !name.starts_with(NEXT_BOOT_PREFIX) {
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
        std::fs::write(&path, [0x0a, 50, 1]).unwrap();
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
        std::fs::write(&target, encode(&[rec("persist.a", "1")])).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(PersistentStore::load(&path).unwrap().records().is_empty());
        assert!(
            path.symlink_metadata().is_ok(),
            "a symlink is left in place"
        );

        assert!(PersistentStore::load(&tmp.0.join("no_dir/persistent_properties")).is_err());
    }

    #[test]
    fn update_skips_unchanged_values() {
        let tmp = TempDir::new("rsprops_persist_update");
        let mut store = PersistentStore::load(&tmp.0.join("persistent_properties")).unwrap();
        let first = store.update("persist.a", "1").expect("new record");
        assert_eq!(decode(&first).unwrap(), vec![rec("persist.a", "1")]);
        assert!(store.update("persist.a", "1").is_none());
        let second = store.update("persist.a", "2").expect("changed");
        assert_eq!(decode(&second).unwrap(), vec![rec("persist.a", "2")]);
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
