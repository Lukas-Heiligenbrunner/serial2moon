mod args;
mod bridge;
mod protocol;

use anyhow::Result;
use log::{error, info};

use args::Config;
use bridge::UartToMoon;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if it exists
    dotenvy::dotenv().ok();

    let config = Config::from_env();

    // Initialize logger
    if config.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    info!("Starting Uart2Moon bridge");
    info!("Socket path: {:?}", config.socket_path);
    info!("Device: {}", config.device);
    info!("Baud rate: {}", config.baud_rate);
    info!("Test mode: {}", config.test_mode);

    // Create and run the bridge
    let bridge = UartToMoon::new(
        config.socket_path,
        config.device,
        config.baud_rate,
        config.test_mode,
    );

    match bridge.run().await {
        Ok(_) => {
            info!("Bridge exited successfully");
        }
        Err(e) => {
            error!("Bridge failed: {e}");
            return Err(e);
        }
    }

    Ok(())
}
