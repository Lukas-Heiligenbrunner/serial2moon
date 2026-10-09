//! In-process simulated Marlin printer. Speaks the same line protocol a real printer
//! does over `tokio::io::duplex`, so it exercises the full serial code path with no hardware.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::time::{MissedTickBehavior, interval, sleep, timeout};

use super::Serial;

/// Simulated homing duration.
const HOME_TIME: Duration = Duration::from_millis(2500);
/// Max simulated heater wait (M109/M190) before giving up and acking anyway.
const HEAT_TIMEOUT: Duration = Duration::from_secs(12);

/// How the simulated printer behaves.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Behave like Prusa firmware (MK3S): identify as Prusa-Firmware, check line numbers
    /// strictly (flushing input on a mismatch, as `FlushSerialRequestResend` does), and run
    /// M600/M601/M602/M603 with host actions and resend-from-saved-line on resume.
    pub prusa: bool,
    /// With `prusa`: run out of filament when this line number arrives.
    pub runout_at: Option<u64>,
}

/// Open a mock printer; returns the host-side end of the duplex pipe.
pub fn open(options: Options) -> Box<dyn Serial> {
    let (host, printer) = tokio::io::duplex(8192);
    tokio::spawn(run(printer, options));
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
    /// M83: E is relative even in absolute mode.
    rel_e: bool,
    autoreport_secs: u64,
    prusa: bool,
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

async fn run<S: Serial>(stream: S, options: Options) {
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut sim = Sim::new();
    sim.prusa = options.prusa;

    // Prusa mode: the firmware's line counter (gcode_LastN), the line saved by a pause,
    // and the position before each recent line (to undo moves a runout discards).
    let mut last_n: u64 = 0;
    let mut paused_at: Option<u64> = None;
    let mut runout_done = false;
    let mut recent: VecDeque<(u64, [f64; 4])> = VecDeque::new();

    let mut ticker = interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut since_report = 0u64; // milliseconds since last autoreport

    // Test hook: when set, request one resend to exercise the host's recovery path.
    let force_resend = std::env::var("S2M_MOCK_FORCE_RESEND").is_ok();
    let mut did_resend = false;

    // Test hook: when set, swallow the `ok` for one command (simulating an `ok` lost on the
    // wire), then answer the host's eventual re-send with `Resend: N+1` ("I already have N").
    let drop_ok = std::env::var("S2M_MOCK_DROP_OK").is_ok();
    let mut dropped: Option<u64> = None;

    // Test hook: simulate a printer whose line counter is stuck (e.g. survived a host
    // reconnect while a partial line jammed the reset). It rejects every *framed* line whose
    // number isn't last+1 — including M110, as if the reset never parsed — until the host
    // realigns to the number it demands. Unframed lines (handshake M115) still pass.
    let mut stuck_last: Option<u64> = std::env::var("S2M_MOCK_DESYNC")
        .ok()
        .and_then(|v| v.parse().ok());

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

                // Once, reject the first numbered command the way Prusa/Marlin does on a
                // corrupted line: `Error:`, then `Resend: N` immediately followed by the
                // request's own `ok`. This exercises the host's recovery: neither the error
                // nor that `ok` may count as the line's ack, or the line counter desyncs.
                // (Every `Resend:` below carries its `ok` too, as on the real firmware.)
                if force_resend && !did_resend && let Some(n) = line_number(raw) && n >= 1 {
                    did_resend = true;
                    let _ = write
                        .write_all(
                            format!("Error:checksum mismatch, Last Line: {}\n", n - 1).as_bytes(),
                        )
                        .await;
                    let _ = write.write_all(format!("Resend: {n}\nok\n").as_bytes()).await;
                    continue;
                }

                // Simulate a stuck line counter: reject framed lines that aren't last+1
                // (M110 included) until the host adopts the demanded number.
                if let Some(prev) = stuck_last
                    && let Some(n) = line_number(raw)
                {
                    if n == prev + 1 {
                        stuck_last = Some(n); // realigned — accept from here on
                    } else {
                        let _ = write
                            .write_all(
                                format!("Error:Line Number is not Last Line Number+1, Last Line: {prev}\n")
                                    .as_bytes(),
                            )
                            .await;
                        let _ = write.write_all(format!("Resend: {}\nok\n", prev + 1).as_bytes()).await;
                        continue;
                    }
                }

                // Simulate a lost `ok`: process one command but send its reply WITHOUT the
                // trailing `ok`; on the host's re-send of that line, report `Resend: N+1`.
                if drop_ok && let Some(n) = line_number(raw) {
                    if dropped == Some(n) {
                        let _ = write.write_all(format!("Resend: {}\nok\n", n + 1).as_bytes()).await;
                        continue;
                    }
                    if dropped.is_none() && n >= 1 {
                        dropped = Some(n);
                        let reply = handle(strip_framing(raw), &mut sim);
                        let body: String = reply
                            .lines()
                            .filter(|l| l.trim() != "ok")
                            .map(|l| format!("{l}\n"))
                            .collect();
                        let _ = write.write_all(body.as_bytes()).await;
                        continue;
                    }
                }

                if options.prusa {
                    let cmd = strip_framing(raw);
                    let upper = cmd.to_ascii_uppercase();
                    let word = upper.split_whitespace().next().unwrap_or("");
                    if let Some(n) = line_number(raw) {
                        if word == "M110" {
                            last_n = parse_axis(&upper, 'N').map_or(n, |v| v as u64);
                        } else if n != last_n + 1 {
                            flush_input(&mut lines).await;
                            let _ = write
                                .write_all(
                                    format!("Error:Line Number is not Last Line Number+1, Last Line: {last_n}\nResend: {}\nok\n", last_n + 1)
                                        .as_bytes(),
                                )
                                .await;
                            continue;
                        } else if options.runout_at == Some(n) && !runout_done && n > 3 {
                            // Filament runout: lines n-3..=n were still queued or planned. The
                            // firmware acks and discards them, returns to where it was, asks
                            // for them again and runs a filament change before reading on.
                            runout_done = true;
                            let from = n - 3;
                            if let Some(&(_, pos)) = recent.iter().find(|(k, _)| *k == from) {
                                sim.pos = pos;
                            }
                            recent.retain(|(k, _)| *k < from);
                            last_n = from - 1;
                            let _ = write
                                .write_all(b"//action:notification Filament Runout Detected\nok\n")
                                .await;
                            sleep(Duration::from_millis(50)).await; // moving back into place
                            flush_input(&mut lines).await;
                            let _ = write.write_all(format!("Resend: {from}\nok\n").as_bytes()).await;
                            filament_change(&mut write).await;
                            continue;
                        } else {
                            last_n = n;
                            recent.push_back((n, sim.pos));
                            if recent.len() > 16 {
                                recent.pop_front();
                            }
                        }
                    }
                    match word {
                        "M600" => {
                            filament_change(&mut write).await;
                            let _ = write.write_all(b"ok\n").await;
                            continue;
                        }
                        // Acks first, then pauses (saving the last line it got) and reports it.
                        "M601" => {
                            let reply: &[u8] = if paused_at.is_none() {
                                paused_at = Some(last_n);
                                b"ok\n//action:paused\n"
                            } else {
                                b"ok\n"
                            };
                            let _ = write.write_all(reply).await;
                            continue;
                        }
                        // Rewinds to the saved line and asks for what follows it, then acks
                        // the M602 itself.
                        "M602" => {
                            if let Some(saved) = paused_at.take() {
                                last_n = saved;
                                flush_input(&mut lines).await;
                                let _ = write
                                    .write_all(
                                        format!("Resend: {}\nok\n//action:resumed\nok\n", saved + 1)
                                            .as_bytes(),
                                    )
                                    .await;
                            } else {
                                let _ = write.write_all(b"ok\n").await;
                            }
                            continue;
                        }
                        "M603" => {
                            paused_at = None;
                            let _ = write
                                .write_all(b"//action:cancel\necho:mock: print stopped\nok\n")
                                .await;
                            continue;
                        }
                        _ => {}
                    }
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

/// Discard input received but not yet processed, like the firmware's `MYSERIAL.flush()`
/// before it requests a resend.
async fn flush_input<R: AsyncBufRead + Unpin>(lines: &mut Lines<R>) {
    while let Ok(Ok(Some(_))) = timeout(Duration::from_millis(5), lines.next_line()).await {}
}

/// Prusa M600 as the host sees it: paused report, user-wait keepalives, resumed report.
async fn filament_change<W: AsyncWriteExt + Unpin>(write: &mut W) {
    let _ = write.write_all(b"//action:paused\n").await;
    for _ in 0..3 {
        sleep(Duration::from_millis(100)).await;
        let _ = write.write_all(b"echo:busy: paused for user\n").await;
    }
    let _ = write.write_all(b"//action:resumed\n").await;
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
        "M115" if sim.prusa => "FIRMWARE_NAME:Prusa-Firmware 3.14.1 based on Marlin \
             FIRMWARE_URL:https://github.com/prusa3d/Prusa-Firmware PROTOCOL_VERSION:1.0 \
             MACHINE_TYPE:Prusa i3 MK3S EXTRUDER_COUNT:1\nCap:AUTOREPORT_TEMP:1\nok\n"
            .to_string(),
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
        "M82" | "M83" => {
            sim.rel_e = word == "M83";
            "ok\n".to_string()
        }
        "G0" | "G1" => {
            for (i, axis) in ['X', 'Y', 'Z', 'E'].into_iter().enumerate() {
                if let Some(v) = parse_axis(&upper, axis) {
                    if sim.absolute && !(i == 3 && sim.rel_e) {
                        sim.pos[i] = v;
                    } else {
                        sim.pos[i] += v;
                    }
                }
            }
            "ok\n".to_string()
        }
        // Prusa's format (no `A:`/`B:` stepper counts, which would read as a bed temp).
        "M114" => {
            let [x, y, z, e] = sim.pos;
            format!(
                "X:{x:.2} Y:{y:.2} Z:{z:.2} E:{e:.2} Count X: {x:.2} Y:{y:.2} Z:{z:.2} E:{e:.2}\nok\n"
            )
        }
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
