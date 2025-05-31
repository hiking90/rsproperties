use std::env;
use rsproperties::{set_socket_dir, set};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    let args: Vec<String> = env::args().collect();

    if args.len() < 3 {
        eprintln!("Usage: {} <property_name> <property_value> [socket_dir]", args[0]);
        eprintln!("Example: {} test.property test.value", args[0]);
        eprintln!("Example: {} test.property test.value /tmp/test_socket", args[0]);
        std::process::exit(1);
    }

    let property_name = &args[1];
    let property_value = &args[2];

    // Set custom socket directory if provided
    if let Some(socket_dir) = args.get(3) {
        println!("Setting socket directory to: {}", socket_dir);
        if set_socket_dir(socket_dir) {
            println!("✓ Socket directory set successfully");
        } else {
            println!("⚠ Socket directory was already set (ignoring new value)");
        }
    }

    println!("Setting property: '{}' = '{}'", property_name, property_value);

    // Use the high-level API which will automatically use the configured socket directory
    match set(property_name, property_value) {
        Ok(_) => {
            println!("✓ Property set successfully!");
        }
        Err(e) => {
            println!("✗ Property set failed: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}
