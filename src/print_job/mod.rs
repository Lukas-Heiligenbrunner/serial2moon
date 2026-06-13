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

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrintCmd {
    Run,
    Pause,
    Cancel,
    /// Hard abort with no clean-up G-code (e.g. the printer reset under us).
    Abort,
}

enum Outcome {
    Completed,
    Cancelled,
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
            s.print_state = PrintState::Printing;
            s.print_filename = fname;
            s.print_message = String::new();
            s.sd_file_path = Some(disp);
            s.sd_file_size = size;
            s.sd_file_position = 0;
            s.sd_progress = 0.0;
            s.sd_is_active = true;
            s.total_duration = 0.0;
            s.print_duration = 0.0;
            s.filament_used = 0.0;
        });

        let app = app.clone();
        let handle = self.clone();
        tokio::spawn(async move {
            let result = stream_file(&app, &path, size, rx).await;
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
            Ok(Outcome::Cancelled) => {
                info!("print cancelled");
                park(app).await; // lift + present (retract while still warm)
                // Safety: drop heaters and fan, and release the steppers.
                let _ = app.serial.send_high("M104 S0").await;
                let _ = app.serial.send_high("M140 S0").await;
                let _ = app.serial.send_high("M107").await;
                let _ = app.serial.send_high("M84").await;
                app.state.update(|s| {
                    s.print_state = PrintState::Cancelled;
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
                PrintCmd::Cancel => return Ok(Outcome::Cancelled),
                PrintCmd::Run => break,
                PrintCmd::Pause => {
                    // Save where we are, park away from the print, then wait for resume.
                    let snap = app.state.snapshot();
                    let saved = snap.position;
                    let abs_e = snap.absolute_extrude;
                    park(app).await;
                    loop {
                        if rx.changed().await.is_err() {
                            return Ok(Outcome::Cancelled);
                        }
                        let next = *rx.borrow_and_update();
                        match next {
                            PrintCmd::Pause => continue,
                            PrintCmd::Run => {
                                unpark(app, saved, abs_e).await;
                                break;
                            }
                            // Cancel/Abort while paused: leave it parked; finish() handles it.
                            PrintCmd::Cancel => return Ok(Outcome::Cancelled),
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
        app.state.update(move |s| {
            s.sd_file_position = filepos;
            s.sd_progress = progress;
            s.print_duration = elapsed;
            s.total_duration = elapsed;
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
