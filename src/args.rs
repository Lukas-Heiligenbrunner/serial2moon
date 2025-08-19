use std::path::PathBuf;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Path to the Unix socket file (where moonraker expects to find Klipper)
    #[arg(short, long, default_value = "/tmp/printer")]
    pub socket_path: PathBuf,
    
    /// Serial port device (e.g., /dev/ttyUSB0, /dev/ttyACM0)
    #[arg(short, long, default_value = "/dev/ttyUSB0")]
    pub device: String,
    
    /// Baud rate for serial communication
    #[arg(short, long, default_value_t = 115200)]
    pub baud_rate: u32,
    
    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,
}