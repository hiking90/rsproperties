// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use log::{error, warn};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::errors::*;

const RESTORECON_PROPERTY: &str = "selinux.restorecon_recursive";

/// Bound on `import` nesting. Real prop files import at most one or two
/// levels deep; the cap exists so pathological nesting fails loudly
/// instead of recursing until the stack overflows. Cycles are cut by the
/// import *stack* (a file cannot import itself, directly or transitively);
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

/// Placeholder for future per-property SELinux permission enforcement.
/// Currently a no-op; see TODO in caller.
fn check_permissions(_key: &str, _value: &str, _context: &str) {
    // TODO: Implement proper permission checking
}

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
/// `import <path>` lines are loaded recursively (with `${property}`
/// expansion against the entries collected so far), and an import that
/// cannot be resolved or read — including one nested deeper than
/// `MAX_IMPORT_DEPTH` — is logged and skipped rather than aborting the
/// rest of the file. Import *cycles* are cut by a canonicalized-path
/// recursion stack; a file legitimately imported twice (shared base, or
/// re-imported after overrides) is re-applied each time, exactly as AOSP's
/// last-wins semantics require — `MAX_TOTAL_LOADS` bounds pathological
/// re-import fan-out. Non-UTF-8 lines are skipped with a warning — prop
/// files on real devices are byte streams and may carry stray non-UTF-8
/// comment bytes.
///
/// On error (I/O failure reading the root file, or more than
/// `MAX_TOTAL_LOADS` file loads across the whole pass), entries parsed
/// before the failure remain in `properties` — the map is an accumulator,
/// not a transaction.
pub fn load_properties_from_file(
    filename: &Path,
    filter: Option<&str>,
    context: &str,
    properties: &mut HashMap<String, String>,
) -> Result<()> {
    let mut visited = HashSet::new();
    let mut loads = 0u32;
    load_properties_impl(
        filename,
        filter,
        context,
        properties,
        0,
        &mut visited,
        &mut loads,
    )
}

/// Expands property references in an import path against the entries
/// loaded so far (AOSP expands against the live property store; during a
/// bulk load the accumulator map is the equivalent source).
///
/// AOSP `init/util.cpp::ExpandProps` parity:
/// - `$$` is an escape for a literal `$`;
/// - `${name}` substitutes the property's value;
/// - `${name:-default}` falls back to `default` when the property is
///   missing **or empty** (AOSP's `GetProperty(name, "")` cannot tell the
///   two apart);
/// - a `$` not followed by `{` or `$`, an unterminated `${`, or a
///   missing/empty property without a default is an error → `None`
///   (AOSP returns `Error()` for each; the caller logs and skips the
///   import).
fn expand_import_path(raw: &str, properties: &HashMap<String, String>) -> Option<String> {
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
        let body = after.strip_prefix('{')?;
        let end = body.find('}')?;
        let reference = &body[..end];
        let (name, default) = match reference.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (reference, None),
        };
        match properties.get(name).map(String::as_str) {
            Some(value) if !value.is_empty() => out.push_str(value),
            // Missing and empty collapse to the default, like AOSP.
            _ => out.push_str(default?),
        }
        rest = &body[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

// Recursion state (depth/visited/loads) travels with every call; a params
// struct would be re-destructured at each of the three call sites for no
// clarity gain.
#[allow(clippy::too_many_arguments)]
fn load_properties_impl(
    filename: &Path,
    filter: Option<&str>,
    context: &str,
    properties: &mut HashMap<String, String>,
    depth: u8,
    visited: &mut HashSet<PathBuf>,
    loads: &mut u32,
) -> Result<()> {
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

    // Canonicalize so `a.prop`, `./a.prop`, and a symlink to it compare
    // equal on the import stack; fall back to the raw path when
    // canonicalization fails (e.g. the file doesn't exist — `File::open`
    // below reports that).
    //
    // `visited` is a recursion *stack*, not a load-once set: the entry is
    // removed again before returning. Only genuine cycles (a file
    // importing itself, directly or transitively) are cut — a file
    // imported twice from different places is re-applied each time, like
    // AOSP init's `LoadProperties` (which has no dedup at all), so
    // re-imports keep their last-wins effect on earlier overrides.
    let canonical = std::fs::canonicalize(filename).unwrap_or_else(|_| filename.to_path_buf());
    if !visited.insert(canonical.clone()) {
        warn!("{filename:?} is already being loaded (import cycle) — skipping");
        return Ok(());
    }
    // From here on every exit must pop the stack entry; wrap the body so
    // one removal covers all paths.
    let result = load_properties_body(
        filename, &canonical, filter, context, properties, depth, visited, loads,
    );
    visited.remove(&canonical);
    result
}

// See `load_properties_impl` for the arguments-count rationale.
#[allow(clippy::too_many_arguments)]
fn load_properties_body(
    filename: &Path,
    canonical: &Path,
    filter: Option<&str>,
    context: &str,
    properties: &mut HashMap<String, String>,
    depth: u8,
    visited: &mut HashSet<PathBuf>,
    loads: &mut u32,
) -> Result<()> {
    // Open the *canonicalized* path — the cycle-detection key above. This
    // narrows (best-effort, does not close) the symlink-swap window between
    // canonicalize and open: a component of the canonical path could still
    // be replaced by a symlink in between, since `File::open` follows
    // symlinks. Fully closing it would need openat2(RESOLVE_NO_SYMLINKS)
    // or an open-then-fstat comparison; the depth/loads caps already keep
    // any confusion bounded, so the narrowing is judged sufficient.
    // Error messages still name the caller's path: on canonicalization
    // failure `canonical` falls back to `filename`, so `File::open` reports
    // the name the user wrote.
    let file =
        File::open(canonical).context_with_location(format!("Failed to open {filename:?}"))?;
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
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if filter.is_none() {
            if let Some(import_path) = line.strip_prefix("import ") {
                // AOSP parity: resolve and load the import, but never let a
                // broken import discard the rest of this file.
                match expand_import_path(import_path.trim(), properties) {
                    Some(expanded) => {
                        if let Err(e) = load_properties_impl(
                            Path::new(&expanded),
                            None,
                            context,
                            properties,
                            depth + 1,
                            visited,
                            loads,
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
            Some(pos) => (line[..pos].trim_end(), line[pos + 1..].trim()),
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
            error!("Line {line_count} of {filename:?}: Ignoring disallowed property '{key}' with special meaning");
            continue;
        }

        check_permissions(key, value, context);
        if let Some(old_value) = properties.insert(key.to_string(), value.to_string()) {
            warn!(
                "Line {line_count} of {filename:?}: Overriding previous property '{key}':'{old_value}' with new value '{value}'"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "android"))]
    use super::*;

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_load_properties_from_file() {
        let mut properties = HashMap::new();
        load_properties_from_file(
            Path::new("tests/android/system_build.prop"),
            None,
            "u:r:init:s0",
            &mut properties,
        )
        .unwrap();
        assert_eq!(
            properties.get("persist.sys.usb.config"),
            Some(&"adb".to_string())
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

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_import_recursion_and_expansion() {
        use std::io::Write;
        let tmp = TempDir::new("rsprops_import_test");
        let dir = &tmp.0;

        let imported = dir.join("imported.prop");
        writeln!(File::create(&imported).unwrap(), "from.import=1").unwrap();

        let root = dir.join("root.prop");
        {
            let mut f = File::create(&root).unwrap();
            writeln!(f, "ro.base={}", dir.display()).unwrap();
            writeln!(f, "import ${{ro.base}}/imported.prop").unwrap();
            // A missing import must not abort the rest of the file.
            writeln!(f, "import {}/missing.prop", dir.display()).unwrap();
            writeln!(f, "after.import=2").unwrap();
        }

        let mut properties = HashMap::new();
        load_properties_from_file(&root, None, "u:r:init:s0", &mut properties).unwrap();
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
        writeln!(File::create(&overlay).unwrap(), "key=from_overlay").unwrap();

        let root = dir.join("root.prop");
        {
            let mut f = File::create(&root).unwrap();
            writeln!(f, "import {}", overlay.display()).unwrap();
            writeln!(f, "key=local").unwrap();
            // The second import must re-apply the overlay (last wins), not
            // be skipped as "already loaded".
            writeln!(f, "import {}", overlay.display()).unwrap();
        }

        let mut properties = HashMap::new();
        load_properties_from_file(&root, None, "u:r:init:s0", &mut properties).unwrap();
        assert_eq!(properties.get("key"), Some(&"from_overlay".to_string()));
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_expand_props_aosp_parity() {
        let mut props = HashMap::new();
        props.insert("a".to_string(), "va".to_string());
        props.insert("empty".to_string(), String::new());
        // `$$` escapes a literal dollar.
        assert_eq!(expand_import_path("x$$y", &props).as_deref(), Some("x$y"));
        assert_eq!(
            expand_import_path("/${a}/f", &props).as_deref(),
            Some("/va/f")
        );
        // `${name:-default}` falls back on missing AND empty values.
        assert_eq!(
            expand_import_path("${miss:-d}", &props).as_deref(),
            Some("d")
        );
        assert_eq!(
            expand_import_path("${empty:-d}", &props).as_deref(),
            Some("d")
        );
        // Missing/empty without a default is an error.
        assert_eq!(expand_import_path("${miss}", &props), None);
        assert_eq!(expand_import_path("${empty}", &props), None);
        // AOSP requires `{` after `$`.
        assert_eq!(expand_import_path("a$b", &props), None);
        // Unterminated reference.
        assert_eq!(expand_import_path("${a", &props), None);
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
        writeln!(File::create(&a).unwrap(), "import {}", b.display()).unwrap();
        writeln!(File::create(&b).unwrap(), "import {}\nkey=v", a.display()).unwrap();

        // The cycle is cut by the visited set at first re-entry (logged +
        // skipped), and the rest of each file still loads.
        let mut properties = HashMap::new();
        load_properties_from_file(&a, None, "u:r:init:s0", &mut properties).unwrap();
        assert_eq!(properties.get("key"), Some(&"v".to_string()));
    }
}
