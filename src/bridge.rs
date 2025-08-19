use anyhow::{Context, Result};
use log::{error, info, warn};
use std::fs;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};
use tokio_serial::SerialPortBuilderExt;

use crate::protocol::{KlipperMessage, ProtocolHandler};

pub struct UartToMoon {
    socket_path: std::path::PathBuf,
    device: String,
    baud_rate: u32,
    test_mode: bool,
}

impl UartToMoon {
    pub fn new(
        socket_path: std::path::PathBuf,
        device: String,
        baud_rate: u32,
        test_mode: bool,
    ) -> Self {
        Self {
            socket_path,
            device,
            baud_rate,
            test_mode,
        }
    }

    pub async fn run(&self) -> Result<()> {
        info!("Starting uart2moon bridge");
        info!("Socket path: {:?}", self.socket_path);
        info!("Device: {}", self.device);
        info!("Baud rate: {}", self.baud_rate);
        info!("Test mode: {}", self.test_mode);

        // Remove existing socket file if it exists
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .context("Failed to remove existing socket file")?;
        }

        // Create the parent directory if it doesn't exist
        if let Some(parent) = self.socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("Failed to create socket directory")?;
        }

        // Setup serial connection or test mode
        let (to_serial_tx, to_serial_rx) = mpsc::channel::<String>(100);
        let (from_serial_tx, mut from_serial_rx) = mpsc::channel::<String>(100);

        if self.test_mode {
            info!("Running in test mode - no serial connection");
            // Start a mock serial handler for test mode
            let _serial_handle =
                tokio::spawn(Self::handle_test_serial(to_serial_rx, from_serial_tx));
        } else {
            // Setup real serial connection
            let serial_port = tokio_serial::new(&self.device, self.baud_rate)
                .open_native_async()
                .context("Failed to open serial port")?;

            let serial_port = Arc::new(Mutex::new(serial_port));

            // Start serial handler
            let _serial_handle = tokio::spawn(Self::handle_serial(
                serial_port,
                to_serial_rx,
                from_serial_tx,
            ));
        }

        // Start Unix socket server
        let listener =
            UnixListener::bind(&self.socket_path).context("Failed to bind Unix socket")?;
        fs::set_permissions(&self.socket_path, Permissions::from_mode(0o777))?;

        info!("Unix socket server listening at {:?}", self.socket_path);

        loop {
            tokio::select! {
                // Handle new socket connections
                conn_result = listener.accept() => {
                    match conn_result {
                        Ok((stream, _)) => {
                            info!("New client connected");
                            let to_serial_tx = to_serial_tx.clone();
                            tokio::spawn(Self::handle_client(stream, to_serial_tx));
                        }
                        Err(e) => {
                            error!("Failed to accept connection: {e}");
                        }
                    }
                }

                // Handle responses from serial port
                serial_response = from_serial_rx.recv() => {
                    if let Some(response) = serial_response {
                        info!("Received from printer: {}", response.trim());
                        // Here we could broadcast to all connected clients if needed
                    }
                }
            }
        }
    }

    async fn handle_client(
        mut stream: UnixStream,
        to_serial_tx: mpsc::Sender<String>,
    ) -> Result<()> {
        let (reader, mut writer) = stream.split();
        let mut buf_reader = BufReader::new(reader);
        let mut buffer = Vec::new();
        let mut protocol_handler = ProtocolHandler::new();

        loop {
            buffer.clear();
            info!("Reading from serial stream");

            // Read until we find the ASCII 0x03 (ETX) terminator
            match buf_reader.read_until(0x03, &mut buffer).await {
                Ok(0) => {
                    info!("Client disconnected");
                    break;
                }
                Ok(_) => {
                    info!("Client connected");
                    // Remove the 0x03 terminator and convert to string
                    if buffer.last() == Some(&0x03) {
                        buffer.pop();
                    }

                    let message_str = match String::from_utf8(buffer.clone()) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("Failed to parse message as UTF-8: {e}");
                            continue;
                        }
                    };

                    let trimmed = message_str.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    info!("Received from client: {trimmed}");

                    // Parse Klipper message
                    match serde_json::from_str::<KlipperMessage>(trimmed) {
                        Ok(message) => {
                            match protocol_handler.handle_message(message).await {
                                Ok((response, gcode_opt)) => {
                                    // Send G-code to printer if needed
                                    info!("Received response: {response:?}");
                                    if let Some(gcode) = gcode_opt {
                                        if let Err(e) = to_serial_tx.send(gcode).await {
                                            error!("Failed to send to serial: {e}");
                                        }
                                    }

                                    // Send response back to client with ASCII 0x03 terminator
                                    let response_json = serde_json::to_string(&response)?;
                                    let mut response_bytes = response_json.into_bytes();
                                    response_bytes.push(0x03); // Add ASCII 0x03 terminator

                                    if let Err(e) = writer.write_all(&response_bytes).await {
                                        error!("Failed to write to client: {e}");
                                        break;
                                    }

                                    // Ensure data is flushed
                                    if let Err(e) = writer.flush().await {
                                        error!("Failed to flush client response: {e}");
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!("Protocol handler error: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse message as JSON: {e} - Raw: {trimmed}");
                            // Try to handle as raw G-code
                            if let Err(e) = to_serial_tx.send(trimmed.to_string()).await {
                                error!("Failed to send raw command to serial: {e}");
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to read from client: {e}");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn handle_serial(
        serial: Arc<Mutex<tokio_serial::SerialStream>>,
        mut to_serial_rx: mpsc::Receiver<String>,
        from_serial_tx: mpsc::Sender<String>,
    ) -> Result<()> {
        // Start a separate task for reading
        let serial_reader = serial.clone();
        let from_serial_tx_clone = from_serial_tx.clone();
        let read_task = tokio::spawn(async move {
            let mut line = String::new();
            loop {
                {
                    let mut serial_guard = serial_reader.lock().await;
                    let mut buf_reader = BufReader::new(&mut *serial_guard);
                    line.clear();

                    match buf_reader.read_line(&mut line).await {
                        Ok(0) => {
                            warn!("Serial port closed");
                            break;
                        }
                        Ok(_) => {
                            if !line.trim().is_empty() {
                                if let Err(e) = from_serial_tx_clone.send(line.clone()).await {
                                    error!("Failed to forward serial response: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to read from serial port: {e}");
                            break;
                        }
                    }
                }
                // Small delay to avoid holding the lock too long
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            }
        });

        // Handle writing in main task
        while let Some(cmd) = to_serial_rx.recv().await {
            info!("Sending to printer: {}", cmd.trim());
            let cmd_with_newline = if cmd.ends_with('\n') {
                cmd
            } else {
                format!("{cmd}\n")
            };

            let mut serial_guard = serial.lock().await;
            if let Err(e) = serial_guard.write_all(cmd_with_newline.as_bytes()).await {
                error!("Failed to write to serial port: {e}");
            }
        }

        read_task.abort();
        Ok(())
    }

    async fn handle_test_serial(
        mut to_serial_rx: mpsc::Receiver<String>,
        from_serial_tx: mpsc::Sender<String>,
    ) -> Result<()> {
        info!("Test serial handler started");

        while let Some(cmd) = to_serial_rx.recv().await {
            info!("TEST: Would send to printer: {}", cmd.trim());

            // Simulate printer response
            let response = match cmd.trim() {
                cmd if cmd.starts_with("M115") => {
                    "FIRMWARE_NAME:uart2moon FIRMWARE_VERSION:0.1.0\nok"
                }
                cmd if cmd.starts_with("M105") => {
                    "ok T:25.0 /0.0 B:25.0 /0.0 T0:25.0 /0.0 @:0 B@:0"
                }
                cmd if cmd.starts_with("G28") => "ok",
                cmd if cmd.starts_with("M112") => "!! Emergency stop activated",
                _ => "ok",
            };

            if let Err(e) = from_serial_tx.send(format!("{response}\n")).await {
                error!("Failed to send test response: {e}");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::time::{timeout, Duration};

    #[test]
    fn test_uart_to_moon_new() {
        let socket_path = std::path::PathBuf::from("/tmp/test_socket");
        let device = "/dev/ttyUSB0".to_string();
        let baud_rate = 115200;
        let test_mode = true;

        let bridge = UartToMoon::new(socket_path.clone(), device.clone(), baud_rate, test_mode);

        assert_eq!(bridge.socket_path, socket_path);
        assert_eq!(bridge.device, device);
        assert_eq!(bridge.baud_rate, baud_rate);
        assert_eq!(bridge.test_mode, test_mode);
    }

    #[tokio::test]
    async fn test_handle_test_serial() {
        let (to_serial_tx, to_serial_rx) = mpsc::channel::<String>(10);
        let (from_serial_tx, mut from_serial_rx) = mpsc::channel::<String>(10);

        // Start the test serial handler
        let handle = tokio::spawn(UartToMoon::handle_test_serial(to_serial_rx, from_serial_tx));

        // Send a test command
        to_serial_tx.send("M115".to_string()).await.unwrap();

        // Check we get a response
        let response = timeout(Duration::from_millis(100), from_serial_rx.recv())
            .await
            .expect("Should receive response within timeout")
            .expect("Should receive a response");

        assert!(response.contains("FIRMWARE_NAME:uart2moon"));

        // Send temperature request
        to_serial_tx.send("M105".to_string()).await.unwrap();
        let response = timeout(Duration::from_millis(100), from_serial_rx.recv())
            .await
            .expect("Should receive response within timeout")
            .expect("Should receive a response");

        assert!(response.contains("T:25.0"));

        // Clean up
        drop(to_serial_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_handle_test_serial_home_command() {
        let (to_serial_tx, to_serial_rx) = mpsc::channel::<String>(10);
        let (from_serial_tx, mut from_serial_rx) = mpsc::channel::<String>(10);

        let handle = tokio::spawn(UartToMoon::handle_test_serial(to_serial_rx, from_serial_tx));

        // Send home command
        to_serial_tx.send("G28".to_string()).await.unwrap();
        let response = timeout(Duration::from_millis(100), from_serial_rx.recv())
            .await
            .expect("Should receive response within timeout")
            .expect("Should receive a response");

        assert!(response.contains("ok"));

        // Clean up
        drop(to_serial_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_handle_test_serial_emergency_stop() {
        let (to_serial_tx, to_serial_rx) = mpsc::channel::<String>(10);
        let (from_serial_tx, mut from_serial_rx) = mpsc::channel::<String>(10);

        let handle = tokio::spawn(UartToMoon::handle_test_serial(to_serial_rx, from_serial_tx));

        // Send emergency stop
        to_serial_tx.send("M112".to_string()).await.unwrap();
        let response = timeout(Duration::from_millis(100), from_serial_rx.recv())
            .await
            .expect("Should receive response within timeout")
            .expect("Should receive a response");

        assert!(response.contains("Emergency stop"));

        // Clean up
        drop(to_serial_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_handle_test_serial_unknown_command() {
        let (to_serial_tx, to_serial_rx) = mpsc::channel::<String>(10);
        let (from_serial_tx, mut from_serial_rx) = mpsc::channel::<String>(10);

        let handle = tokio::spawn(UartToMoon::handle_test_serial(to_serial_rx, from_serial_tx));

        // Send unknown command
        to_serial_tx.send("UNKNOWN".to_string()).await.unwrap();
        let response = timeout(Duration::from_millis(100), from_serial_rx.recv())
            .await
            .expect("Should receive response within timeout")
            .expect("Should receive a response");

        assert!(response.contains("ok"));

        // Clean up
        drop(to_serial_tx);
        let _ = handle.await;
    }

    // Integration test for socket creation (only test setup, not full run to avoid hanging)
    #[test]
    fn test_socket_path_creation() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test_socket");

        let bridge = UartToMoon::new(
            socket_path.clone(),
            "/dev/ttyUSB0".to_string(),
            115200,
            true,
        );
        assert_eq!(bridge.socket_path, socket_path);
    }
}
