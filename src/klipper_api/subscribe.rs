//! Per-connection subscription state plus the status selection / delta logic.
//!
//! Klipper sends the full subscribed status once (in the subscribe response), then only
//! changed fields thereafter. Diffing is per-connection, so each client tracks its own
//! last-pushed snapshot.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// `object name -> requested fields` (None means "all fields").
pub type ObjectSpec = BTreeMap<String, Option<Vec<String>>>;

/// Mutable per-connection subscription state.
pub struct SubState {
    /// Status objects this connection subscribed to (None until objects/subscribe).
    pub status_spec: Option<ObjectSpec>,
    /// Template merged into each status push.
    pub status_template: Value,
    /// Last selected status pushed, for delta computation.
    pub last_status: Map<String, Value>,
    /// Whether this connection wants console output.
    pub gcode_output: bool,
    /// Template merged into each console push.
    pub gcode_template: Value,
}

impl Default for SubState {
    fn default() -> Self {
        SubState {
            status_spec: None,
            status_template: json!({ "method": "process_status_update" }),
            last_status: Map::new(),
            gcode_output: false,
            gcode_template: json!({ "method": "process_gcode_response" }),
        }
    }
}

/// Parse the `{objects: {name: null|[fields]}}` shape of a subscribe/query request.
pub fn parse_objects(params: &Value) -> ObjectSpec {
    let mut spec = BTreeMap::new();
    if let Some(objs) = params.get("objects").and_then(|v| v.as_object()) {
        for (name, fields) in objs {
            let fields = fields.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|f| f.as_str().map(String::from))
                    .collect::<Vec<_>>()
            });
            spec.insert(name.clone(), fields);
        }
    }
    spec
}

/// Project the full status map down to the requested objects/fields.
pub fn select(full: &BTreeMap<String, Value>, spec: &ObjectSpec) -> Map<String, Value> {
    let mut out = Map::new();
    for (name, fields) in spec {
        let Some(obj) = full.get(name) else { continue };
        match fields {
            None => {
                out.insert(name.clone(), obj.clone());
            }
            Some(fields) => {
                let map = obj.as_object();
                let mut sub = Map::new();
                if let Some(map) = map {
                    for f in fields {
                        if let Some(v) = map.get(f) {
                            sub.insert(f.clone(), v.clone());
                        }
                    }
                }
                out.insert(name.clone(), Value::Object(sub));
            }
        }
    }
    out
}

/// Per-object, per-field delta from `old` to `new`.
pub fn diff(old: &Map<String, Value>, new: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (name, new_obj) in new {
        match old.get(name) {
            Some(old_obj) if old_obj == new_obj => {}
            Some(old_obj) => match (old_obj.as_object(), new_obj.as_object()) {
                (Some(o), Some(n)) => {
                    let mut changed = Map::new();
                    for (k, v) in n {
                        if o.get(k) != Some(v) {
                            changed.insert(k.clone(), v.clone());
                        }
                    }
                    if !changed.is_empty() {
                        out.insert(name.clone(), Value::Object(changed));
                    }
                }
                _ => {
                    out.insert(name.clone(), new_obj.clone());
                }
            },
            None => {
                out.insert(name.clone(), new_obj.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert(
            "extruder".into(),
            json!({"temperature": 25.0, "target": 0.0}),
        );
        m.insert("fan".into(), json!({"speed": 0.0}));
        m
    }

    #[test]
    fn select_filters_fields() {
        let spec: ObjectSpec = [(
            "extruder".to_string(),
            Some(vec!["temperature".to_string()]),
        )]
        .into_iter()
        .collect();
        let s = select(&full(), &spec);
        assert_eq!(s["extruder"], json!({"temperature": 25.0}));
    }

    #[test]
    fn diff_reports_only_changes() {
        let old = select(&full(), &all_spec());
        let mut changed = full();
        changed.insert(
            "extruder".into(),
            json!({"temperature": 200.0, "target": 200.0}),
        );
        let new = select(&changed, &all_spec());
        let d = diff(&old, &new);
        assert_eq!(d.len(), 1);
        assert_eq!(
            d["extruder"],
            json!({"temperature": 200.0, "target": 200.0})
        );
    }

    fn all_spec() -> ObjectSpec {
        [("extruder".to_string(), None), ("fan".to_string(), None)]
            .into_iter()
            .collect()
    }
}
