//! The serial session: the sole writer to the printer, with automatic reconnection.
//!
//! A *supervisor* task owns the reconnect loop: it (re)opens the transport, runs the
//! per-connection logic, and on disconnect/reset backs off and tries again. The public
//! [`SerialHandle`] (and its channels) is stable across reconnects.
//!
//! Within a connection, a *reader* task parses every inbound line (feeding temperatures
//! to the state actor and console text to subscribers), while the supervisor runs the
//! depth-1 "send line → await ok" handshake with two priority levels and tracks motion.

pub mod motion;
pub mod parser;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::print_job::PrintHandle;
use crate::state::{KlippyState, StateHandle};
use crate::transport;
use motion::Motion;
use parser::{Line, TempReport};

/// Max time to wait for an `ok` (reset by every `busy:`); covers slow homing/leveling.
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

struct Cmd {
    line: String,
    ack: oneshot::Sender<Result<()>>,
}

/// Ack-relevant events the reader forwards to the supervisor.
enum AckEvent {
    Ok,
    Busy,
    Resend(u64),
    Error(String),
}

/// How a single connection ended.
enum ConnectionEnd {
    /// Public channels closed — the whole session should stop.
    Shutdown,
    /// Transport died (EOF / IO error) — reconnect.
    Disconnected,
}

/// Handle for submitting G-code to the printer. Cheap to clone.
#[derive(Clone)]
pub struct SerialHandle {
    high: mpsc::Sender<Cmd>,
    low: mpsc::Sender<Cmd>,
}

impl SerialHandle {
    /// Submit an interactive/control command (jumps ahead of the print feed).
    pub async fn send_high(&self, line: impl Into<String>) -> Result<()> {
        Self::submit(&self.high, line.into()).await
    }

    /// Submit a print-stream line (yields to interactive commands).
    pub async fn send_low(&self, line: impl Into<String>) -> Result<()> {
        Self::submit(&self.low, line.into()).await
    }

    async fn submit(ch: &mpsc::Sender<Cmd>, line: String) -> Result<()> {
        let (ack, rx) = oneshot::channel();
        ch.send(Cmd { line, ack })
            .await
            .map_err(|_| anyhow!("serial session closed"))?;
        rx.await
            .map_err(|_| anyhow!("serial session dropped before ack"))?
    }
}

/// Spawn the serial supervisor and return a stable handle to it.
pub fn spawn(
    config: Arc<Config>,
    state: StateHandle,
    console: broadcast::Sender<String>,
    print: PrintHandle,
) -> SerialHandle {
    let (high_tx, high_rx) = mpsc::channel(64);
    let (low_tx, low_rx) = mpsc::channel(1);
    tokio::spawn(supervisor(config, state, console, print, high_rx, low_rx));
    SerialHandle {
        high: high_tx,
        low: low_tx,
    }
}

/// Reconnect loop. Owns the command receivers so the handle survives reconnects.
async fn supervisor(
    config: Arc<Config>,
    state: StateHandle,
    console: broadcast::Sender<String>,
    print: PrintHandle,
    mut high_rx: mpsc::Receiver<Cmd>,
    mut low_rx: mpsc::Receiver<Cmd>,
) {
    let mut backoff = BACKOFF_MIN;
    loop {
        state.update(|s| {
            s.klippy_state = KlippyState::Startup;
            s.state_message = "Connecting to printer".to_string();
        });

        let transport = match transport::open(&config).await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, backoff = ?backoff, "failed to open printer; retrying");
                state.update(move |s| {
                    s.klippy_state = KlippyState::Error;
                    s.state_message = format!("Printer unavailable: {e}");
                });
                if !reject_for(backoff, &mut high_rx, &mut low_rx).await {
                    return;
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };

        info!("printer transport connected");
        backoff = BACKOFF_MIN;
        let end = run_connection(
            transport,
            &state,
            &console,
            &print,
            &mut high_rx,
            &mut low_rx,
        )
        .await;
        match end {
            ConnectionEnd::Shutdown => {
                info!("serial session shutting down");
                return;
            }
            ConnectionEnd::Disconnected => warn!("printer disconnected; reconnecting"),
        }

        // Fail any in-flight print and briefly back off before reconnecting.
        print.abort().await;
        if !reject_for(BACKOFF_MIN, &mut high_rx, &mut low_rx).await {
            return;
        }
    }
}

/// While the printer is offline, reject incoming commands (so callers fail fast instead
/// of hanging) for `dur`. Returns false if the public channels closed (app shutdown).
async fn reject_for(
    dur: Duration,
    high_rx: &mut mpsc::Receiver<Cmd>,
    low_rx: &mut mpsc::Receiver<Cmd>,
) -> bool {
    let deadline = sleep(dur);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = &mut deadline => return true,
            cmd = high_rx.recv() => match cmd {
                Some(c) => { let _ = c.ack.send(Err(anyhow!("printer offline"))); }
                None => return false,
            },
            cmd = low_rx.recv() => match cmd {
                Some(c) => { let _ = c.ack.send(Err(anyhow!("printer offline"))); }
                None => return false,
            },
        }
    }
}

async fn run_connection(
    transport: Box<dyn transport::Serial>,
    state: &StateHandle,
    console: &broadcast::Sender<String>,
    print: &PrintHandle,
    high_rx: &mut mpsc::Receiver<Cmd>,
    low_rx: &mut mpsc::Receiver<Cmd>,
) -> ConnectionEnd {
    let (read, mut write) = tokio::io::split(transport);
    let (ack_tx, mut ack_rx) = mpsc::channel(64);
    let (restart_tx, mut restart_rx) = mpsc::unbounded_channel();

    let reader = tokio::spawn(reader(
        read,
        state.clone(),
        console.clone(),
        ack_tx,
        print.clone(),
        restart_tx,
    ));
    tokio::pin!(reader);

    // Initialize the printer for this connection: identify, enable temp autoreport.
    let _ = send_and_wait(&mut write, "M115", &mut ack_rx).await;
    let _ = send_and_wait(&mut write, "M155 S1", &mut ack_rx).await;
    state.update(|s| {
        s.klippy_state = KlippyState::Ready;
        s.state_message = "Printer is ready".to_string();
    });
    info!("printer initialized; reporting ready");

    let mut motion = Motion::new();
    let end = loop {
        tokio::select! {
            biased;
            _ = &mut reader => break ConnectionEnd::Disconnected,
            _ = restart_rx.recv() => {
                // Printer reset mid-session (watchdog/brownout): the link is still up, so
                // re-initialize in place rather than reconnecting. Homing/position are lost.
                warn!("re-initializing printer after reset");
                let _ = send_and_wait(&mut write, "M155 S1", &mut ack_rx).await;
                motion = Motion::new();
                push_motion(state, &motion);
            }
            cmd = recv_cmd(high_rx, low_rx) => {
                let Some(cmd) = cmd else { break ConnectionEnd::Shutdown };
                let result = send_and_wait(&mut write, &cmd.line, &mut ack_rx).await;
                if result.is_ok() && motion.apply(&cmd.line) {
                    push_motion(state, &motion);
                }
                let _ = cmd.ack.send(result);
            }
        }
    };

    if !matches!(end, ConnectionEnd::Disconnected) {
        reader.abort();
    }
    end
}

/// Biased receive: drain interactive commands before print-feed lines.
async fn recv_cmd(
    high_rx: &mut mpsc::Receiver<Cmd>,
    low_rx: &mut mpsc::Receiver<Cmd>,
) -> Option<Cmd> {
    tokio::select! {
        biased;
        c = high_rx.recv() => c,
        c = low_rx.recv() => c,
    }
}

fn push_motion(state: &StateHandle, m: &Motion) {
    let pos = m.position();
    let absolute = m.absolute();
    let abs_extrude = m.absolute_extrude();
    let homed = m.homed_axes();
    state.update(move |s| {
        s.position = pos;
        s.gcode_position = pos;
        s.absolute_coordinates = absolute;
        s.absolute_extrude = abs_extrude;
        s.homed_axes = homed;
    });
}

fn apply_temps(state: &StateHandle, t: TempReport) {
    state.update(move |s| {
        if let Some(v) = t.ext_temp {
            s.extruder_temp = v;
        }
        if let Some(v) = t.ext_target {
            s.extruder_target = v;
        }
        if let Some(v) = t.ext_power {
            s.extruder_power = v;
        }
        if let Some(v) = t.bed_temp {
            s.bed_temp = v;
        }
        if let Some(v) = t.bed_target {
            s.bed_target = v;
        }
        if let Some(v) = t.bed_power {
            s.bed_power = v;
        }
    });
}

async fn reader<R: AsyncReadExt + Unpin>(
    read: R,
    state: StateHandle,
    console: broadcast::Sender<String>,
    ack_tx: mpsc::Sender<AckEvent>,
    print: PrintHandle,
    restart_tx: mpsc::UnboundedSender<()>,
) {
    let mut lines = BufReader::new(read).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(raw)) => match parser::classify(&raw) {
                Line::Ok(t) => {
                    if !t.is_empty() {
                        apply_temps(&state, t);
                    }
                    let _ = ack_tx.send(AckEvent::Ok).await;
                }
                Line::Temp(t) => apply_temps(&state, t),
                Line::Busy => {
                    let _ = ack_tx.send(AckEvent::Busy).await;
                }
                Line::Resend(n) => {
                    let _ = ack_tx.send(AckEvent::Resend(n)).await;
                }
                Line::Error(msg) => {
                    warn!(error = %msg, "printer reported error");
                    let _ = console.send(format!("!! {msg}"));
                    let _ = ack_tx.send(AckEvent::Error(msg)).await;
                }
                Line::Echo(msg) => {
                    let _ = console.send(format!("// {msg}"));
                }
                Line::Start => {
                    warn!("printer reset (start banner) detected");
                    let _ = console.send("// printer reset detected".to_string());
                    state.update(|s| s.homed_axes.clear());
                    // Fail any active print; nothing was safely delivered after the reset.
                    print.abort().await;
                    // Ask the supervisor to re-initialize the printer in place. Keep reading:
                    // the link is still up and temps/acks keep arriving on it.
                    let _ = restart_tx.send(());
                }
                Line::Other(msg) => {
                    debug!(line = %msg, "printer output");
                    let _ = console.send(msg);
                }
            },
            Ok(None) => {
                warn!("serial connection closed by peer");
                return;
            }
            Err(e) => {
                warn!(error = %e, "serial read error");
                return;
            }
        }
    }
}

async fn send_and_wait<W: AsyncWriteExt + Unpin>(
    write: &mut W,
    line: &str,
    ack_rx: &mut mpsc::Receiver<AckEvent>,
) -> Result<()> {
    // Discard any acks left over from a previous command before issuing this one.
    while ack_rx.try_recv().is_ok() {}

    write_line(write, line).await?;

    let mut resends = 0u32;
    loop {
        match timeout(ACK_TIMEOUT, ack_rx.recv()).await {
            Ok(Some(AckEvent::Ok)) => return Ok(()),
            Ok(Some(AckEvent::Busy)) => continue,
            Ok(Some(AckEvent::Resend(n))) => {
                resends += 1;
                debug!(
                    requested_line = n,
                    attempt = resends,
                    "printer requested resend"
                );
                if resends > 5 {
                    bail!("too many resend requests for line: {line}");
                }
                write_line(write, line).await?;
            }
            Ok(Some(AckEvent::Error(msg))) => {
                // Non-fatal: logged to console already; treat as command completion.
                debug!(error = %msg, "treating printer error as ack");
                return Ok(());
            }
            Ok(None) => bail!("serial reader stopped"),
            Err(_) => bail!("timed out waiting for ok to: {line}"),
        }
    }
}

async fn write_line<W: AsyncWriteExt + Unpin>(write: &mut W, line: &str) -> Result<()> {
    write.write_all(line.as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await?;
    Ok(())
}
