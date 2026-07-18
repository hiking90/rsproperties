// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Lookup hot-path benchmarks.
//!
//! Deliberately restricted to APIs that exist unchanged since v0.5.0
//! (`build_trie`, `PropertyInfoEntry::parse_from_file`,
//! `SystemProperties::new_area`, `add`, `get_with_result`, `find`) so the
//! same file can be dropped onto any tag for an A/B comparison:
//!
//! ```sh
//! git worktree add /tmp/rsprops-0.5.0 v0.5.0
//! cp -r rsproperties/benches /tmp/rsprops-0.5.0/rsproperties/
//! # add the [[bench]] + criterion dev-dep stanza to its Cargo.toml, then
//! cargo bench --features builder -p rsproperties
//! ```

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion};
use rsproperties::{build_trie, PropertyInfoEntry, SystemProperties};

fn build_property_info(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();

    let contexts_path = dir.join("property_contexts");
    let mut f = File::create(&contexts_path).unwrap();
    // A handful of contexts so the trie has real branching, plus prefixes
    // of varying depth so lookups walk more than one node.
    writeln!(f, "ro. u:object_r:ro_prop:s0 prefix string").unwrap();
    writeln!(f, "ro.build. u:object_r:build_prop:s0 prefix string").unwrap();
    writeln!(f, "persist. u:object_r:persist_prop:s0 prefix string").unwrap();
    writeln!(f, "sys. u:object_r:system_prop:s0 prefix string").unwrap();
    writeln!(f, "bench. u:object_r:bench_prop:s0 prefix string").unwrap();
    drop(f);

    let (entries, errors) = PropertyInfoEntry::parse_from_file(&contexts_path, false).unwrap();
    assert!(errors.is_empty(), "parse errors: {errors:?}");

    let data = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();
    File::create(dir.join("property_info"))
        .unwrap()
        .write_all(&data)
        .unwrap();
}

fn setup() -> (SystemProperties, PathBuf) {
    let dir = std::env::temp_dir().join(format!("rsprops_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    build_property_info(&dir);

    let mut props = SystemProperties::new_area(&dir).expect("new_area");
    for i in 0..100 {
        props
            .add(&format!("bench.prop.number.{i}"), &format!("value_{i}"))
            .unwrap();
    }
    props
        .add(
            "ro.build.fingerprint.bench",
            "generic/aosp/bench:14/UQ1A/1:user/release-keys",
        )
        .unwrap();
    props.add("sys.short", "1").unwrap();
    (props, dir)
}

fn bench_lookups(c: &mut Criterion) {
    let (props, dir) = setup();

    // Deep name, ~9-byte value: the common get() shape.
    c.bench_function("get_hit_deep", |b| {
        b.iter(|| props.get_with_result(std::hint::black_box("bench.prop.number.42")))
    });

    // Short name, 1-byte value: dominated by per-lookup overhead.
    c.bench_function("get_hit_short", |b| {
        b.iter(|| props.get_with_result(std::hint::black_box("sys.short")))
    });

    // Long-ish value (~44 bytes): weights the value NUL-scan/copy.
    c.bench_function("get_hit_long_value", |b| {
        b.iter(|| props.get_with_result(std::hint::black_box("ro.build.fingerprint.bench")))
    });

    // Known context, absent property: trie walk + area miss.
    c.bench_function("get_miss", |b| {
        b.iter(|| props.get_with_result(std::hint::black_box("bench.prop.number.9999")))
    });

    // Index lookup only (no value read).
    c.bench_function("find_index", |b| {
        b.iter(|| props.find(std::hint::black_box("bench.prop.number.42")))
    });

    drop(props);
    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(benches, bench_lookups);
criterion_main!(benches);
