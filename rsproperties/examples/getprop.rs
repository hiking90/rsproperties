// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `getprop` - Android-compatible property getter
//!
//! This example mimics Android's `getprop` command functionality.
//! It can get system properties with optional default values.
//!
//! Usage:
//!   getprop [property_name] [default_value]
//!   getprop --properties-dir <dir> [property_name] [default_value]
//!
//! Examples:
//!   getprop                                    # List all properties
//!   getprop ro.build.version.sdk               # Get specific property
//!   getprop ro.build.version.sdk 0             # Get with default value
//!   getprop --properties-dir ./props ro.test   # Use custom properties directory

use clap::Parser;
use rsproperties::PropertyConfig;

#[derive(Parser, Debug)]
#[command(name = "getprop")]
#[command(about = "Android-compatible property getter")]
#[command(
    long_about = "This tool mimics Android's getprop command functionality.\nIt can retrieve system properties with optional default values."
)]
struct Args {
    /// Property name to retrieve
    #[arg(help = "Name of the property to retrieve")]
    property_name: Option<String>,

    /// Default value if property is not found
    #[arg(help = "Default value to return if property is not found")]
    default_value: Option<String>,

    /// Custom properties directory
    #[arg(long, help = "Custom properties directory")]
    properties_dir: Option<std::path::PathBuf>,
}

fn main() {
    // Initialize logging (optional)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Parse command line arguments
    let args = Args::parse();

    // Create configuration
    let config = if let Some(props_dir) = args.properties_dir {
        PropertyConfig::with_properties_dir(props_dir)
    } else {
        PropertyConfig::default()
    };

    // Initialize rsproperties only if custom directories are specified
    if config.properties_dir.is_some() || config.socket_dir.is_some() {
        rsproperties::init(config);
    }

    // Execute the appropriate command
    match args.property_name {
        Some(name) => {
            // Get specific property
            let value = if let Some(default) = args.default_value {
                rsproperties::get_with_default(&name, &default)
            } else {
                let result = rsproperties::get(&name);
                if result.is_empty() {
                    // Property not found, print nothing (Android getprop behavior)
                    return;
                }
                result
            };
            println!("{}", value);
        }
        None => {
            // List all properties (simplified implementation)
            println!("Listing all properties is not yet implemented.");
            println!("Use 'getprop <property_name>' to get a specific property.");
            std::process::exit(1);
        }
    }
}
