# rsproperties

A high-performance Rust crate for reading and writing Android system properties on both Linux and Android platforms.

## Overview

`rsproperties` provides a pure Rust implementation for interacting with Android's property system. It allows you to read, write, and monitor system properties without relying on external libraries or shell commands.

## Features

- 🚀 **High Performance**: Direct memory-mapped access to property areas
- 🎯 **Cross Platform**: Works on both Linux and Android
- 📊 **Property Monitoring**: Wait for property changes with callbacks
- 🛠️ **Serialization**: Export and import property contexts
- 🏗️ **Builder Support**: Create custom property contexts (feature gated)

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
rsproperties = "0.1.0"

# Enable builder features for property context creation
rsproperties = { version = "0.1.0", features = ["builder"] }
```

## Usage

### Basic Property Operations

```rust
use rsproperties;

// Initialize the property system (required on Android)
#[cfg(target_os = "android")]
rsproperties::init(None); // Uses default path "/dev/__properties__"

// Get a property value with default fallback
let sdk_version = rsproperties::get_with_default("ro.build.version.sdk", "0");
println!("Android SDK Version: {}", sdk_version);

// Get a property value (returns Option)
if let Some(brand) = rsproperties::get("ro.product.brand") {
    println!("Device Brand: {}", brand);
}

// Set a property (requires appropriate permissions)
rsproperties::set("debug.test.property", "test_value").unwrap();
```

### Socket Directory Configuration

For property setting operations, you can configure the socket directory globally:

```rust
use rsproperties::set_socket_dir;

// Configure custom socket directory (can only be called once)
if set_socket_dir("/custom/socket/dir") {
    println!("Socket directory configured successfully");
} else {
    println!("Socket directory was already configured");
}

// Now all property set operations will use the custom directory
rsproperties::set("test.property", "test.value").unwrap();
```

You can also use environment variables:

```bash
# Set socket directory via environment variable
export PROPERTY_SERVICE_SOCKET_DIR="/tmp/test_socket"

# Set protocol version (defaults to V2)
export PROPERTY_SERVICE_VERSION="2"

# Override specific socket paths
export PROPERTY_SERVICE_SOCKET="/custom/path/property_service"
export PROPERTY_SERVICE_FOR_SYSTEM_SOCKET="/custom/path/property_service_for_system"
```

Priority order for socket directory configuration:
1. `set_socket_dir()` function call
2. `PROPERTY_SERVICE_SOCKET_DIR` environment variable
3. Default directory: `/dev/socket`

### Property Monitoring

```rust
use std::time::Duration;

// Wait for a property to change
let callback = |name: &str, value: &str| {
    println!("Property {} changed to: {}", name, value);
};

rsproperties::wait_for_property("sys.boot_completed", Some(Duration::from_secs(30)), callback);
```

### Advanced Usage

```rust
// List all properties
let all_props = rsproperties::list_all_properties();
for (key, value) in all_props {
    println!("{} = {}", key, value);
}

// Check if property exists
if rsproperties::exists("ro.debuggable") {
    println!("Device is debuggable");
}
```

## Supported Property Types

- **System Properties**: `ro.*` (read-only system properties)
- **Build Properties**: `ro.build.*` (build information)
- **Debug Properties**: `debug.*` (debugging flags)
- **Custom Properties**: User-defined properties
- **Vendor Properties**: `vendor.*` (vendor-specific properties)

## Architecture

The crate implements Android's property system architecture:

- **Property Areas**: Memory-mapped regions containing property data
- **Property Info**: Metadata about property permissions and types
- **Context Nodes**: Security contexts for property access
- **Trie Structure**: Efficient prefix-based property lookup

## Performance

- Direct memory access without system calls for reads
- Zero-copy property value retrieval
- Efficient trie-based property lookup
- Minimal memory overhead

## Platform Support

- ✅ **Android**: Full native support
- ✅ **Linux**: Emulation mode for development and testing
- ❌ **Windows/macOS**: Not supported (Android-specific functionality)

## Building

```bash
# Standard build
cargo build

# Build with builder features
cargo build --features builder

# Run tests (requires Android property files in tests/android/)
cargo test
```

## Contributing

Contributions are welcome! Please ensure:

1. Code follows Rust best practices
2. Tests pass on both Android and Linux
3. Documentation is updated for new features
4. Security implications are considered for property access

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Security Notice

This crate provides low-level access to Android's property system. Ensure proper permissions are set when writing properties, as incorrect usage may affect system stability.
