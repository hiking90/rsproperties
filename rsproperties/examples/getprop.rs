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
use std::io::Write as _;

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

    // A config without directories changes nothing, so `init` runs either
    // way; it fails only when the directory was already set differently.
    let mut config = PropertyConfig::new();
    if let Some(props_dir) = args.properties_dir {
        config = config.properties_dir(props_dir);
    }
    if let Err(e) = rsproperties::init(config) {
        let _ = writeln!(std::io::stderr(), "Error: {e}");
        std::process::exit(1);
    }

    // Execute the appropriate command
    match args.property_name {
        Some(name) => {
            // Get specific property
            let value = if let Some(default) = args.default_value {
                rsproperties::get_or(&name, default)
            } else {
                // Missing or empty: print an empty line (toybox getprop).
                rsproperties::get::<String>(&name).unwrap_or_default()
            };
            // `let _ =`: a closed stdout must not turn into a panic
            // (exit code 101).
            let _ = writeln!(std::io::stdout(), "{value}");
        }
        None => {
            // List all readable properties, sorted by name like toybox
            // getprop: `foreach` reports them in trie order.
            let props = match rsproperties::system_properties() {
                Ok(props) => props,
                Err(e) => {
                    eprintln!("getprop: {e}");
                    std::process::exit(1);
                }
            };
            let mut all = std::collections::BTreeMap::new();
            let walk = props.foreach(|index| {
                match props.read_index_with(index, |name, value, _serial| {
                    (name.to_owned(), value.to_owned())
                }) {
                    Ok((name, value)) => {
                        all.insert(name, value);
                    }
                    // e.g. a non-UTF-8 value: skip it, keep listing.
                    Err(e) => log::warn!("skipping a property: {e}"),
                }
            });
            let mut out = std::io::stdout().lock();
            for (name, value) in &all {
                let _ = writeln!(out, "[{name}]: [{value}]");
            }
            if let Err(e) = walk {
                eprintln!("getprop: {e}");
                std::process::exit(1);
            }
        }
    }
}
