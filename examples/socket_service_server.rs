// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::thread;

use rsproperties::{PropertySocketService, create_property_channel};
use rsproperties::errors::Result;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    println!("Starting Property Socket Service with Channel Example");

    // Create a channel for receiving property messages
    let (sender, receiver) = create_property_channel();

    // Create socket service with the sender
    let socket_service = PropertySocketService::new(sender.clone(), sender)?;

    println!("Socket service created successfully!");
    println!("Both property_service and property_service_for_system sockets are now active");
    println!("Listening for property messages on channel...");

    // Start the socket service in a separate thread
    let service_handle = thread::spawn(move || {
        if let Err(e) = socket_service.run() {
            eprintln!("Socket service error: {}", e);
        }
    });

    // Listen for property messages in the main thread
    let receiver_handle = thread::spawn(move || {
        println!("Channel receiver started, waiting for property messages...");

        loop {
            match receiver.recv() {
                Ok(property_msg) => {
                    println!("Received property: '{}' = '{}'", property_msg.key, property_msg.value);

                    // Here you can process the received property data
                    // For example: update a configuration, trigger an action, etc.
                    match property_msg.key.as_str() {
                        "sys.powerctl" => {
                            println!("System power control command received: {}", property_msg.value);
                        }
                        key if key.starts_with("debug.") => {
                            println!("Debug property received: {} = {}", key, property_msg.value);
                        }
                        _ => {
                            println!("General property received: {} = {}", property_msg.key, property_msg.value);
                        }
                    }
                }
                Err(e) => {
                    println!("Channel receiver error: {}", e);
                    break;
                }
            }
        }
    });

    println!("\nTo test the service, run from another terminal:");
    println!("cargo run --example socket_service_client test.property test.value");
    println!("\nPress Ctrl+C to stop the service");

    // Wait for either thread to complete (they run indefinitely)
    // In a real application, you might want to handle shutdown signals
    if let Err(e) = receiver_handle.join() {
        eprintln!("Receiver thread panicked: {:?}", e);
    }

    if let Err(e) = service_handle.join() {
        eprintln!("Service thread panicked: {:?}", e);
    }

    Ok(())
}
