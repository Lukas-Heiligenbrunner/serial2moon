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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// A command is considered hung only after this long with NO data at all from the printer.
/// Blocking G-code (M109/M190 heat waits, G28/G29) can take minutes, but the printer keeps
/// streaming temperature lines meanwhile — so we time out on silence, not on elapsed time.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the command wait wakes to check for silence.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Shared "last byte received from the printer" timestamp, used to distinguish a busy
/// printer (still streaming temps) from a hung/disconnected one.
type LastSeen = Arc<Mutex<Instant>>;

/// Serial link counters, surfaced as the Klipper `mcu` object's stats (bytes + sequence
/// numbers ≈ commands sent / lines received).
#[derive(Default)]
struct Stats {
    bytes_read: AtomicU64,
    bytes_write: AtomicU64,
    send_seq: AtomicU64,
    receive_seq: AtomicU64,
}

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

/// A restart requested over the API (Klipper's `RESTART` / `FIRMWARE_RESTART`).
#[derive(Clone, Copy, Debug)]
enum RestartKind {
    /// `RESTART`: re-initialize the printer over the existing link (reset line numbering,
    /// re-run the handshake) — analogous to Klipper reloading config.
    Reinit,
    /// `FIRMWARE_RESTART`: drop and reopen the serial transport — analogous to Klipper
    /// reconnecting to the MCU. Aborts any in-flight print.
    Reconnect,
}

/// Handle for submitting G-code to the printer. Cheap to clone.
#[derive(Clone)]
pub struct SerialHandle {
    high: mpsc::Sender<Cmd>,
    low: mpsc::Sender<Cmd>,
    restart: mpsc::Sender<RestartKind>,
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

    /// Klipper `RESTART`: re-initialize the printer over the existing link.
    pub async fn restart(&self) -> Result<()> {
        self.restart
            .send(RestartKind::Reinit)
            .await
            .map_err(|_| anyhow!("serial session closed"))
    }

    /// Klipper `FIRMWARE_RESTART`: drop and reopen the serial connection.
    pub async fn firmware_restart(&self) -> Result<()> {
        self.restart
            .send(RestartKind::Reconnect)
            .await
            .map_err(|_| anyhow!("serial session closed"))
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
    let (restart_tx, restart_rx) = mpsc::channel(4);
    tokio::spawn(supervisor(
        config, state, console, print, high_rx, low_rx, restart_rx,
    ));
    SerialHandle {
        high: high_tx,
        low: low_tx,
        restart: restart_tx,
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
    mut restart_rx: mpsc::Receiver<RestartKind>,
) {
    let mut backoff = BACKOFF_MIN;
    loop {
        state.update(|s| {
            s.klippy_state = KlippyState::Startup;
            s.state_message = "Connecting to printer".to_string();
        });

        let (transport, baud) = match transport::open(&config).await {
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
            baud,
            &state,
            &console,
            &print,
            &mut high_rx,
            &mut low_rx,
            &mut restart_rx,
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

#[allow(clippy::too_many_arguments)]
async fn run_connection(
    transport: Box<dyn transport::Serial>,
    baud: u32,
    state: &StateHandle,
    console: &broadcast::Sender<String>,
    print: &PrintHandle,
    high_rx: &mut mpsc::Receiver<Cmd>,
    low_rx: &mut mpsc::Receiver<Cmd>,
    api_restart_rx: &mut mpsc::Receiver<RestartKind>,
) -> ConnectionEnd {
    let (read, mut write) = tokio::io::split(transport);
    let (ack_tx, mut ack_rx) = mpsc::channel(64);
    let (restart_tx, mut restart_rx) = mpsc::unbounded_channel();
    let last_seen: LastSeen = Arc::new(Mutex::new(Instant::now()));
    let stats = Arc::new(Stats::default());

    let reader = tokio::spawn(reader(
        read,
        state.clone(),
        console.clone(),
        ack_tx,
        print.clone(),
        restart_tx,
        last_seen.clone(),
        stats.clone(),
    ));
    tokio::pin!(reader);

    // Publish MCU link stats to the state ~1/s (matches Klipper's cadence; avoids churn).
    // Also derive a "load" = serial-link utilization (bytes/s vs the baud's byte capacity),
    // since the real bottleneck on a legacy printer is the serial link, not MCU compute we
    // can't see. ~10 bits per byte (8N1 + start/stop).
    let stats_task = tokio::spawn({
        let state = state.clone();
        let stats = stats.clone();
        let capacity = (baud as f64) / 10.0; // bytes/s, 0 for the mock
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut prev_total = 0u64;
            loop {
                tick.tick().await;
                let bytes_read = stats.bytes_read.load(Ordering::Relaxed);
                let bytes_write = stats.bytes_write.load(Ordering::Relaxed);
                let send_seq = stats.send_seq.load(Ordering::Relaxed);
                let receive_seq = stats.receive_seq.load(Ordering::Relaxed);

                let total = bytes_read + bytes_write;
                let per_sec = total.saturating_sub(prev_total) as f64; // interval is 1 s
                prev_total = total;
                let load = if capacity > 0.0 {
                    (per_sec / capacity).clamp(0.0, 1.0)
                } else {
                    0.0
                };

                state.update(move |s| {
                    s.mcu_bytes_read = bytes_read;
                    s.mcu_bytes_write = bytes_write;
                    s.mcu_send_seq = send_seq;
                    s.mcu_receive_seq = receive_seq;
                    s.mcu_load = load;
                });
            }
        }
    });

    // Line-number counter for the checksummed Marlin protocol. M110 N0 resets the
    // printer's counter; framing then starts at N1. `init` does the reset + handshake.
    let mut line_no: u64 = 0;
    macro_rules! send {
        ($cmd:expr) => {
            send_and_wait(
                &mut write,
                $cmd,
                &mut ack_rx,
                &last_seen,
                &stats,
                &mut line_no,
            )
            .await
        };
    }

    // Initialize the printer for this connection: reset line numbering, identify, enable
    // temp autoreport, and discover steel-sheet profiles (the reader parses the M850
    // reports into state). Done before reporting ready so the sheet macros are present
    // when Mainsail reads the config. `line_no` starts at 0; M110 N0 resets the printer
    // to match, so the first framed command after it is N1.
    let _ = send!("M110 N0");
    let _ = send!("M115");
    let _ = send!("M155 S1");
    for id in 0..8 {
        let _ = send!(&format!("M850 S{id}"));
    }
    state.update(|s| {
        s.klippy_state = KlippyState::Ready;
        s.state_message = "Printer is ready".to_string();
    });
    info!(sheets = ?state.snapshot().sheets.len(), "printer initialized; reporting ready");

    let mut motion = Motion::new();
    let end = loop {
        tokio::select! {
            biased;
            _ = &mut reader => break ConnectionEnd::Disconnected,
            _ = restart_rx.recv() => {
                // Printer reset mid-session (watchdog/brownout): the link is still up, so
                // re-initialize in place rather than reconnecting. The printer's line counter
                // reset too, so re-sync with M110. Homing/position are lost.
                warn!("re-initializing printer after reset");
                line_no = 0;
                let _ = send!("M110 N0");
                let _ = send!("M155 S1");
                motion = Motion::new();
                push_motion(state, &motion);
            }
            Some(kind) = api_restart_rx.recv() => match kind {
                // FIRMWARE_RESTART: tear the link down so the supervisor reopens the
                // transport and runs a full handshake. The supervisor aborts the print.
                RestartKind::Reconnect => {
                    warn!("firmware restart requested; reconnecting transport");
                    break ConnectionEnd::Disconnected;
                }
                // RESTART: re-initialize over the existing link, like the reset path above.
                RestartKind::Reinit => {
                    warn!("restart requested; re-initializing printer");
                    print.abort().await;
                    line_no = 0;
                    let _ = send!("M110 N0");
                    let _ = send!("M155 S1");
                    motion = Motion::new();
                    push_motion(state, &motion);
                    state.update(|s| {
                        s.klippy_state = KlippyState::Ready;
                        s.state_message = "Printer is ready".to_string();
                    });
                }
            },
            cmd = recv_cmd(high_rx, low_rx) => {
                let Some(cmd) = cmd else { break ConnectionEnd::Shutdown };
                let result = send!(&cmd.line);
                if result.is_ok() && motion.apply(&cmd.line) {
                    push_motion(state, &motion);
                }
                let _ = cmd.ack.send(result);
            }
        }
    };

    stats_task.abort();
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

#[allow(clippy::too_many_arguments)]
async fn reader<R: AsyncReadExt + Unpin>(
    read: R,
    state: StateHandle,
    console: broadcast::Sender<String>,
    ack_tx: mpsc::Sender<AckEvent>,
    print: PrintHandle,
    restart_tx: mpsc::UnboundedSender<()>,
    last_seen: LastSeen,
    stats: Arc<Stats>,
) {
    let mut lines = BufReader::new(read).lines();
    let mut identified = false;
    loop {
        match lines.next_line().await {
            Ok(Some(raw)) => {
                debug!(target: "serial2moon::rx", "{}", raw.trim_end());
                // Any line means the printer is alive — used to gate the command timeout.
                *last_seen.lock().unwrap() = Instant::now();
                stats
                    .bytes_read
                    .fetch_add(raw.len() as u64 + 1, Ordering::Relaxed);
                stats.receive_seq.fetch_add(1, Ordering::Relaxed);
                // Capture the printer's identity from its M115 reply (once per connection).
                if let Some((firmware, machine)) = parser::parse_firmware(&raw) {
                    if !identified {
                        identified = true;
                        info!(%firmware, %machine, "printer identified");
                        let _ = console.send(format!("// connected to {firmware}"));
                    }
                    state.update(move |s| {
                        s.mcu_version = firmware;
                        if !machine.is_empty() {
                            s.machine_type = machine;
                        }
                    });
                }
                // Discover steel-sheet profiles from M850 reports (upsert by id); show the
                // active one in the status line (Mainsail can't highlight a macro button).
                if let Some((id, label, z, active)) = parser::parse_sheet(&raw) {
                    debug!(id, %label, z, active, "discovered steel sheet");
                    let active_label = label.clone();
                    state.update(move |s| {
                        s.sheets.retain(|sh| sh.id != id);
                        s.sheets.push(crate::state::Sheet {
                            id,
                            label,
                            z: Some(z),
                        });
                        s.sheets.sort_by_key(|sh| sh.id);
                        if active {
                            s.display_message = format!("Active sheet: {active_label}");
                        }
                    });
                }
                // React to host action commands from the printer's LCD (pause/resume/cancel
                // a USB print). Errors (e.g. no active print) are ignored.
                if let Some(action) = parser::parse_action(&raw) {
                    match action.as_str() {
                        "pause" | "paused" => {
                            info!("printer requested pause");
                            let _ = print.pause(&state).await;
                        }
                        "resume" | "resumed" => {
                            info!("printer requested resume");
                            let _ = print.resume(&state).await;
                        }
                        "cancel" => {
                            info!("printer requested cancel");
                            let _ = print.cancel().await;
                        }
                        _ => {}
                    }
                }
                match parser::classify(&raw) {
                    Line::Ok(t) => {
                        if !t.is_empty() {
                            apply_temps(&state, t);
                            // A solicited temp reply (M105) — echo to the console so it shows
                            // on demand (Mainsail's "hide temperatures" toggle filters these).
                            let _ = console.send(raw.trim().to_string());
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
                        // Suppress M115 capability/firmware noise from the console (it's
                        // parsed above and condensed into one "// connected" line).
                        if msg.starts_with("Cap:") || msg.contains("FIRMWARE_NAME:") {
                            debug!(line = %msg, "printer output (suppressed)");
                        } else {
                            let _ = console.send(msg);
                        }
                    }
                }
            }
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

/// Marlin line checksum: XOR of every byte up to (not including) the `*`.
fn checksum(body: &str) -> u8 {
    body.bytes().fold(0u8, |acc, b| acc ^ b)
}

/// Frame a command with a line number and checksum: `N<n> <cmd>*<checksum>`.
fn frame(line_no: u64, cmd: &str) -> String {
    let body = format!("N{line_no} {cmd}");
    let cs = checksum(&body);
    format!("{body}*{cs}")
}

/// Send a command with a line number + checksum and wait for `ok`, re-sending on `Resend:`
/// and only advancing the line number on success. `line_no` is the number used for this
/// command (set to 0 before an `M110 N0` reset).
async fn send_and_wait<W: AsyncWriteExt + Unpin>(
    write: &mut W,
    cmd: &str,
    ack_rx: &mut mpsc::Receiver<AckEvent>,
    last_seen: &LastSeen,
    stats: &Stats,
    line_no: &mut u64,
) -> Result<()> {
    // Discard any acks left over from a previous command before issuing this one.
    while ack_rx.try_recv().is_ok() {}

    let n = *line_no;
    let framed = frame(n, cmd);
    write_line(write, &framed).await?;
    stats
        .bytes_write
        .fetch_add(framed.len() as u64 + 1, Ordering::Relaxed);
    stats.send_seq.fetch_add(1, Ordering::Relaxed);

    let mut resends = 0u32;
    loop {
        match timeout(POLL_INTERVAL, ack_rx.recv()).await {
            Ok(Some(AckEvent::Ok)) => {
                *line_no = n + 1; // advance only once the printer accepted the line
                return Ok(());
            }
            Ok(Some(AckEvent::Busy)) => continue,
            Ok(Some(AckEvent::Resend(requested))) => {
                resends += 1;
                debug!(
                    requested,
                    line_no = n,
                    attempt = resends,
                    "printer requested resend"
                );
                if resends > 10 {
                    bail!("too many resend requests for line N{n}: {cmd}");
                }
                // Depth-1: the requested line is our in-flight one — resend it verbatim.
                write_line(write, &framed).await?;
            }
            Ok(Some(AckEvent::Error(msg))) => {
                // Non-fatal: logged to console already; treat as command completion.
                debug!(error = %msg, "treating printer error as ack");
                *line_no = n + 1;
                return Ok(());
            }
            Ok(None) => bail!("serial reader stopped"),
            Err(_) => {
                // No ack yet. Blocking commands (M109/M190/G28/G29) can run for minutes,
                // but the printer keeps streaming temps meanwhile — so only fail on real
                // silence (likely a hang/disconnect), not on elapsed time.
                let idle = last_seen.lock().unwrap().elapsed();
                if idle >= SILENCE_TIMEOUT {
                    bail!("no response to '{cmd}' — printer silent for {idle:?}");
                }
            }
        }
    }
}

async fn write_line<W: AsyncWriteExt + Unpin>(write: &mut W, line: &str) -> Result<()> {
    debug!(target: "serial2moon::tx", "{line}");
    write.write_all(line.as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{checksum, frame};

    #[test]
    fn marlin_checksum_and_frame() {
        // Matches the canonical OctoPrint/Marlin `N0 M110 N0*125` reset line.
        assert_eq!(checksum("N0 M110 N0"), 125);
        assert_eq!(frame(0, "M110 N0"), "N0 M110 N0*125");
        assert_eq!(frame(1, "M115"), format!("N1 M115*{}", checksum("N1 M115")));
    }
}
