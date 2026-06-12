//! Real USB serial transport, with printer autodetection via M115.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, warn};

use super::Serial;
use crate::config::Config;

/// Bauds tried during autodetection, most-likely first (115200 covers Prusa & most Marlin).
const CANDIDATE_BAUDS: &[u32] = &[115200, 250000, 230400, 57600, 38400, 19200, 9600];

/// How long to keep trying M115 on a freshly-opened port. Must exceed the printer's
/// reset-and-boot time, since many boards (Prusa MK3, Arduino-based) reset on DTR when the
/// port opens and can't answer for a few seconds.
const HANDSHAKE_WINDOW: Duration = Duration::from_secs(8);
/// Re-send M115 at least this often during the handshake.
const M115_INTERVAL: Duration = Duration::from_millis(1200);

pub async fn open(config: &Config) -> Result<(Box<dyn Serial>, u32)> {
    match config.serial_port.as_deref() {
        Some(port) => match config.baud {
            Some(baud) => connect(port, baud).await,
            None => detect_on_port(port).await,
        },
        None => autodetect(config.baud).await,
    }
}

/// Try every candidate baud on a single (explicit) port.
async fn detect_on_port(port: &str) -> Result<(Box<dyn Serial>, u32)> {
    for &baud in CANDIDATE_BAUDS {
        match connect(port, baud).await {
            Ok(connected) => return Ok(connected),
            Err(e) => warn!(port, baud, error = %e, "no printer at this baud"),
        }
    }
    bail!("could not reach a printer on {port} at any known baud");
}

/// No port configured: scan the connected serial devices and pick the first that
/// handshakes as a Marlin printer.
async fn autodetect(baud: Option<u32>) -> Result<(Box<dyn Serial>, u32)> {
    let ports = candidate_ports();
    if ports.is_empty() {
        bail!("no serial devices present — is the printer connected?");
    }
    info!(?ports, "autodetecting printer among serial devices");
    for port in &ports {
        let result = match baud {
            Some(b) => connect(port, b).await,
            None => detect_on_port(port).await,
        };
        match result {
            Ok(connected) => {
                info!(port, "selected printer");
                return Ok(connected);
            }
            Err(e) => warn!(port, error = %e, "no Marlin response; skipping"),
        }
    }
    bail!(
        "no responding Marlin printer among {} serial device(s)",
        ports.len()
    );
}

/// Open `port` at `baud`, ride through the connect-time reset, and confirm it's a printer
/// by repeatedly sending M115 until it answers. On success the live, ready stream is
/// returned (we do NOT reopen — that would reset the printer a second time), along with
/// the baud (used to compute serial-link utilization).
async fn connect(port: &str, baud: u32) -> Result<(Box<dyn Serial>, u32)> {
    info!(port, baud, "probing serial");
    let mut stream = tokio_serial::new(port, baud)
        .timeout(Duration::from_millis(500))
        .open_native_async()
        .with_context(|| format!("opening {port} at {baud}"))?;

    if handshake(&mut stream).await? {
        info!(port, baud, "printer detected");
        Ok((Box::new(stream), baud))
    } else {
        bail!("no FIRMWARE_NAME reply within {HANDSHAKE_WINDOW:?}");
    }
}

/// Re-send M115 periodically for up to [`HANDSHAKE_WINDOW`], scanning the stream for a
/// Marlin `FIRMWARE_NAME:` reply. Tolerant of the boot-time reset and of continuous
/// chatter/garbage (bounded by an overall deadline rather than per-line timeouts).
async fn handshake<S: AsyncReadExt + AsyncWriteExt + Unpin>(stream: &mut S) -> Result<bool> {
    let start = Instant::now();
    let mut last_send: Option<Instant> = None;
    let mut acc = String::new();
    let mut buf = [0u8; 512];

    while start.elapsed() < HANDSHAKE_WINDOW {
        if last_send.is_none_or(|t| t.elapsed() >= M115_INTERVAL) {
            let _ = stream.write_all(b"M115\n").await;
            let _ = stream.flush().await;
            last_send = Some(Instant::now());
        }

        // Read in short bursts so we cycle back and re-send M115 on a lull.
        match timeout(Duration::from_millis(800), stream.read(&mut buf)).await {
            Ok(Ok(0)) => return Ok(false), // EOF — port went away
            Ok(Ok(n)) => {
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                if acc.contains("FIRMWARE_NAME:") {
                    return Ok(true);
                }
                if acc.len() > 4096 {
                    acc.drain(..acc.len() - 1024); // bound memory; keep the tail
                }
            }
            Ok(Err(_)) => {} // read error (e.g. wrong-baud framing) — keep trying
            Err(_) => {}     // 800 ms lull — loop re-sends M115
        }
    }
    Ok(false)
}

/// Connected serial devices, most-stable first. Prefers `/dev/serial/by-id/*` (stable
/// across reboots/replugs); falls back to raw `ttyACM*`/`ttyUSB*` nodes.
fn candidate_ports() -> Vec<String> {
    let by_id: Vec<String> = std::fs::read_dir("/dev/serial/by-id")
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if !by_id.is_empty() {
        return by_id;
    }
    let mut ports = Vec::new();
    for prefix in ["/dev/ttyACM", "/dev/ttyUSB"] {
        for n in 0..8 {
            let p = format!("{prefix}{n}");
            if std::path::Path::new(&p).exists() {
                ports.push(p);
            }
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    use nix::fcntl::OFlag;
    use nix::pty::{grantpt, posix_openpt, ptsname_r, unlockpt};

    /// End-to-end serial path over a real pseudo-terminal: a fake Marlin replies to M115,
    /// and we assert the handshake/connect detects it via the `FIRMWARE_NAME:` gate.
    #[tokio::test]
    async fn connects_to_printer_over_pty() {
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

        let detected = connect(&slave_path, 115200).await.is_ok();
        assert!(detected, "expected to detect the fake printer");
    }
}
