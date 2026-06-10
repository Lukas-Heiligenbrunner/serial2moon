//! Method router for the Klipper API server.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::subscribe::{self, SubState};
use crate::app::App;
use crate::gcode;
use crate::state::KlippyState;
use crate::state::eventtime::eventtime;

/// Methods we advertise via `list_endpoints`.
const ENDPOINTS: &[&str] = &[
    "info",
    "objects/list",
    "objects/query",
    "objects/subscribe",
    "gcode/script",
    "gcode/help",
    "gcode/subscribe_output",
    "gcode/restart",
    "gcode/firmware_restart",
    "register_remote_method",
    "emergency_stop",
    "list_endpoints",
];

/// Handle one request. Returns the response value to send back, or `None` for
/// notifications (requests without an `id`).
pub async fn handle_request(app: &App, sub: &Arc<Mutex<SubState>>, req: Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let result = route(app, sub, &method, &params).await;

    match id {
        Some(id) => Some(match result {
            Ok(r) => json!({ "id": id, "result": r }),
            Err(e) => json!({ "id": id, "error": { "message": e.to_string() } }),
        }),
        None => {
            if let Err(e) = result {
                warn!(method, error = %e, "notification handling failed");
            }
            None
        }
    }
}

async fn route(
    app: &App,
    sub: &Arc<Mutex<SubState>>,
    method: &str,
    params: &Value,
) -> anyhow::Result<Value> {
    match method {
        "info" => Ok(info(app)),
        "objects/list" => Ok(objects_list(app)),
        "objects/query" => Ok(objects_query(app, params)),
        "objects/subscribe" => Ok(objects_subscribe(app, sub, params).await),
        "gcode/subscribe_output" => Ok(gcode_subscribe_output(sub, params).await),
        "gcode/script" => {
            let script = params.get("script").and_then(Value::as_str).unwrap_or("");
            gcode::execute(app, script).await?;
            Ok(json!({}))
        }
        "gcode/help" => Ok(json!({})),
        "gcode/restart" | "gcode/firmware_restart" => Ok(json!({})),
        "register_remote_method" => {
            let name = params
                .get("response_template")
                .and_then(|t| t.get("remote_method"));
            info!(?name, "register_remote_method (accepted, not invoked)");
            Ok(json!({}))
        }
        "emergency_stop" => {
            let _ = app.serial.send_high("M112").await;
            app.state.update(|s| {
                s.klippy_state = KlippyState::Shutdown;
                s.state_message = "Emergency stop".to_string();
            });
            Ok(json!({}))
        }
        "list_endpoints" => Ok(json!({ "endpoints": ENDPOINTS })),
        other => {
            // Never error on an unknown method — ack empty so Moonraker doesn't stall.
            warn!(method = other, "unknown API method acked as empty");
            Ok(json!({}))
        }
    }
}

fn info(app: &App) -> Value {
    let s = app.state.snapshot();
    json!({
        "state": s.klippy_state.as_str(),
        "state_message": s.state_message,
        "hostname": hostname(),
        "klipper_path": "/opt/serial2moon",
        "python_path": "/usr/bin/python3",
        "process_id": std::process::id(),
        "cpu_info": "serial2moon bridge",
        "log_file": "/dev/null",
        "config_file": app.config.gcode_dir.display().to_string(),
        "software_version": concat!("serial2moon v", env!("CARGO_PKG_VERSION")),
    })
}

fn objects_list(app: &App) -> Value {
    let objects: Vec<String> = app.state.snapshot().full_status().into_keys().collect();
    json!({ "objects": objects })
}

fn objects_query(app: &App, params: &Value) -> Value {
    let spec = subscribe::parse_objects(params);
    let full = app.state.snapshot().full_status();
    let status = subscribe::select(&full, &spec);
    json!({ "eventtime": eventtime(), "status": status })
}

async fn objects_subscribe(app: &App, sub: &Arc<Mutex<SubState>>, params: &Value) -> Value {
    let spec = subscribe::parse_objects(params);
    let full = app.state.snapshot().full_status();
    let initial = subscribe::select(&full, &spec);

    let mut guard = sub.lock().await;
    if let Some(t) = params.get("response_template") {
        guard.status_template = t.clone();
    }
    guard.last_status = initial.clone();
    guard.status_spec = Some(spec);
    drop(guard);

    json!({ "eventtime": eventtime(), "status": initial })
}

async fn gcode_subscribe_output(sub: &Arc<Mutex<SubState>>, params: &Value) -> Value {
    let mut guard = sub.lock().await;
    if let Some(t) = params.get("response_template") {
        guard.gcode_template = t.clone();
    }
    guard.gcode_output = true;
    json!({})
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "serial2moon".to_string())
}
