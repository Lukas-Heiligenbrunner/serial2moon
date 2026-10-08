//! The canonical printer model and its projection into Klipper "printer objects".
//!
//! [`PrinterState`] is the single source of truth, mutated only by the state actor.
//! [`PrinterState::full_status`] renders it into the `{object: {field: value}}` map
//! that `objects/query` and `objects/subscribe` operate on.

use std::collections::BTreeMap;

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KlippyState {
    Startup,
    Ready,
    Shutdown,
    Error,
}

impl KlippyState {
    pub fn as_str(self) -> &'static str {
        match self {
            KlippyState::Startup => "startup",
            KlippyState::Ready => "ready",
            KlippyState::Shutdown => "shutdown",
            KlippyState::Error => "error",
        }
    }
}

/// Print job lifecycle. String values must match Klipper's vocabulary exactly,
/// or Mainsail's print panel misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintState {
    Standby,
    Printing,
    Paused,
    Complete,
    Cancelled,
    Error,
}

impl PrintState {
    pub fn as_str(self) -> &'static str {
        match self {
            PrintState::Standby => "standby",
            PrintState::Printing => "printing",
            PrintState::Paused => "paused",
            PrintState::Complete => "complete",
            PrintState::Cancelled => "cancelled",
            PrintState::Error => "error",
        }
    }
}

/// A steel-sheet profile exposed as a selectable macro. `z` is the stored live-Z offset
/// (None until discovered from the printer's `M850` report).
#[derive(Debug, Clone)]
pub struct Sheet {
    pub id: u8,
    pub label: String,
    pub z: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct PrinterState {
    pub klippy_state: KlippyState,
    pub state_message: String,

    // Heaters / fan
    pub extruder_temp: f64,
    pub extruder_target: f64,
    pub extruder_power: f64,
    pub bed_temp: f64,
    pub bed_target: f64,
    pub bed_power: f64,
    pub fan_speed: f64,

    // Motion (X, Y, Z, E)
    pub position: [f64; 4],
    pub gcode_position: [f64; 4],
    pub homing_origin: [f64; 4],
    pub homed_axes: String,
    pub absolute_coordinates: bool,
    pub absolute_extrude: bool,
    pub speed: f64,
    pub speed_factor: f64,
    pub extrude_factor: f64,

    // Limits (advertised to the frontend)
    pub axis_minimum: [f64; 4],
    pub axis_maximum: [f64; 4],
    pub max_velocity: f64,
    pub max_accel: f64,

    // Print job
    pub print_state: PrintState,
    pub print_filename: String,
    pub print_message: String,
    pub total_duration: f64,
    pub print_duration: f64,
    pub filament_used: f64,
    pub sd_file_path: Option<String>,
    pub sd_progress: f64,
    pub sd_is_active: bool,
    pub sd_file_position: u64,
    pub sd_file_size: u64,
    pub display_message: String,
    pub current_layer: Option<u64>,
    pub total_layer: Option<u64>,

    // Config
    pub extruder_max_temp: f64,
    pub bed_max_temp: f64,
    pub gcode_dir: String,
    /// Steel-sheet profiles (auto-discovered via M850 and/or configured), exposed as macros.
    pub sheets: Vec<Sheet>,
    /// Whether host power control is available (a host-control dir is configured). When
    /// true, the HOST_REBOOT / HOST_SHUTDOWN macros are exposed.
    pub host_control: bool,

    // MCU (the serial firmware) info + link stats, surfaced as the Klipper `mcu` object.
    pub mcu_version: String,
    pub machine_type: String,
    pub mcu_bytes_read: u64,
    pub mcu_bytes_write: u64,
    pub mcu_send_seq: u64,
    pub mcu_receive_seq: u64,
    /// Serial-link utilization 0.0–1.0, surfaced as the MCU "load".
    pub mcu_load: f64,

    // Host stats, surfaced as the Klipper `system_stats` object (see `sysstats`).
    /// 1-minute load average.
    pub sysload: f64,
    /// serial2moon's own CPU time in seconds.
    pub cputime: f64,
    /// Available host memory in kB.
    pub memavail: u64,
}

impl PrinterState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        axis_maximum: [f64; 4],
        max_velocity: f64,
        max_accel: f64,
        extruder_max_temp: f64,
        bed_max_temp: f64,
        gcode_dir: String,
        sheets: Vec<(u8, String)>,
        host_control: bool,
    ) -> Self {
        PrinterState {
            klippy_state: KlippyState::Startup,
            state_message: "serial2moon starting up".to_string(),
            extruder_temp: 0.0,
            extruder_target: 0.0,
            extruder_power: 0.0,
            bed_temp: 0.0,
            bed_target: 0.0,
            bed_power: 0.0,
            fan_speed: 0.0,
            position: [0.0; 4],
            gcode_position: [0.0; 4],
            homing_origin: [0.0; 4],
            homed_axes: String::new(),
            absolute_coordinates: true,
            absolute_extrude: true,
            speed: 0.0,
            speed_factor: 1.0,
            extrude_factor: 1.0,
            axis_minimum: [0.0, 0.0, 0.0, 0.0],
            axis_maximum,
            max_velocity,
            max_accel,
            print_state: PrintState::Standby,
            print_filename: String::new(),
            print_message: String::new(),
            total_duration: 0.0,
            print_duration: 0.0,
            filament_used: 0.0,
            sd_file_path: None,
            sd_progress: 0.0,
            sd_is_active: false,
            sd_file_position: 0,
            sd_file_size: 0,
            display_message: String::new(),
            current_layer: None,
            total_layer: None,
            extruder_max_temp,
            bed_max_temp,
            gcode_dir,
            mcu_version: "unknown".to_string(),
            machine_type: String::new(),
            mcu_bytes_read: 0,
            mcu_bytes_write: 0,
            mcu_send_seq: 0,
            mcu_receive_seq: 0,
            mcu_load: 0.0,
            sysload: 0.0,
            cputime: 0.0,
            memavail: 0,
            sheets: sheets
                .into_iter()
                .map(|(id, label)| Sheet { id, label, z: None })
                .collect(),
            host_control,
        }
    }

    fn xyz(p: &[f64; 4]) -> Value {
        json!([p[0], p[1], p[2], p[3]])
    }

    /// Render the full `{object: {field: value}}` status map.
    pub fn full_status(&self) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();

        m.insert(
            "webhooks".into(),
            json!({
                "state": self.klippy_state.as_str(),
                "state_message": self.state_message,
            }),
        );

        m.insert(
            "toolhead".into(),
            json!({
                "position": Self::xyz(&self.position),
                "homed_axes": self.homed_axes,
                "axis_minimum": Self::xyz(&self.axis_minimum),
                "axis_maximum": Self::xyz(&self.axis_maximum),
                "extruder": "extruder",
                "max_velocity": self.max_velocity,
                "max_accel": self.max_accel,
                "max_accel_to_decel": self.max_accel / 2.0,
                "square_corner_velocity": 5.0,
                "print_time": self.print_duration,
                "estimated_print_time": self.print_duration,
                "stalls": 0,
            }),
        );

        m.insert(
            "gcode_move".into(),
            json!({
                "speed_factor": self.speed_factor,
                "speed": self.speed,
                "extrude_factor": self.extrude_factor,
                "absolute_coordinates": self.absolute_coordinates,
                "absolute_extrude": self.absolute_extrude,
                "homing_origin": Self::xyz(&self.homing_origin),
                "position": Self::xyz(&self.position),
                "gcode_position": Self::xyz(&self.gcode_position),
            }),
        );

        m.insert(
            "motion_report".into(),
            json!({
                "live_position": Self::xyz(&self.position),
                "live_velocity": 0.0,
                "live_extruder_velocity": 0.0,
            }),
        );

        m.insert(
            "extruder".into(),
            json!({
                "temperature": round2(self.extruder_temp),
                "target": self.extruder_target,
                "power": self.extruder_power,
                "can_extrude": self.extruder_temp >= 170.0,
                "pressure_advance": 0.0,
                "smooth_time": 0.0,
            }),
        );

        m.insert(
            "heater_bed".into(),
            json!({
                "temperature": round2(self.bed_temp),
                "target": self.bed_target,
                "power": self.bed_power,
            }),
        );

        m.insert(
            "fan".into(),
            json!({ "speed": self.fan_speed, "rpm": Value::Null }),
        );

        // Moonraker queries `heaters.available_sensors` to decide which temperatures to
        // record in its temperature store — which is what powers Mainsail's temp graph.
        m.insert(
            "heaters".into(),
            json!({
                "available_heaters": ["extruder", "heater_bed"],
                "available_sensors": ["extruder", "heater_bed"],
            }),
        );

        // The serial firmware presented as Klipper's `mcu` object, so it appears in
        // Mainsail's Machine tab with its firmware version and link statistics.
        m.insert(
            "mcu".into(),
            json!({
                "mcu_version": self.mcu_version,
                "mcu_build_versions": concat!("serial2moon v", env!("CARGO_PKG_VERSION")),
                "mcu_constants": { "MCU": self.machine_type },
                "last_stats": {
                    // Mainsail's MCU "load" = mcu_task_avg + 3*mcu_task_stddev/0.0025 (capped
                    // at 100%), computed only when both are non-zero. We put serial-link
                    // utilization in mcu_task_avg with a tiny non-zero stddev so it renders.
                    "mcu_awake": self.mcu_load * 5.0,
                    "mcu_task_avg": self.mcu_load,
                    "mcu_task_stddev": 0.000_001,
                    "bytes_write": self.mcu_bytes_write,
                    "bytes_read": self.mcu_bytes_read,
                    "bytes_retransmit": 0,
                    "bytes_invalid": 0,
                    "send_seq": self.mcu_send_seq,
                    "receive_seq": self.mcu_receive_seq,
                    "freq": 16_000_000,
                },
            }),
        );

        // Host load / memory, as Klipper's statistics module reports them. Frontends and
        // integrations (Mainsail host stats, Home Assistant's moonraker) expect it present.
        m.insert(
            "system_stats".into(),
            json!({
                "sysload": self.sysload,
                "cputime": self.cputime,
                "memavail": self.memavail,
            }),
        );

        m.insert(
            "display_status".into(),
            json!({ "message": self.display_message, "progress": self.sd_progress }),
        );

        m.insert(
            "print_stats".into(),
            json!({
                "filename": self.print_filename,
                "total_duration": round2(self.total_duration),
                "print_duration": round2(self.print_duration),
                "filament_used": round2(self.filament_used),
                "state": self.print_state.as_str(),
                "message": self.print_message,
                "info": { "total_layer": self.total_layer, "current_layer": self.current_layer },
            }),
        );

        m.insert(
            "virtual_sdcard".into(),
            json!({
                "file_path": self.sd_file_path,
                "progress": self.sd_progress,
                "is_active": self.sd_is_active,
                "file_position": self.sd_file_position,
                "file_size": self.sd_file_size,
            }),
        );

        m.insert(
            "pause_resume".into(),
            json!({ "is_paused": self.print_state == PrintState::Paused }),
        );

        let idle_state = match self.print_state {
            PrintState::Printing | PrintState::Paused => "Printing",
            _ => "Ready",
        };
        m.insert(
            "idle_timeout".into(),
            json!({ "state": idle_state, "printing_time": round2(self.print_duration) }),
        );

        m.insert("configfile".into(), self.configfile());

        // Expose the sheet macros as printer objects too — Mainsail builds its macro list
        // (the buttons) from objects named `gcode_macro <name>`, not from the configfile.
        for sheet in &self.sheets {
            m.insert(
                format!("gcode_macro {}", sheet_macro_name(&sheet.label)),
                json!({}),
            );
        }
        // Host power-control macros (only when a host watcher is wired up).
        if self.host_control {
            m.insert("gcode_macro HOST_REBOOT".into(), json!({}));
            m.insert("gcode_macro HOST_SHUTDOWN".into(), json!({}));
        }

        m
    }

    /// The `configfile` object. Mainsail reads `settings.virtual_sdcard.path` to find
    /// the G-code directory, and the section presence to decide UI capabilities.
    fn configfile(&self) -> Value {
        let mut settings = json!({
            "virtual_sdcard": { "path": self.gcode_dir },
            "printer": {
                "kinematics": "cartesian",
                "max_velocity": self.max_velocity,
                "max_accel": self.max_accel,
            },
            // min_temp/max_temp set the allowed range for the frontend's temperature inputs.
            "extruder": {
                "min_temp": 0.0,
                "max_temp": self.extruder_max_temp,
                "min_extrude_temp": 170.0,
            },
            "heater_bed": { "min_temp": 0.0, "max_temp": self.bed_max_temp },
            "pause_resume": {},
            "display_status": {},
            // Advertise the print-control macros so frontends (Mainsail) recognize them
            // and stop warning that they are undefined. serial2moon handles these commands
            // directly in its G-code layer.
            "gcode_macro PAUSE": { "rename_existing": "BASE_PAUSE" },
            "gcode_macro RESUME": { "rename_existing": "BASE_RESUME" },
            "gcode_macro CANCEL_PRINT": { "rename_existing": "BASE_CANCEL_PRINT" },
        });
        // One selectable macro per steel sheet (parameters/metadata for the buttons).
        if let Some(obj) = settings.as_object_mut() {
            for sheet in &self.sheets {
                obj.insert(
                    format!("gcode_macro {}", sheet_macro_name(&sheet.label)),
                    json!({}),
                );
            }
            if self.host_control {
                obj.insert("gcode_macro HOST_REBOOT".into(), json!({}));
                obj.insert("gcode_macro HOST_SHUTDOWN".into(), json!({}));
            }
        }
        json!({
            "config": settings.clone(),
            "settings": settings,
            "save_config_pending": false,
            "save_config_pending_items": {},
            "warnings": [],
        })
    }
}

/// Mainsail macro name for a steel-sheet label (sanitized + uppercased), e.g.
/// "Smooth PEI" -> "SHEET_SMOOTH_PEI".
pub fn sheet_macro_name(label: &str) -> String {
    let mut s = String::from("SHEET_");
    for c in label.chars() {
        s.push(if c.is_ascii_alphanumeric() {
            c.to_ascii_uppercase()
        } else {
            '_'
        });
    }
    s
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
