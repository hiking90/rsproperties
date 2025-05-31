use std::env;
use rsproperties::PropertySocketService;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    let socket_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/property_service_example".to_string());

    println!("Starting property socket service at: {}", socket_path);
    println!("Press Ctrl+C to stop the service");

    let service = PropertySocketService::new(Some(&socket_path))?;
    service.run()?;

    Ok(())
}
