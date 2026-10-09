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

/// Open the configured transport, returning a connected byte stream and the link baud
/// (0 for the mock, which has no real line rate).
pub async fn open(config: &Config) -> Result<(Box<dyn Serial>, u32)> {
    match config.transport {
        TransportKind::Mock => {
            let options = mock::Options {
                prusa: config.mock_prusa,
                runout_at: config.mock_runout_at,
            };
            Ok((mock::open(options), 0))
        }
        TransportKind::Serial => serial::open(config).await,
    }
}
