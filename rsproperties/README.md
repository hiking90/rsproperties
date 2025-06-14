# rsproperties

[![Crates.io](https://img.shields.io/crates/v/rsproperties.svg)](https://crates.io/crates/rsproperties)
[![Documentation](https://docs.rs/rsproperties/badge.svg)](https://docs.rs/rsproperties)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

A pure Rust implementation of Android's property system, providing cross-platform access to Android system properties on both Linux and Android platforms.

## Features

- **Complete Android Properties Implementation**: Full Rust implementation of Android's property system - read, write, and monitor properties exactly like Android native code
- **Cross-Platform Compatibility**: Works seamlessly on both Android devices and Linux systems without modification
- **Pure Rust Solution**: No dependencies on Android's C libraries or JNI - everything implemented in safe Rust
- **Real-time Property Monitoring**: Watch for property changes in real-time, enabling reactive applications
- **High Performance**: Optimized for speed with direct memory access and zero-copy operations
- **Drop-in Replacement**: Compatible with Android's property naming conventions and value constraints
- **Linux Emulation**: Full Android property system emulation on Linux for development and testing
- **Thread-Safe Design**: Safe to use across multiple threads without external synchronization

## Quick Start

Add `rsproperties` to your `Cargo.toml`:

```toml
[dependencies]
rsproperties = "0.2"

# Optional features
[features]
builder = ["rsproperties/builder"]  # Enable property database building
```

### Basic Usage

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

### Property Monitoring

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

### Setting Properties

Setting properties requires a running property service (like `rsproperties-service`):

```rust
use rsproperties;

// Basic property setting
if let Err(e) = rsproperties::set("debug.my_app.enabled", "true") {
    eprintln!("Failed to set property: {}", e);
}

// Set application configuration
rsproperties::set("debug.my_app.log_level", "verbose")?;
rsproperties::set("debug.my_app.port", "8080")?;

// Set system properties (may require elevated permissions)
rsproperties::set("sys.my_service.ready", "1")?;

// Set persistent properties (survive reboots on Android)
rsproperties::set("persist.my_app.config", "production")?;
```

#### Property Setting Requirements

**On Android:**
- Properties are set through the property service
- Some properties require specific SELinux permissions
- `ro.*` properties are read-only and cannot be modified
- System properties may require root or system privileges

**On Linux:**
- Requires `rsproperties-service` to be running
- Properties are stored in memory-mapped files
- All properties are writable unless explicitly restricted

#### Property Setting Examples by Type

```rust
// Debug properties - usually writable by applications
rsproperties::set("debug.my_app.trace", "enabled")?;
rsproperties::set("debug.my_app.verbose", "true")?;

// Vendor properties - device-specific configuration
rsproperties::set("vendor.my_app.hw_config", "v2")?;

// Custom application properties
rsproperties::set("my.company.app.version", "1.2.3")?;
rsproperties::set("my.company.app.api_key", "abc123")?;

// System state properties
rsproperties::set("sys.my_service.status", "running")?;
rsproperties::set("sys.my_service.pid", "1234")?;
```

### Error Handling

```rust
use rsproperties::{Result, Error};

fn handle_property_operation() -> Result<()> {
    match rsproperties::set("debug.my_app.config", "value") {
        Ok(_) => println!("Property set successfully"),
        Err(e) => {
            eprintln!("Failed to set property: {}", e);
            // Error provides context and location information
        }
    }
    Ok(())
}

// Batch property operations with error handling
fn set_app_config() -> Result<()> {
    let properties = [
        ("debug.my_app.enabled", "true"),
        ("debug.my_app.log_level", "info"),
        ("debug.my_app.trace", "disabled"),
    ];

    for (key, value) in &properties {
        match rsproperties::set(key, value) {
            Ok(_) => println!("Set {}: {}", key, value),
            Err(e) => {
                eprintln!("Failed to set {}: {}", key, e);
                return Err(e);
            }
        }
    }
    Ok(())
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
- **Socket Communication**: Unix domain socket property setting
- **Memory Mapping**: Efficient memory-mapped property storage
- **Property Service**: Use with `rsproperties-service` for full daemon functionality

## API Reference

### Configuration

- `PropertyConfig` - Configuration for property system initialization
- `PropertyConfig::builder()` - Builder pattern for configuration
- `PropertyConfig::with_properties_dir()` - Create config with only properties directory
- `PropertyConfig::with_socket_dir()` - Create config with only socket directory
- `PropertyConfig::with_both_dirs()` - Create config with both directories
- `init(config)` - Initialize the property system

### Property Operations

- `get<T>(name)` - Get property value parsed to specified type (returns Err if not found)
- `get_or<T>(name, default)` - Get property with default fallback (never fails)
- `set<T>(name, value)` - Set property value (requires property service)

### System Properties

- `system_properties()` - Get global SystemProperties instance
- `properties_dir()` - Get the configured properties directory
- `SystemProperties::get_with_result(name)` - Get property with error handling
- `SystemProperties::find(name)` - Find property index by name
- `SystemProperties::wait_any()` - Wait for any property change
- `SystemProperties::wait(index, timeout)` - Wait for specific property change

### Socket Configuration

- `socket_dir()` - Get the configured socket directory for property service
- Socket directory priority: `set_socket_dir()` > `PROPERTY_SERVICE_SOCKET_DIR` env var > `/dev/socket`

### Advanced Features (with `builder` feature)

- `SystemProperties::new_area(dir)` - Create new property area
- `SystemProperties::add(name, value)` - Add new property
- `SystemProperties::update(index, value)` - Update existing property
- `SystemProperties::set(name, value)` - Set property (create or update)
- `load_properties_from_file()` - Load properties from build.prop files

## Thread Safety

All operations are thread-safe and can be used concurrently:

```rust
use std::thread;

// Multiple threads can safely access properties
let handles: Vec<_> = (0..10).map(|i| {
    thread::spawn(move || {
        let prop_name = format!("debug.thread.{}", i);
        let value: String = rsproperties::get_or(&prop_name, "default".to_string());
        println!("Thread {}: {} = {}", i, prop_name, value);
    })
}).collect();

for handle in handles {
    handle.join().unwrap();
}
```

### Building Property Databases (with `builder` feature)

```rust
#[cfg(feature = "builder")]
use rsproperties::{load_properties_from_file, SystemProperties};
use std::collections::HashMap;
use std::path::Path;

// Load properties from Android build.prop files
let mut properties = HashMap::new();
load_properties_from_file(
    Path::new("system_build.prop"),
    None,
    "u:r:init:s0",
    &mut properties
)?;

// Create a system properties area for testing or service
let mut system_properties = SystemProperties::new_area(Path::new("./test_props"))?;

// Add loaded properties to the area
for (key, value) in properties {
    system_properties.add(&key, &value)?;
}

// Now properties can be read normally
let sdk_version: i32 = rsproperties::get_or("ro.build.version.sdk", 0);
```

## Constants

The library exposes Android-compatible constants:

```rust
use rsproperties::{PROP_VALUE_MAX, PROP_DIRNAME};

// Maximum property value length (92 bytes for most properties)
assert_eq!(PROP_VALUE_MAX, 92);

// Default Android properties directory
assert_eq!(PROP_DIRNAME, "/dev/__properties__");
```

## Performance

- **Memory-mapped access**: Direct memory access for optimal performance
- **Zero-copy reads**: Efficient property value retrieval
- **Atomic operations**: Thread-safe property updates
- **Futex-based waiting**: Efficient property change notifications

## Examples

The crate includes Android-compatible command-line tools:

- **`getprop.rs`** - Android-compatible property getter with support for custom directories
- **`setprop.rs`** - Android-compatible property setter with validation and error handling

Run examples with:
```bash
# Get a property with default value
cargo run --example getprop ro.build.version.sdk 0

# Set a property
cargo run --example setprop debug.my_app.test true
```

## Related Crates

- **`rsproperties-service`** - Full async property service daemon for Linux environments

## Building

```bash
# Build the library
cargo build

# Build with all features
cargo build --all-features

# Run tests
cargo test

# Build documentation
cargo doc --open
```

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Contributing

Contributions are welcome! Please ensure:

1. All tests pass: `cargo test`
2. Code is formatted: `cargo fmt`
3. No clippy warnings: `cargo clippy --all-targets --all-features`

This implementation is based on Android's property system and maintains compatibility with Android's property semantics and behavior.
