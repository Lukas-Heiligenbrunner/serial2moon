//! Klipper API server: a Unix domain socket Moonraker connects to, believing it is Klipper.

pub mod codec;
pub mod dispatch;
pub mod subscribe;

use std::sync::Arc;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::codec::Framed;
use tracing::{info, warn};

use crate::app::App;
use crate::state::eventtime::eventtime;
use codec::EtxCodec;
use subscribe::SubState;

/// Bind the socket and serve connections until the process exits.
pub async fn serve(app: App) -> Result<()> {
    let path = &app.config.uds_path;
    // A stale socket file blocks bind; remove it first.
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding unix socket {}", path.display()))?;
    // Allow a Moonraker running under a different uid (e.g. in another container) to connect.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777));
    }
    info!(socket = %path.display(), "Klipper API server listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(app, stream).await {
                warn!(error = %e, "connection ended with error");
            }
        });
    }
}

async fn handle_conn(app: App, stream: UnixStream) -> Result<()> {
    info!("Moonraker connected");
    let (mut sink, mut input) = Framed::new(stream, EtxCodec).split();

    // All writes to this socket funnel through one task.
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(256);
    let writer = tokio::spawn(async move {
        while let Some(v) = out_rx.recv().await {
            if sink.send(v).await.is_err() {
                break;
            }
        }
    });

    let sub = Arc::new(Mutex::new(SubState::default()));
    let pusher = tokio::spawn(status_pusher(app.clone(), sub.clone(), out_tx.clone()));
    let console = tokio::spawn(console_pusher(
        app.console.subscribe(),
        sub.clone(),
        out_tx.clone(),
    ));

    // Each request is handled concurrently so a long-running G-code (e.g. homing)
    // can't block an emergency stop or a status query on the same connection.
    while let Some(msg) = input.next().await {
        let req = msg.context("decoding request")?;
        let app = app.clone();
        let sub = sub.clone();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            if let Some(resp) = dispatch::handle_request(&app, &sub, req).await {
                let _ = out_tx.send(resp).await;
            }
        });
    }

    info!("Moonraker disconnected");
    pusher.abort();
    console.abort();
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

/// Push status deltas to the connection whenever printer state changes.
async fn status_pusher(app: App, sub: Arc<Mutex<SubState>>, out_tx: mpsc::Sender<Value>) {
    let mut rx = app.state.subscribe();
    loop {
        if rx.changed().await.is_err() {
            break;
        }
        let full = rx.borrow_and_update().full_status();

        let mut guard = sub.lock().await;
        let Some(spec) = guard.status_spec.clone() else {
            continue;
        };
        let current = subscribe::select(&full, &spec);
        let delta = subscribe::diff(&guard.last_status, &current);
        if delta.is_empty() {
            continue;
        }
        guard.last_status = current;
        let template = guard.status_template.clone();
        drop(guard);

        let msg = with_params(
            template,
            json!({ "eventtime": eventtime(), "status": Value::Object(delta) }),
        );
        if out_tx.send(msg).await.is_err() {
            break;
        }
    }
}

/// Push Marlin console output (echo/error lines) to subscribed connections.
async fn console_pusher(
    mut rx: broadcast::Receiver<String>,
    sub: Arc<Mutex<SubState>>,
    out_tx: mpsc::Sender<Value>,
) {
    loop {
        match rx.recv().await {
            Ok(line) => {
                let guard = sub.lock().await;
                if !guard.gcode_output {
                    continue;
                }
                let template = guard.gcode_template.clone();
                drop(guard);
                let msg = with_params(template, json!([line]));
                if out_tx.send(msg).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "console output lagged");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Merge a response template (e.g. `{"method": "process_status_update"}`) with its params.
fn with_params(template: Value, params: Value) -> Value {
    let mut obj = template.as_object().cloned().unwrap_or_default();
    obj.insert("params".to_string(), params);
    Value::Object(obj)
}
