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
//!
//! Exit code 0 is the success signal, like AOSP setprop (which prints
//! nothing on success). Errors go to stderr with exit code 1.

use clap::Parser;
use rsproperties::PropertyConfig;
use std::io::Write as _;

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

    // Initialize rsproperties only if custom directories are specified
    // (PropertyConfig is #[non_exhaustive] — build it through the builder).
    if args.properties_dir.is_some() || args.socket_dir.is_some() {
        let mut builder = PropertyConfig::builder();
        if let Some(dir) = args.properties_dir {
            builder = builder.properties_dir(dir);
        }
        if let Some(dir) = args.socket_dir {
            builder = builder.socket_dir(dir);
        }
        rsproperties::init(builder.build());
    }

    // Validate property name and value
    if let Err(msg) = validate_property(&args.property_name) {
        // `let _ =` on every output write: a closed stderr must not turn
        // into a panic (exit code 101) that hides the real exit code.
        let _ = writeln!(std::io::stderr(), "Error: {msg}");
        std::process::exit(1);
    }

    // Set the property. Nothing is printed on success.
    if let Err(e) = rsproperties::set(&args.property_name, &args.property_value) {
        let _ = writeln!(
            std::io::stderr(),
            "Failed to set property '{}': {}",
            args.property_name,
            e
        );
        std::process::exit(1);
    }
}

/// Name checks only; length limits are checked by `rsproperties::set`,
/// whose V2 caps differ by target (Android uses init's limit).
fn validate_property(name: &str) -> Result<(), String> {
    // Basic validation similar to Android's setprop
    if name.is_empty() {
        return Err("Property name cannot be empty".to_string());
    }

    // Same name rules as the library and AOSP `IsLegalPropertyName`
    // (ASCII alphanumerics plus `_ . - @ :`).
    rsproperties::wire::validate_property_name(name).map_err(|e| e.to_string())?;

    // Warn about read-only properties
    if name.starts_with("ro.") {
        let _ = writeln!(
            std::io::stderr(),
            "Warning: Property '{name}' is read-only and may not be settable"
        );
    }

    Ok(())
}
