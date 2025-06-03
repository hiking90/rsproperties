# rsproperties

A pure Rust implementation of Android's property system for Linux and Android.

## Overview

- **`rsproperties`**: Core library for getting/setting Android properties
- **`rsproperties-service`**: Full property service for Linux

**Note:** This project is actively evolving. While core APIs are stable, some features may be refined in future releases.

## Features

- Direct memory-mapped property access
- Property monitoring with change notifications
- Cross-platform (Linux and Android)
- Async property service for Linux

## Installation

```toml
[dependencies]
rsproperties = "0.1.0"
rsproperties-service = "0.1.0"  # For Linux property service
```

## Usage

### Basic Operations

```rust
use rsproperties::{self, PropertyConfig};

// Initialize the property system
let config = PropertyConfig {
    properties_dir: Some("./test_properties".into()),
    socket_dir: Some("./test_socket".into()),
};
rsproperties::init(config);

// Get property values
let sdk_version = rsproperties::get_with_default("ro.build.version.sdk", "0");
println!("Android SDK Version: {}", sdk_version);

// Set properties (requires appropriate permissions)
rsproperties::set("debug.test.property", "test_value")?;
```

### Linux Property Service

```rust
use rsproperties_service;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = rsproperties::PropertyConfig {
        properties_dir: Some("./properties".into()),
        socket_dir: Some("./socket".into()),
    };

    let (_socket_service, _properties_service) = rsproperties_service::run(
        config, vec![], vec![]
    ).await?;

    tokio::signal::ctrl_c().await?;
    Ok(())
}
```

### Property Monitoring

```rust
let system_properties = rsproperties::system_properties();

// Wait for any property change
if let Some(serial) = system_properties.wait_any() {
    println!("Properties changed, serial: {}", serial);
}

// Wait for specific property
if let Ok(Some(prop_index)) = system_properties.find("sys.boot_completed") {
    if let Some(_) = system_properties.wait(Some(&prop_index), None) {
        println!("Boot completed");
    }
}
```

## Platform Support

- **Android**: Native property system access
- **Linux**: Full property service emulation

> **Note**: SELinux support is under development.

## Building

```bash
cargo build --workspace
cargo test --workspace
```

## License

Licensed under the Apache License, Version 2.0.
