mod args;
mod bridge;
mod protocol;

use anyhow::Result;
use clap::Parser;
use log::{info, error};

use args::Args;
use bridge::UartToMoon;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    
    // Initialize logger
    if args.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    info!("Starting Uart2Moon bridge");
    info!("Socket path: {:?}", args.socket_path);
    info!("Device: {}", args.device);
    info!("Baud rate: {}", args.baud_rate);

    // Create and run the bridge
    let bridge = UartToMoon::new(args.socket_path, args.device, args.baud_rate, args.test_mode);
    
    match bridge.run().await {
        Ok(_) => {
            info!("Bridge exited successfully");
        }
        Err(e) => {
            error!("Bridge failed: {}", e);
            return Err(e);
        }
    }

    Ok(())
}
