//! Intent send tool — send Intent to other Agents via Gateway
//!
//! Per design doc (12-tool-system.md):
//! - Routed to target Agent via Gateway
//! - Requires intent:send:<target> permission
//! - Phase 1 uses synchronous Intent routing; Phase 2+ supports async Intent

use acowork_core::mqtt_proto::{self, DataEnvelope, data_envelope::Payload};
use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

use crate::mqtt::client::MqttQoS;

/// Intent send tool - send an Intent to another Agent through the Gateway.
///
/// ADR-040 / ADR-033: publishes the Intent as an MQTT `ControlCommand`
/// to the target agent's control topic. The Gateway's broker routes the
/// message directly — the target agent's `control_handler` dispatches it
/// as an `IntentReceived` inbound event.
pub struct IntentSendTool {
    agent_id: String,
    mqtt_slot: crate::http::server::SharedMqttClientSlot,
}

impl IntentSendTool {
    pub fn new(
        agent_id: String,
        mqtt_slot: crate::http::server::SharedMqttClientSlot,
    ) -> Self {
        Self { agent_id, mqtt_slot }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "intent_send".to_string(),
            description: "Send an Intent message to another Agent via Gateway routing. The target Agent must declare intent:receive permission.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Target Agent ID (reverse-domain, e.g. 'com.example.calendar')"
                    },
                    "action": {
                        "type": "string",
                        "description": "Intent action name (e.g. 'schedule', 'query', 'notify')"
                    },
                    "params": {
                        "type": "object",
                        "description": "Intent payload (key-value data for the target Agent)"
                    },
                    "async": {
                        "type": "boolean",
                        "description": "If true, don't wait for response (fire-and-forget). Default: false.",
                        "default": false
                    }
                },
                "required": ["target", "action"]
            }),
        }
    }
}

#[async_trait]
impl Tool for IntentSendTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let target = match params.get("target").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'target'".to_string()),
                    token_usage: None,
                });
            }
        };

        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) if !a.trim().is_empty() => a.trim().to_string(),
            _ => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'action'".to_string()),
                    token_usage: None,
                });
            }
        };

        let intent_params = params
            .get("params")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let async_ = params
            .get("async")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Validate target format (reverse-domain)
        if !target.contains('.') {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "Invalid target '{}'. Must be a reverse-domain Agent ID (e.g. 'com.example.calendar')",
                    target
                )),
                token_usage: None,
            });
        }

        // ADR-040 / ADR-033: publish Intent via MQTT to the target agent's
        // control topic. The Gateway broker routes the message; the target
        // agent's control_handler dispatches it as IntentReceived.
        let slot = self.mqtt_slot.lock().await;
        match slot.as_ref() {
            Some(mqtt_shared) => {
                let mqtt = mqtt_shared.lock().await;
                let intent = mqtt_proto::Intent {
                    from: self.agent_id.clone(),
                    action: action.clone(),
                    params_json: serde_json::to_string(&intent_params)
                        .unwrap_or_else(|_| "{}".to_string()),
                };
                let control_cmd = mqtt_proto::ControlCommand {
                    agent_id: target.clone(),
                    command: Some(mqtt_proto::control_command::Command::Intent(intent)),
                };
                let envelope = DataEnvelope {
                    version: 1,
                    payload: Some(Payload::ControlCommand(control_cmd)),
                };
                let topic = format!("acowork/agents/{}/sessions/control/intent", target);
                match mqtt
                    .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
                    .await
                {
                    Ok(()) => {
                        let mode = if async_ { "async" } else { "sync" };
                        Ok(ToolResult {
                            ok: true,
                            content: format!(
                                "Intent sent to '{}' action='{}' mode={} via MQTT",
                                target, action, mode,
                            ),
                            error: None,
                            token_usage: None,
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(format!("Failed to publish intent via MQTT: {}", e)),
                        token_usage: None,
                    }),
                }
            }
            None => Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(
                    "MQTT client not connected. intent_send requires Gateway mode (MQTT transport)."
                        .to_string(),
                ),
                token_usage: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_tool() -> IntentSendTool {
        IntentSendTool::new(
            "com.test.agent".to_string(),
            Arc::new(tokio::sync::Mutex::new(None)),
        )
    }

    #[test]
    fn test_intent_send_spec() {
        let spec = IntentSendTool::spec_value();
        assert_eq!(spec.name, "intent_send");
        assert!(spec.input_schema["properties"]["target"].is_object());
        assert!(spec.input_schema["properties"]["action"].is_object());
    }

    #[tokio::test]
    async fn test_intent_send_missing_target() {
        let tool = test_tool();
        let result = tool
            .execute(serde_json::json!({ "action": "schedule" }), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("Missing required parameter 'target'")
        );
    }

    #[tokio::test]
    async fn test_intent_send_missing_action() {
        let tool = test_tool();
        let result = tool
            .execute(
                serde_json::json!({ "target": "com.example.calendar" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("Missing required parameter 'action'")
        );
    }

    #[tokio::test]
    async fn test_intent_send_invalid_target() {
        let tool = test_tool();
        let result = tool
            .execute(
                serde_json::json!({ "target": "calendar", "action": "schedule" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("reverse-domain"));
    }

    /// Without MQTT, a valid intent call returns an error explaining the
    /// requirement. This replaces the old placeholder that silently returned
    /// ok: true without actually dispatching anything.
    #[tokio::test]
    async fn test_intent_send_requires_mqtt() {
        let tool = test_tool();
        let result = tool
            .execute(
                serde_json::json!({
                    "target": "com.example.calendar",
                    "action": "schedule",
                    "params": { "time": "10:00", "title": "Team sync" }
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok, "intent_send without MQTT should return ok: false");
        let err = result.error.expect("should have error message");
        assert!(
            err.contains("MQTT client not connected"),
            "error should explain MQTT requirement, got: {}",
            err
        );
    }
}
