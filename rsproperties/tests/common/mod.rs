#[cfg(not(target_os = "android"))]
use std::fs::{create_dir_all, remove_dir_all};
#[cfg(not(target_os = "android"))]
use std::sync::Once;
#[cfg(not(target_os = "android"))]
use std::{path::PathBuf, str::FromStr};

#[cfg(not(target_os = "android"))]
use rsproperties::PropertyConfig;

#[cfg(not(target_os = "android"))]
pub static TEST_PROPERTIES_DIR: &str = "__properties__";

#[cfg(not(target_os = "android"))]
static INIT: Once = Once::new();

#[allow(dead_code)]
pub fn init_test() {
    #[cfg(not(target_os = "android"))]
    INIT.call_once(|| {
        let properties_dir = PathBuf::from_str(TEST_PROPERTIES_DIR)
            .expect("Failed to parse properties directory path");
        let socket_dir = properties_dir.join("sockets");

        remove_dir_all(&socket_dir).unwrap_or_default();
        create_dir_all(&socket_dir).expect("Failed to create socket directory");

        rsproperties::init(PropertyConfig::with_both_dirs(properties_dir, socket_dir));
    });
}
