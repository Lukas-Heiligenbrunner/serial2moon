//! In-process simulated Marlin printer. Speaks the same line protocol a real printer
//! does over `tokio::io::duplex`, so it exercises the full serial code path with no hardware.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::{MissedTickBehavior, interval, sleep};

use super::Serial;

/// Simulated homing duration.
const HOME_TIME: Duration = Duration::from_millis(2500);
/// Max simulated heater wait (M109/M190) before giving up and acking anyway.
const HEAT_TIMEOUT: Duration = Duration::from_secs(12);

/// Open a mock printer; returns the host-side end of the duplex pipe.
pub fn open() -> Box<dyn Serial> {
    let (host, printer) = tokio::io::duplex(8192);
    tokio::spawn(run(printer));
    Box::new(host)
}

#[derive(Default)]
struct Sim {
    ext_temp: f64,
    ext_target: f64,
    bed_temp: f64,
    bed_target: f64,
    fan: f64,
    pos: [f64; 4],
    absolute: bool,
    autoreport_secs: u64,
}

impl Sim {
    fn new() -> Self {
        Sim {
            ext_temp: 22.0,
            bed_temp: 22.0,
            absolute: true,
            ..Default::default()
        }
    }

    /// Move temperatures toward their targets (and toward ambient when off).
    fn tick(&mut self) {
        self.ext_temp += (self.ext_target.max(22.0) - self.ext_temp) * 0.18;
        self.bed_temp += (self.bed_target.max(22.0) - self.bed_temp) * 0.10;
    }

    fn temp_report(&self) -> String {
        let ep = if self.ext_temp < self.ext_target {
            1.0
        } else {
            0.0
        };
        let bp = if self.bed_temp < self.bed_target {
            1.0
        } else {
            0.0
        };
        format!(
            "T:{:.2} /{:.2} B:{:.2} /{:.2} @:{:.0} B@:{:.0}",
            self.ext_temp,
            self.ext_target,
            self.bed_temp,
            self.bed_target,
            ep * 127.0,
            bp * 127.0
        )
    }
}

fn parse_axis(cmd: &str, axis: char) -> Option<f64> {
    cmd.split_whitespace()
        .find_map(|tok| tok.strip_prefix(axis).and_then(|v| v.parse::<f64>().ok()))
}

/// Strip the Marlin `N<n> ` line-number prefix and `*<checksum>` suffix, leaving the bare
/// command (a real printer does the same before parsing).
fn strip_framing(s: &str) -> &str {
    let s = s.trim();
    let s = s.split('*').next().unwrap_or(s);
    if let Some(rest) = s.strip_prefix('N') {
        rest.trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start()
    } else {
        s
    }
}

/// Extract the line number from a framed command (`N5 ...` -> 5).
fn line_number(s: &str) -> Option<u64> {
    s.trim()
        .strip_prefix('N')?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

async fn run<S: Serial>(stream: S) {
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut sim = Sim::new();

    let mut ticker = interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut since_report = 0u64; // milliseconds since last autoreport

    // Test hook: when set, request one resend to exercise the host's recovery path.
    let force_resend = std::env::var("S2M_MOCK_FORCE_RESEND").is_ok();
    let mut did_resend = false;

    // Marlin prints a banner on power-up.
    let _ = write.write_all(b"start\n").await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                sim.tick();
                if sim.autoreport_secs > 0 {
                    since_report += 250;
                    if since_report >= sim.autoreport_secs * 1000 {
                        since_report = 0;
                        let _ = write.write_all(format!("{}\n", sim.temp_report()).as_bytes()).await;
                    }
                }
            }
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let raw = line.trim();
                if raw.is_empty() { continue; }

                // Once, demand a resend of the first numbered command (no `ok`).
                if force_resend && !did_resend && let Some(n) = line_number(raw) && n >= 1 {
                    did_resend = true;
                    let _ = write.write_all(format!("Resend: {n}\n").as_bytes()).await;
                    continue;
                }

                let cmd = strip_framing(raw);
                let upper = cmd.to_ascii_uppercase();
                let word = upper.split_whitespace().next().unwrap_or("");
                let reply = handle(cmd, &mut sim);

                // Emulate the time slow commands take on a real machine. (Per-move pacing
                // is handled by the print job, independent of file size.)
                match word {
                    "G28" => sleep(HOME_TIME).await,
                    "M109" | "M190" => wait_for_temp(&mut sim, word, &mut write).await,
                    _ => {}
                }

                if write.write_all(reply.as_bytes()).await.is_err() { break; }
            }
        }
    }
}

/// Block (like Marlin's `M109`/`M190`) until the relevant heater reaches its target,
/// emitting temperature lines as it climbs so the frontend shows the ramp.
async fn wait_for_temp<W: AsyncWriteExt + Unpin>(sim: &mut Sim, word: &str, write: &mut W) {
    let start = tokio::time::Instant::now();
    loop {
        sim.tick();
        let _ = write
            .write_all(format!("{}\n", sim.temp_report()).as_bytes())
            .await;
        let reached = match word {
            "M109" => sim.ext_target > 0.0 && (sim.ext_target - sim.ext_temp).abs() < 2.0,
            "M190" => sim.bed_target > 0.0 && (sim.bed_target - sim.bed_temp).abs() < 2.0,
            _ => true,
        };
        if reached || start.elapsed() >= HEAT_TIMEOUT {
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }
}

/// Produce the Marlin reply for one command (always ends with an `ok` line).
fn handle(cmd: &str, sim: &mut Sim) -> String {
    let upper = cmd.to_ascii_uppercase();
    let word = upper.split_whitespace().next().unwrap_or("");

    match word {
        "M115" => "FIRMWARE_NAME:Marlin 2.1.2 (serial2moon-mock) SOURCE_CODE_URL:n/a \
             PROTOCOL_VERSION:1.0 MACHINE_TYPE:Mock EXTRUDER_COUNT:1 \
             Cap:AUTOREPORT_TEMP:1 Cap:EEPROM:1\nok\n"
            .to_string(),
        "M105" => format!("ok {}\n", sim.temp_report()),
        "M155" => {
            sim.autoreport_secs = parse_axis(&upper, 'S').map(|v| v as u64).unwrap_or(0);
            "ok\n".to_string()
        }
        "M104" | "M109" => {
            if let Some(s) = parse_axis(&upper, 'S') {
                sim.ext_target = s;
            }
            "ok\n".to_string()
        }
        "M140" | "M190" => {
            if let Some(s) = parse_axis(&upper, 'S') {
                sim.bed_target = s;
            }
            "ok\n".to_string()
        }
        "M106" => {
            sim.fan = parse_axis(&upper, 'S').unwrap_or(255.0) / 255.0;
            "ok\n".to_string()
        }
        "M107" => {
            sim.fan = 0.0;
            "ok\n".to_string()
        }
        "G28" => {
            sim.pos = [0.0, 0.0, 0.0, sim.pos[3]];
            "ok\n".to_string()
        }
        "G90" => {
            sim.absolute = true;
            "ok\n".to_string()
        }
        "G91" => {
            sim.absolute = false;
            "ok\n".to_string()
        }
        "G0" | "G1" => {
            for (i, axis) in ['X', 'Y', 'Z', 'E'].into_iter().enumerate() {
                if let Some(v) = parse_axis(&upper, axis) {
                    if sim.absolute {
                        sim.pos[i] = v;
                    } else {
                        sim.pos[i] += v;
                    }
                }
            }
            "ok\n".to_string()
        }
        "M114" => format!(
            "X:{:.2} Y:{:.2} Z:{:.2} E:{:.2} Count A:0 B:0 C:0\nok\n",
            sim.pos[0], sim.pos[1], sim.pos[2], sim.pos[3]
        ),
        "M112" => "ok\n".to_string(),
        "M850" => {
            // Mimic Prusa's sheet report: sheets 0 and 1 calibrated, the rest uncalibrated.
            match parse_axis(&upper, 'S').map(|v| v as u8) {
                Some(0) | None => "Sheet 0 Z-1.0000 R-400 LSmooth B60 P0 A1\nok\n".to_string(),
                Some(1) => "Sheet 1 Z-1.2000 R-480 LTextur B0 P0 A0\nok\n".to_string(),
                Some(_) => "ok\n".to_string(),
            }
        }
        _ => "ok\n".to_string(),
    }
}
