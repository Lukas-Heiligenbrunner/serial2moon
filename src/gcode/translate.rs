//! Execute a `gcode/script` payload: intercept Klipper-specific commands, translate
//! Klipper-isms to Marlin, forward standard G-code to the printer.
//!
//! Invariant (the load-bearing one): **every command must complete with success** so the
//! Klipper API call returns and Moonraker never hangs. Unknown extended commands are
//! acked-and-logged rather than dropped silently.

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::app::App;
use crate::state::KlippyState;

/// Run a (possibly multi-line) script to completion.
pub async fn execute(app: &App, script: &str) -> Result<()> {
    for raw in script.split('\n') {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        execute_line(app, line).await?;
    }
    Ok(())
}

async fn execute_line(app: &App, line: &str) -> Result<()> {
    let word = line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();

    match word.as_str() {
        // ---- Print control (handled by the print job manager) ----
        "SDCARD_PRINT_FILE" => {
            let filename = param_str(line, "FILENAME").unwrap_or_default();
            if let Err(e) = app.print.start(app, &filename).await {
                // Make the failure visible: a quiet error here looks like "nothing happens"
                // in the UI. The usual cause is a gcode-dir mismatch with Moonraker.
                let dir = app.config.gcode_dir.display();
                warn!(%filename, gcode_dir = %dir, error = %e, "failed to start print");
                let _ = app.console.send(format!(
                    "!! Cannot start '{filename}': {e}. Searched gcode dir: {dir}"
                ));
                return Err(e);
            }
        }
        "PAUSE" => app.print.pause(app).await?,
        "RESUME" => app.print.resume(app).await?,
        "CANCEL_PRINT" => app.print.cancel(app).await?,

        // ---- Emergency stop ----
        "M112" => emergency_stop(app).await?,

        // ---- Klipper-isms translated to Marlin ----
        "SET_HEATER_TEMPERATURE" => {
            let heater = param_str(line, "HEATER").unwrap_or_else(|| "extruder".into());
            let target = param_f64(line, "TARGET").unwrap_or(0.0);
            if heater.eq_ignore_ascii_case("heater_bed") {
                set_bed_target(app, target).await?;
            } else {
                set_ext_target(app, target).await?;
            }
        }
        "TURN_OFF_HEATERS" => {
            set_ext_target(app, 0.0).await?;
            set_bed_target(app, 0.0).await?;
        }
        "SET_FAN_SPEED" => {
            let speed = param_f64(line, "SPEED").unwrap_or(0.0).clamp(0.0, 1.0);
            forward_fan(app, speed).await?;
        }

        // ---- No-op Klipper features (ack only) ----
        w if NOOP_COMMANDS.contains(&w) => {
            debug!(command = w, "acked Klipper-only command as no-op");
        }

        // ---- Standard G/M codes ----
        w if is_marlin_code(w) => {
            optimistic_update(app, line, w);
            app.serial.send_high(line.to_string()).await?;
        }

        // ---- Unknown extended command: ack and log, never hang ----
        other => {
            info!(command = other, "unknown command acked as no-op");
        }
    }
    Ok(())
}

/// Klipper macros/commands we accept but don't forward to a Marlin printer.
const NOOP_COMMANDS: &[&str] = &[
    "SET_PRESSURE_ADVANCE",
    "SET_VELOCITY_LIMIT",
    "SET_GCODE_OFFSET",
    "SET_GCODE_VARIABLE",
    "BED_MESH_CALIBRATE",
    "BED_MESH_PROFILE",
    "BED_MESH_CLEAR",
    "Z_TILT_ADJUST",
    "QUAD_GANTRY_LEVEL",
    "SAVE_CONFIG",
    "RESPOND",
    "STATUS",
    "CLEAR_PAUSE",
    "SET_DISPLAY_GROUP",
];

/// True for `G<digit...>` / `M<digit...>` style codes.
fn is_marlin_code(word: &str) -> bool {
    let mut chars = word.chars();
    matches!(chars.next(), Some('G') | Some('M') | Some('T'))
        && chars.next().is_some_and(|c| c.is_ascii_digit())
}

async fn emergency_stop(app: &App) -> Result<()> {
    let _ = app.serial.send_high("M112").await;
    app.state.update(|s| {
        s.klippy_state = KlippyState::Shutdown;
        s.state_message = "Emergency stop (M112)".to_string();
    });
    Ok(())
}

async fn set_ext_target(app: &App, target: f64) -> Result<()> {
    app.state.update(move |s| s.extruder_target = target);
    app.serial.send_high(format!("M104 S{target:.0}")).await
}

async fn set_bed_target(app: &App, target: f64) -> Result<()> {
    app.state.update(move |s| s.bed_target = target);
    app.serial.send_high(format!("M140 S{target:.0}")).await
}

async fn forward_fan(app: &App, speed: f64) -> Result<()> {
    app.state.update(move |s| s.fan_speed = speed);
    if speed <= 0.0 {
        app.serial.send_high("M107").await
    } else {
        app.serial
            .send_high(format!("M106 S{:.0}", speed * 255.0))
            .await
    }
}

/// Reflect a forwarded standard command in state immediately (the UI shouldn't wait for
/// the next temperature report to see a new target).
fn optimistic_update(app: &App, line: &str, word: &str) {
    match word {
        "M104" | "M109" => {
            if let Some(s) = axis_f64(line, 'S') {
                app.state.update(move |st| st.extruder_target = s);
            }
        }
        "M140" | "M190" => {
            if let Some(s) = axis_f64(line, 'S') {
                app.state.update(move |st| st.bed_target = s);
            }
        }
        "M106" => {
            let s = axis_f64(line, 'S').unwrap_or(255.0) / 255.0;
            app.state.update(move |st| st.fan_speed = s.clamp(0.0, 1.0));
        }
        "M107" => app.state.update(|st| st.fan_speed = 0.0),
        "M220" => {
            if let Some(s) = axis_f64(line, 'S') {
                app.state.update(move |st| st.speed_factor = s / 100.0);
            }
        }
        "M221" => {
            if let Some(s) = axis_f64(line, 'S') {
                app.state.update(move |st| st.extrude_factor = s / 100.0);
            }
        }
        "G90" => app.state.update(|st| st.absolute_coordinates = true),
        "G91" => app.state.update(|st| st.absolute_coordinates = false),
        "M117" => {
            let msg = line["M117".len()..].trim().to_string();
            app.state.update(move |st| st.display_message = msg);
        }
        _ => {}
    }
}

/// Extract `KEY=VALUE` (Klipper style) as a string. A quoted value may contain spaces
/// (e.g. `FILENAME="Shelly Mini Lid.gcode"`), so we cannot split on whitespace first.
fn param_str(line: &str, key: &str) -> Option<String> {
    let upper = line.to_ascii_uppercase();
    let needle = format!("{}=", key.to_ascii_uppercase());
    let mut from = 0;
    loop {
        let idx = from + upper[from..].find(&needle)?;
        // Require a token boundary before the key so we don't match inside a word.
        if idx == 0 || line.as_bytes()[idx - 1].is_ascii_whitespace() {
            return Some(parse_value(&line[idx + needle.len()..]));
        }
        from = idx + needle.len();
    }
}

/// Parse a parameter value: a quoted run (single or double) including spaces, else a
/// single whitespace-delimited token.
fn parse_value(after: &str) -> String {
    match after.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let rest = &after[q.len_utf8()..];
            match rest.find(q) {
                Some(end) => rest[..end].to_string(),
                None => rest.to_string(),
            }
        }
        _ => {
            let end = after.find(char::is_whitespace).unwrap_or(after.len());
            after[..end].to_string()
        }
    }
}

fn param_f64(line: &str, key: &str) -> Option<f64> {
    param_str(line, key).and_then(|v| v.parse().ok())
}

/// Extract a `<Axis><number>` token (Marlin style), e.g. `S200` -> 200.0.
fn axis_f64(line: &str, axis: char) -> Option<f64> {
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix(axis).and_then(|v| v.parse::<f64>().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_klipper_params() {
        let line = r#"SDCARD_PRINT_FILE FILENAME="benchy.gcode""#;
        assert_eq!(param_str(line, "FILENAME").as_deref(), Some("benchy.gcode"));
        let line = "SET_HEATER_TEMPERATURE HEATER=heater_bed TARGET=60";
        assert_eq!(param_str(line, "HEATER").as_deref(), Some("heater_bed"));
        assert_eq!(param_f64(line, "TARGET"), Some(60.0));
    }

    #[test]
    fn parses_quoted_filename_with_spaces() {
        let line = r#"SDCARD_PRINT_FILE FILENAME="Shelly Mini Lid_0.15mm_PETG_MK3S_3h47m.gcode""#;
        assert_eq!(
            param_str(line, "FILENAME").as_deref(),
            Some("Shelly Mini Lid_0.15mm_PETG_MK3S_3h47m.gcode")
        );
        // single quotes and a subdirectory path
        let line = "SDCARD_PRINT_FILE FILENAME='sub dir/My Part.gcode'";
        assert_eq!(
            param_str(line, "FILENAME").as_deref(),
            Some("sub dir/My Part.gcode")
        );
        // unquoted still works
        let line = "SDCARD_PRINT_FILE FILENAME=plain.gcode";
        assert_eq!(param_str(line, "FILENAME").as_deref(), Some("plain.gcode"));
    }

    #[test]
    fn parses_marlin_axes() {
        assert_eq!(axis_f64("M104 S200", 'S'), Some(200.0));
        assert_eq!(axis_f64("G1 X10 Y20.5 E1.2", 'Y'), Some(20.5));
        assert_eq!(axis_f64("G28", 'X'), None);
    }

    #[test]
    fn recognizes_marlin_codes() {
        assert!(is_marlin_code("M104"));
        assert!(is_marlin_code("G1"));
        assert!(is_marlin_code("T0"));
        assert!(!is_marlin_code("PAUSE"));
        assert!(!is_marlin_code("SET_HEATER_TEMPERATURE"));
    }
}
