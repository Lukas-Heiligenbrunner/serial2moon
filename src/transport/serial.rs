//! Real USB serial transport, with optional baud autodetection via M115.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, warn};

use super::Serial;
use crate::config::Config;

/// Bauds tried during autodetection, most-likely first.
const CANDIDATE_BAUDS: &[u32] = &[250000, 115200, 230400, 57600, 38400, 19200, 9600];

pub async fn open(config: &Config) -> Result<Box<dyn Serial>> {
    let port = config
        .serial_port
        .as_deref()
        .context("--transport serial requires --serial-port (e.g. /dev/serial/by-id/...)")?;

    let baud = match config.baud {
        Some(b) => {
            info!(port, baud = b, "opening serial port at configured baud");
            b
        }
        None => detect_baud(port).await?,
    };

    let stream = tokio_serial::new(port, baud)
        .timeout(Duration::from_millis(500))
        .open_native_async()
        .with_context(|| format!("opening {port} at {baud} baud"))?;
    Ok(Box::new(stream))
}

/// Probe each candidate baud, sending M115 and looking for a `FIRMWARE_NAME:` reply.
/// Gating strictly on that token avoids false positives from line noise.
async fn detect_baud(port: &str) -> Result<u32> {
    for &baud in CANDIDATE_BAUDS {
        info!(port, baud, "probing serial baud");
        match probe(port, baud).await {
            Ok(true) => {
                info!(port, baud, "detected printer firmware");
                return Ok(baud);
            }
            Ok(false) => {}
            Err(e) => warn!(port, baud, error = %e, "probe failed"),
        }
    }
    bail!("could not autodetect baud for {port}; pass --baud explicitly");
}

async fn probe(port: &str, baud: u32) -> Result<bool> {
    let stream = tokio_serial::new(port, baud)
        .timeout(Duration::from_millis(500))
        .open_native_async()?;
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();

    // First line after a baud switch is frequently garbage; ask twice.
    for _ in 0..2 {
        write.write_all(b"M115\n").await?;
        write.flush().await?;
    }

    let deadline = Duration::from_millis(1500);
    while let Ok(Ok(Some(line))) = timeout(deadline, lines.next_line()).await {
        if line.contains("FIRMWARE_NAME:") {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    use nix::fcntl::OFlag;
    use nix::pty::{grantpt, posix_openpt, ptsname_r, unlockpt};

    /// End-to-end serial path over a real pseudo-terminal: a fake Marlin replies to
    /// M115, and we assert autodetection picks it up via the `FIRMWARE_NAME:` gate.
    #[tokio::test]
    async fn autodetects_baud_over_pty() {
        let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let slave_path = ptsname_r(&master).unwrap();

        // Fake printer on the master end: answer every M115 with a firmware banner.
        std::thread::spawn(move || {
            let mut m = master;
            let mut buf = [0u8; 512];
            loop {
                match m.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if String::from_utf8_lossy(&buf[..n]).contains("M115") {
                            let _ = m.write_all(b"FIRMWARE_NAME:Marlin 2.1.2 (pty-fake)\nok\n");
                            let _ = m.flush();
                        }
                    }
                }
            }
        });

        let baud = detect_baud(&slave_path).await.expect("autodetect");
        assert!(CANDIDATE_BAUDS.contains(&baud), "unexpected baud {baud}");
    }
}
