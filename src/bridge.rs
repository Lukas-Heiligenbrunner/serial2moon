use anyhow::{Context, Result};
use log::{error, info, warn};
use serde_json;
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
}

impl UartToMoon {
    pub fn new(socket_path: std::path::PathBuf, device: String, baud_rate: u32) -> Self {
        Self {
            socket_path,
            device,
            baud_rate,
        }
    }

    pub async fn run(&self) -> Result<()> {
        info!("Starting uart2moon bridge");
        info!("Socket path: {:?}", self.socket_path);
        info!("Device: {}", self.device);
        info!("Baud rate: {}", self.baud_rate);

        // Remove existing socket file if it exists
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .context("Failed to remove existing socket file")?;
        }

        // Create the parent directory if it doesn't exist
        if let Some(parent) = self.socket_path.parent() {
            tokio::fs::create_dir_all(parent).await
                .context("Failed to create socket directory")?;
        }

        // Setup serial connection
        let serial_port = tokio_serial::new(&self.device, self.baud_rate)
            .open_native_async()
            .context("Failed to open serial port")?;
            
        let serial_port = Arc::new(Mutex::new(serial_port));

        // Create channels for communication between socket and serial handlers
        let (to_serial_tx, to_serial_rx) = mpsc::channel::<String>(100);
        let (from_serial_tx, mut from_serial_rx) = mpsc::channel::<String>(100);

        // Start serial handler
        let _serial_handle = tokio::spawn(Self::handle_serial(serial_port, to_serial_rx, from_serial_tx));

        // Start Unix socket server
        let listener = UnixListener::bind(&self.socket_path)
            .context("Failed to bind Unix socket")?;
        
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
                            error!("Failed to accept connection: {}", e);
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

    async fn handle_client(mut stream: UnixStream, to_serial_tx: mpsc::Sender<String>) -> Result<()> {
        let (reader, mut writer) = stream.split();
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        let mut protocol_handler = ProtocolHandler::new();

        loop {
            line.clear();
            match buf_reader.read_line(&mut line).await {
                Ok(0) => {
                    info!("Client disconnected");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    info!("Received from client: {}", trimmed);

                    // Parse Klipper message
                    match serde_json::from_str::<KlipperMessage>(trimmed) {
                        Ok(message) => {
                            match protocol_handler.handle_message(message).await {
                                Ok((response, gcode_opt)) => {
                                    // Send G-code to printer if needed
                                    if let Some(gcode) = gcode_opt {
                                        if let Err(e) = to_serial_tx.send(gcode).await {
                                            error!("Failed to send to serial: {}", e);
                                        }
                                    }

                                    // Send response back to client
                                    let response_json = serde_json::to_string(&response)?;
                                    if let Err(e) = writer.write_all(format!("{}\n", response_json).as_bytes()).await {
                                        error!("Failed to write to client: {}", e);
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!("Protocol handler error: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse message as JSON: {} - Raw: {}", e, trimmed);
                            // Try to handle as raw G-code
                            if let Err(e) = to_serial_tx.send(trimmed.to_string()).await {
                                error!("Failed to send raw command to serial: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to read from client: {}", e);
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
                                    error!("Failed to forward serial response: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to read from serial port: {}", e);
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
                format!("{}\n", cmd)
            };
            
            let mut serial_guard = serial.lock().await;
            if let Err(e) = serial_guard.write_all(cmd_with_newline.as_bytes()).await {
                error!("Failed to write to serial port: {}", e);
            }
        }

        read_task.abort();
        Ok(())
    }

}