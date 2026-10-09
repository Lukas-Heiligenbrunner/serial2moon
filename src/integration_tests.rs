//! Full-stack tests: the real serial session, print job and printer state against the
//! mock printer, driven the way Moonraker drives serial2moon. Every print extrudes a known
//! amount, so `M114`'s final E proves no line was lost or duplicated along the way.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout};

use crate::app::App;
use crate::config::Config;
use crate::print_job::PrintHandle;
use crate::state::{KlippyState, PrintState, PrinterState, StateHandle};

const MOVES: usize = 200;
const E_PER_MOVE: f64 = 0.1;

struct Rig {
    app: App,
    dir: PathBuf,
    console: broadcast::Receiver<String>,
    /// Every print state seen, in order (consecutive duplicates collapsed).
    states: Arc<Mutex<Vec<PrintState>>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Start serial2moon against a mock printer; `pace` stretches each print to that many s.
async fn rig(name: &str, pace: u64, mock_args: &[&str]) -> Rig {
    // Quiet unless RUST_LOG is set (e.g. RUST_LOG=serial2moon=debug -- --nocapture).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .with_test_writer()
        .try_init();
    let dir = std::env::temp_dir().join(format!("s2m-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let gcode_dir = dir.to_string_lossy().to_string();
    let pace = pace.to_string();
    let mut args = vec![
        "serial2moon",
        "--transport",
        "mock",
        "--gcode-dir",
        &gcode_dir,
        "--mock-print-seconds",
        &pace,
    ];
    args.extend_from_slice(mock_args);
    let config = Arc::new(Config::parse_from(args));
    let state = StateHandle::spawn(PrinterState::new(
        config.axis_maximum(),
        config.max_velocity,
        config.max_accel,
        config.extruder_max_temp,
        config.bed_max_temp,
        gcode_dir.clone(),
        config.parsed_sheets(),
        false,
    ));
    let (console, console_rx) = broadcast::channel(4096);
    let print = PrintHandle::new();
    let serial = crate::serial_session::spawn(
        config.clone(),
        state.clone(),
        console.clone(),
        print.clone(),
    );
    let app = App {
        config,
        state,
        serial,
        console,
        print,
    };

    let states = Arc::new(Mutex::new(Vec::new()));
    let mut watch = app.state.subscribe();
    let seen = states.clone();
    tokio::spawn(async move {
        while watch.changed().await.is_ok() {
            let s = watch.borrow_and_update().print_state;
            let mut seen = seen.lock().unwrap();
            if seen.last() != Some(&s) {
                seen.push(s);
            }
        }
    });

    let rig = Rig {
        app,
        dir,
        console: console_rx,
        states,
    };
    rig.wait_for("printer ready", |s| s.klippy_state == KlippyState::Ready)
        .await;
    rig
}

impl Rig {
    async fn wait_for(&self, what: &str, pred: impl Fn(&PrinterState) -> bool) {
        let waited = timeout(Duration::from_secs(30), async {
            while !pred(&self.app.state.snapshot()) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "timed out waiting for {what}");
    }

    /// Write a relative-E print of `MOVES` extrusion moves (`extra` inserted halfway) and
    /// start it.
    async fn print(&self, extra: &str) {
        let mut gcode = String::from("M83\n");
        for i in 0..MOVES {
            if i == MOVES / 2 {
                gcode.push_str(extra);
            }
            gcode.push_str(&format!(
                "G1 X{} Y{} E{E_PER_MOVE} F1200\n",
                10 + i % 100,
                20 + i % 50
            ));
        }
        std::fs::write(self.dir.join("t.gcode"), gcode).unwrap();
        self.app.print.start(&self.app, "t.gcode").await.unwrap();
    }

    /// The printer's E position, read back with M114.
    async fn extruded(&mut self) -> f64 {
        while self.console.try_recv().is_ok() {}
        self.app.serial.send_high("M114").await.unwrap();
        let reply = timeout(Duration::from_secs(5), async {
            loop {
                match self.console.recv().await {
                    Ok(line) if line.starts_with("X:") => return line,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(e) => panic!("console closed: {e}"),
                }
            }
        })
        .await
        .expect("no M114 reply on the console");
        let e = reply.split_whitespace().find_map(|t| t.strip_prefix("E:"));
        e.and_then(|e| e.parse().ok()).expect("E in the M114 reply")
    }

    /// Everything printed to the console so far.
    fn console_lines(&mut self) -> Vec<String> {
        std::iter::from_fn(|| {
            loop {
                match self.console.try_recv() {
                    Ok(line) => return Some(line),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(_) => return None,
                }
            }
        })
        .collect()
    }

    fn saw(&self, state: PrintState) -> bool {
        self.states.lock().unwrap().contains(&state)
    }
}

fn assert_all_extruded(e: f64) {
    let expected = MOVES as f64 * E_PER_MOVE;
    assert!(
        (e - expected).abs() < 1e-6,
        "extruded {e} mm, expected {expected}: lines were lost or duplicated"
    );
}

#[tokio::test]
async fn m600_filament_change_pauses_and_continues_without_losing_lines() {
    let mut rig = rig("m600", 0, &["--mock-prusa"]).await;
    rig.print("M600\n").await;
    rig.wait_for("print to complete", |s| {
        s.print_state == PrintState::Complete
    })
    .await;
    assert!(
        rig.saw(PrintState::Paused),
        "shown as paused during the change"
    );
    assert!(!rig.app.state.snapshot().firmware_paused);
    assert_all_extruded(rig.extruded().await);
}

#[tokio::test]
async fn filament_runout_replays_the_lines_the_printer_discarded() {
    let mut rig = rig("runout", 0, &["--mock-prusa", "--mock-runout-at", "60"]).await;
    rig.print("").await;
    rig.wait_for("print to complete", |s| {
        s.print_state == PrintState::Complete
    })
    .await;
    assert!(
        rig.saw(PrintState::Paused),
        "shown as paused during the change"
    );
    let console = rig.console_lines();
    assert!(
        console
            .iter()
            .any(|l| l.contains("Filament Runout Detected")),
        "runout reported on the console: {console:?}"
    );
    assert_all_extruded(rig.extruded().await);
}

#[tokio::test]
async fn pause_on_a_prusa_uses_the_firmware_pause() {
    let mut rig = rig("prusa-pause", 3, &["--mock-prusa"]).await;
    rig.print("").await;
    rig.wait_for("some progress", |s| s.sd_progress > 0.2).await;

    rig.app.print.pause(&rig.app.state).await.unwrap();
    // Only the firmware's own pause (M601) reports back `//action:paused`.
    rig.wait_for("the firmware to pause", |s| {
        s.firmware_paused && s.print_state == PrintState::Paused
    })
    .await;
    let held_at = rig.app.state.snapshot().sd_progress;
    sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.app.state.snapshot().sd_progress, held_at, "stream held");

    rig.app.print.resume(&rig.app.state).await.unwrap();
    rig.wait_for("print to complete", |s| {
        s.print_state == PrintState::Complete
    })
    .await;
    assert!(!rig.app.state.snapshot().firmware_paused);
    assert_all_extruded(rig.extruded().await);
}

#[tokio::test]
async fn cancel_while_paused_stops_the_print_in_the_firmware() {
    let mut rig = rig("prusa-cancel", 3, &["--mock-prusa"]).await;
    rig.print("").await;
    rig.wait_for("some progress", |s| s.sd_progress > 0.2).await;
    rig.app.print.pause(&rig.app.state).await.unwrap();
    rig.wait_for("the firmware to pause", |s| s.firmware_paused)
        .await;

    rig.app.print.cancel().await.unwrap();
    rig.wait_for("cancelled", |s| s.print_state == PrintState::Cancelled)
        .await;
    let console = rig.console_lines();
    assert!(
        console.iter().any(|l| l.contains("mock: print stopped")),
        "M603 sent: {console:?}"
    );
    assert!(!rig.app.state.snapshot().firmware_paused);
}

#[tokio::test]
async fn pause_on_generic_marlin_parks_from_the_host() {
    let mut rig = rig("marlin-pause", 3, &[]).await;
    rig.print("").await;
    rig.wait_for("some progress", |s| s.sd_progress > 0.2).await;
    rig.app.print.pause(&rig.app.state).await.unwrap();
    rig.wait_for("paused", |s| s.print_state == PrintState::Paused)
        .await;
    sleep(Duration::from_millis(300)).await;
    assert!(
        !rig.app.state.snapshot().firmware_paused,
        "no M601 on Marlin"
    );
    rig.app.print.resume(&rig.app.state).await.unwrap();
    rig.wait_for("print to complete", |s| {
        s.print_state == PrintState::Complete
    })
    .await;
    // The park's retract and the unpark's unretract cancel out.
    assert_all_extruded(rig.extruded().await);
}
