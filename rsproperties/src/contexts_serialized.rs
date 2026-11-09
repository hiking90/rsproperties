// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

#[cfg(feature = "builder")]
use std::ffi::CStr;
use std::path::Path;
#[cfg(feature = "builder")]
use std::path::PathBuf;

use crate::errors::*;
#[cfg(feature = "builder")]
use log::info;
use log::{debug, error, warn};
#[cfg(feature = "builder")]
use rustix::fs;

#[cfg(feature = "builder")]
use crate::context_node::PropertyAreaMutGuard;
use crate::context_node::{ContextNode, PropertyAreaGuard};
use crate::property_area::{PropertyArea, PropertyAreaMap};
use crate::property_info_parser::{PropertyInfoArea, PropertyInfoAreaFile};

/// Filenames the property directory reserves for its own bookkeeping. A
/// context named after one of these would make `ContextNode::open()` (which
/// renames a fresh area file over its path via `PropertyAreaMap::new_rw`)
/// destroy that bookkeeping: `.writer_lock` → the flock the writer just
/// acquired keeps guarding an orphan inode, so a second writer can "win"
/// the lock and both writers replace each other's areas;
/// `properties_serial` → the serial mapping created right after the node
/// loop replaces the node's file again, leaving the node on an orphan
/// inode invisible to readers; `property_info` → the trie file itself is
/// destroyed. (`.writer_lock` is also covered by the leading-dot rule in
/// `try_build_context_node`.)
///
/// Compared (like the duplicate check) on the ASCII-case-folded name: on a
/// case-insensitive filesystem (macOS APFS default, where this crate's
/// tests run writable) `"PROPERTY_INFO"` would otherwise pass the exact
/// match yet replace the real `property_info`.
const RESERVED_FILENAMES: &[&str] = &[".writer_lock", "properties_serial", "property_info"];

/// Name of the writer lock file inside the properties directory.
#[cfg(feature = "builder")]
const WRITER_LOCK_FILENAME: &str = ".writer_lock";

/// Name prefix of the temp file `AreaWriterLock::publish_property_info`
/// renames into place: `.property_info.tmp-<pid>`. Same format as the
/// 0.6-era service's temp, so the sweep also removes those.
#[cfg(feature = "builder")]
const PROPERTY_INFO_TMP_PREFIX: &str = ".property_info.tmp-";

/// Decodes one `ContextNode` entry from the property-info area. Returns
/// `Err` on corrupt offset, missing NUL terminator, or non-UTF-8 name —
/// callers tag the slot as `None` so the surrounding `Vec<Option<_>>`
/// indices stay aligned with the parser's `context_index` values.
fn try_build_context_node(
    area: &PropertyInfoArea<'_>,
    dirname: &Path,
    writable: bool,
    i: usize,
    seen_names: &mut std::collections::HashSet<String>,
) -> Result<ContextNode> {
    let context_offset = area.context_offset(i)?;
    // `cstr()` reports out-of-range offsets and missing NUL terminators as
    // errors; an *empty* name is still rejected here — it would produce a
    // plausible-looking node whose filename is the directory itself,
    // failing `open()` far from the actual corruption.
    let context_cstr = area.cstr(context_offset)?;
    if context_cstr.is_empty() {
        return Err(Error::FileValidation(format!(
            "context entry {i}: empty context name at offset {context_offset}"
        )));
    }
    let context_name = context_cstr.to_str()?;
    // Legitimate SELinux context names are pure ASCII. Rejecting anything
    // else closes what the ASCII case-fold below cannot: macOS APFS also
    // aliases Unicode case variants and NFC/NFD normalization forms onto
    // one file, so two "different" non-ASCII names could pass the
    // duplicate check yet replace each other's area files — exactly the
    // destructive collision these checks exist to prevent.
    if !context_name.is_ascii() {
        return Err(Error::FileValidation(format!(
            "context entry {i}: context name {context_name:?} contains non-ASCII characters"
        )));
    }
    // The context name comes from file content and becomes a filename via
    // `dirname.join()` — which *replaces* `dirname` entirely when handed an
    // absolute path, and descends on `/` or `..`. On the writable path the
    // resulting file is later replaced by `PropertyAreaMap::new_rw`, so a
    // corrupt or malicious property_info must not be able to point a node
    // outside the properties directory. Legitimate SELinux context names
    // (`u:object_r:...:s0`) never contain a path separator, so requiring a
    // single normal component loses nothing.
    {
        use std::path::Component;
        let mut components = Path::new(context_name).components();
        // The explicit `contains('/')` check closes what `components()`
        // normalizes away: `"a/"`, `"a//"`, and `"a/."` all yield a single
        // `Normal("a")` component yet are not plain filenames.
        if context_name.contains('/')
            || !matches!(
                (components.next(), components.next()),
                (Some(Component::Normal(_)), None)
            )
        {
            return Err(Error::FileValidation(format!(
                "context entry {i}: context name {context_name:?} is not a plain filename"
            )));
        }
    }
    // The writer's own files in this directory — `.writer_lock` and the
    // temp files it renames into place (`.area.tmp-*`,
    // `.property_info.tmp-*`) — all start with `.`; no SELinux context
    // does. Rejecting the prefix keeps every reader off an unpublished
    // temp file (property_area.rs [SM3]) and every writer off those files.
    if context_name.starts_with('.') {
        return Err(Error::FileValidation(format!(
            "context entry {i}: context name {context_name:?} starts with '.'"
        )));
    }
    // Same-directory collisions are as destructive as directory escape —
    // see `RESERVED_FILENAMES`. Both checks compare the ASCII-case-folded
    // name so a case-insensitive filesystem cannot be used to alias two
    // "different" names onto one file (the fold is also why the seen-set
    // key is an owned `String` rather than a borrow of the mmap'd name).
    let folded_name = context_name.to_ascii_lowercase();
    if RESERVED_FILENAMES.contains(&folded_name.as_str()) {
        return Err(Error::FileValidation(format!(
            "context entry {i}: context name {context_name:?} collides with a reserved filename"
        )));
    }
    // Two entries with the same name would share one file: the later
    // node's `open()` replaces the earlier node's area, silently losing its
    // properties within the same instance. First entry wins; later
    // duplicates are skipped like any other corrupt entry.
    if !seen_names.insert(folded_name) {
        return Err(Error::FileValidation(format!(
            "context entry {i}: duplicate context name {context_name:?}"
        )));
    }
    // The owned context is only consumed by `open()` (writable path) for
    // SELinux labeling; read-only nodes skip the allocation.
    let context = writable.then(|| context_cstr.to_owned());
    Ok(ContextNode::new(
        writable,
        context,
        dirname.join(context_name),
    ))
}

// Pre-defined CStr constants to avoid unsafe code at runtime
// Using const_str macro or safer compile-time construction
#[cfg(feature = "builder")]
const PROPERTIES_SERIAL_CONTEXT: &CStr = c"u:object_r:properties_serial:s0";

/// Exclusive writer lock of a properties directory: a non-blocking
/// `flock(LOCK_EX)` on `<dir>/.writer_lock`, held until this value is
/// dropped (the kernel also releases it when the process dies).
///
/// The lock belongs to the open file description, so a second
/// [`acquire`](Self::acquire) on the same directory fails even inside the
/// same process. `acquire` also removes the temp files a crashed writer
/// left in the directory (`.area.tmp-<pid>-<n>`,
/// `.property_info.tmp-<pid>`); it does so only after taking the lock, so
/// it never removes a live writer's temp file.
///
/// Take the lock *before* writing `property_info`, so a second writer
/// fails before it replaces anything the first one published:
///
/// ```rust,no_run
/// # fn main() -> rsproperties::Result<()> {
/// use rsproperties::{build_trie, AreaWriterLock, PropertyInfoEntry, SystemProperties};
/// use std::path::Path;
///
/// let (entries, _errors) =
///     PropertyInfoEntry::parse_from_file(Path::new("property_contexts"), false)?;
/// let trie = build_trie(&entries, "u:object_r:default_prop:s0", "string")?;
///
/// let lock = AreaWriterLock::acquire(Path::new("/run/my_properties"))?;
/// lock.publish_property_info(&trie)?;
/// let mut props = SystemProperties::new_area_locked(lock)?;
/// props.set("debug.example", "1")?;
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "builder")]
#[derive(Debug)]
#[must_use = "dropping the lock releases the properties directory"]
pub struct AreaWriterLock {
    dir: PathBuf,
    /// Holds the flock; never read.
    _file: std::fs::File,
}

#[cfg(feature = "builder")]
impl AreaWriterLock {
    /// Takes the writer lock of `dir`.
    ///
    /// Creates `dir` if it is missing (the last component only, mode 0711
    /// minus the umask's bits, like AOSP init's `/dev/__properties__`),
    /// opens `<dir>/.writer_lock` (`O_NOFOLLOW`, mode 0600, re-asserted with
    /// `fchmod`), takes a non-blocking exclusive `flock`, then removes
    /// leftover writer temp files.
    ///
    /// # Errors
    ///
    /// A lock held by another instance (any process, or this one) fails
    /// with `Error::Lock`, returned as is (not wrapped in
    /// `Error::Context`), before any area file, `property_info` or temp
    /// file in `dir` is changed (only the mode of `.writer_lock` is reset
    /// to 0600). Other failures are `Error::Context` naming the path.
    pub fn acquire(dir: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;

        if !dir.is_dir() {
            info!("Creating directory: {dir:?}");
            match fs::mkdir(dir, fs::Mode::RWXU | fs::Mode::XGRP | fs::Mode::XOTH) {
                Ok(()) => {}
                // Lost a create race with a concurrent writer — the flock
                // below is the real single-writer arbiter, so an
                // already-existing *directory* is not an error here.
                // Re-check with is_dir(): EEXIST for a plain file at this
                // path is a real configuration error and deferring it would
                // surface later as a confusing ENOTDIR.
                Err(rustix::io::Errno::EXIST) if dir.is_dir() => {}
                Err(e) => {
                    return Err(Error::from(e))
                        .context_with_location(format!("Failed to create {dir:?}"))
                }
            }
        }

        let lock_path = dir.join(WRITER_LOCK_FILENAME);
        // O_NOFOLLOW + explicit mode, like the area files created by
        // `PropertyAreaMap::new_rw`: this file is the single-writer
        // arbiter, so a symlink planted at `.writer_lock` must not be able
        // to redirect the `flock` to a different inode (two writers each
        // locking a different file would both "win"). Mode 0600, not 0644:
        // `flock(LOCK_EX)` succeeds on a read-only fd, so any user who can
        // open the file could otherwise squat the exclusive lock and block
        // the legitimate writer forever — nobody but the owner ever needs
        // to open this file.
        let lock_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(fs::OFlags::NOFOLLOW.bits() as _)
            .mode(0o600)
            .open(&lock_path)
            .context_with_location(format!("Failed to open writer lock {lock_path:?}"))?;
        // `.mode(0o600)` above only applies when the file is *created*; a
        // leftover lock file with wider permissions (e.g. 0644 from an
        // older version) would keep them and defeat the anti-squat
        // rationale. Re-assert the mode on the open fd before taking the
        // lock. (An attacker who already holds an open fd is not stopped —
        // this closes the window for every open that comes after.)
        fs::fchmod(&lock_file, fs::Mode::RUSR | fs::Mode::WUSR)
            .context_with_location(format!("Failed to restrict mode of {lock_path:?}"))?;
        fs::flock(&lock_file, fs::FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            error!("Another writer holds the property area lock {lock_path:?}: {e}");
            Error::Lock(format!(
                "Writable property area already owned by another instance ({lock_path:?}): {e}"
            ))
        })?;

        // Only under the lock: a temp file in `dir` now belongs to a writer
        // that no longer runs.
        sweep_writer_temps(dir);
        Ok(Self {
            dir: dir.to_path_buf(),
            _file: lock_file,
        })
    }

    /// The properties directory this lock guards.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes `trie` (the output of [`build_trie`](crate::build_trie)) as
    /// `<dir>/property_info`.
    ///
    /// The bytes go to `.property_info.tmp-<pid>` (created exclusively,
    /// mode 0444 regardless of the umask), are synced, are checked by the
    /// same loader readers use, and the temp file is renamed onto
    /// `property_info`. Readers that already opened `property_info` keep
    /// the previous file (rename keeps the old inode). Bytes the loader
    /// rejects are not published. Any failure leaves `property_info`
    /// unchanged and removes the temp file this call created; if the temp
    /// name already exists (another call on this lock is in progress), the
    /// call fails without touching it. May be called more than once.
    ///
    /// # Errors
    ///
    /// `Error::Context` naming the path.
    pub fn publish_property_info(&self, trie: &[u8]) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let path = self.dir.join("property_info");
        let tmp = self
            .dir
            .join(format!("{PROPERTY_INFO_TMP_PREFIX}{}", std::process::id()));
        // `create_new` = O_CREAT|O_EXCL, which also refuses a symlink at
        // the temp name. No cleanup when this fails: the existing file is
        // not ours (a concurrent call on this lock).
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(fs::OFlags::NOFOLLOW.bits() as _)
            .mode(0o444)
            .open(&tmp)
            .context_with_location(format!("Failed to create {tmp:?}"))?;

        let write_and_rename = || -> Result<()> {
            // The umask may have cleared bits of 0444; readers accept only
            // files without group/other write, and this makes the mode
            // exact.
            fs::fchmod(&file, fs::Mode::RUSR | fs::Mode::RGRP | fs::Mode::ROTH)
                .context_with_location(format!("Failed to set mode of {tmp:?}"))?;
            (&file)
                .write_all(trie)
                .context_with_location(format!("Failed to write {tmp:?}"))?;
            file.sync_all()
                .context_with_location(format!("Failed to sync {tmp:?}"))?;
            PropertyInfoAreaFile::load_path(&tmp)
                .context_with_location(format!("Rejected property_info trie in {tmp:?}"))?;
            std::fs::rename(&tmp, &path)
                .context_with_location(format!("Failed to rename {tmp:?} to {path:?}"))
        };
        let result = write_and_rename();
        drop(file);
        result.inspect_err(|_| {
            if let Err(e) = std::fs::remove_file(&tmp) {
                warn!("Failed to remove {tmp:?}: {e}");
            }
        })
    }
}

/// Removes regular files directly in `dir` named
/// `.area.tmp-<digits>-<digits>` or `.property_info.tmp-<digits>`. Errors
/// are logged and ignored. Symlinks are left alone. Call only while
/// holding `dir`'s writer lock (`AreaWriterLock::acquire`).
#[cfg(feature = "builder")]
fn sweep_writer_temps(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn!("Cannot list {dir:?} for stale writer temp files: {e}");
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                warn!("Cannot read an entry of {dir:?}: {e}");
                continue;
            }
        };
        if !entry.file_name().to_str().is_some_and(is_writer_temp_name) {
            continue;
        }
        let path = entry.path();
        match std::fs::symlink_metadata(&path) {
            Ok(md) if md.file_type().is_file() => {}
            Ok(_) => continue,
            Err(e) => {
                warn!("Cannot stat stale writer temp file {path:?}: {e}");
                continue;
            }
        }
        match std::fs::remove_file(&path) {
            Ok(()) => debug!("Removed stale writer temp file {path:?}"),
            Err(e) => warn!("Failed to remove stale writer temp file {path:?}: {e}"),
        }
    }
}

/// `.area.tmp-<digits>-<digits>` or `.property_info.tmp-<digits>`; every
/// digit run must be non-empty.
#[cfg(feature = "builder")]
fn is_writer_temp_name(name: &str) -> bool {
    fn digits(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }
    if let Some(rest) = name.strip_prefix(crate::property_area::AREA_TMP_PREFIX) {
        return rest
            .split_once('-')
            .is_some_and(|(pid, seq)| digits(pid) && digits(seq));
    }
    name.strip_prefix(PROPERTY_INFO_TMP_PREFIX)
        .is_some_and(digits)
}

pub(crate) struct ContextsSerialized {
    property_info_area_file: PropertyInfoAreaFile,
    /// `None` slots are corrupt context entries that were skipped during init.
    /// We keep the slot so that `context_index` values produced by the
    /// property-info parser line up with the vector's indices.
    context_nodes: Vec<Option<ContextNode>>,
    serial_property_area_map: PropertyAreaMap,
    /// Writer lock held for the lifetime of a writable instance (`None`
    /// when read-only). `PropertyAreaMap::new_rw` replaces area files, so
    /// without this lock a second writer would silently replace the files
    /// the first one still owns; with it, the loser fails before touching
    /// anything.
    #[cfg(feature = "builder")]
    _writer_lock: Option<AreaWriterLock>,
}

impl ContextsSerialized {
    /// Read-only instance over `dirname`.
    pub(crate) fn new_ro(dirname: &Path, load_default_path: bool) -> Result<Self> {
        let (property_info_area_file, context_nodes) =
            Self::load_nodes(false, dirname, load_default_path)?;
        let serial_filename = dirname.join("properties_serial");
        let serial_property_area_map =
            PropertyAreaMap::new_ro(&serial_filename).inspect_err(|e| {
                error!("Failed to map serial property area {serial_filename:?}: {e}")
            })?;
        Ok(Self {
            property_info_area_file,
            context_nodes,
            serial_property_area_map,
            #[cfg(feature = "builder")]
            _writer_lock: None,
        })
    }

    /// Writable instance: takes `lock` (or acquires the directory's lock
    /// when `None`), then loads `<dirname>/property_info` and creates every
    /// area.
    #[cfg(feature = "builder")]
    pub(crate) fn new_rw(dirname: &Path, lock: Option<AreaWriterLock>) -> Result<Self> {
        // Must precede the load and the `open()` calls below: the areas
        // must match the `property_info` readers open, and `open()`
        // replaces area files, so a losing second writer has to bail out
        // *before* touching anything the winner owns.
        let lock = match lock {
            Some(lock) => lock,
            None => {
                // A dir without `property_info` fails with the loader's
                // error before `acquire` creates the dir or `.writer_lock`
                // (0.6.0 behavior). `symlink_metadata`: a dangling symlink
                // counts as present, takes the lock, and then fails the
                // `O_NOFOLLOW` load. The probe only picks the error; the
                // areas are built from the load below, under the lock.
                let tree = dirname.join("property_info");
                if std::fs::symlink_metadata(&tree).is_err() {
                    PropertyInfoAreaFile::load_path(&tree)?;
                }
                AreaWriterLock::acquire(dirname)?
            }
        };
        // Under the lock no writer of this crate can replace
        // `property_info` (`publish_property_info` needs the lock).
        let (property_info_area_file, context_nodes) = Self::load_nodes(true, dirname, false)?;

        // `open()` takes `&self` (interior mutability via its RwLock) —
        // a `&mut` walk here would misread as structural mutation.
        for node in context_nodes.iter().flatten() {
            node.open()?;
        }

        let serial_filename = dirname.join("properties_serial");
        let serial_property_area_map =
            PropertyAreaMap::new_rw(&serial_filename, Some(PROPERTIES_SERIAL_CONTEXT))
                .inspect_err(|e| {
                    error!("Failed to map serial property area {serial_filename:?}: {e}")
                })?;

        Ok(Self {
            property_info_area_file,
            context_nodes,
            serial_property_area_map,
            _writer_lock: Some(lock),
        })
    }

    /// Loads `property_info` and builds one node slot per context entry.
    fn load_nodes(
        writable: bool,
        dirname: &Path,
        load_default_path: bool,
    ) -> Result<(PropertyInfoAreaFile, Vec<Option<ContextNode>>)> {
        let tree_filename = dirname.join("property_info");

        let property_info_area_file = if load_default_path {
            PropertyInfoAreaFile::load_default_path()
        } else {
            PropertyInfoAreaFile::load_path(tree_filename.as_path())
        }?;

        let property_info_area = property_info_area_file.property_info_area();
        let num_context_nodes = property_info_area.num_contexts();
        // The count is untrusted file data; it sizes the allocation below
        // AND bounds the loop. Two gates before allocating:
        // - the table-bounds check rejects a count whose declared table
        //   doesn't fit in the file (e.g. u32::MAX), so a small corrupt
        //   file can't drive a giant `Vec` allocation;
        // - the absolute cap bounds the ~25x amplification (4 table bytes
        //   → one `Option<ContextNode>` slot) still reachable from a
        //   *genuinely large* file that really contains its table. Real
        //   Android property_info files declare a few thousand contexts;
        //   the cap is far above any legitimate build.
        const MAX_CONTEXTS: usize = 65_536;
        if num_context_nodes > MAX_CONTEXTS {
            return Err(Error::FileValidation(format!(
                "context table declares {num_context_nodes} entries (max {MAX_CONTEXTS})"
            )));
        }
        if num_context_nodes > 0 {
            property_info_area
                .context_offset(num_context_nodes - 1)
                .map_err(|e| {
                    Error::FileValidation(format!(
                        "context table ({num_context_nodes} entries) exceeds property_info bounds: {e}"
                    ))
                })?;
        }
        let mut context_nodes: Vec<Option<ContextNode>> = Vec::with_capacity(num_context_nodes);

        let mut seen_names = std::collections::HashSet::new();
        for i in 0..num_context_nodes {
            match try_build_context_node(&property_info_area, dirname, writable, i, &mut seen_names)
            {
                Ok(n) => context_nodes.push(Some(n)),
                Err(e) => {
                    warn!("context entry {i} skipped: {e}");
                    context_nodes.push(None);
                }
            }
        }

        Ok((property_info_area_file, context_nodes))
    }

    /// Shared slot lookup for the accessors below: one bounds check via
    /// `.get()` (which also covers the parser's `u32::MAX` "no context"
    /// sentinel, since `len <= u32::MAX`) plus the `None`-slot
    /// (skipped-at-init) case.
    ///
    /// `miss_is_expected` picks the log level of the out-of-range case:
    /// a name lookup missing is the normal fallback flow on the get hot
    /// path (`debug!`), while an out-of-range *explicit* `context_index` —
    /// which this crate itself produced earlier — signals internal
    /// inconsistency or corruption (`warn!`). A `None` slot (entry existed
    /// in property_info but was corrupt at init) logs at `debug!` here:
    /// the corruption was already reported once with `warn!` during init,
    /// and this accessor sits on the get hot path.
    fn context_node_at(
        &self,
        index: u32,
        what: &dyn std::fmt::Display,
        miss_is_expected: bool,
    ) -> Result<&ContextNode> {
        match self.context_nodes.get(index as usize) {
            Some(Some(node)) => Ok(node),
            Some(None) => {
                // `debug!`, not `error!`: the corruption was already
                // reported once (warn) at init; this arm sits on the get
                // hot path and would otherwise log on every lookup of an
                // affected property. The returned error still carries the
                // full context.
                debug!("Context entry {index} for {what} was skipped during init");
                // `FileValidation`, NOT `NotFound`: callers fold `NotFound`
                // into "property absent" (`SystemProperties::find` returns
                // `Ok(None)`), and a context that *exists* in property_info
                // but was corrupt at init must not masquerade as absence —
                // it has to propagate as an error.
                Err(Error::FileValidation(format!(
                    "context entry {index} for {what} unavailable (corrupt at init)"
                )))
            }
            None => {
                if miss_is_expected {
                    debug!(
                        "No context for {what}: index={index}, max_contexts={}",
                        self.context_nodes.len()
                    );
                } else {
                    warn!(
                        "Context index out of range for {what}: index={index}, max_contexts={}",
                        self.context_nodes.len()
                    );
                }
                Err(Error::NotFound(format!("no context for {what}")))
            }
        }
    }

    pub(crate) fn prop_area_for_name(&self, name: &str) -> Result<(PropertyAreaGuard<'_>, u32)> {
        let (index, _) = self
            .property_info_area_file
            .property_info_area()
            .get_property_info_indexes(name);
        // `{name:?}`: read paths and `SystemProperties::set` (via `find`)
        // reach here with an unvalidated name, and log this error.
        let node = self.context_node_at(index, &format_args!("property {name:?}"), true)?;
        let area = node
            .property_area()
            .inspect_err(|e| error!("Failed to get property area for {name:?}: {e}"))?;
        Ok((area, index))
    }

    #[cfg(feature = "builder")]
    pub(crate) fn prop_area_mut_for_name(
        &self,
        name: &str,
    ) -> Result<(PropertyAreaMutGuard<'_>, u32)> {
        let (index, _) = self
            .property_info_area_file
            .property_info_area()
            .get_property_info_indexes(name);
        // `{name:?}`: `SystemProperties::add` reaches here before the name
        // is validated, and its log line prints this error.
        let node = self.context_node_at(index, &format_args!("property {name:?}"), true)?;
        let area = node
            .property_area_mut()
            .inspect_err(|e| error!("Failed to get mutable property area for {name:?}: {e}"))?;
        Ok((area, index))
    }

    pub(crate) fn serial_prop_area(&self) -> &PropertyArea {
        self.serial_property_area_map.property_area()
    }

    /// Whether `properties_serial` is mapped writable (property_area.rs
    /// \[R-WRITE\]); checked before the serial `fetch_add`.
    #[cfg(feature = "builder")]
    pub(crate) fn serial_area_writable(&self) -> bool {
        self.serial_property_area_map.is_writable()
    }

    pub(crate) fn prop_area_with_index(&self, context_index: u32) -> Result<PropertyAreaGuard<'_>> {
        self.context_node_at(
            context_index,
            &format_args!("context index {context_index}"),
            false,
        )?
        .property_area()
        .inspect_err(|e| {
            error!("Failed to get property area for context index {context_index}: {e}")
        })
    }

    #[cfg(feature = "builder")]
    pub(crate) fn prop_area_mut_with_index(
        &self,
        context_index: u32,
    ) -> Result<PropertyAreaMutGuard<'_>> {
        self.context_node_at(
            context_index,
            &format_args!("context index {context_index}"),
            false,
        )?
        .property_area_mut()
        .inspect_err(|e| {
            error!("Failed to get mutable property area for context index {context_index}: {e}")
        })
    }
}

#[cfg(all(test, feature = "builder", not(target_os = "android")))]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("rsprops_lock_{tag}_{}", std::process::id()));
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

    fn plant(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"stale").unwrap();
        path
    }

    #[test]
    fn writer_temp_name_patterns() {
        for ok in [
            ".area.tmp-1-0",
            ".area.tmp-4294967295-17",
            ".property_info.tmp-2",
        ] {
            assert!(is_writer_temp_name(ok), "{ok}");
        }
        for not in [
            ".area.tmp-",
            ".area.tmp-1",
            ".area.tmp-1-",
            ".area.tmp--1",
            ".area.tmp-abc-1",
            ".area.tmp-1-0x",
            ".property_info.tmp-",
            ".property_info.tmp-2a",
            "property_info",
            ".writer_lock",
        ] {
            assert!(!is_writer_temp_name(not), "{not}");
        }
    }

    #[test]
    fn acquire_sweeps_only_writer_temps() {
        let tmp = TempDir::new("sweep");
        let outside = TempDir::new("sweep_outside");
        let removed = [
            plant(&tmp.0, ".area.tmp-123-4"),
            plant(&tmp.0, ".property_info.tmp-99"),
        ];
        let kept = [
            plant(&tmp.0, ".area.tmp-abc"),
            plant(&tmp.0, ".area.tmp-1-x"),
            plant(&tmp.0, "u:object_r:foo:s0"),
        ];
        // A symlink with a temp name pointing outside the directory: the
        // sweep must not delete it (nor what it points to).
        let target = plant(&outside.0, "target");
        let link = tmp.0.join(".property_info.tmp-2");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let _lock = AreaWriterLock::acquire(&tmp.0).unwrap();

        for path in &removed {
            assert!(!path.exists(), "{path:?} must be swept");
        }
        for path in &kept {
            assert!(path.exists(), "{path:?} must be kept");
        }
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "symlink with a temp name must be kept"
        );
        assert!(target.exists());
    }

    #[test]
    fn sweep_runs_only_under_lock() {
        let tmp = TempDir::new("sweep_lock");
        let first = AreaWriterLock::acquire(&tmp.0).unwrap();
        // A live writer's in-progress temp file.
        let live = plant(&tmp.0, ".area.tmp-1-0");

        let second = AreaWriterLock::acquire(&tmp.0);
        assert!(
            matches!(second, Err(Error::Lock(_))),
            "a held lock must fail with an unwrapped Error::Lock: {second:?}"
        );
        assert!(
            live.exists(),
            "the loser must not sweep the winner's temp file"
        );
        drop(first);
    }

    fn new_rw_err(dir: &Path) -> Error {
        match ContextsSerialized::new_rw(dir, None) {
            Ok(_) => panic!("new_rw on {dir:?} must fail"),
            Err(e) => e,
        }
    }

    /// `new_rw` takes the lock before it reads `property_info`: with the
    /// lock held elsewhere, an unloadable `property_info` is not read.
    #[test]
    fn new_rw_locks_before_reading_property_info() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = TempDir::new("lockfirst");
        let held = AreaWriterLock::acquire(&tmp.0).unwrap();
        // Present but not a trie (0444, so no owner-write warning).
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(tmp.0.join("property_info"))
            .unwrap()
            .write_all(b"not a trie")
            .unwrap();
        let err = new_rw_err(&tmp.0);
        assert!(matches!(err, Error::Lock(_)), "{err:?}");
        drop(held);
    }

    /// Without `property_info`, `new_rw` creates neither the dir nor the
    /// lock file and returns the loader's error.
    #[test]
    fn new_rw_without_property_info_creates_nothing() {
        let parent = TempDir::new("noinfo");
        let dir = parent.0.join("props");
        let err = new_rw_err(&dir);
        assert!(
            matches!(&err, Error::Context { source, .. } if matches!(&**source, Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound)),
            "{err:?}"
        );
        assert!(!dir.exists(), "{dir:?} must not be created");
    }

    /// A dangling symlink at `property_info` counts as present: the lock
    /// is taken (`.writer_lock` is left behind), then the `O_NOFOLLOW`
    /// load fails.
    #[test]
    fn new_rw_dangling_property_info_takes_lock() {
        let tmp = TempDir::new("dangling");
        std::os::unix::fs::symlink(tmp.0.join("missing"), tmp.0.join("property_info")).unwrap();
        let err = new_rw_err(&tmp.0);
        assert!(!matches!(err, Error::Lock(_)), "{err:?}");
        assert!(tmp.0.join(".writer_lock").exists());
    }

    #[test]
    fn acquire_creates_missing_dir() {
        let parent = TempDir::new("mkdir");
        let dir = parent.0.join("props");
        let lock = AreaWriterLock::acquire(&dir).unwrap();
        assert!(dir.is_dir());
        assert_eq!(lock.dir(), dir.as_path());
        assert!(dir.join(".writer_lock").exists());
    }
}
