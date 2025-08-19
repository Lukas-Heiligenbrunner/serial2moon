use std::env;
use std::path::PathBuf;

#[derive(Debug)]
pub struct Config {
    /// Path to the Unix socket file (where moonraker expects to find Klipper)
    pub socket_path: PathBuf,

    /// Serial port device (e.g., /dev/ttyUSB0, /dev/ttyACM0)
    pub device: String,

    /// Baud rate for serial communication
    pub baud_rate: u32,

    /// Enable verbose logging
    pub verbose: bool,

    /// Test mode - run without connecting to serial device
    pub test_mode: bool,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            socket_path: PathBuf::from(
                env::var("SOCKET_PATH").unwrap_or_else(|_| "/tmp/printer".to_string()),
            ),
            device: env::var("DEVICE").unwrap_or_else(|_| "/dev/ttyUSB0".to_string()),
            baud_rate: env::var("BAUD_RATE")
                .unwrap_or_else(|_| "115200".to_string())
                .parse()
                .expect("BAUD_RATE must be a valid number"),
            verbose: env::var("VERBOSE")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            test_mode: env::var("TEST_MODE")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
        }
    }
}
