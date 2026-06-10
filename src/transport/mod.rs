//! Printer transport. Both the real serial port and the in-process mock expose the
//! same byte-level interface (`AsyncRead + AsyncWrite`), so the serial session code is
//! identical for both.

pub mod mock;
pub mod serial;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::config::{Config, TransportKind};

/// Anything byte-stream-like we can talk Marlin G-code over.
pub trait Serial: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Serial for T {}

/// Open the configured transport, returning a connected byte stream.
pub async fn open(config: &Config) -> Result<Box<dyn Serial>> {
    match config.transport {
        TransportKind::Mock => Ok(mock::open()),
        TransportKind::Serial => serial::open(config).await,
    }
}
