// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `init` and the global reader, in a process of their own (one test: the
//! directories are set once per process).
//!
//! - `init` sets each directory once, accepts the directory it already
//!   holds and refuses a different one; a refused call sets neither
//!   directory.
//! - A global reader that cannot open the store is not cached: once a
//!   writer has created the store, the next access opens it.

#![cfg(all(feature = "builder", not(target_os = "android")))]

use std::path::{Path, PathBuf};

use rsproperties::{
    build_trie, AreaWriterLock, Error, PropertyConfig, PropertyInfoEntry, SystemProperties,
};

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writer on `dir` with one context, holding `test.x = 1`.
fn writer(dir: &Path) -> SystemProperties {
    std::fs::create_dir_all(dir).unwrap();
    let contexts = dir.join("property_contexts");
    std::fs::write(&contexts, "test. u:object_r:test_prop:s0 prefix string\n").unwrap();
    let (entries, errors) = PropertyInfoEntry::parse_from_file(&contexts, false).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let trie = build_trie(&entries, "u:object_r:default_prop:s0", "string").unwrap();

    let lock = AreaWriterLock::acquire(dir).unwrap();
    lock.publish_property_info(&trie).unwrap();
    let mut props = SystemProperties::new_area_locked(lock).unwrap();
    props.add("test.x", "1").unwrap();
    props
}

#[test]
fn init_is_set_once_and_the_reader_retries() {
    let dir = Dir(std::env::temp_dir().join(format!("rsprops_global_init_{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    let socket = dir.0.join("sockets");

    // A refused call sets neither directory: the socket dir conflicts, so
    // the properties dir named with it stays unset.
    rsproperties::init(PropertyConfig::new().socket_dir(&socket)).unwrap();
    let err = rsproperties::init(
        PropertyConfig::new()
            .properties_dir(dir.0.join("other"))
            .socket_dir(socket.join("other")),
    )
    .unwrap_err();
    assert!(matches!(err, Error::AlreadyInitialized(_)), "{err:?}");
    rsproperties::init(
        PropertyConfig::new()
            .properties_dir(&dir.0)
            .socket_dir(&socket),
    )
    .expect("the refused call left the properties dir unset");

    // The same directories again succeed; a different one fails.
    rsproperties::init(PropertyConfig::new().properties_dir(&dir.0)).unwrap();
    rsproperties::init(PropertyConfig::new().socket_dir(&socket)).unwrap();
    let err =
        rsproperties::init(PropertyConfig::new().properties_dir(dir.0.join("other"))).unwrap_err();
    assert!(matches!(err, Error::AlreadyInitialized(_)), "{err:?}");
    assert_eq!(rsproperties::properties_dir(), dir.0);
    assert_eq!(rsproperties::socket_dir(), socket);

    // No store yet: the reader fails each time, without caching the error.
    for _ in 0..2 {
        match rsproperties::try_system_properties() {
            Err(Error::Init(_)) => {}
            Err(e) => panic!("expected Error::Init, got {e:?}"),
            Ok(_) => panic!("the store does not exist yet"),
        }
    }
    assert_eq!(
        rsproperties::get_or("test.x", String::from("unset")),
        "unset"
    );

    // The store appears: the next access opens it.
    let _writer = writer(&dir.0);
    let props = rsproperties::try_system_properties().expect("store created after the first try");
    assert_eq!(props.get("test.x").unwrap(), "1");
    assert_eq!(rsproperties::get::<String>("test.x").unwrap(), "1");
}
