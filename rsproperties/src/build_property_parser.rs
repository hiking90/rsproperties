// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use log::{error, warn};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::errors::*;

const RESTORECON_PROPERTY: &str = "selinux.restorecon_recursive";

/// Bound on `import` nesting. Real prop files import at most one or two
/// levels deep; the cap exists so pathological nesting fails loudly
/// instead of recursing until the stack overflows. Cycles are cut by the
/// import *stack* (an unfiltered import of a file already on the stack is
/// skipped; a filtered import follows no `import` lines and is exempt);
/// the depth cap bounds only nesting, NOT fan-out — that is
/// [`MAX_TOTAL_LOADS`]' job.
const MAX_IMPORT_DEPTH: u8 = 8;

/// Bound on the total number of file loads in one
/// `load_properties_from_file` call. The recursion stack allows the same
/// file to be imported (and re-applied) from multiple places — AOSP
/// last-wins parity — so without a total budget, N same-child imports per
/// level nested `MAX_IMPORT_DEPTH` deep would re-parse the leaf N^depth
/// times. A real Android build loads a few dozen prop files; 1,000 is far
/// beyond any legitimate tree while stopping a crafted one immediately.
const MAX_TOTAL_LOADS: u32 = 1_000;

// C `isspace` in the "C" locale — what AOSP `LoadProperties` trims.
// `char::is_ascii_whitespace` omits `\x0b`; `str::trim` adds Unicode
// whitespace such as U+00A0, which AOSP keeps.
use crate::parse::is_c_space;

/// Bound on one line's in-memory size. Real prop-file lines are a few
/// hundred bytes; the cap exists so a crafted newline-less file cannot
/// grow a single `read_until` buffer without bound — the same threat
/// model as [`MAX_IMPORT_DEPTH`] / [`MAX_TOTAL_LOADS`].
pub(crate) const MAX_LINE_LEN: usize = 64 * 1024;

/// `read_until(b'\n')` with `MAX_LINE_LEN` as a hard memory bound.
/// Returns `(bytes_read_this_line, truncated)`; `(0, false)` is EOF. An
/// over-long line is drained (in bounded chunks) to its newline and
/// reported as `truncated = true` with only the first `MAX_LINE_LEN`
/// bytes in `buf` — callers warn and skip it.
pub(crate) fn read_bounded_line(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
) -> std::io::Result<(usize, bool)> {
    buf.clear();
    // `+ 1`: reading one byte past the cap distinguishes "line of exactly
    // MAX_LINE_LEN bytes" (fine) from "line longer than the cap".
    // UFCS on a reborrow: plain `reader.take(...)` resolves to `take`
    // *by value* on the opaque `impl BufRead` and fails to move out of
    // the `&mut`.
    let read = Read::take(&mut *reader, MAX_LINE_LEN as u64 + 1).read_until(b'\n', buf)?;
    if buf.len() <= MAX_LINE_LEN || buf.ends_with(b"\n") {
        return Ok((read, false));
    }
    // Over-long line: keep only the bounded prefix and drain the rest up
    // to (and including) its newline through the reader's own buffer —
    // memory stays bounded no matter the line length.
    buf.truncate(MAX_LINE_LEN);
    let mut total = read;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((total, true)); // EOF inside the over-long line
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(nl) => {
                reader.consume(nl + 1);
                return Ok((total + nl + 1, true));
            }
            None => {
                let n = available.len();
                reader.consume(n);
                total += n;
            }
        }
    }
}

/// Loads `key=value` pairs from an Android build.prop-style file into
/// `properties`.
///
/// Mirrors AOSP init's `LoadProperties`: when `filter` is `None`/empty,
/// `import <path>` lines are loaded recursively (with the `${property}`
/// expansion described below), and an import that
/// cannot be resolved or read — including one nested deeper than
/// `MAX_IMPORT_DEPTH` — is logged and skipped rather than aborting the
/// rest of the file. Import *cycles* are cut by a canonicalized-path
/// recursion stack; a file legitimately imported twice (shared base, or
/// re-imported after overrides) is re-applied each time, exactly as AOSP's
/// last-wins semantics require — `MAX_TOTAL_LOADS` bounds pathological
/// re-import fan-out. Non-UTF-8 lines are skipped with a warning — prop
/// files on real devices are byte streams and may carry stray non-UTF-8
/// comment bytes. Lines, keys, values, and `import` paths and filters are
/// trimmed of C `isspace` characters only (space, `\t`, `\n`, `\v`, `\f`,
/// `\r`); other Unicode whitespace is kept, like AOSP.
///
/// A value goes through the same expansion as an import path (AOSP
/// `ExpandProps`) before it is stored: `${name}` becomes the value
/// `properties` holds for `name`, `${name:-default}` becomes `default`
/// when that value is missing or empty, `$$` becomes `$`, and a `$` at the
/// end of the value is removed. An entry whose value cannot be expanded
/// (a `$` followed by anything else, no closing `}`, an empty name, or a
/// missing or empty property without a non-empty default) is logged at
/// `error` and dropped, and a value `properties` already holds for its
/// name stays. Write `$$` for a `$` that is part of a value.
///
/// The references are looked up in `properties`: the entries this call
/// and its imports have stored so far, and whatever the map held before
/// the call. AOSP init looks them up in the property area instead, where
/// the entries of the build.prop files it is loading are not stored yet
/// (init drops `b=${a}` when `a` comes from one of those files; this
/// loader stores `b` with `a`'s value), and where properties from other
/// sources, such as the kernel-derived `ro.boot.*`, are present. To make
/// such a property available here, put it in `properties` before the
/// call.
///
/// Every file, the root one included, is opened like AOSP init's
/// `ReadFile`: with `O_NOFOLLOW`, so a symlink as the last path component
/// fails to open (an error for the root file; a skipped import otherwise),
/// and a file writable by group or others is skipped with a warning —
/// for the root file this returns
/// `Ok(`[`PropFileOutcome::SkippedWritable`]`)` with `properties`
/// unchanged. A checkout under umask 002 leaves files with mode 0664, so
/// check the returned value where a file that loads nothing is a
/// failure:
///
/// ```rust,no_run
/// use rsproperties::{load_properties_from_file, PropFileOutcome};
/// use std::{collections::HashMap, path::Path};
///
/// # fn main() -> rsproperties::Result<()> {
/// let path = Path::new("/system/build.prop");
/// let mut properties = HashMap::new();
/// match load_properties_from_file(path, None, &mut properties)? {
///     PropFileOutcome::Loaded => {}
///     PropFileOutcome::SkippedWritable { mode } => {
///         eprintln!("{path:?} not loaded: mode {mode:o} allows group or other writes");
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// On error (I/O failure reading the root file, or more than
/// `MAX_TOTAL_LOADS` file loads across the whole pass), entries parsed
/// before the failure remain in `properties` — the map is an accumulator,
/// not a transaction.
///
/// No SELinux context is involved. AOSP's `LoadProperties` picks one from
/// the file's path (`u:r:init:s0`, or the vendor context under `/vendor`
/// and `/odm`) and drops the keys `CheckPermissions` denies it; this crate
/// has no SELinux policy to ask, so of `CheckPermissions` only the
/// property-name check applies.
pub fn load_properties_from_file(
    filename: &Path,
    filter: Option<&str>,
    properties: &mut HashMap<String, String>,
) -> Result<PropFileOutcome> {
    load_properties_from_file_with(filename, filter, properties, |_, _| true)
}

/// [`load_properties_from_file`] with a check of the caller's on every
/// entry: `accept(name, value)` returns whether the entry is kept.
///
/// `accept` stands where AOSP init's `LoadProperties` calls
/// `CheckPermissions`, after that function's property-name check, which
/// the loader does itself. It is called for an entry that passed the
/// filter, the `ctl.*` / `sys.powerctl` / `selinux.restorecon_recursive`
/// exclusion, the value expansion and the name check, with the trimmed
/// name and the expanded value, before the entry is put in `properties`. An entry it refuses is logged at
/// `warn` (file, line and name) and dropped; a value `properties` already
/// holds for that name stays, as in init, where a refused entry does not
/// replace the earlier one. Entries of imported files go through the same
/// `accept`.
///
/// A service that checks values against their `property_contexts` type
/// (init `CheckType`) passes a closure over
/// [`crate::property_type_in_trie`]:
///
/// ```rust,no_run
/// use rsproperties::{load_properties_from_file_with, property_type_in_trie};
/// use std::{collections::HashMap, path::Path};
///
/// # fn main() -> rsproperties::Result<()> {
/// # let trie: Vec<u8> = Vec::new();
/// let mut properties = HashMap::new();
/// let _outcome = load_properties_from_file_with(
///     Path::new("/system/build.prop"),
///     None,
///     &mut properties,
///     |name, value| match property_type_in_trie(&trie, name) {
///         // Only `int` here; init's `CheckType` covers the other types.
///         Ok(Some(t)) if t == "int" => value.is_empty() || value.parse::<i64>().is_ok(),
///         Ok(_) => true,
///         Err(_) => false,
///     },
/// )?;
/// # Ok(())
/// # }
/// ```
///
/// The returned value and the errors are those of
/// [`load_properties_from_file`].
pub fn load_properties_from_file_with(
    filename: &Path,
    filter: Option<&str>,
    properties: &mut HashMap<String, String>,
    mut accept: impl FnMut(&str, &str) -> bool,
) -> Result<PropFileOutcome> {
    let mut visited = HashSet::new();
    let mut loads = 0u32;
    load_properties_impl(
        filename,
        filter,
        properties,
        0,
        &mut visited,
        &mut loads,
        &mut accept,
    )
}

/// What [`load_properties_from_file`] or
/// [`load_properties_from_file_with`] did with the file it was given.
///
/// About that file only: an `import` it names can be skipped (unreadable,
/// group- or other-writable, nested too deep) and the result is still
/// [`PropFileOutcome::Loaded`]; those are logged at `warn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a skipped file loads nothing; check for `SkippedWritable`"]
pub enum PropFileOutcome {
    /// The file was read to its end. Lines the loader does not accept
    /// (illegal names, `ctl.*`, non-UTF-8, over-long) and entries the
    /// caller's check refused were dropped.
    Loaded,
    /// The file was not read because its mode has the group- or
    /// other-write bit (`mode & 0o022 != 0`) — AOSP init's `ReadFile`
    /// rule. `properties` is unchanged.
    SkippedWritable {
        /// The file's permission bits (`st_mode & 0o7777`).
        mode: u32,
    },
}

/// Expands property references in an import path or an entry's value
/// against the entries loaded so far, including entries read earlier in
/// the same load pass and whatever the caller put in the map beforehand.
/// This differs from AOSP, which expands against the live property store:
/// there, entries of the current bulk load are not yet visible, and store
/// values such as kernel-derived `ro.boot.*` are visible without being in
/// any prop file.
///
/// AOSP `init/util.cpp::ExpandProps` parity:
/// - `$$` is an escape for a literal `$`;
/// - `${name}` substitutes the property's value;
/// - `${name:-default}` falls back to `default` when the property is
///   missing **or empty** (AOSP's `GetProperty(name, "")` cannot tell the
///   two apart);
/// - a `$` at the end of the input is dropped and expansion succeeds;
/// - a `$` not followed by `{` or `$`, an unterminated `${`, a
///   zero-length name (`${}`, `${:-d}`), or a missing/empty property
///   without a non-empty default is an error → `None` (AOSP returns
///   `Error()` for each; the caller logs and skips the import or the
///   entry).
fn expand_props(raw: &str, properties: &HashMap<String, String>) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
            continue;
        }
        // AOSP: a `$` at the very end of the input ends expansion
        // successfully, dropping the `$`.
        if after.is_empty() {
            return Some(out);
        }
        let body = after.strip_prefix('{')?;
        let end = body.find('}')?;
        let reference = &body[..end];
        let (name, default) = match reference.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (reference, None),
        };
        // AOSP rejects a zero-length property name even with a default.
        if name.is_empty() {
            return None;
        }
        match properties.get(name).map(String::as_str) {
            Some(value) if !value.is_empty() => out.push_str(value),
            // Missing and empty collapse to the default, like AOSP; an
            // empty default (`${x:-}`) counts as no default there.
            _ => out.push_str(default.filter(|d| !d.is_empty())?),
        }
        rest = &body[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

// Recursion state (depth/visited/loads) and the caller's entry check
// travel with every call; a params struct would be re-destructured at each
// of the three call sites for no clarity gain. `accept` is `dyn` so the
// recursion is compiled once, not once per closure type.
fn load_properties_impl(
    filename: &Path,
    filter: Option<&str>,
    properties: &mut HashMap<String, String>,
    depth: u8,
    visited: &mut HashSet<PathBuf>,
    loads: &mut u32,
    accept: &mut dyn FnMut(&str, &str) -> bool,
) -> Result<PropFileOutcome> {
    if depth > MAX_IMPORT_DEPTH {
        // `Parse`, not `LimitExceeded`: the import call site swallows this
        // (log + skip, AOSP parity) — only the *global* loads budget below
        // aborts the pass.
        return Err(Error::Parse(format!(
            "import nesting deeper than {MAX_IMPORT_DEPTH} levels at {filename:?}"
        )));
    }
    // The stack-based cycle cut permits duplicate loads by design; this
    // budget is what keeps that from amplifying exponentially.
    // `LimitExceeded` is load-bearing: the import call site uses the
    // variant to tell "abort the whole pass" from per-import failures.
    *loads += 1;
    if *loads > MAX_TOTAL_LOADS {
        return Err(Error::LimitExceeded(format!(
            "more than {MAX_TOTAL_LOADS} file loads in one pass (import amplification) at {filename:?}"
        )));
    }

    // Canonicalize so `a.prop`, `./a.prop`, and a path through a symlinked
    // directory compare equal on the import stack (a symlink as the last
    // component fails the `O_NOFOLLOW` open in `open_prop_file`); fall back
    // to the raw path when canonicalization fails (e.g. the file doesn't
    // exist — the open below reports that).
    //
    // `visited` is a recursion *stack*, not a load-once set: the entry is
    // removed again before returning. Only genuine cycles through
    // unfiltered imports are cut (see the `filtered` exemption below) — a file
    // imported twice from different places is re-applied each time, like
    // AOSP init's `LoadProperties` (which has no dedup at all), so
    // re-imports keep their last-wins effect on earlier overrides.
    let canonical = std::fs::canonicalize(filename).unwrap_or_else(|_| filename.to_path_buf());
    // A filtered load never processes `import` lines (AOSP `flen == 0`
    // condition, mirrored in `load_properties_body`), so it cannot recurse
    // and is exempt from the cycle check: AOSP re-applies a filtered import
    // of a file that is still being loaded higher up the stack.
    let filtered = filter.is_some_and(|f| !f.is_empty());
    if !filtered && !visited.insert(canonical.clone()) {
        warn!("{filename:?} is already being loaded (import cycle) — skipping");
        // Only an import gets here (the stack is empty for the root
        // file), and the import call site does not read the outcome.
        return Ok(PropFileOutcome::Loaded);
    }
    // From here on every exit must pop the stack entry; wrap the body so
    // one removal covers all paths.
    let result = load_properties_body(filename, filter, properties, depth, visited, loads, accept);
    if !filtered {
        visited.remove(&canonical);
    }
    result
}

/// Opens a prop file the way AOSP init's `ReadFile` does
/// (`init/util.cpp`): `O_NOFOLLOW`, so a symlink as the last path
/// component fails the open (`ELOOP`), and a file whose mode has the
/// group- or other-write bit is skipped with a warning (`Ok(Err(mode))`,
/// the permission bits).
/// rsproperties-service's `persistent_properties::read_file` applies the
/// same rule to the persistent properties file; change both together.
///
/// Opens the caller's path, not the canonicalized cycle key:
/// `O_NOFOLLOW` on the canonical path would accept a symlinked prop file,
/// which init refuses. Directory components are still resolved at open
/// time, so they can differ from what `canonicalize` saw if one is
/// swapped in between; the depth/loads caps bound the effect on cycle
/// detection, the same window the canonical-path open had.
fn open_prop_file(filename: &Path) -> Result<std::result::Result<File, u32>> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // std adds O_CLOEXEC itself.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as _)
        .open(filename)
        .context_with_location(format!("Failed to open {filename:?}"))?;
    let mode = file
        .metadata()
        .context_with_location(format!("Failed to stat {filename:?}"))?
        .permissions()
        .mode()
        & 0o7777;
    // 0o022 = S_IWGRP | S_IWOTH, the bits init's `ReadFile` refuses.
    if mode & 0o022 != 0 {
        warn!(
            "{filename:?}: skipping group- or world-writable prop file (mode {mode:o}), like AOSP init"
        );
        return Ok(Err(mode));
    }
    Ok(Ok(file))
}

fn load_properties_body(
    filename: &Path,
    filter: Option<&str>,
    properties: &mut HashMap<String, String>,
    depth: u8,
    visited: &mut HashSet<PathBuf>,
    loads: &mut u32,
    accept: &mut dyn FnMut(&str, &str) -> bool,
) -> Result<PropFileOutcome> {
    let file = match open_prop_file(filename)? {
        Ok(file) => file,
        Err(mode) => return Ok(PropFileOutcome::SkippedWritable { mode }),
    };
    let mut reader = BufReader::new(file);
    let filter = filter.filter(|s| !s.is_empty());

    // Read raw bytes per line instead of `lines()`: a single non-UTF-8 byte
    // anywhere in the file (even in a comment) would otherwise abort the
    // whole load with an `InvalidData` I/O error.
    let mut raw_line = Vec::new();
    let mut line_count = 0usize;
    loop {
        // Lazy context: this runs per line — the closure only allocates on
        // the error path.
        let (read, truncated) = read_bounded_line(&mut reader, &mut raw_line)
            .with_context_location(|| {
                format!("Failed to read line {} of {filename:?}", line_count + 1)
            })?;
        if read == 0 {
            break;
        }
        line_count += 1;
        if truncated {
            warn!(
                "Line {line_count} of {filename:?}: skipping line longer than {MAX_LINE_LEN} bytes"
            );
            continue;
        }

        let Ok(line) = std::str::from_utf8(&raw_line) else {
            warn!("Line {line_count} of {filename:?}: skipping non-UTF-8 line");
            continue;
        };
        let line = line.trim_matches(is_c_space);

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if filter.is_none() {
            if let Some(import_rest) = line.strip_prefix("import ") {
                // AOSP `LoadProperties` splits `import <path> [<filter>]` at
                // the first space after the path and forwards the filter to
                // the nested load. Treating the whole rest as the path made
                // every filtered import fail to open (ENOENT) and get
                // skipped. An empty filter is mapped to `None` by the
                // nested `load_properties_body`.
                let import_rest = import_rest.trim_start_matches(is_c_space);
                let (import_path, import_filter) = match import_rest.split_once(' ') {
                    Some((path, filter)) => (path, Some(filter.trim_start_matches(is_c_space))),
                    None => (import_rest, None),
                };
                // AOSP parity: resolve and load the import, but never let a
                // broken import discard the rest of this file.
                match expand_props(import_path, properties) {
                    Some(expanded) => {
                        if let Err(e) = load_properties_impl(
                            Path::new(&expanded),
                            import_filter,
                            properties,
                            depth + 1,
                            visited,
                            loads,
                            accept,
                        ) {
                            // A *global* budget exhaustion is not a broken
                            // import — swallowing it would keep walking the
                            // rest of the tree with cheap per-import
                            // failures and report overall success on a
                            // truncated load. Abort the pass loudly.
                            // Branch on the error variant itself, not on
                            // re-inspecting the counter: post-state
                            // inference would silently misclassify errors
                            // if the increment ever moved.
                            if matches!(e, Error::LimitExceeded(_)) {
                                return Err(e);
                            }
                            warn!(
                                "Line {line_count} of {filename:?}: couldn't load import {expanded:?}: {e}"
                            );
                        }
                    }
                    None => warn!(
                        "Line {line_count} of {filename:?}: couldn't expand import path {import_path:?}"
                    ),
                }
                continue;
            }
        }

        let (key, value) = match line.find('=') {
            // Value end: already trimmed with the line, like AOSP (which
            // trims only the value start after splitting at `=`).
            Some(pos) => (
                line[..pos].trim_end_matches(is_c_space),
                line[pos + 1..].trim_start_matches(is_c_space),
            ),
            None => continue,
        };

        // `=value` produces an empty key; it would silently occupy a map
        // slot no valid property name can ever address.
        if key.is_empty() {
            warn!("Line {line_count} of {filename:?}: ignoring entry with empty key");
            continue;
        }

        if let Some(filter) = filter {
            if let Some(prefix) = filter.strip_suffix('*') {
                if !key.starts_with(prefix) {
                    continue;
                }
            } else if key != filter {
                continue;
            }
        }

        if key.starts_with("ctl.") || key == "sys.powerctl" || key == RESTORECON_PROPERTY {
            error!("Line {line_count} of {filename:?}: Ignoring disallowed property {key:?} with special meaning");
            continue;
        }

        // AOSP `LoadProperties` passes the value through `ExpandProps`
        // before `CheckPermissions` and drops the entry when that fails,
        // so the checks and the map see the expanded value. A value
        // without `$` expands to itself. The value stays out of the log
        // (AOSP logs it).
        let value: Cow<'_, str> = if value.contains('$') {
            match expand_props(value, properties) {
                Some(expanded) => Cow::Owned(expanded),
                None => {
                    error!(
                        "Line {line_count} of {filename:?}: dropping property {key:?}: couldn't expand its value"
                    );
                    continue;
                }
            }
        } else {
            Cow::Borrowed(value)
        };

        // AOSP `LoadProperties` drops a line whose key fails
        // `IsLegalPropertyName` (first step of `CheckPermissions`); such a
        // key could be stored but never `set` again.
        if let Err(e) = crate::wire::validate_property_name(key) {
            warn!("Line {line_count} of {filename:?}: ignoring illegal property name: {e}");
            continue;
        }

        // The rest of AOSP `CheckPermissions` (the SELinux and type
        // checks), as far as the caller supplies it. Before the map is
        // touched: a refused entry leaves an earlier value of the name in
        // place, like AOSP.
        if !accept(key, &value) {
            warn!(
                "Line {line_count} of {filename:?}: dropping property {key:?}: refused by the caller's check"
            );
            continue;
        }

        // Values stay out of the log, like every other log site that sees
        // one (they may carry tokens or device identifiers). Warn only when
        // the value actually changes, like AOSP `LoadProperties`: a
        // re-applied duplicate `import` is not an override.
        match properties.get_mut(key) {
            Some(old) if *old != *value => {
                *old = value.into_owned();
                warn!("Line {line_count} of {filename:?}: Overriding previous property {key:?}");
            }
            Some(_) => {}
            None => {
                properties.insert(key.to_string(), value.into_owned());
            }
        }
    }

    Ok(PropFileOutcome::Loaded)
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "android"))]
    use super::*;

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_load_properties_from_file() {
        let tmp = TempDir::new("rsprops_fixture_test");
        let file = crate::test_support::copy_android_fixture("system_build.prop", &tmp.0);
        let mut properties = HashMap::new();
        load(&file, &mut properties);
        assert_eq!(
            properties.get("persist.sys.usb.config"),
            Some(&"adb".to_string())
        );
    }

    /// An unfiltered load that must read `path` itself.
    #[cfg(not(target_os = "android"))]
    #[track_caller]
    fn load(path: &Path, properties: &mut HashMap<String, String>) {
        assert_eq!(
            load_properties_from_file(path, None, properties).unwrap(),
            PropFileOutcome::Loaded,
            "{path:?}"
        );
    }

    /// Removes the directory when dropped, so a failing assert doesn't
    /// leave temp litter behind.
    #[cfg(not(target_os = "android"))]
    struct TempDir(PathBuf);

    #[cfg(not(target_os = "android"))]
    impl TempDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("{label}_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    #[cfg(not(target_os = "android"))]
    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// `File::create` with mode 0644: under a umask such as 002 a plain
    /// `File::create` leaves the group-write bit, and the loader skips such
    /// files (`open_prop_file`).
    #[cfg(not(target_os = "android"))]
    fn create_prop(path: &Path) -> std::io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(path)
    }

    /// AOSP `LoadProperties` trims with C `isspace` (bionic: ASCII space
    /// and `\t`..`\r`, false for every byte >= 0x80), at the line ends, the
    /// key end and the value start. AOSP's `tmp > key` loop bound never
    /// trims the key's first byte, but that byte is never a space after
    /// the line-start trim, so `trim_end_matches` on the key gives the same
    /// result; a line of only whitespace is skipped by both.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_trim_is_c_isspace_only() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_trim_test");
        let path = tmp.0.join("trim.prop");
        {
            let mut f = create_prop(&path).unwrap();
            writeln!(f, "ro.nbsp.value=foo\u{a0}").unwrap();
            writeln!(f, "ro.lead.value=\u{3000}bar").unwrap();
            writeln!(f, "\u{3000}ro.lead.key=1").unwrap();
            writeln!(f, "ro.vt.value=\x0bbaz\x0b").unwrap();
            writeln!(f, " \t ro.ws.key \x0c= \r qux \x0b\r").unwrap();
        }

        let mut properties = HashMap::new();
        load(&path, &mut properties);
        assert_eq!(
            properties.get("ro.nbsp.value").map(String::as_str),
            Some("foo\u{a0}")
        );
        assert_eq!(
            properties.get("ro.lead.value").map(String::as_str),
            Some("\u{3000}bar")
        );
        // U+3000 stays part of the key, which then fails
        // `validate_property_name`: the line is skipped.
        assert!(!properties.contains_key("ro.lead.key"));
        assert!(!properties.contains_key("\u{3000}ro.lead.key"));
        assert_eq!(
            properties.get("ro.vt.value").map(String::as_str),
            Some("baz")
        );
        assert_eq!(properties.get("ro.ws.key").map(String::as_str), Some("qux"));
        assert_eq!(properties.len(), 4, "{properties:?}");
    }

    /// AOSP init opens prop files with `O_NOFOLLOW` (`ReadFile`): a
    /// symlink as the last component fails the open — an error for the
    /// root file, a skipped import (rest of the file still loaded).
    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_symlinked_prop_file_is_refused() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_symlink_test");
        let dir = &tmp.0;
        let target = dir.join("target.prop");
        writeln!(create_prop(&target).unwrap(), "from.target=1").unwrap();
        let link = dir.join("link.prop");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut properties = HashMap::new();
        let err = load_properties_from_file(&link, None, &mut properties)
            .expect_err("symlinked root file must not be opened");
        assert!(format!("{err:?}").contains("Failed to open"), "{err:?}");
        assert!(properties.is_empty());

        let root = dir.join("root.prop");
        writeln!(
            create_prop(&root).unwrap(),
            "import {}\nafter.import=2",
            link.display()
        )
        .unwrap();
        load(&root, &mut properties);
        assert_eq!(properties.get("from.target"), None);
        assert_eq!(
            properties.get("after.import").map(String::as_str),
            Some("2")
        );
    }

    /// AOSP init skips a prop file with `S_IWGRP` or `S_IWOTH` set
    /// ("Skipping insecure file"). The root file is skipped with
    /// `Ok(SkippedWritable)`, which carries its permission bits; an import
    /// is skipped and the importing file still loads and reports `Loaded`.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_group_or_world_writable_prop_file_is_skipped() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("rsprops_writable_test");
        let dir = &tmp.0;
        let set_mode = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };

        let root = dir.join("root.prop");
        let group_w = dir.join("group_w.prop");
        let other_w = dir.join("other_w.prop");
        writeln!(create_prop(&group_w).unwrap(), "from.group_w=1").unwrap();
        writeln!(create_prop(&other_w).unwrap(), "from.other_w=1").unwrap();
        writeln!(
            create_prop(&root).unwrap(),
            "import {}\nimport {}\nin.root=1",
            group_w.display(),
            other_w.display()
        )
        .unwrap();
        set_mode(&group_w, 0o664);
        set_mode(&other_w, 0o646);

        let mut properties = HashMap::new();
        load(&root, &mut properties);
        assert_eq!(
            properties.keys().map(String::as_str).collect::<Vec<_>>(),
            ["in.root"]
        );

        // A skipped root leaves what the map already holds.
        set_mode(&root, 0o620);
        let before = properties.clone();
        assert_eq!(
            load_properties_from_file(&root, None, &mut properties)
                .expect("an insecure root file is skipped, not an error"),
            PropFileOutcome::SkippedWritable { mode: 0o620 }
        );
        assert_eq!(properties, before);

        // The filter does not change the rule.
        assert_eq!(
            load_properties_from_file(&root, Some("in.*"), &mut properties).unwrap(),
            PropFileOutcome::SkippedWritable { mode: 0o620 }
        );
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_import_recursion_and_expansion() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_import_test");
        let dir = &tmp.0;

        let imported = dir.join("imported.prop");
        writeln!(create_prop(&imported).unwrap(), "from.import=1").unwrap();

        let root = dir.join("root.prop");
        {
            let mut f = create_prop(&root).unwrap();
            writeln!(f, "ro.base={}", dir.display()).unwrap();
            writeln!(f, "import ${{ro.base}}/imported.prop").unwrap();
            // A missing import must not abort the rest of the file.
            writeln!(f, "import {}/missing.prop", dir.display()).unwrap();
            writeln!(f, "after.import=2").unwrap();
        }

        let mut properties = HashMap::new();
        load(&root, &mut properties);
        assert_eq!(properties.get("from.import"), Some(&"1".to_string()));
        assert_eq!(properties.get("after.import"), Some(&"2".to_string()));
    }

    /// AOSP's `LoadProperties` has no import dedup: a file imported twice
    /// legitimately re-applies with last-wins semantics. An earlier
    /// load-once visited set silently changed final values for real-world
    /// override patterns — this pins the re-apply behavior.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_duplicate_import_reapplies_last_wins() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_lastwins_test");
        let dir = &tmp.0;

        let overlay = dir.join("overlay.prop");
        writeln!(create_prop(&overlay).unwrap(), "key=from_overlay").unwrap();

        let root = dir.join("root.prop");
        {
            let mut f = create_prop(&root).unwrap();
            writeln!(f, "import {}", overlay.display()).unwrap();
            writeln!(f, "key=local").unwrap();
            // The second import must re-apply the overlay (last wins), not
            // be skipped as "already loaded".
            writeln!(f, "import {}", overlay.display()).unwrap();
        }

        let mut properties = HashMap::new();
        load(&root, &mut properties);
        assert_eq!(properties.get("key"), Some(&"from_overlay".to_string()));
    }

    /// `accept` is where AOSP `LoadProperties` calls `CheckPermissions`:
    /// it sees the trimmed name and value of every entry the loader's own
    /// checks let through (imported files included), and an entry it
    /// refuses neither enters the map nor replaces an earlier value.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_accept_is_called_per_entry_before_the_map_changes() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_accept_test");
        let dir = &tmp.0;

        let imported = dir.join("imported.prop");
        writeln!(
            create_prop(&imported).unwrap(),
            "kept.then.refused=bad\nrefused.then.kept=2\nimported.refused=bad"
        )
        .unwrap();

        let root = dir.join("root.prop");
        {
            let mut f = create_prop(&root).unwrap();
            writeln!(f, "kept.then.refused=1").unwrap();
            writeln!(f, "refused.then.kept=bad").unwrap();
            writeln!(f, "  spaced.key \t=  spaced value \t").unwrap();
            // Dropped by the loader before `accept`.
            writeln!(f, "ctl.start=svc").unwrap();
            writeln!(f, ".illegal=1").unwrap();
            writeln!(f, "import {}", imported.display()).unwrap();
        }

        let mut seen = Vec::new();
        let mut properties = HashMap::new();
        let outcome = load_properties_from_file_with(&root, None, &mut properties, |key, value| {
            seen.push((key.to_string(), value.to_string()));
            value != "bad"
        })
        .unwrap();
        assert_eq!(outcome, PropFileOutcome::Loaded);

        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            seen,
            [
                pair("kept.then.refused", "1"),
                pair("refused.then.kept", "bad"),
                pair("spaced.key", "spaced value"),
                pair("kept.then.refused", "bad"),
                pair("refused.then.kept", "2"),
                pair("imported.refused", "bad"),
            ]
        );
        let mut loaded: Vec<_> = properties.into_iter().collect();
        loaded.sort();
        assert_eq!(
            loaded,
            [
                pair("kept.then.refused", "1"),
                pair("refused.then.kept", "2"),
                pair("spaced.key", "spaced value"),
            ]
        );

        // A filter is applied before `accept`.
        let mut seen = Vec::new();
        let mut properties = HashMap::new();
        let _ =
            load_properties_from_file_with(&root, Some("spaced.*"), &mut properties, |key, _| {
                seen.push(key.to_string());
                true
            })
            .unwrap();
        assert_eq!(seen, ["spaced.key"]);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_expand_props_aosp_parity() {
        let mut props = HashMap::new();
        props.insert("a".to_string(), "va".to_string());
        props.insert("empty".to_string(), String::new());
        // `$$` escapes a literal dollar.
        assert_eq!(expand_props("x$$y", &props).as_deref(), Some("x$y"));
        assert_eq!(expand_props("/${a}/f", &props).as_deref(), Some("/va/f"));
        // `${name:-default}` falls back on missing AND empty values.
        assert_eq!(expand_props("${miss:-d}", &props).as_deref(), Some("d"));
        assert_eq!(expand_props("${empty:-d}", &props).as_deref(), Some("d"));
        // Missing/empty without a default is an error.
        assert_eq!(expand_props("${miss}", &props), None);
        assert_eq!(expand_props("${empty}", &props), None);
        // AOSP requires `{` after `$`.
        assert_eq!(expand_props("a$b", &props), None);
        // Unterminated reference.
        assert_eq!(expand_props("${a", &props), None);
        // A trailing lone `$` is dropped, expansion succeeds.
        assert_eq!(expand_props("/a/b$", &props).as_deref(), Some("/a/b"));
        // Zero-length name is an error even with a default.
        assert_eq!(expand_props("${:-d}", &props), None);
        assert_eq!(expand_props("${}", &props), None);
        // An empty default counts as no default.
        assert_eq!(expand_props("/v/${miss:-}/b", &props), None);
        assert_eq!(expand_props("/v/${empty:-}/b", &props), None);
    }

    /// AOSP `LoadProperties` expands a value (`ExpandProps`) before
    /// `CheckPermissions` and drops the entry when that fails. The cases
    /// are the `/data/local.prop` lines loaded on an API 37 emulator
    /// (android-17, userdebug), with `preset.type` in the map standing in
    /// for a property init finds in the property area. The one difference
    /// is `same_file`: init dropped it ("property 'debug.rsp.plain' doesn't
    /// exist"), because it looks the name up in the property area, where
    /// the file's entries are not stored yet.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_values_are_expanded_before_the_checks() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_value_expansion_test");
        let root = tmp.0.join("root.prop");
        writeln!(
            create_prop(&root).unwrap(),
            "debug.rsp.plain=abc\n\
             debug.rsp.expand=${{preset.type}}-x\n\
             debug.rsp.same_file=${{debug.rsp.plain}}\n\
             debug.rsp.default=${{debug.rsp.nonexistent:-dflt}}\n\
             debug.rsp.empty_default=${{debug.rsp.nonexistent:-}}\n\
             debug.rsp.missing=${{debug.rsp.nonexistent}}\n\
             debug.rsp.dollar=a$$b\n\
             debug.rsp.bare=a$b\n\
             debug.rsp.trailing=abc$\n\
             debug.rsp.unterminated=${{preset.type\n\
             debug.rsp.zero_name=${{:-d}}\n\
             debug.rsp.kept=first\n\
             debug.rsp.kept=${{debug.rsp.nonexistent}}\n\
             debug.rsp.refused=${{preset.type}}\n\
             debug.rsp.last=end"
        )
        .unwrap();

        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        let mut seen = Vec::new();
        let mut properties = HashMap::from([pair("preset.type", "userdebug")]);
        let outcome = load_properties_from_file_with(&root, None, &mut properties, |key, value| {
            seen.push((key.to_string(), value.to_string()));
            key != "debug.rsp.refused"
        })
        .unwrap();
        assert_eq!(outcome, PropFileOutcome::Loaded);

        // `accept` sees the expanded value, and no entry whose value could
        // not be expanded.
        let expanded = [
            pair("debug.rsp.plain", "abc"),
            pair("debug.rsp.expand", "userdebug-x"),
            pair("debug.rsp.same_file", "abc"),
            pair("debug.rsp.default", "dflt"),
            pair("debug.rsp.dollar", "a$b"),
            pair("debug.rsp.trailing", "abc"),
            pair("debug.rsp.kept", "first"),
            pair("debug.rsp.refused", "userdebug"),
            pair("debug.rsp.last", "end"),
        ];
        assert_eq!(seen, expanded);

        let mut loaded: Vec<_> = properties.into_iter().collect();
        loaded.sort();
        let mut expected: Vec<_> = expanded
            .into_iter()
            .filter(|(key, _)| key != "debug.rsp.refused")
            .chain([pair("preset.type", "userdebug")])
            .collect();
        expected.sort();
        assert_eq!(loaded, expected);
    }

    /// AOSP `LoadProperties`: `import <path> <filter>` loads only the keys
    /// the filter selects from `<path>`.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_import_with_filter() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_import_filter_test");
        let dir = &tmp.0;

        let vendor = dir.join("vendor.prop");
        writeln!(
            create_prop(&vendor).unwrap(),
            "ro.vendor.a=1\nro.vendor.b=2\nro.other=3\nexact.key=4"
        )
        .unwrap();

        let root = dir.join("root.prop");
        {
            let mut f = create_prop(&root).unwrap();
            writeln!(f, "import {} ro.vendor.*", vendor.display()).unwrap();
            writeln!(f, "import {}   exact.key", vendor.display()).unwrap();
        }

        let mut properties = HashMap::new();
        load(&root, &mut properties);
        assert_eq!(properties.get("ro.vendor.a"), Some(&"1".to_string()));
        assert_eq!(properties.get("ro.vendor.b"), Some(&"2".to_string()));
        assert_eq!(properties.get("exact.key"), Some(&"4".to_string()));
        assert_eq!(properties.get("ro.other"), None);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_read_bounded_line_truncates_giant_line() {
        let mut data = vec![b'x'; MAX_LINE_LEN + 100];
        data.push(b'\n');
        data.extend_from_slice(b"next=1\n");
        let mut reader = BufReader::new(std::io::Cursor::new(data));
        let mut buf = Vec::new();

        let (read, truncated) = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert!(truncated);
        assert_eq!(buf.len(), MAX_LINE_LEN);
        assert_eq!(read, MAX_LINE_LEN + 101); // whole line consumed, incl. newline

        // The next line must start cleanly after the drained newline.
        let (_, truncated) = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert!(!truncated);
        assert_eq!(&buf[..], b"next=1\n");

        let (read, _) = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert_eq!(read, 0); // EOF
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_import_cycle_is_cut() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_cycle_test");
        let dir = &tmp.0;

        let a = dir.join("a.prop");
        let b = dir.join("b.prop");
        writeln!(create_prop(&a).unwrap(), "import {}", b.display()).unwrap();
        writeln!(create_prop(&b).unwrap(), "import {}\nkey=v", a.display()).unwrap();

        // The cycle is cut by the visited set at first re-entry (logged +
        // skipped), and the rest of each file still loads.
        let mut properties = HashMap::new();
        load(&a, &mut properties);
        assert_eq!(properties.get("key"), Some(&"v".to_string()));
    }

    /// A filtered import of a file still on the import stack is applied,
    /// like AOSP: a filtered load follows no imports, so it is not a cycle.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_filtered_reimport_of_ancestor_is_applied() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_filtered_reimport_test");
        let dir = &tmp.0;

        let a = dir.join("a.prop");
        let b = dir.join("b.prop");
        writeln!(create_prop(&a).unwrap(), "k=1\nimport {}", b.display()).unwrap();
        writeln!(create_prop(&b).unwrap(), "k=2\nimport {} k", a.display()).unwrap();

        let mut properties = HashMap::new();
        load(&a, &mut properties);
        assert_eq!(properties.get("k"), Some(&"1".to_string()));
    }
}
