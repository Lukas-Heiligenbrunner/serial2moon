//! Shared application context handed to the API dispatcher and G-code layer.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::config::Config;
use crate::print_job::PrintHandle;
use crate::serial_session::SerialHandle;
use crate::state::StateHandle;

#[derive(Clone)]
pub struct App {
    pub config: Arc<Config>,
    pub state: StateHandle,
    pub serial: SerialHandle,
    /// Console output stream (Marlin echo/error lines) fanned out to subscribers.
    pub console: broadcast::Sender<String>,
    pub print: PrintHandle,
}
