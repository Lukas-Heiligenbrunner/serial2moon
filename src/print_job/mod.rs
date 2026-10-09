//! Print job manager. serial2moon owns reading the G-code file from disk and streaming
//! it to the printer (this is what Klipper's virtual_sdcard does), updating progress and
//! print_stats as it goes. Pause/resume/cancel are driven over a `watch` channel.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, watch};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::app::App;
use crate::config::TransportKind;
use crate::state::{PrintState, StateHandle};

/// Don't bother sleeping for paces shorter than this (avoids thousands of sub-ms sleeps
/// on large files); accumulate the budget and sleep in coarser chunks.
const MIN_PACE_SLEEP: f64 = 0.005;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrintCmd {
    Run,
    /// The user asked to pause: park (Prusa: M601) and wait for resume.
    Pause,
    /// The printer paused by itself and parked: just stop sending until it resumes.
    Hold,
    Cancel,
    /// Hard abort with no clean-up G-code (e.g. the printer reset under us).
    Abort,
}

/// `watch::Sender::send_if_modified` helper: switch `cmd` from `from` to `to`.
fn replace_if(cmd: &mut PrintCmd, from: PrintCmd, to: PrintCmd) -> bool {
    let hit = *cmd == from;
    if hit {
        *cmd = to;
    }
    hit
}

/// Wall-clock bookkeeping for one print: time since start and time spent paused. The
/// caller passes the current time in, so the arithmetic is testable without waiting.
struct PrintClock {
    start: Instant,
    paused: Duration,
    paused_since: Option<Instant>,
}

impl PrintClock {
    fn new(now: Instant) -> Self {
        PrintClock {
            start: now,
            paused: Duration::ZERO,
            paused_since: None,
        }
    }

    fn pause(&mut self, now: Instant) {
        self.paused_since.get_or_insert(now);
    }

    fn resume(&mut self, now: Instant) {
        if let Some(since) = self.paused_since.take() {
            self.paused += now.saturating_duration_since(since);
        }
    }

    /// `(total, paused)` seconds at `now`; an ongoing pause counts as paused.
    fn times(&self, now: Instant) -> (f64, f64) {
        let ongoing = self
            .paused_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        (
            now.saturating_duration_since(self.start).as_secs_f64(),
            (self.paused + ongoing).as_secs_f64(),
        )
    }
}

type SharedClock = Arc<std::sync::Mutex<PrintClock>>;

fn publish_durations(state: &StateHandle, clock: &SharedClock) {
    let (total, paused) = clock.lock().unwrap().times(Instant::now());
    state.update(move |s| s.update_durations(total, paused));
}

/// Keep the durations moving once a second, also through long heat-up waits and pauses
/// when no lines are sent.
async fn tick_durations(state: StateHandle, clock: SharedClock) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        publish_durations(&state, &clock);
    }
}

enum Outcome {
    Completed,
    /// `in_firmware_pause`: the printer holds the print in its own paused state (M601).
    Cancelled {
        in_firmware_pause: bool,
    },
    Aborted,
}

/// Handle to the (at most one) active print. Cheap to clone.
#[derive(Clone, Default)]
pub struct PrintHandle {
    active: Arc<Mutex<Option<watch::Sender<PrintCmd>>>>,
}

impl PrintHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin printing `filename` (resolved within the configured gcode dir).
    pub async fn start(&self, app: &App, filename: &str) -> Result<()> {
        let mut guard = self.active.lock().await;
        if guard.is_some() {
            bail!("a print is already in progress");
        }

        let path = app.config.gcode_dir.join(filename);
        let size = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("cannot open gcode file: {}", path.display()))?
            .len();

        let (tx, rx) = watch::channel(PrintCmd::Run);
        *guard = Some(tx);
        drop(guard);

        info!(filename, size, "print started");
        let fname = filename.to_string();
        let disp = path.to_string_lossy().to_string();
        app.state.update(move |s| {
            s.reset_print_stats();
            s.print_state = PrintState::Printing;
            s.print_filename = fname;
            s.sd_file_path = Some(disp);
            s.sd_file_size = size;
            s.sd_is_active = true;
        });

        let app = app.clone();
        let handle = self.clone();
        let clock: SharedClock = Arc::new(std::sync::Mutex::new(PrintClock::new(Instant::now())));
        tokio::spawn(async move {
            let ticker = tokio::spawn(tick_durations(app.state.clone(), clock.clone()));
            let result = stream_file(&app, &path, size, rx, &clock).await;
            ticker.abort();
            publish_durations(&app.state, &clock); // final values, as the print ended
            handle.finish(&app, result).await;
        });
        Ok(())
    }

    pub async fn pause(&self, state: &StateHandle) -> Result<()> {
        let guard = self.active.lock().await;
        let tx = guard.as_ref().context("no active print to pause")?;
        let _ = tx.send(PrintCmd::Pause);
        state.update(|s| s.print_state = PrintState::Paused);
        Ok(())
    }

    pub async fn resume(&self, state: &StateHandle) -> Result<()> {
        let guard = self.active.lock().await;
        let tx = guard.as_ref().context("no active print to resume")?;
        let _ = tx.send(PrintCmd::Run);
        state.update(|s| s.print_state = PrintState::Printing);
        Ok(())
    }

    pub async fn cancel(&self) -> Result<()> {
        let guard = self.active.lock().await;
        let tx = guard.as_ref().context("no active print to cancel")?;
        let _ = tx.send(PrintCmd::Cancel);
        Ok(())
    }

    /// Abort the active print without sending clean-up G-code. Used when the printer
    /// resets mid-print: there is nothing safe to send, so just fail the job.
    /// No-op if nothing is printing.
    pub async fn abort(&self) {
        if let Some(tx) = self.active.lock().await.as_ref() {
            let _ = tx.send(PrintCmd::Abort);
        }
    }

    /// Klipper `SDCARD_RESET_FILE` (Mainsail's "clear" after a print): back to standby.
    /// Unlike Klipper it refuses to touch an active print rather than stopping it.
    pub async fn reset_file(&self, state: &StateHandle) -> Result<()> {
        // Hold the lock across the update so a print starting concurrently is ordered after.
        let guard = self.active.lock().await;
        if guard.is_some() {
            bail!("a print is in progress; cancel it first");
        }
        state.update(|s| s.reset_print_stats());
        drop(guard);
        Ok(())
    }

    /// The printer reported pausing by itself (`//action:paused`: an M600 filament change,
    /// including one triggered by filament runout, or our own M601). It parked already, so
    /// only hold the stream; a pause the user asked for stays as it is.
    pub async fn firmware_paused(&self, state: &StateHandle) {
        let guard = self.active.lock().await;
        if let Some(tx) = guard.as_ref() {
            tx.send_if_modified(|c| replace_if(c, PrintCmd::Run, PrintCmd::Hold));
        }
        let printing = guard.is_some();
        state.update(move |s| {
            s.firmware_paused = true;
            if printing {
                s.print_state = PrintState::Paused;
            }
        });
    }

    /// The printer reported resuming by itself (`//action:resumed`): release a hold. A
    /// pause the user asked for stays until they resume.
    pub async fn firmware_resumed(&self, state: &StateHandle) {
        let guard = self.active.lock().await;
        let released = guard.as_ref().is_some_and(|tx| {
            tx.send_if_modified(|c| replace_if(c, PrintCmd::Hold, PrintCmd::Run))
        });
        state.update(move |s| {
            s.firmware_paused = false;
            if released {
                s.print_state = PrintState::Printing;
            }
        });
    }

    async fn finish(&self, app: &App, result: Result<Outcome>) {
        *self.active.lock().await = None;
        match result {
            Ok(Outcome::Completed) => {
                info!("print complete");
                app.state.update(|s| {
                    s.print_state = PrintState::Complete;
                    s.sd_progress = 1.0;
                    s.sd_is_active = false;
                });
            }
            Ok(Outcome::Cancelled { in_firmware_pause }) => {
                info!(in_firmware_pause, "print cancelled");
                if in_firmware_pause || app.state.snapshot().firmware_paused {
                    // The printer holds the paused print: its own stop (M603) lifts, parks,
                    // cools and clears that state. Parking from here would fight it.
                    let _ = app.serial.send_control("M603").await;
                } else {
                    park(app).await; // lift + present (retract while still warm)
                }
                // Safety: drop heaters and fan, and release the steppers.
                let _ = app.serial.send_high("M104 S0").await;
                let _ = app.serial.send_high("M140 S0").await;
                let _ = app.serial.send_high("M107").await;
                let _ = app.serial.send_high("M84").await;
                app.state.update(|s| {
                    s.print_state = PrintState::Cancelled;
                    s.firmware_paused = false;
                    s.sd_is_active = false;
                    s.extruder_target = 0.0;
                    s.bed_target = 0.0;
                });
            }
            Ok(Outcome::Aborted) => {
                warn!("print aborted (printer reset)");
                app.state.update(|s| {
                    s.print_state = PrintState::Error;
                    s.print_message = "Printer reset during print".to_string();
                    s.sd_is_active = false;
                });
            }
            Err(e) => {
                warn!(error = %e, "print failed");
                let msg = e.to_string();
                app.state.update(move |s| {
                    s.print_state = PrintState::Error;
                    s.print_message = msg;
                    s.sd_is_active = false;
                });
            }
        }
    }
}

fn strip_comment(raw: &str) -> &str {
    let cut = raw.find(';').unwrap_or(raw.len());
    raw[..cut].trim()
}

/// Park the toolhead away from the print: retract, lift Z, and present the bed (Y front).
/// Sent high-priority so it jumps ahead of the (paused) print feed.
async fn park(app: &App) {
    let lift = app.config.pause_z_lift;
    let retract = app.config.pause_retract;
    let park_y = app.config.axis_maximum()[1];

    let _ = app.serial.send_high("M83").await; // relative extrusion for the retract
    if retract > 0.0 {
        let _ = app
            .serial
            .send_high(format!("G1 E-{retract:.2} F2400"))
            .await;
    }
    let _ = app.serial.send_high("G91").await; // relative moves for the lift
    if lift > 0.0 {
        let _ = app.serial.send_high(format!("G1 Z{lift:.2} F600")).await;
    }
    let _ = app.serial.send_high("G90").await; // back to absolute
    let _ = app
        .serial
        .send_high(format!("G1 X0 Y{park_y:.0} F3000"))
        .await;
}

/// Reverse [`park`]: return to the saved position and unretract, restoring the E mode the
/// print was using so the stream continues seamlessly.
async fn unpark(app: &App, saved: [f64; 4], absolute_extrude: bool) {
    let retract = app.config.pause_retract;
    let [x, y, z, _] = saved;
    let _ = app.serial.send_high("G90").await;
    let _ = app
        .serial
        .send_high(format!("G1 X{x:.2} Y{y:.2} F3000"))
        .await;
    let _ = app.serial.send_high(format!("G1 Z{z:.2} F600")).await;
    if retract > 0.0 {
        let _ = app.serial.send_high("M83").await;
        let _ = app
            .serial
            .send_high(format!("G1 E{retract:.2} F2400"))
            .await;
    }
    let _ = app
        .serial
        .send_high(if absolute_extrude { "M82" } else { "M83" })
        .await;
}

async fn stream_file(
    app: &App,
    path: &Path,
    size: u64,
    mut rx: watch::Receiver<PrintCmd>,
    clock: &SharedClock,
) -> Result<Outcome> {
    let file = File::open(path).await?;
    let mut lines = BufReader::new(file).lines();
    let start = Instant::now();
    let mut pos: u64 = 0;

    // For the mock printer, pace the whole job to roughly a target duration regardless of
    // file size, so prints are observable instead of finishing instantly.
    let pace_secs = (app.config.transport == TransportKind::Mock && size > 0)
        .then_some(app.config.mock_print_seconds)
        .filter(|s| *s > 0)
        .map(|s| s as f64);

    while let Some(raw) = lines.next_line().await? {
        // Honor pause/cancel before sending each line.
        loop {
            let cmd = *rx.borrow_and_update();
            match cmd {
                PrintCmd::Abort => return Ok(Outcome::Aborted),
                PrintCmd::Cancel => {
                    return Ok(Outcome::Cancelled {
                        in_firmware_pause: false,
                    });
                }
                PrintCmd::Run => break,
                // Pause: the user asked — park (on a Prusa: the firmware's own pause, M601).
                // Hold: the printer paused itself (M600 filament change, runout) and parked
                // already — only stop sending until it reports resuming.
                PrintCmd::Pause | PrintCmd::Hold => {
                    let snap = app.state.snapshot();
                    let (saved, abs_e) = (snap.position, snap.absolute_extrude);
                    let prusa = snap.is_prusa();
                    let sent_m601 = cmd == PrintCmd::Pause && prusa;
                    let host_parked = cmd == PrintCmd::Pause && !prusa;
                    clock.lock().unwrap().pause(Instant::now());
                    if sent_m601 {
                        // The firmware saves its place, lifts, parks, cools the nozzle and
                        // reports `//action:paused`; its LCD then offers Resume.
                        let lift = app.config.pause_z_lift;
                        let _ = app.serial.send_control(format!("M601 Z{lift:.2}")).await;
                    } else if host_parked {
                        park(app).await;
                    }
                    loop {
                        let in_firmware_pause = sent_m601 || app.state.snapshot().firmware_paused;
                        if rx.changed().await.is_err() {
                            return Ok(Outcome::Cancelled { in_firmware_pause });
                        }
                        let next = *rx.borrow_and_update();
                        match next {
                            PrintCmd::Pause | PrintCmd::Hold => continue,
                            PrintCmd::Run => {
                                clock.lock().unwrap().resume(Instant::now());
                                if sent_m601 || (prusa && app.state.snapshot().firmware_paused) {
                                    // Reheat, return, unretract. Even if its `paused` report
                                    // hasn't arrived yet: a paused Prusa still executes what
                                    // it receives, so never stream on without this.
                                    let _ = app.serial.send_control("M602").await;
                                } else if host_parked {
                                    unpark(app, saved, abs_e).await;
                                }
                                break;
                            }
                            // Cancel/Abort while paused: leave it parked; finish() handles it.
                            PrintCmd::Cancel => {
                                let in_firmware_pause =
                                    sent_m601 || app.state.snapshot().firmware_paused;
                                return Ok(Outcome::Cancelled { in_firmware_pause });
                            }
                            PrintCmd::Abort => return Ok(Outcome::Aborted),
                        }
                    }
                }
            }
        }

        pos += raw.len() as u64 + 1; // include newline for byte progress
        let line = strip_comment(&raw);
        if !line.is_empty() {
            app.serial.send_low(line.to_string()).await?;
        }

        let elapsed = start.elapsed().as_secs_f64();
        let progress = if size > 0 {
            (pos as f64 / size as f64).min(1.0)
        } else {
            0.0
        };
        let filepos = pos.min(size);
        // (Durations are published by `tick_durations`.)
        app.state.update(move |s| {
            s.sd_file_position = filepos;
            s.sd_progress = progress;
        });

        // Pace to the target duration: sleep when we're ahead of schedule.
        if let Some(total) = pace_secs {
            let lag = total * progress - elapsed;
            if lag >= MIN_PACE_SLEEP {
                sleep(Duration::from_secs_f64(lag)).await;
            }
        }
    }

    Ok(Outcome::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PrinterState;

    fn state() -> StateHandle {
        StateHandle::spawn(PrinterState::new(
            [250.0, 210.0, 210.0, 0.0],
            200.0,
            1000.0,
            300.0,
            120.0,
            "./gcodes".into(),
            vec![],
            false,
        ))
    }

    /// State updates are applied asynchronously by the state actor: wait until `pred` holds.
    async fn eventually(state: &StateHandle, pred: impl Fn(&PrinterState) -> bool) -> bool {
        for _ in 0..200 {
            if pred(&state.snapshot()) {
                return true;
            }
            sleep(Duration::from_millis(5)).await;
        }
        false
    }

    #[test]
    fn print_clock_separates_paused_time() {
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let mut clock = PrintClock::new(t0);
        clock.pause(at(10));
        clock.pause(at(12)); // a repeated pause doesn't restart the pause
        clock.resume(at(25));
        assert_eq!(clock.times(at(40)), (40.0, 15.0));
        clock.pause(at(50));
        assert_eq!(clock.times(at(60)), (60.0, 25.0), "an ongoing pause counts");
        clock.resume(at(70));
        clock.resume(at(80)); // resume without a pause is a no-op
        assert_eq!(clock.times(at(90)), (90.0, 35.0));
    }

    #[tokio::test]
    async fn reset_file_clears_a_finished_print() {
        let state = state();
        state.update(|s| {
            s.print_state = PrintState::Complete;
            s.print_filename = "benchy.gcode".into();
        });
        assert!(eventually(&state, |s| s.print_state == PrintState::Complete).await);

        PrintHandle::new().reset_file(&state).await.unwrap();

        assert!(
            eventually(&state, |s| s.print_state == PrintState::Standby
                && s.print_filename.is_empty())
            .await
        );
    }

    #[tokio::test]
    async fn reset_file_refuses_while_a_print_is_active() {
        let state = state();
        state.update(|s| {
            s.print_state = PrintState::Printing;
            s.print_filename = "benchy.gcode".into();
        });
        let handle = PrintHandle::new();
        let (tx, _rx) = watch::channel(PrintCmd::Run);
        *handle.active.lock().await = Some(tx);

        assert!(handle.reset_file(&state).await.is_err());
        sleep(Duration::from_millis(50)).await;
        let s = state.snapshot();
        assert_eq!(s.print_state, PrintState::Printing);
        assert_eq!(s.print_filename, "benchy.gcode");
    }

    /// A handle with an active print whose stream is currently in `cmd`.
    async fn printing(state: &StateHandle, cmd: PrintCmd) -> PrintHandle {
        let print_state = if cmd == PrintCmd::Run {
            PrintState::Printing
        } else {
            PrintState::Paused
        };
        state.update(move |s| s.print_state = print_state);
        let handle = PrintHandle::new();
        *handle.active.lock().await = Some(watch::channel(cmd).0);
        handle
    }

    async fn stream_cmd(handle: &PrintHandle) -> PrintCmd {
        *handle.active.lock().await.as_ref().unwrap().borrow()
    }

    #[tokio::test]
    async fn firmware_pause_holds_the_stream_without_parking() {
        // M600 / runout: the printer parked itself, we must only stop sending.
        let state = state();
        let handle = printing(&state, PrintCmd::Run).await;
        handle.firmware_paused(&state).await;
        assert_eq!(stream_cmd(&handle).await, PrintCmd::Hold);
        assert!(
            eventually(&state, |s| s.print_state == PrintState::Paused
                && s.firmware_paused)
            .await
        );
    }

    #[tokio::test]
    async fn firmware_resume_releases_the_hold() {
        let state = state();
        let handle = printing(&state, PrintCmd::Hold).await;
        state.update(|s| s.firmware_paused = true);
        handle.firmware_resumed(&state).await;
        assert_eq!(stream_cmd(&handle).await, PrintCmd::Run);
        assert!(
            eventually(&state, |s| s.print_state == PrintState::Printing
                && !s.firmware_paused)
            .await
        );
    }

    #[tokio::test]
    async fn firmware_reports_never_override_a_users_pause() {
        let state = state();
        let handle = printing(&state, PrintCmd::Pause).await;
        handle.firmware_paused(&state).await; // our own M601 being reported back
        assert_eq!(stream_cmd(&handle).await, PrintCmd::Pause);
        handle.firmware_resumed(&state).await; // e.g. an M600 finished meanwhile
        assert_eq!(stream_cmd(&handle).await, PrintCmd::Pause);
        sleep(Duration::from_millis(50)).await;
        assert_eq!(state.snapshot().print_state, PrintState::Paused);
    }

    #[tokio::test]
    async fn firmware_pause_without_a_print_only_records_it() {
        // E.g. an M600 typed into the console while idle.
        let state = state();
        PrintHandle::new().firmware_paused(&state).await;
        assert!(eventually(&state, |s| s.firmware_paused).await);
        assert_eq!(state.snapshot().print_state, PrintState::Standby);
    }
}
