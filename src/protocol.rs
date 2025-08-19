use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Klipper protocol message structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KlipperMessage {
    pub id: Option<u64>,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

/// Klipper response structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KlipperResponse {
    pub id: Option<u64>,
    pub result: Option<serde_json::Value>,
    pub error: Option<KlipperError>,
}

/// Klipper error structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KlipperError {
    pub message: String,
    pub code: i32,
}

impl KlipperResponse {
    pub fn success(id: Option<u64>, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<u64>, message: String, code: i32) -> Self {
        Self {
            id,
            result: None,
            error: Some(KlipperError { message, code }),
        }
    }
}

/// Protocol handler for translating between Klipper and G-code
pub struct ProtocolHandler {
    printer_status: HashMap<String, serde_json::Value>,
}

impl ProtocolHandler {
    pub fn new() -> Self {
        let mut status = HashMap::new();

        // Initialize basic printer status that moonraker expects
        status.insert(
            "state".to_string(),
            serde_json::Value::String("ready".to_string()),
        );
        status.insert(
            "state_message".to_string(),
            serde_json::Value::String("Printer is ready".to_string()),
        );

        Self {
            printer_status: status,
        }
    }

    /// Handle incoming Klipper protocol message and convert to G-code if needed
    pub async fn handle_message(
        &mut self,
        message: KlipperMessage,
    ) -> Result<(KlipperResponse, Option<String>)> {
        log::info!("Handling Klipper message: {:?}", message);

        match message.method.as_str() {
            "info" => {
                let result = serde_json::json!({
                    "state": "ready",
                    "state_message": "Printer is ready",
                    "hostname": "uart2moon",
                    "software_version": "uart2moon-0.1.0",
                    "cpu_info": "uart2moon",
                    "python_path": "/usr/bin/python3"
                });
                Ok((KlipperResponse::success(message.id, result), None))
            }
            "objects/list" => {
                let result = serde_json::json!({
                    "objects": ["gcode_move", "toolhead", "extruder", "heater_bed", "print_stats"]
                });
                Ok((KlipperResponse::success(message.id, result), None))
            }
            "objects/query" => {
                // Return current printer status
                let result = serde_json::json!(self.printer_status);
                Ok((KlipperResponse::success(message.id, result), None))
            }
            "gcode/script" => {
                if let Some(params) = message.params {
                    if let Some(script) = params.get("script") {
                        if let Some(gcode) = script.as_str() {
                            log::info!("Sending G-code: {}", gcode);
                            let result = serde_json::json!({});
                            return Ok((
                                KlipperResponse::success(message.id, result),
                                Some(gcode.to_string()),
                            ));
                        }
                    }
                }
                Ok((
                    KlipperResponse::error(
                        message.id,
                        "Invalid gcode script parameters".to_string(),
                        -1,
                    ),
                    None,
                ))
            }
            "emergency_stop" => {
                log::warn!("Emergency stop requested");
                let result = serde_json::json!({});
                Ok((
                    KlipperResponse::success(message.id, result),
                    Some("M112".to_string()),
                ))
            }
            _ => {
                log::warn!("Unhandled method: {}", message.method);
                let result = serde_json::json!({});
                Ok((KlipperResponse::success(message.id, result), None))
            }
        }
    }

    /// Process G-code response from printer and update status
    #[allow(dead_code)]
    pub fn process_printer_response(&mut self, response: &str) {
        log::debug!("Processing printer response: {}", response.trim());

        // Update printer status based on response
        if response.contains("ok") {
            self.printer_status.insert(
                "state".to_string(),
                serde_json::Value::String("ready".to_string()),
            );
        } else if response.contains("Error") || response.contains("!!") {
            self.printer_status.insert(
                "state".to_string(),
                serde_json::Value::String("error".to_string()),
            );
            self.printer_status.insert(
                "state_message".to_string(),
                serde_json::Value::String(response.trim().to_string()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_protocol_handler_new() {
        let handler = ProtocolHandler::new();
        assert_eq!(
            handler.printer_status.get("state").unwrap(),
            &json!("ready")
        );
        assert_eq!(
            handler.printer_status.get("state_message").unwrap(),
            &json!("Printer is ready")
        );
    }

    #[tokio::test]
    async fn test_handle_info_message() {
        let mut handler = ProtocolHandler::new();
        let message = KlipperMessage {
            id: Some(1),
            method: "info".to_string(),
            params: None,
        };

        let (response, gcode) = handler.handle_message(message).await.unwrap();

        assert_eq!(response.id, Some(1));
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        assert!(gcode.is_none());

        let result = response.result.unwrap();
        assert_eq!(result["state"], "ready");
        assert_eq!(result["hostname"], "uart2moon");
    }

    #[tokio::test]
    async fn test_handle_objects_list_message() {
        let mut handler = ProtocolHandler::new();
        let message = KlipperMessage {
            id: Some(2),
            method: "objects/list".to_string(),
            params: None,
        };

        let (response, gcode) = handler.handle_message(message).await.unwrap();

        assert_eq!(response.id, Some(2));
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        assert!(gcode.is_none());

        let result = response.result.unwrap();
        assert!(result["objects"].is_array());
    }

    #[tokio::test]
    async fn test_handle_gcode_script_message() {
        let mut handler = ProtocolHandler::new();
        let message = KlipperMessage {
            id: Some(3),
            method: "gcode/script".to_string(),
            params: Some(json!({"script": "G28"})),
        };

        let (response, gcode) = handler.handle_message(message).await.unwrap();

        assert_eq!(response.id, Some(3));
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        assert_eq!(gcode, Some("G28".to_string()));
    }

    #[tokio::test]
    async fn test_handle_emergency_stop_message() {
        let mut handler = ProtocolHandler::new();
        let message = KlipperMessage {
            id: Some(4),
            method: "emergency_stop".to_string(),
            params: None,
        };

        let (response, gcode) = handler.handle_message(message).await.unwrap();

        assert_eq!(response.id, Some(4));
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        assert_eq!(gcode, Some("M112".to_string()));
    }

    #[tokio::test]
    async fn test_handle_invalid_gcode_script_message() {
        let mut handler = ProtocolHandler::new();
        let message = KlipperMessage {
            id: Some(5),
            method: "gcode/script".to_string(),
            params: Some(json!({"invalid": "param"})),
        };

        let (response, gcode) = handler.handle_message(message).await.unwrap();

        assert_eq!(response.id, Some(5));
        assert!(response.result.is_none());
        assert!(response.error.is_some());
        assert!(gcode.is_none());

        let error = response.error.unwrap();
        assert_eq!(error.message, "Invalid gcode script parameters");
        assert_eq!(error.code, -1);
    }

    #[tokio::test]
    async fn test_handle_unknown_method() {
        let mut handler = ProtocolHandler::new();
        let message = KlipperMessage {
            id: Some(6),
            method: "unknown/method".to_string(),
            params: None,
        };

        let (response, gcode) = handler.handle_message(message).await.unwrap();

        assert_eq!(response.id, Some(6));
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        assert!(gcode.is_none());
    }

    #[test]
    fn test_process_printer_response_ok() {
        let mut handler = ProtocolHandler::new();
        handler.process_printer_response("ok T:25.0 /0.0");

        assert_eq!(
            handler.printer_status.get("state").unwrap(),
            &json!("ready")
        );
    }

    #[test]
    fn test_process_printer_response_error() {
        let mut handler = ProtocolHandler::new();
        handler.process_printer_response("!! Emergency stop");

        assert_eq!(
            handler.printer_status.get("state").unwrap(),
            &json!("error")
        );
        assert_eq!(
            handler.printer_status.get("state_message").unwrap(),
            &json!("!! Emergency stop")
        );
    }

    #[test]
    fn test_klipper_response_success() {
        let response = KlipperResponse::success(Some(1), json!({"test": "data"}));

        assert_eq!(response.id, Some(1));
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        assert_eq!(response.result.unwrap()["test"], "data");
    }

    #[test]
    fn test_klipper_response_error() {
        let response = KlipperResponse::error(Some(1), "Test error".to_string(), 500);

        assert_eq!(response.id, Some(1));
        assert!(response.result.is_none());
        assert!(response.error.is_some());

        let error = response.error.unwrap();
        assert_eq!(error.message, "Test error");
        assert_eq!(error.code, 500);
    }
}
