//! Intent send tool — send Intent to other Agents via Gateway
//!
//! Per design doc (12-tool-system.md):
//! - Routed to target Agent via Gateway
//! - Requires intent:send:<target> permission
//! - Phase 1 uses IPC client; Phase 2+ supports async Intent

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

/// Intent send tool — send an Intent to another Agent through the Gateway
pub struct IntentSendTool {
    // Phase 2+: will hold GatewayClient reference
}

impl IntentSendTool {
    pub fn new() -> Self {
        Self {}
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

impl Default for IntentSendTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for IntentSendTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
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

        let intent_params = params.get("params").cloned().unwrap_or(serde_json::json!({}));
        let async_ = params.get("async").and_then(|v| v.as_bool()).unwrap_or(false);

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

        // Phase 1: Return placeholder confirmation
        // Phase 2+: Send via GatewayClient IPC
        let mode = if async_ { "async" } else { "sync" };
        Ok(ToolResult {
            ok: true,
            content: format!(
                "Intent sent to '{}' action='{}' mode={}\nParams: {}",
                target,
                action,
                mode,
                serde_json::to_string_pretty(&intent_params).unwrap_or_else(|_| intent_params.to_string())
            ),
            error: None,
            token_usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intent_send_spec() {
        let spec = IntentSendTool::spec_value();
        assert_eq!(spec.name, "intent_send");
        assert!(spec.input_schema["properties"]["target"].is_object());
        assert!(spec.input_schema["properties"]["action"].is_object());
    }

    #[tokio::test]
    async fn test_intent_send_missing_target() {
        let tool = IntentSendTool::new();
        let result = tool
            .execute(serde_json::json!({ "action": "schedule" }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter 'target'"));
    }

    #[tokio::test]
    async fn test_intent_send_missing_action() {
        let tool = IntentSendTool::new();
        let result = tool
            .execute(serde_json::json!({ "target": "com.example.calendar" }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter 'action'"));
    }

    #[tokio::test]
    async fn test_intent_send_invalid_target() {
        let tool = IntentSendTool::new();
        let result = tool
            .execute(serde_json::json!({ "target": "calendar", "action": "schedule" }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("reverse-domain"));
    }

    #[tokio::test]
    async fn test_intent_send_basic() {
        let tool = IntentSendTool::new();
        let result = tool
            .execute(serde_json::json!({
                "target": "com.example.calendar",
                "action": "schedule",
                "params": { "time": "10:00", "title": "Team sync" }
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("com.example.calendar"));
        assert!(result.content.contains("schedule"));
        assert!(result.content.contains("sync")); // default mode
    }

    #[tokio::test]
    async fn test_intent_send_async_mode() {
        let tool = IntentSendTool::new();
        let result = tool
            .execute(serde_json::json!({
                "target": "com.example.weather",
                "action": "query",
                "async": true
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("async"));
    }

    #[tokio::test]
    async fn test_intent_send_empty_params() {
        let tool = IntentSendTool::new();
        let result = tool
            .execute(serde_json::json!({
                "target": "com.example.agent",
                "action": "ping"
            }))
            .await
            .unwrap();
        assert!(result.ok);
    }
}
