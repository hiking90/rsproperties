use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "example_service")]
#[command(
    about = "Example service for rsproperties-service. Missing directories are created and \
             nothing is deleted: existing files are reused; property_info, areas and sockets \
             are recreated."
)]
struct Args {
    /// Properties directory path
    #[arg(long, help = "Directory path for system properties")]
    properties_dir: Option<PathBuf>,

    /// Socket directory path
    #[arg(long, help = "Directory path for property service sockets")]
    socket_dir: Option<PathBuf>,

    /// property_contexts files
    #[arg(
        long = "property-contexts",
        help = "property_contexts file (repeatable)"
    )]
    property_contexts: Vec<PathBuf>,

    /// build.prop files
    #[arg(long = "build-prop", help = "build.prop file (repeatable)")]
    build_prop: Vec<PathBuf>,

    /// Persistent properties file
    #[arg(
        long,
        help = "Store persist.* properties in this file (AOSP persistent_properties format)"
    )]
    persistent_properties_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Parse command line arguments
    let args = Args::parse();

    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Setup directories
    let properties_dir = args
        .properties_dir
        .unwrap_or_else(|| PathBuf::from("__properties__"));
    let socket_dir = args
        .socket_dir
        .unwrap_or_else(|| properties_dir.join("sockets"));

    // Create the directories if missing. Nothing is deleted: the paths come
    // from the command line and may hold other software's files. The modes
    // are those `run` uses (0711 properties dir, 0755 socket dir), minus
    // the umask's bits; `create_dir_all` would use 0777 minus the umask's
    // bits, world-writable under umask 0000.
    DirBuilder::new()
        .recursive(true)
        .mode(0o711)
        .create(&properties_dir)?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&socket_dir)?;

    println!("📁 Directories:");
    println!("   Properties: {properties_dir:?}");
    println!("   Sockets: {socket_dir:?}");

    // Create PropertyConfig
    let config = rsproperties::PropertyConfig::new()
        .properties_dir(properties_dir)
        .socket_dir(socket_dir);

    println!("🚀 Starting rsproperties services...");

    // Initialize the services
    let mut options = rsproperties_service::ServiceOptions::new()
        .property_contexts_files(args.property_contexts)
        .build_prop_files(args.build_prop)
        // No services to control here: log ctl.* requests and refuse them.
        .control_handler(|msg| {
            println!(
                "ctl.{} {:?} from pid {:?}: refused",
                msg.action, msg.target, msg.pid
            );
            false
        });
    if let Some(path) = args.persistent_properties_file {
        options = options.persistent_properties_file(path);
    }
    let mut service = rsproperties_service::run(config, options).await?;

    println!("✅ Services started successfully!");
    println!("🔄 Services are running. Press Ctrl+C to stop.");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\n🛑 Shutdown signal received...");
        }
        // The service ended by itself; `shutdown` below returns why.
        _ = service.join() => {}
    }

    // Unlinks the sockets and releases the writer lock before returning.
    if let Err(e) = service.shutdown().await {
        eprintln!("❌ {e}");
    }

    println!("👋 Services stopped.");
    Ok(())
}
