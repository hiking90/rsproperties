// Common test utilities and setup functions

#![allow(dead_code)]

use std::path::{PathBuf, Path};
use std::fs::{self, File};
use std::io::Write;
use std::sync::{
    Mutex,
    Once,
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(all(feature = "builder", target_os = "linux"))]
use std::sync::mpsc::Sender;
use std::fs::{create_dir, remove_dir_all};
use std::collections::HashMap;

use rsproperties::{
    PropertyConfig,
    SystemProperties,
    PropertyInfoEntry,
    build_trie,
    load_properties_from_file,
    dirname,
};

#[cfg(all(feature = "builder", target_os = "linux"))]
use rsproperties::{PropertySocketService, create_property_channel, PropertyMessage};

/// Common properties directory for all tests
pub const TEST_PROPERTIES_DIR: &str = "__properties__";

static INIT: Once = Once::new();

// Global state for the test property service
#[cfg(all(feature = "builder", target_os = "linux"))]
static GLOBAL_SERVICE_STATE: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

// Global channel sender for property messages
#[cfg(all(feature = "builder", target_os = "linux"))]
static GLOBAL_PROPERTY_SENDER: Mutex<Option<Sender<PropertyMessage>>> = Mutex::new(None);

/// Initialize the system properties for tests using the common properties directory
/// This ensures all tests use the same __properties__ folder and starts a global TestPropertyService
pub fn init_test() {
    INIT.call_once(|| {
        fs::remove_dir_all(TEST_PROPERTIES_DIR).unwrap_or_default();
        fs::create_dir(TEST_PROPERTIES_DIR).unwrap_or_else(|e| {
            panic!("Failed to create test properties directory '{}': {}", TEST_PROPERTIES_DIR, e);
        });

        let config = PropertyConfig::with_both_dirs(
            properties_dir(),
            socket_dir());
        rsproperties::init(Some(config));
        println!("✓ Test properties initialized with directory: {}", TEST_PROPERTIES_DIR);

        let _guard = system_properties_area();

        // Start global test property service in background thread
        #[cfg(all(feature = "builder", target_os = "linux"))]
        start_global_test_property_service();
    });
}

#[cfg(all(feature = "builder", target_os = "linux"))]
fn start_global_test_property_service() {
    use std::thread;
    use std::time::Duration;

    let mut state_guard = GLOBAL_SERVICE_STATE.lock().unwrap();
    if state_guard.is_some() {
        return; // Already started
    }

    let is_running = Arc::new(AtomicBool::new(false));
    *state_guard = Some(is_running.clone());
    drop(state_guard); // Release the lock before spawning thread

    // Create property channel
    let (sender, receiver) = create_property_channel();

    // Store the sender globally for potential future use
    {
        let mut sender_guard = GLOBAL_PROPERTY_SENDER.lock().unwrap();
        *sender_guard = Some(sender.clone());
    }

    // Get system properties area
    let system_properties = system_properties_area();

    // Start receiver thread to handle property messages
    let receiver_system_properties = system_properties.clone();
    thread::spawn(move || {
        println!("Starting property message receiver...");
        while let Ok(property_msg) = receiver.recv() {
            println!("Received property message: '{}' = '{}'", property_msg.key, property_msg.value);

            // Update SystemProperties with the received property
            match receiver_system_properties.lock() {
                Ok(mut sys_props) => {
                    match sys_props.find(&property_msg.key) {
                        Ok(Some(prop_ref)) => {
                            if let Err(e) = sys_props.update(&prop_ref, &property_msg.value) {
                                eprintln!("Failed to update property '{}': {}", property_msg.key, e);
                            } else {
                                println!("✓ Updated property: '{}' = '{}'", property_msg.key, property_msg.value);
                            }
                        },
                        Ok(None) => {
                            if let Err(e) = sys_props.add(&property_msg.key, &property_msg.value) {
                                eprintln!("Failed to add property '{}': {}", property_msg.key, e);
                            } else {
                                println!("✓ Added new property: '{}' = '{}'", property_msg.key, property_msg.value);
                            }
                        },
                        Err(e) => {
                            eprintln!("Failed to find property '{}': {}", property_msg.key, e);
                        }
                    }
                },
                Err(e) => {
                    eprintln!("Failed to lock system properties: {}", e);
                }
            }
        }
        println!("Property message receiver stopped");
    });

    // Start socket service thread
    thread::spawn(move || {
        println!("Starting global test property service...");

        // Create and run the socket service with both senders
        match PropertySocketService::new(sender.clone(), sender) {
            Ok(service) => {
                is_running.store(true, Ordering::SeqCst);

                if let Err(e) = service.run() {
                    eprintln!("Global property socket service error: {}", e);
                }

                is_running.store(false, Ordering::SeqCst);
                println!("Global test property service stopped");
            },
            Err(e) => {
                eprintln!("Failed to create global test property service: {}", e);
            }
        }
    });

    // Give the service time to start
    thread::sleep(Duration::from_millis(200));
    println!("✓ Global test property service initialization complete");
}

/// Check if the global test property service is running
#[cfg(all(feature = "builder", target_os = "linux"))]
pub fn is_global_service_running() -> bool {
    let state_guard = GLOBAL_SERVICE_STATE.lock().unwrap();
    if let Some(ref is_running) = *state_guard {
        is_running.load(Ordering::SeqCst)
    } else {
        false
    }
}

#[cfg(not(all(feature = "builder", target_os = "linux")))]
pub fn is_global_service_running() -> bool {
    false
}

pub fn properties_dir() -> PathBuf {
    PathBuf::from(TEST_PROPERTIES_DIR).join("properties")
}

pub fn socket_dir() -> PathBuf {
    PathBuf::from(TEST_PROPERTIES_DIR).join("socket")
}

#[cfg(all(feature = "builder", target_os = "linux"))]
pub fn system_properties_area() -> Arc<Mutex<SystemProperties>> {
    use std::sync::OnceLock;

    static SYSTEM_PROPERTIES: OnceLock<Arc<Mutex<SystemProperties>>> = OnceLock::new();

    SYSTEM_PROPERTIES.get_or_init(|| {
        Arc::new(Mutex::new(build_property_dir()))
    }).clone()
}


#[cfg(all(feature = "builder", target_os = "linux"))]
fn build_property_dir() -> SystemProperties {
    let property_contexts_files = vec![
        "tests/android/plat_property_contexts",
        "tests/android/system_ext_property_contexts",
        "tests/android/vendor_property_contexts",
    ];

    let mut property_infos = Vec::new();
    for file in property_contexts_files {
        let (mut property_info, errors) = PropertyInfoEntry::parse_from_file(Path::new(file), false).unwrap();
        if !errors.is_empty() {
            log::error!("{:?}", errors);
        }
        property_infos.append(&mut property_info);
    }

    let data: Vec<u8> = build_trie(&property_infos, "u:object_r:build_prop:s0", "string").unwrap();

    let dir = dirname();
    remove_dir_all(dir).unwrap_or_default();
    create_dir(dir).unwrap_or_default();
    File::create(dir.join("property_info")).unwrap().write_all(&data).unwrap();

    let properties = load_properties();

    let dir = dirname();
    let mut system_properties = SystemProperties::new_area(dir)
        .unwrap_or_else(|e| panic!("Cannot create system properties: {}. Please check if {dir:?} exists.", e));
    for (key, value) in properties.iter() {
        match system_properties.find(key.as_str()).unwrap() {
            Some(prop_ref) => {
                system_properties.update(&prop_ref, value.as_str()).unwrap();
            },
            None => {
                system_properties.add(key.as_str(), value.as_str()).unwrap();
            }
        }
    }

    system_properties
}

#[cfg(all(feature = "builder", target_os = "linux"))]
fn load_properties() -> HashMap<String, String> {
    let build_prop_files = vec![
        "tests/android/product_build.prop",
        "tests/android/system_build.prop",
        "tests/android/system_dlkm_build.prop",
        "tests/android/system_ext_build.prop",
        "tests/android/vendor_build.prop",
        "tests/android/vendor_dlkm_build.prop",
        "tests/android/vendor_odm_build.prop",
        "tests/android/vendor_odm_dlkm_build.prop",
    ];

    let mut properties = HashMap::new();
    for file in build_prop_files {
        load_properties_from_file(Path::new(file), None, "u:r:init:s0", &mut properties).unwrap();
    }

    properties
}

/// Get the global property sender for sending property messages
#[cfg(all(feature = "builder", target_os = "linux"))]
pub fn get_global_property_sender() -> Option<Sender<PropertyMessage>> {
    let sender_guard = GLOBAL_PROPERTY_SENDER.lock().unwrap();
    sender_guard.clone()
}

#[cfg(not(all(feature = "builder", target_os = "linux")))]
pub fn get_global_property_sender() -> Option<()> {
    None
}

// /// Test property service for handling property set operations during tests
// #[cfg(all(feature = "builder", target_os = "linux"))]
// pub struct TestPropertyService {
//     service_handle: Option<std::thread::JoinHandle<()>>,
//     socket_path: String,
//     is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
//     _system_properties: Arc<Mutex<SystemProperties>>,
// }

// #[cfg(all(feature = "builder", target_os = "linux"))]
// impl TestPropertyService {
//     /// Create a new test property service
//     pub fn new() -> Self {
//         let socket_path = format!("{}/local_property_service", TEST_PROPERTIES_DIR);
//         let system_properties = system_properties_area();

//         Self {
//             service_handle: None,
//             socket_path,
//             is_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
//             _system_properties: system_properties,
//         }
//     }

//     /// Start the test property service
//     pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
//         println!("Starting test property service at: {}", self.socket_path);

//         // Ensure the socket directory exists
//         if let Some(parent) = std::path::Path::new(&self.socket_path).parent() {
//             std::fs::create_dir_all(parent)?;
//         }

//         // Create the socket service
//         let service = PropertySocketService::new()?;
//         let is_running = self.is_running.clone();

//         // Start the service in a background thread
//         let handle = std::thread::spawn(move || {
//             is_running.store(true, std::sync::atomic::Ordering::SeqCst);
//             if let Err(e) = service.run() {
//                 eprintln!("Property socket service error: {}", e);
//             }
//             is_running.store(false, std::sync::atomic::Ordering::SeqCst);
//         });

//         // Give the service time to start
//         std::thread::sleep(std::time::Duration::from_millis(100));

//         self.service_handle = Some(handle);

//         println!("✓ Test property service started successfully");
//         Ok(())
//     }

//     /// Stop the test property service
//     pub fn stop(&mut self) {
//         if let Some(handle) = self.service_handle.take() {
//             println!("Stopping test property service...");

//             // Signal the service to stop by removing the socket file
//             if std::path::Path::new(&self.socket_path).exists() {
//                 let _ = std::fs::remove_file(&self.socket_path);
//             }

//             // Wait for the thread to finish with a timeout
//             let _ = handle.join();

//             self.is_running.store(false, std::sync::atomic::Ordering::SeqCst);

//             println!("✓ Test property service stopped");
//         }
//     }

//     /// Check if the service is running
//     pub fn is_running(&self) -> bool {
//         self.is_running.load(std::sync::atomic::Ordering::SeqCst) &&
//         std::path::Path::new(&self.socket_path).exists()
//     }

//     /// Get the socket path
//     pub fn socket_path(&self) -> &str {
//         &self.socket_path
//     }
// }

// #[cfg(all(feature = "builder", target_os = "linux"))]
// impl Drop for TestPropertyService {
//     fn drop(&mut self) {
//         self.stop();
//     }
// }

