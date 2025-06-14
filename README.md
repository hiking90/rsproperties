# rsproperties

A pure Rust implementation of Android's property system for Linux and Android.

## Overview

`rsproperties` is a comprehensive Rust library that provides Android system property functionality across platforms:

- **`rsproperties`**: Core library for getting/setting Android properties with memory-mapped access
- **`rsproperties-service`**: Full async property service implementation for Linux environments

The library implements Android's property system semantics, including property contexts, SELinux integration, and futex-based property change notifications.

**Note:** This project is actively evolving. While core APIs are stable, some features may be refined in future releases.

## Features

- **Complete Android Properties Implementation**: Full Rust implementation of Android's property system - read, write, and monitor properties exactly like Android native code
- **Cross-Platform Compatibility**: Works seamlessly on both Android devices and Linux systems without modification
- **Pure Rust Solution**: No dependencies on Android's C libraries or JNI - everything implemented in safe Rust
- **Real-time Property Monitoring**: Watch for property changes in real-time, enabling reactive applications
- **High Performance**: Optimized for speed with direct memory access and zero-copy operations
- **Drop-in Replacement**: Compatible with Android's property naming conventions and value constraints
- **Linux Emulation**: Full Android property system emulation on Linux for development and testing
- **Thread-Safe Design**: Safe to use across multiple threads without external synchronization

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
rsproperties = "0.2"
# For Linux property service daemon
rsproperties-service = "0.2"

# Optional features
[features]
builder = ["rsproperties/builder"]  # Enable property database building
```

## Quick Start

### Basic Property Operations

```rust
use rsproperties;

// Get property with default value (no initialization needed for default configuration)
let sdk_version: String = rsproperties::get_or("ro.build.version.sdk", "0".to_string());
println!("SDK Version: {}", sdk_version);

// Get property with type parsing and default fallback
let sdk_version: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
let is_debuggable: bool = rsproperties::get_or("ro.debuggable", false);

// Get property with error handling
match rsproperties::get::<String>("ro.build.version.release") {
    Ok(version) => println!("Android Version: {}", version),
    Err(e) => eprintln!("Failed to get version: {}", e),
}

// Set property (requires property service to be running)
if let Err(e) = rsproperties::set("debug.my_app.enabled", "true") {
    eprintln!("Failed to set property: {}", e);
}
```

### Custom Configuration

```rust
use rsproperties::PropertyConfig;

// Configure custom directories
let config = PropertyConfig {
    properties_dir: Some("/custom/properties".into()),
    socket_dir: Some("/custom/socket".into()),
};
rsproperties::init(config);

// Using the builder pattern
let config = PropertyConfig::builder()
    .properties_dir("/my/properties")
    .socket_dir("/my/socket")
    .build();
rsproperties::init(config);

// Convenience methods
rsproperties::init(PropertyConfig::with_properties_dir("/my/props"));
```

### Property Monitoring and Waiting

```rust
use rsproperties;

let system_properties = rsproperties::system_properties();

// Wait for any property change
std::thread::spawn(|| {
    if let Some(new_serial) = system_properties.wait_any() {
        println!("Properties changed, new serial: {}", new_serial);
    }
});

// Wait for specific property change
std::thread::spawn(|| {
    if let Ok(Some(prop_index)) = system_properties.find("sys.boot_completed") {
        println!("Waiting for boot completion...");
        if let Some(_) = system_properties.wait(Some(&prop_index), None) {
            println!("System boot completed!");
        }
    }
});

// Monitor multiple properties
let monitored_props = vec![
    "sys.boot_completed",
    "dev.bootcomplete",
    "service.bootanim.exit"
];

for prop_name in monitored_props {
    match system_properties.get_with_result(prop_name) {
        Ok(value) => println!("{}: {}", prop_name, value),
        Err(_) => println!("{}: <not set>", prop_name),
    }
}
```

### Linux Property Service

For Linux environments, you can run a full property service daemon:

```rust
use rsproperties_service;
use rsproperties::PropertyConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure the service
    let config = PropertyConfig {
        properties_dir: Some("./properties".into()),
        socket_dir: Some("./socket".into()),
    };

    // Optional: Load property contexts and build.prop files
    let property_contexts = vec![
        "plat_property_contexts".into(),
        "vendor_property_contexts".into(),
    ];

    let build_props = vec![
        "system_build.prop".into(),
        "vendor_build.prop".into(),
    ];

    // Start the property service
    let (_socket_service, _properties_service) = rsproperties_service::run(
        config,
        property_contexts,
        build_props
    ).await?;

    println!("Property service running...");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;
    println!("Property service shutting down...");

    Ok(())
}
```

### Command Line Tools

The library includes Android-compatible command line tools:

#### getprop - Get Properties
```bash
# Get specific property
./getprop ro.build.version.sdk

# Get with default value
./getprop ro.build.version.sdk 0

# Use custom properties directory
./getprop --properties-dir ./my_props ro.product.device
```

#### setprop - Set Properties
```bash
# Set a property
./setprop debug.my_app.log_level verbose

# Use custom directories
./setprop --properties-dir ./props --socket-dir ./socket debug.test true
```

## Advanced Usage

### Building Property Databases

With the `builder` feature enabled, you can create property databases:

```rust
#[cfg(feature = "builder")]
use rsproperties::{load_properties_from_file, build_trie, SystemProperties};
use std::collections::HashMap;

// Load properties from build.prop files
let mut properties = HashMap::new();
load_properties_from_file(
    &Path::new("system_build.prop"),
    None,
    "u:r:init:s0",
    &mut properties
)?;

// Build property database
let property_infos = vec![/* PropertyInfoEntry objects */];
let trie_data = build_trie(&property_infos, "u:object_r:build_prop:s0", "string")?;

// Create system properties area
let mut system_properties = SystemProperties::new_area(&properties_dir)?;

// Add properties to the area
for (key, value) in properties {
    system_properties.add(&key, &value)?;
}
```

### Error Handling

```rust
use rsproperties::{Result, Error};

fn handle_property_operation() -> Result<()> {
    match rsproperties::set("ro.test.readonly", "value") {
        Ok(_) => println!("Property set successfully"),
        Err(e) => {
            eprintln!("Failed to set property: {}", e);
            // Error type is concrete, handle specific error types if needed
        }
    }
    Ok(())
}
```

### Thread Safety

All operations are thread-safe and can be used concurrently:

```rust
use std::thread;
use std::sync::Arc;

// Multiple threads can safely access properties
let handles: Vec<_> = (0..10).map(|i| {
    thread::spawn(move || {
        let prop_name = format!("debug.thread.{}", i);
        let value = rsproperties::get_with_default(&prop_name, "default");
        println!("Thread {}: {} = {}", i, prop_name, value);
    })
}).collect();

for handle in handles {
    handle.join().unwrap();
}
```

## Platform Support

### Android
- **Native Integration**: Direct access to `/dev/__properties__`
- **Property Contexts**: Full SELinux property context support
- **Bionic Compatibility**: Compatible with Android's property implementation
- **Standard Properties**: Access to all standard Android properties

### Linux
- **Full Emulation**: Complete Android property system emulation
- **Property Service**: Async property service daemon
- **Socket Communication**: Unix domain socket property setting
- **Memory Mapping**: Efficient memory-mapped property storage

### Cross-Platform Features
- **Futex Support**: Property change notifications on Linux and Android
- **Configurable Paths**: Custom property and socket directories
- **Environment Variables**: `PROPERTY_SERVICE_SOCKET_DIR` support

## Property Naming Conventions

The library follows Android property naming conventions:

- **Read-only properties**: `ro.*` (e.g., `ro.build.version.sdk`)
- **System properties**: `sys.*` (e.g., `sys.boot_completed`)
- **Persist properties**: `persist.*` (e.g., `persist.sys.timezone`)
- **Debug properties**: `debug.*` (e.g., `debug.my_app.log_level`)
- **Vendor properties**: `vendor.*` (e.g., `vendor.audio.config`)

### Property Constraints
- **Name length**: Maximum 32 characters
- **Value length**: Maximum 92 characters (except ro.* properties)
- **Character set**: Alphanumeric, dots, underscores, and hyphens

## Performance Characteristics

- **Memory-mapped access**: Direct memory access for optimal performance
- **Zero-copy reads**: Efficient property value retrieval
- **Atomic operations**: Thread-safe property updates
- **Futex-based waiting**: Efficient property change notifications

## Building and Testing

```bash
# Build the entire workspace
cargo build --workspace

# Build with all features
cargo build --workspace --all-features

# Run tests
cargo test --workspace

# Run tests with logging
RUST_LOG=debug cargo test --workspace

# Build examples
cargo build --examples

# Run property service example
cargo run --bin property_service_example
```

### Cross-compilation for Android

```bash
# Add Android target
rustup target add aarch64-linux-android

# Build for Android
cargo build --target aarch64-linux-android --workspace
```

## Examples

See the `examples/` directory for complete working examples:

- **`getprop.rs`**: Android-compatible property getter
- **`setprop.rs`**: Android-compatible property setter
- **Property service examples**: Complete property service implementations

## Contributing

Contributions are welcome! Please ensure:

1. All tests pass: `cargo test --workspace`
2. Code is formatted: `cargo fmt --all`
3. No clippy warnings: `cargo clippy --workspace --all-targets --all-features`
4. Documentation is updated for new features

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Acknowledgments

This implementation is based on Android's property system and maintains compatibility with Android's property semantics and behavior.
