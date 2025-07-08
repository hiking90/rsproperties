// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `setprop` - Android-compatible property setter
//!
//! This example mimics Android's `setprop` command functionality.
//! It can set system properties via the property service.
//!
//! Usage:
//!   setprop <property_name> <property_value>
//!   setprop --properties-dir <dir> --socket-dir <dir> <property_name> <property_value>
//!
//! Examples:
//!   setprop debug.test.prop test_value         # Set a debug property
//!   setprop persist.sys.usb.config adb         # Set USB configuration
//!   setprop --socket-dir ./socket debug.prop value  # Use custom socket directory

use clap::Parser;
use rsproperties::PropertyConfig;

#[derive(Parser, Debug)]
#[command(name = "setprop")]
#[command(about = "Android-compatible property setter")]
#[command(
    long_about = "This tool mimics Android's setprop command functionality.\nIt sets system properties via the property service socket connection."
)]
struct Args {
    /// Property name to set
    #[arg(help = "Name of the property to set")]
    property_name: String,

    /// Property value to set
    #[arg(help = "Value to set for the property")]
    property_value: String,

    /// Custom properties directory
    #[arg(long, help = "Custom properties directory")]
    properties_dir: Option<std::path::PathBuf>,

    /// Custom socket directory
    #[arg(long, help = "Custom socket directory")]
    socket_dir: Option<std::path::PathBuf>,
}

fn main() {
    // Initialize logging (optional)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Parse command line arguments
    let args = Args::parse();

    // Create configuration
    let config = PropertyConfig {
        properties_dir: args.properties_dir,
        socket_dir: args.socket_dir,
    };

    // Initialize rsproperties only if custom directories are specified
    if config.properties_dir.is_some() || config.socket_dir.is_some() {
        rsproperties::init(config);
    }

    // Validate property name and value
    if let Err(msg) = validate_property(&args.property_name, &args.property_value) {
        eprintln!("Error: {msg}");
        std::process::exit(1);
    }

    // Set the property
    match rsproperties::set(&args.property_name, &args.property_value) {
        Ok(_) => {
            println!(
                "Property '{}' set to '{}'",
                args.property_name, args.property_value
            );
        }
        Err(e) => {
            eprintln!("Failed to set property '{}': {}", args.property_name, e);
            std::process::exit(1);
        }
    }
}

fn validate_property(name: &str, value: &str) -> Result<(), String> {
    // Basic validation similar to Android's setprop
    if name.is_empty() {
        return Err("Property name cannot be empty".to_string());
    }

    if name.len() > 256 {
        return Err("Property name too long (max 256 characters)".to_string());
    }

    if value.len() > 8192 {
        return Err("Property value too long (max 8192 characters)".to_string());
    }

    // Check for invalid characters in property name
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(
            "Invalid characters in property name (only alphanumeric, ., _, - allowed)".to_string(),
        );
    }

    // Warn about read-only properties
    if name.starts_with("ro.") {
        eprintln!("Warning: Property '{name}' is read-only and may not be settable");
    }

    Ok(())
}
