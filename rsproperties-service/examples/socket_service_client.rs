use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::thread;
use rsproperties::{set, PropertyConfig};

/// Example properties directory for the client (shared with server)
const EXAMPLE_PROPERTIES_DIR: &str = "example_properties";

/// Configuration for retry logic
const MAX_RETRY_ATTEMPTS: u32 = 3;
const RETRY_DELAY_MS: u64 = 1000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    let args: Vec<String> = env::args().collect();

    // Parse command line arguments
    let config = parse_arguments(&args)?;

    // Configure directories based on the flag
    setup_configuration(&config);

    // Attempt to set property with retry logic
    set_property_with_retry(&config)?;

    Ok(())
}

#[derive(Debug)]
struct ClientConfig {
    property_name: String,
    property_value: String,
    use_example_server: bool,
    verbose: bool,
    max_retries: u32,
}

fn parse_arguments(args: &[String]) -> Result<ClientConfig, Box<dyn std::error::Error>> {
    // Check for flags
    let use_example_server = args.contains(&"--with-example-server".to_string());
    let verbose = args.contains(&"--verbose".to_string()) || args.contains(&"-v".to_string());
    let help = args.contains(&"--help".to_string()) || args.contains(&"-h".to_string());

    if help {
        print_help(&args[0]);
        std::process::exit(0);
    }

    // Filter out flags from args for property name/value parsing
    let filtered_args: Vec<String> = args.iter()
        .filter(|arg| !matches!(arg.as_str(), "--with-example-server" | "--verbose" | "-v"))
        .cloned()
        .collect();

    if filtered_args.len() < 3 {
        print_usage_and_exit(&args[0]);
    }

    // Parse max retries if provided
    let max_retries = if let Some(pos) = args.iter().position(|arg| arg == "--max-retries") {
        if pos + 1 < args.len() {
            args[pos + 1].parse().unwrap_or(MAX_RETRY_ATTEMPTS)
        } else {
            MAX_RETRY_ATTEMPTS
        }
    } else {
        MAX_RETRY_ATTEMPTS
    };

    Ok(ClientConfig {
        property_name: filtered_args[1].clone(),
        property_value: filtered_args[2].clone(),
        use_example_server,
        verbose,
        max_retries,
    })
}

fn print_help(program_name: &str) {
    println!("Property Socket Service Client");
    println!("A client for setting properties via the socket service");
    println!();
    println!("USAGE:");
    println!("    {} [OPTIONS] <property_name> <property_value>", program_name);
    println!();
    println!("OPTIONS:");
    println!("    --with-example-server    Connect to the example socket service server");
    println!("                            (uses example_properties/ directory structure)");
    println!("    --verbose, -v           Enable verbose output");
    println!("    --max-retries <n>       Maximum number of retry attempts (default: {})", MAX_RETRY_ATTEMPTS);
    println!("    --help, -h              Print this help message");
    println!();
    println!("EXAMPLES:");
    println!("    {} test.property test.value", program_name);
    println!("    {} debug.example hello", program_name);
    println!("    {} sys.powerctl shutdown", program_name);
    println!("    {} persist.sys.usb.config adb", program_name);
    println!();
    println!("    # Connect to example server:");
    println!("    {} --with-example-server test.property test.value", program_name);
    println!("    {} --with-example-server --verbose debug.test enabled", program_name);
    println!("    {} --with-example-server --max-retries 5 important.prop value", program_name);
    println!();
    println!("NOTE:");
    println!("    When using --with-example-server, make sure to run the example server first:");
    println!("    cargo run --features builder --example socket_service_server");
}

fn print_usage_and_exit(program_name: &str) {
    eprintln!("Usage: {} [OPTIONS] <property_name> <property_value>", program_name);
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --with-example-server    Connect to the example socket service server");
    eprintln!("  --verbose, -v           Enable verbose output");
    eprintln!("  --max-retries <n>       Maximum retry attempts");
    eprintln!("  --help, -h              Show help message");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  {} test.property test.value", program_name);
    eprintln!("  {} --with-example-server debug.example hello", program_name);
    eprintln!();
    eprintln!("Use --help for detailed information.");
    std::process::exit(1);
}

fn setup_configuration(config: &ClientConfig) {
    if config.use_example_server {
        println!("🔗 Using example server configuration");
        let prop_config = PropertyConfig::with_both_dirs(
            properties_dir(),
            socket_dir()
        );
        rsproperties::init(Some(prop_config));

        if config.verbose {
            println!("📁 Properties directory: {}", properties_dir().display());
            println!("📁 Socket directory: {}", socket_dir().display());
        }

        // Check if socket directory exists
        if !socket_dir().exists() {
            println!("⚠️  Warning: Socket directory does not exist: {}", socket_dir().display());
            println!("   Make sure the example server is running.");
        }
    } else {
        println!("🔗 Using system default configuration");
        rsproperties::init(None);

        if config.verbose {
            println!("📁 Using system default socket directory");
        }
    }
}

fn set_property_with_retry(config: &ClientConfig) -> Result<(), Box<dyn std::error::Error>> {
    println!("🔌 Connecting to property socket service...");

    if config.verbose {
        println!("📊 Configuration:");
        println!("   Property: '{}' = '{}'", config.property_name, config.property_value);
        println!("   Max retries: {}", config.max_retries);
        println!("   Example server: {}", config.use_example_server);
    }

    let start_time = Instant::now();
    let mut last_error = None;

    for attempt in 1..=config.max_retries {
        if config.verbose && attempt > 1 {
            println!("🔄 Attempt {} of {}", attempt, config.max_retries);
        }

        match set(&config.property_name, &config.property_value) {
            Ok(_) => {
                let elapsed = start_time.elapsed();
                println!("✓ Property set successfully!");

                if config.verbose {
                    println!("⏱️  Operation completed in {:?}", elapsed);
                    println!("🎯 Attempts used: {}", attempt);
                }

                if config.use_example_server {
                    println!("  The example server should have processed this property change.");
                    println!("  Check the server console output for confirmation.");
                } else {
                    println!("  The system property service has processed this change.");
                }

                return Ok(());
            }
            Err(e) => {
                last_error = Some(e);

                if attempt < config.max_retries {
                    if config.verbose {
                        println!("⚠️  Attempt {} failed, retrying in {}ms...", attempt, RETRY_DELAY_MS);
                    } else {
                        print!("⚠️  Retrying... ");
                    }
                    thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
                } else {
                    break;
                }
            }
        }
    }

    // All attempts failed
    let elapsed = start_time.elapsed();
    println!("✗ Property set failed after {} attempts", config.max_retries);

    if let Some(error) = last_error {
        println!("💥 Last error: {}", error);
    }

    if config.verbose {
        println!("⏱️  Total time elapsed: {:?}", elapsed);
    }

    if config.use_example_server {
        println!("🔧 Troubleshooting:");
        println!("  1. Make sure the example socket service server is running:");
        println!("     cargo run --features builder --example socket_service_server");
        println!("  2. Check if the socket directory exists: {}", socket_dir().display());
        println!("  3. Verify directory permissions");
    } else {
        println!("🔧 Troubleshooting:");
        println!("  1. Make sure the system property service is available");
        println!("  2. Check system permissions");
        println!("  3. Verify the property name is valid");
    }

    std::process::exit(1);
}

/// Validate property name and value before sending
fn _validate_property(name: &str, value: &str) -> Result<(), String> {
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
    if !name.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return Err("Invalid characters in property name (only alphanumeric, ., _, - allowed)".to_string());
    }

    Ok(())
}

/// Get the properties directory path (shared with server)
fn properties_dir() -> PathBuf {
    PathBuf::from(EXAMPLE_PROPERTIES_DIR).join("properties")
}

/// Get the socket directory path (shared with server)
fn socket_dir() -> PathBuf {
    PathBuf::from(EXAMPLE_PROPERTIES_DIR).join("socket")
}
