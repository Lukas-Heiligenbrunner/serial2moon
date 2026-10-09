//! serial2moon — bridge a legacy Marlin G-code printer to Moonraker by emulating the
//! Klipper API server over a Unix domain socket.

mod app;
mod config;
mod gcode;
#[cfg(test)]
mod integration_tests;
mod klipper_api;
mod logfile;
mod print_job;
mod serial_session;
mod state;
mod transport;

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
use logfile::RotatingFile;
use print_job::PrintHandle;
use state::{PrinterState, StateHandle};

/// Set up logging to stdout and, when `log_dir` is set, additionally to
/// `<log_dir>/serial2moon.log` (size-capped, with compressed history — see [`logfile`]).
/// Returns the appender guard, which must be kept alive.
fn init_logging(config: &Config) -> Option<WorkerGuard> {
    // Default to debug for our own crate (every serial line + full diagnostics) but info for
    // dependencies (no hyper/tokio/mio spam). Override wholesale with RUST_LOG.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,serial2moon=debug"));

    let log_file = config.log_dir.as_deref().and_then(|dir| {
        let path = dir.join("serial2moon.log");
        let max_bytes = config.log_max_size_mb * 1024 * 1024;
        RotatingFile::open(&path, max_bytes, config.log_max_files)
            .inspect_err(|e| eprintln!("serial2moon: cannot open log {}: {e}", path.display()))
            .ok()
    });
    let (file_layer, guard) = match log_file {
        Some(file) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
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

/// Default contents written to the editable config file when it doesn't exist yet.
const DEFAULT_CONFIG: &str = "\
# serial2moon configuration — editable from Mainsail (Machine -> Configuration Files).
# Lines are KEY=VALUE; '#' starts a comment. After editing, RESTART serial2moon to apply
# (reboot the Pi, or: sudo systemctl restart serial2moon).

# Log verbosity. Default logs every serial line sent/received + much more. Set to plain
# 'info' for a quieter log, or 'debug' to also include (noisy) dependency logs.
RUST_LOG=info,serial2moon=debug

# Log retention: serial2moon.log is rotated at this size (MB) and this many older logs are
# kept gzip-compressed (serial2moon.log.1.gz = newest). The default level writes ~23 MB per
# print hour, so the defaults keep about the last 9 print hours in ~55 MB.
#S2M_LOG_MAX_SIZE_MB=20
#S2M_LOG_MAX_FILES=10

# Hotend / bed maximum temperature (C) — bounds the temperature inputs in the UI.
S2M_EXTRUDER_MAX_TEMP=300
S2M_BED_MAX_TEMP=120

# Pause/cancel parking: lift the toolhead (mm) and retract filament (mm); 0 disables.
# Prusa printers pause with their own M601 (lift applies, the retract is the firmware's).
S2M_PAUSE_LIFT=5
S2M_PAUSE_RETRACT=1

# Force a specific serial device / baud (otherwise both are autodetected):
#S2M_SERIAL_PORT=/dev/serial/by-id/usb-Prusa_Research...-if00
#S2M_BAUD=115200
";

/// Seed the editable config file with defaults if it doesn't exist yet (so it shows up in
/// Mainsail for editing), then return its path for loading.
fn seed_default_config(path: &str) {
    let p = std::path::Path::new(path);
    if p.exists() {
        return;
    }
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(p, DEFAULT_CONFIG) {
        eprintln!("serial2moon: could not write default config {path}: {e}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    // Load the editable config file (Moonraker's config dir on the Pi) before parsing, so
    // its KEY=VALUE settings populate the environment. Values already set in the process
    // environment (e.g. by compose) take precedence, so transport/paths stay locked.
    if let Ok(path) = std::env::var("S2M_CONFIG_FILE") {
        seed_default_config(&path);
        let _ = dotenvy::from_path(std::path::Path::new(&path));
    }
    let config = Config::parse();
    let _log_guard = init_logging(&config);
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
        config.parsed_sheets(),
        config.host_control_dir.is_some(),
    ));
    state::sysstats::spawn(state.clone());

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
