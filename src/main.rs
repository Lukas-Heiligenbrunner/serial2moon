//! serial2moon — bridge a legacy Marlin G-code printer to Moonraker by emulating the
//! Klipper API server over a Unix domain socket.

mod app;
mod config;
mod gcode;
mod klipper_api;
mod print_job;
mod serial_session;
mod state;
mod transport;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::broadcast;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use app::App;
use config::Config;
use print_job::PrintHandle;
use state::{PrinterState, StateHandle};

/// Set up logging to stdout and, when `log_dir` is set, additionally to
/// `<log_dir>/serial2moon.log`. Returns the appender guard, which must be kept alive.
fn init_logging(log_dir: Option<&Path>) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let (file_layer, guard) = match log_dir {
        Some(dir) => {
            let _ = std::fs::create_dir_all(dir);
            let appender = tracing_appender::rolling::never(dir, "serial2moon.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(fmt::layer().with_ansi(false).with_writer(writer)),
                Some(guard),
            )
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(file_layer)
        .init();
    guard
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let config = Config::parse();
    let _log_guard = init_logging(config.log_dir.as_deref());
    info!(transport = ?config.transport, socket = %config.uds_path.display(), "starting serial2moon");

    tokio::fs::create_dir_all(&config.gcode_dir)
        .await
        .with_context(|| format!("creating gcode dir {}", config.gcode_dir.display()))?;
    // Resolve to an absolute path so it can be compared against Moonraker's gcodes path —
    // a mismatch is the usual reason a print "does nothing".
    let gcode_abs =
        std::fs::canonicalize(&config.gcode_dir).unwrap_or_else(|_| config.gcode_dir.clone());
    info!(gcode_dir = %gcode_abs.display(), "serving G-code files from this directory (must match Moonraker's gcodes path)");

    // State actor.
    let state = StateHandle::spawn(PrinterState::new(
        config.axis_maximum(),
        config.max_velocity,
        config.max_accel,
        config.extruder_max_temp,
        config.bed_max_temp,
        config.gcode_dir.to_string_lossy().to_string(),
    ));

    // Console (Marlin echo/error) fan-out.
    let (console, _) = broadcast::channel::<String>(256);

    // The serial supervisor owns transport (re)connection + printer init; the handle is
    // stable across reconnects.
    let config = Arc::new(config);
    let print = PrintHandle::new();
    let serial = serial_session::spawn(
        config.clone(),
        state.clone(),
        console.clone(),
        print.clone(),
    );

    let app = App {
        config: config.clone(),
        state,
        serial,
        console,
        print,
    };

    tokio::select! {
        r = klipper_api::serve(app) => r?,
        _ = shutdown_signal() => info!("shutdown signal received"),
    }

    let _ = std::fs::remove_file(&config.uds_path);
    info!("serial2moon stopped");
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
