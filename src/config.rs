//! Runtime configuration, sourced from CLI flags and environment (`.env` via dotenvy).

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TransportKind {
    /// In-process simulated Marlin printer. No hardware required.
    Mock,
    /// Real USB serial device.
    Serial,
}

/// serial2moon — bridge a legacy Marlin G-code printer to Moonraker by
/// emulating the Klipper API server over a Unix domain socket.
#[derive(Debug, Clone, Parser)]
#[command(name = "serial2moon", version, about)]
pub struct Config {
    /// Path of the Unix domain socket Moonraker connects to (klippy_uds_address).
    #[arg(long, env = "S2M_UDS", default_value = "/tmp/klippy_uds")]
    pub uds_path: PathBuf,

    /// Directory holding G-code files (must match Moonraker's upload dir / virtual_sdcard path).
    #[arg(long, env = "S2M_GCODE_DIR", default_value = "./gcodes")]
    pub gcode_dir: PathBuf,

    /// Transport backend.
    #[arg(long, value_enum, env = "S2M_TRANSPORT", default_value = "mock")]
    pub transport: TransportKind,

    /// Serial device (e.g. /dev/serial/by-id/...). When --transport serial and this is
    /// omitted, serial2moon autodetects the printer among the connected serial devices.
    #[arg(long, env = "S2M_SERIAL_PORT")]
    pub serial_port: Option<String>,

    /// Serial baud rate. Omit to autodetect via M115.
    #[arg(long, env = "S2M_BAUD")]
    pub baud: Option<u32>,

    /// Max X/Y/Z travel (mm), advertised to the frontend as axis limits.
    #[arg(long, env = "S2M_BED_SIZE", default_value = "220,220,250")]
    pub bed_size: String,

    /// Max velocity (mm/s) advertised to the frontend.
    #[arg(long, env = "S2M_MAX_VELOCITY", default_value_t = 300.0)]
    pub max_velocity: f64,

    /// Max acceleration (mm/s^2) advertised to the frontend.
    #[arg(long, env = "S2M_MAX_ACCEL", default_value_t = 3000.0)]
    pub max_accel: f64,

    /// Max hotend temperature (°C) advertised to the frontend (sets the UI input limit).
    #[arg(long, env = "S2M_EXTRUDER_MAX_TEMP", default_value_t = 300.0)]
    pub extruder_max_temp: f64,

    /// Max bed temperature (°C) advertised to the frontend (sets the UI input limit).
    #[arg(long, env = "S2M_BED_MAX_TEMP", default_value_t = 120.0)]
    pub bed_max_temp: f64,

    /// Mock printer only: stretch a simulated print to roughly this many seconds, paced
    /// by file progress so it's independent of file size. 0 disables pacing (instant).
    #[arg(long, env = "S2M_MOCK_PRINT_SECONDS", default_value_t = 60)]
    pub mock_print_seconds: u64,

    /// If set, also write logs to `<dir>/serial2moon.log` (in addition to stdout). In the
    /// Pi image this points at Moonraker's logs dir so the log is downloadable in Mainsail.
    #[arg(long, env = "S2M_LOG_DIR")]
    pub log_dir: Option<PathBuf>,

    /// Prusa steel-sheet profiles to expose as Mainsail buttons, as `id:Label` pairs
    /// (id 0-7, the LCD order), e.g. "0:Smooth,1:Textured,2:Satin". Selecting one sends
    /// `M850 S<id> A1`.
    #[arg(long, env = "S2M_SHEETS")]
    pub sheets: Option<String>,

    /// On pause/cancel, lift the toolhead this many mm (0 disables). Restored on resume.
    #[arg(long, env = "S2M_PAUSE_LIFT", default_value_t = 5.0)]
    pub pause_z_lift: f64,

    /// On pause/cancel, retract this many mm of filament (0 disables). Restored on resume.
    #[arg(long, env = "S2M_PAUSE_RETRACT", default_value_t = 1.0)]
    pub pause_retract: f64,
}

impl Config {
    /// Parse `sheets` ("id:Label,id:Label") into (id, label) pairs.
    pub fn parsed_sheets(&self) -> Vec<(u8, String)> {
        let mut out = Vec::new();
        let Some(spec) = self.sheets.as_deref() else {
            return out;
        };
        for part in spec.split(',') {
            if let Some((id, label)) = part.trim().split_once(':')
                && let Ok(id) = id.trim().parse::<u8>()
                && id <= 7
                && !label.trim().is_empty()
            {
                out.push((id, label.trim().to_string()));
            }
        }
        out
    }

    /// Parse `bed_size` ("x,y,z") into axis maxima.
    pub fn axis_maximum(&self) -> [f64; 4] {
        let mut out = [220.0, 220.0, 250.0, 0.0];
        for (i, part) in self.bed_size.split(',').take(3).enumerate() {
            if let Ok(v) = part.trim().parse::<f64>() {
                out[i] = v;
            }
        }
        out
    }
}
