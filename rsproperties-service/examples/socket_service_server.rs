// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::{thread, time::Duration, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}};
use std::path::PathBuf;
use std::fs;
use std::sync::mpsc;

#[cfg(all(feature = "builder", target_os = "linux"))]
use rsproperties::{
    PropertySocketService,
    create_property_channel,
    PropertyConfig,
    SystemProperties,
    PropertyInfoEntry,
    build_trie,
    load_properties_from_file,
};
use rsproperties::errors::Result;

/// Example properties directory for the server
const EXAMPLE_PROPERTIES_DIR: &str = "example_properties";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("🚀 Starting Advanced Property Socket Service Example");
    println!("This example demonstrates a complete property system setup");

    #[cfg(all(feature = "builder", target_os = "linux"))]
    {
        run_property_server()
    }
    #[cfg(not(all(feature = "builder", target_os = "linux")))]
    {
        println!("❌ This example requires the 'builder' feature and Linux OS");
        println!("   Build with: cargo run --features builder --example socket_service_server");
        Ok(())
    }
}

#[cfg(all(feature = "builder", target_os = "linux"))]
fn run_property_server() -> Result<()> {
    // Setup signal handling for graceful shutdown
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    ctrlc::set_handler(move || {
        println!("\n🛑 Received shutdown signal, stopping services...");
        shutdown_clone.store(true, Ordering::SeqCst);
    }).expect("Error setting Ctrl-C handler");

    // Setup example properties directory
    setup_example_properties_dir()?;

    // Initialize rsproperties with custom directories
    let config = PropertyConfig::with_both_dirs(
        properties_dir(),
        socket_dir()
    );
    rsproperties::init(Some(config));
    println!("✓ Initialized property system with custom directories");

    // Create SystemProperties area with test data
    let system_properties = create_system_properties_area()?;
    println!("✓ Created SystemProperties area with test data");

    // Create property channel for message passing
    let (sender, receiver) = create_property_channel();
    println!("✓ Created property channel for message passing");

    // Create socket service
    let socket_service = PropertySocketService::new(sender.clone(), sender.clone())?;
    println!("✓ Created socket service");
    println!("   property_service socket: {}/property_service", socket_dir().display());
    println!("   property_service_for_system socket: {}/property_service_for_system", socket_dir().display());

    // Start socket service in background thread
    let shutdown_socket = shutdown.clone();
    let service_handle = thread::spawn(move || {
        println!("🔧 Socket service starting...");
        loop {
            if shutdown_socket.load(Ordering::SeqCst) {
                println!("🔧 Socket service shutting down...");
                break;
            }
            // Run with timeout to check shutdown signal
            if let Err(e) = socket_service.run() {
                eprintln!("❌ Socket service error: {}", e);
                break;
            }
        }
    });

    // Start property message receiver thread
    let system_properties_clone = system_properties.clone();
    let shutdown_receiver = shutdown.clone();
    let receiver_handle = thread::spawn(move || {
        println!("📨 Property message receiver started, waiting for messages...");

        loop {
            if shutdown_receiver.load(Ordering::SeqCst) {
                println!("📨 Property message receiver shutting down...");
                break;
            }

            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(property_msg) => {
                    handle_property_message(&system_properties_clone, property_msg);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Continue checking shutdown signal
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    println!("📨 Property channel disconnected");
                    break;
                }
            }
        }
        println!("📨 Property message receiver stopped");
    });

    // Give services time to start
    thread::sleep(Duration::from_millis(500));

    print_service_info();

    // Wait for shutdown signal or thread completion
    while !shutdown.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));

        // Check if threads are still alive
        if receiver_handle.is_finished() || service_handle.is_finished() {
            println!("⚠️  One of the service threads has stopped");
            break;
        }
    }

    // Graceful shutdown
    println!("🛑 Initiating graceful shutdown...");

    // Wait for threads to finish
    if let Err(e) = receiver_handle.join() {
        eprintln!("❌ Receiver thread panic: {:?}", e);
    }

    if let Err(e) = service_handle.join() {
        eprintln!("❌ Service thread panic: {:?}", e);
    }

    // Cleanup
    cleanup_example_properties_dir();
    println!("✓ Shutdown complete");

    Ok(())
}

/// Handle property message with better error handling and logging
#[cfg(all(feature = "builder", target_os = "linux"))]
fn handle_property_message(
    system_properties: &Arc<Mutex<SystemProperties>>,
    property_msg: rsproperties::PropertyMessage
) {
    println!("📦 Received property: '{}' = '{}'", property_msg.key, property_msg.value);

    // Validate property name and value
    if let Err(e) = validate_property(&property_msg.key, &property_msg.value) {
        eprintln!("❌ Invalid property: {}", e);
        return;
    }

    // Update SystemProperties with the received property
    match system_properties.lock() {
        Ok(mut sys_props) => {
            match sys_props.find(&property_msg.key) {
                Ok(Some(prop_ref)) => {
                    if let Err(e) = sys_props.update(&prop_ref, &property_msg.value) {
                        eprintln!("❌ Failed to update property '{}': {}", property_msg.key, e);
                    } else {
                        println!("✅ Updated property: '{}' = '{}'", property_msg.key, property_msg.value);
                    }
                },
                Ok(None) => {
                    if let Err(e) = sys_props.add(&property_msg.key, &property_msg.value) {
                        eprintln!("❌ Failed to add property '{}': {}", property_msg.key, e);
                    } else {
                        println!("✅ Added new property: '{}' = '{}'", property_msg.key, property_msg.value);
                    }
                },
                Err(e) => {
                    eprintln!("❌ Failed to find property '{}': {}", property_msg.key, e);
                }
            }
        },
        Err(e) => {
            eprintln!("❌ Failed to lock system properties: {}", e);
        }
    }

    // Handle special properties
    handle_special_properties(&property_msg.key, &property_msg.value);
}

/// Validate property name and value
#[cfg(all(feature = "builder", target_os = "linux"))]
fn validate_property(key: &str, value: &str) -> Result<()> {
    use rsproperties::errors::Error;

    if key.is_empty() {
        return Err(Error::new_file_validation("Property name cannot be empty".to_string()).into());
    }

    if key.len() > 256 {
        return Err(Error::new_file_validation("Property name too long".to_string()).into());
    }

    if value.len() > 8192 {
        return Err(Error::new_file_validation("Property value too long".to_string()).into());
    }

    // Check for invalid characters in property name
    if !key.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return Err(Error::new_file_validation("Invalid characters in property name".to_string()).into());
    }

    Ok(())
}

/// Print service information
#[cfg(all(feature = "builder", target_os = "linux"))]
fn print_service_info() {
    println!("\n🎯 Property Socket Service is now running!");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("📁 Properties directory: {}", properties_dir().display());
    println!("🔌 Socket directory: {}", socket_dir().display());
    println!("\n🧪 Test the service with the client:");
    println!("   # Using example server configuration (recommended):");
    println!("   cargo run --features builder --example socket_service_client -- --with-example-server test.property test.value");
    println!("   cargo run --features builder --example socket_service_client -- --with-example-server debug.example hello");
    println!("   cargo run --features builder --example socket_service_client -- --with-example-server sys.powerctl shutdown");
    println!("   cargo run --features builder --example socket_service_client -- --with-example-server persist.sys.usb.config adb");
    println!();
    println!("   # Using system default configuration:");
    println!("   cargo run --features builder --example socket_service_client test.property test.value");
    println!("\n📂 Example server uses custom directory structure:");
    println!("   - Properties: {}", properties_dir().display());
    println!("   - Sockets: {}", socket_dir().display());
    println!("   Use --with-example-server flag to connect to this server instance.");
    println!("\n⏹️  Press Ctrl+C to stop the service");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}

#[cfg(all(feature = "builder", target_os = "linux"))]
fn handle_special_properties(key: &str, value: &str) {
    match key {
        "sys.powerctl" => {
            println!("🔋 System power control command received: {}", value);
            match value {
                "shutdown" => println!("   → Would initiate system shutdown"),
                "reboot" => println!("   → Would initiate system reboot"),
                _ => println!("   → Unknown power control command"),
            }
        }
        key if key.starts_with("debug.") => {
            println!("🐛 Debug property received: {} = {}", key, value);
            println!("   → Debug logging or feature toggle");
        }
        key if key.starts_with("ro.") => {
            println!("🔒 Read-only property received: {} = {}", key, value);
            println!("   → This is typically a system configuration");
        }
        key if key.starts_with("persist.") => {
            println!("💾 Persistent property received: {} = {}", key, value);
            println!("   → This value will persist across reboots");
        }
        _ => {
            println!("📝 General property received: {} = {}", key, value);
        }
    }
}

#[cfg(all(feature = "builder", target_os = "linux"))]
fn setup_example_properties_dir() -> Result<()> {
    // Clean and create directories
    let _ = fs::remove_dir_all(EXAMPLE_PROPERTIES_DIR);
    fs::create_dir_all(properties_dir())?;
    fs::create_dir_all(socket_dir())?;

    println!("✓ Created example directories");
    Ok(())
}

#[cfg(all(feature = "builder", target_os = "linux"))]
fn cleanup_example_properties_dir() {
    let _ = fs::remove_dir_all(EXAMPLE_PROPERTIES_DIR);
    println!("✓ Cleaned up example directories");
}

#[cfg(all(feature = "builder", target_os = "linux"))]
fn properties_dir() -> PathBuf {
    PathBuf::from(EXAMPLE_PROPERTIES_DIR).join("properties")
}

#[cfg(all(feature = "builder", target_os = "linux"))]
fn socket_dir() -> PathBuf {
    PathBuf::from(EXAMPLE_PROPERTIES_DIR).join("socket")
}

#[cfg(all(feature = "builder", target_os = "linux"))]
fn create_system_properties_area() -> Result<Arc<Mutex<SystemProperties>>> {
    use std::fs::File;
    use std::io::Write;
    use std::collections::HashMap;

    // Load property context files (using test files as examples)
    let property_contexts_files = vec![
        "tests/android/plat_property_contexts",
        "tests/android/system_ext_property_contexts",
        "tests/android/vendor_property_contexts",
    ];

    let mut property_infos = Vec::new();
    for file in property_contexts_files {
        if let Ok((mut property_info, errors)) = PropertyInfoEntry::parse_from_file(std::path::Path::new(file), false) {
            if !errors.is_empty() {
                eprintln!("⚠️  Warnings parsing {}: {:?}", file, errors);
            }
            property_infos.append(&mut property_info);
        } else {
            eprintln!("⚠️  Could not load {}, using minimal property contexts", file);
        }
    }

    // If no property context files were found, create minimal ones
    if property_infos.is_empty() {
        eprintln!("⚠️  No property context files found, creating minimal setup");
    }

    // Build trie and write property_info file
    let data: Vec<u8> = build_trie(&property_infos, "u:object_r:build_prop:s0", "string")?;

    let props_dir = properties_dir();
    File::create(props_dir.join("property_info"))?.write_all(&data)?;

    // Load some example properties
    let mut properties = HashMap::new();

    // Try to load from test build.prop files, fallback to example properties
    let build_prop_files = vec![
        "tests/android/system_build.prop",
        "tests/android/vendor_build.prop",
    ];

    let mut loaded_any = false;
    for file in build_prop_files {
        if load_properties_from_file(std::path::Path::new(file), None, "u:r:init:s0", &mut properties).is_ok() {
            loaded_any = true;
        }
    }

    // If no build.prop files were loaded, add some example properties
    if !loaded_any {
        properties.insert("ro.build.version.release".to_string(), "14".to_string());
        properties.insert("ro.build.version.sdk".to_string(), "34".to_string());
        properties.insert("ro.product.model".to_string(), "Example Device".to_string());
        properties.insert("persist.sys.usb.config".to_string(), "adb".to_string());
        println!("✓ Using example properties (test files not found)");
    } else {
        println!("✓ Loaded properties from test files");
    }

    // Create SystemProperties area
    let mut system_properties = SystemProperties::new_area(&props_dir)?;

    // Add properties to the system
    for (key, value) in properties.iter() {
        match system_properties.find(key.as_str())? {
            Some(prop_ref) => {
                if let Err(e) = system_properties.update(&prop_ref, value.as_str()) {
                    eprintln!("⚠️  Failed to update property '{}': {}", key, e);
                }
            },
            None => {
                if let Err(e) = system_properties.add(key.as_str(), value.as_str()) {
                    eprintln!("⚠️  Failed to add property '{}': {}", key, e);
                }
            }
        }
    }

    println!("✓ Loaded {} properties into system", properties.len());

    Ok(Arc::new(Mutex::new(system_properties)))
}
