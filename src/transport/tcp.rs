//! Raw G-code over TCP: the Marlin simulator's serial socket, or a network serial bridge
//! (ESP3D, ser2net, telnet). Same M115 handshake as the USB serial transport, minus the
//! baud scan and the DTR reset.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::info;

use super::Serial;
use super::serial::handshake;
use crate::config::Config;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn open(config: &Config) -> Result<(Box<dyn Serial>, u32)> {
    connect(&config.tcp_addr, config.baud.unwrap_or(0)).await
}

async fn connect(addr: &str, baud: u32) -> Result<(Box<dyn Serial>, u32)> {
    info!(addr, "connecting to TCP printer");
    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("timed out connecting to {addr}"))?
        .with_context(|| format!("connecting to {addr}"))?;
    stream.set_nodelay(true).ok(); // G-code lines are tiny; don't let Nagle batch them

    if !handshake(&mut stream).await? {
        bail!("no FIRMWARE_NAME reply from {addr}");
    }
    // Drain duplicate M115 replies so they don't reach the console.
    let mut buf = [0u8; 512];
    while let Ok(Ok(n)) = timeout(Duration::from_millis(250), stream.read(&mut buf)).await {
        if n == 0 {
            break;
        }
    }
    info!(addr, "printer detected");
    Ok((Box::new(stream), baud))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// A fake Marlin on a loopback socket answers M115; connect must detect it.
    #[tokio::test]
    async fn connects_to_printer_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if String::from_utf8_lossy(&buf[..n]).contains("M115") {
                            let _ = sock
                                .write_all(b"FIRMWARE_NAME:Marlin 2.1.3 (tcp-fake)\nok\n")
                                .await;
                        }
                    }
                }
            }
        });
        assert!(
            connect(&addr, 0).await.is_ok(),
            "expected to detect the fake printer"
        );
    }

    #[tokio::test]
    async fn fails_when_nothing_listens() {
        // Bind then drop to get a port that is (almost certainly) closed.
        let addr = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(connect(&addr.to_string(), 0).await.is_err());
    }
}
