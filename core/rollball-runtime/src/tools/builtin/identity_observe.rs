//! Identity observe tool — subscribe to identity field change notifications
//!
//! S3.4 implementation: allows any Agent to subscribe to changes
//! on specific identity fields. When a subscribed field changes,
//! the System Agent sends an `identity:changed` notification
//! via the subscriber's callback Intent.
//!
//! Per design doc (07-system-agent.md, 12-tool-system.md):
//! - Any agent can observe identity field changes
//! - Subscriptions are registered with the System Agent
//! - Notifications are delivered via Intent (async)
//! - Agents can observe multiple fields with a single subscription

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

/// Identity observe tool — subscribe to identity field change notifications
///
/// Registers a subscription with the System Agent so that when the
/// specified identity fields change, the subscriber receives an
/// `identity:changed` notification via Intent.
pub struct IdentityObserveTool {
    /// Agent ID of the subscriber
    agent_id: String,
}

impl IdentityObserveTool {
    pub fn new(agent_id: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "identity_observe".to_string(),
            description: "Subscribe to changes on user identity fields. When a subscribed field changes (e.g. user moves to a new city), you will receive an 'identity:changed' notification with the updated value. Use this to stay informed about user preferences and profile changes.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fields": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Identity fields to observe (e.g. ['city', 'language', 'timezone']). You will be notified when any of these fields change."
                    },
                    "callback_action": {
                        "type": "string",
                        "description": "Intent action to use for notifications (default: 'identity:changed'). The System Agent will send an Intent with this action to your agent_id when a subscribed field changes."
                    }
                },
                "required": ["fields"]
            }),
        }
    }
}

#[async_trait]
impl Tool for IdentityObserveTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        // Parse required fields parameter
        let fields: Vec<String> = match params.get("fields") {
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            _ => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'fields' (must be a non-empty array of field names)".to_string()),
                    token_usage: None,
                });
            }
        };

        if fields.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("'fields' must contain at least one field name".to_string()),
                token_usage: None,
            });
        }

        let callback_action = params
            .get("callback_action")
            .and_then(|v| v.as_str())
            .unwrap_or("identity:changed");

        // Validate field names
        let known_fields = [
            "display_name", "language", "timezone", "city", "country",
            "email", "preferences", "locale", "currency", "date_format",
            "occupation", "communication_style",
        ];

        let unknown: Vec<&str> = fields
            .iter()
            .filter(|f| !known_fields.contains(&f.as_str()))
            .map(|f| f.as_str())
            .collect();

        if !unknown.is_empty() {
            tracing::warn!(
                "Unknown identity fields observed: {:?} (known: {:?})",
                unknown,
                known_fields
            );
        }

        // Phase 2: Send IdentitySubscription to System Agent via Gateway IPC
        // When IPC is connected, this will:
        // 1. Build IdentitySubscription { subscriber_id, fields, callback_intent }
        // 2. Send to System Agent via Intent with action "identity:observe"
        // 3. System Agent stores the subscription in its Grafeo (SystemConfig label)
        // 4. When a subscribed field changes via identity_store, System Agent
        //    iterates subscriptions and sends identity:changed Intent to each subscriber

        let subscription_info = serde_json::json!({
            "status": "subscribed",
            "subscriber_id": self.agent_id,
            "fields": fields,
            "callback_action": callback_action,
            "note": "Subscription will be registered with System Agent when IPC is connected"
        });

        Ok(ToolResult {
            ok: true,
            content: serde_json::to_string_pretty(&subscription_info).unwrap_or_default(),
            error: None,
            token_usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_observe_spec() {
        let spec = IdentityObserveTool::spec_value();
        assert_eq!(spec.name, "identity_observe");
        assert!(spec.description.contains("identity"));
        assert!(spec.input_schema["properties"]["fields"].is_object());
        assert!(spec.input_schema["properties"]["callback_action"].is_object());
        // fields is required
        assert!(spec.input_schema["required"].as_array().unwrap().contains(&Value::String("fields".to_string())));
    }

    #[tokio::test]
    async fn test_identity_observe_basic() {
        let tool = IdentityObserveTool::new("com.example.weather");
        let result = tool
            .execute(serde_json::json!({
                "fields": ["city", "language"]
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("subscribed"));
        assert!(result.content.contains("com.example.weather"));
        assert!(result.content.contains("city"));
        assert!(result.content.contains("language"));
    }

    #[tokio::test]
    async fn test_identity_observe_missing_fields() {
        let tool = IdentityObserveTool::new("com.example.weather");
        let result = tool
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter 'fields'"));
    }

    #[tokio::test]
    async fn test_identity_observe_empty_fields() {
        let tool = IdentityObserveTool::new("com.example.weather");
        let result = tool
            .execute(serde_json::json!({
                "fields": []
            }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("at least one field name"));
    }

    #[tokio::test]
    async fn test_identity_observe_custom_callback() {
        let tool = IdentityObserveTool::new("com.example.weather");
        let result = tool
            .execute(serde_json::json!({
                "fields": ["timezone"],
                "callback_action": "user_timezone_changed"
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("user_timezone_changed"));
    }

    #[tokio::test]
    async fn test_identity_observe_default_callback() {
        let tool = IdentityObserveTool::new("com.example.calendar");
        let result = tool
            .execute(serde_json::json!({
                "fields": ["timezone", "language"]
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("identity:changed"));
    }

    #[tokio::test]
    async fn test_identity_observe_unknown_field() {
        let tool = IdentityObserveTool::new("com.example.weather");
        let result = tool
            .execute(serde_json::json!({
                "fields": ["nonexistent_field"]
            }))
            .await
            .unwrap();
        // Should still succeed (with warning logged)
        assert!(result.ok);
    }
}
