use std::{path::PathBuf, str::FromStr};
use std::fs::{remove_dir_all, create_dir_all};
use std::sync::Once;

use rsproperties::PropertyConfig;

pub static TEST_PROPERTIES_DIR: &str = "__properties__";

static INIT: Once = Once::new();

pub fn init_test() {
    INIT.call_once(|| {
        let properties_dir = PathBuf::from_str(TEST_PROPERTIES_DIR)
            .expect("Failed to parse properties directory path");
        let socket_dir = properties_dir.join("sockets");

        remove_dir_all(&socket_dir).unwrap_or_default();
        create_dir_all(&socket_dir).expect("Failed to create socket directory");

        rsproperties::init(
            PropertyConfig::with_both_dirs(properties_dir, socket_dir)
        );
    });
}
