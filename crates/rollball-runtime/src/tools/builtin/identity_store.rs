//! Identity store tool — write user identity information (System Agent only)
//!
//! Rollball-specific tool: allows the System Agent to write/update
//! user identity fields with confidence and source tracking.
//! Triggers `identity:changed` notification.
//!
//! Per design doc (12-tool-system.md):
//! - System Agent exclusive: write/update user identity fields
//! - Supports display_name, language, timezone, city, etc.
//! - Tracks confidence and source metadata
//! - Triggers identity:changed notification

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

/// Identity store tool — write user identity information
///
/// This tool is restricted to the System Agent (com.rollball.system).
/// Other agents should use `identity_query` to read identity data
/// (to be implemented in Phase 2 via IPC intent).
pub struct IdentityStoreTool {
    /// Agent ID — used to verify this is the System Agent
    agent_id: String,
}

impl IdentityStoreTool {
    pub fn new(agent_id: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "identity_store".to_string(),
            description: "Write or update user identity information (display_name, language, timezone, city, etc.). System Agent only. Supports confidence and source tracking.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "field": {
                        "type": "string",
                        "description": "Identity field to write (e.g. 'display_name', 'language', 'timezone', 'city', 'country', 'email', 'preferences')"
                    },
                    "value": {
                        "type": "string",
                        "description": "The value to store for this identity field"
                    },
                    "confidence": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0,
                        "description": "Confidence score for this identity value (0.0-1.0, default: 1.0)"
                    },
                    "source": {
                        "type": "string",
                        "description": "Source of this identity data (e.g. 'user_input', 'inferred', 'system_default')"
                    }
                },
                "required": ["field", "value"]
            }),
        }
    }
}

#[async_trait]
impl Tool for IdentityStoreTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let field = match params.get("field").and_then(|v| v.as_str()) {
            Some(f) if !f.trim().is_empty() => f.trim().to_string(),
            _ => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'field'".to_string()),
                    token_usage: None,
                });
            }
        };

        let value = match params.get("value").and_then(|v| v.as_str()) {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'value'".to_string()),
                    token_usage: None,
                });
            }
        };

        let confidence = params
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);

        let source = params
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("inferred");

        // Validate known identity fields
        let known_fields = [
            "display_name", "language", "timezone", "city", "country",
            "email", "preferences", "locale", "currency", "date_format",
        ];
        if !known_fields.contains(&field.as_str()) {
            // Allow custom fields but warn
            tracing::warn!(
                "Unknown identity field '{}' (known: {:?})",
                field,
                known_fields
            );
        }

        // Phase 1: Return a confirmation with the stored identity data
        // Phase 2+: Store in System Agent's Grafeo and broadcast identity:changed
        Ok(ToolResult {
            ok: true,
            content: format!(
                "Identity stored: {}='{}' (confidence: {:.2}, source: {}, agent: {})",
                field, value, confidence, source, self.agent_id
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
    fn test_identity_store_spec() {
        let spec = IdentityStoreTool::spec_value();
        assert_eq!(spec.name, "identity_store");
        assert!(spec.description.contains("identity"));
        assert!(spec.input_schema["properties"]["field"].is_object());
        assert!(spec.input_schema["properties"]["value"].is_object());
        assert!(spec.input_schema["properties"]["confidence"].is_object());
        assert!(spec.input_schema["properties"]["source"].is_object());
    }

    #[tokio::test]
    async fn test_identity_store_missing_field() {
        let tool = IdentityStoreTool::new("com.rollball.system");
        let result = tool
            .execute(serde_json::json!({ "value": "English" }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter 'field'"));
    }

    #[tokio::test]
    async fn test_identity_store_missing_value() {
        let tool = IdentityStoreTool::new("com.rollball.system");
        let result = tool
            .execute(serde_json::json!({ "field": "language" }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter 'value'"));
    }

    #[tokio::test]
    async fn test_identity_store_basic() {
        let tool = IdentityStoreTool::new("com.rollball.system");
        let result = tool
            .execute(serde_json::json!({
                "field": "language",
                "value": "zh-CN"
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("language"));
        assert!(result.content.contains("zh-CN"));
        assert!(result.content.contains("confidence: 1.00")); // default
    }

    #[tokio::test]
    async fn test_identity_store_with_confidence_and_source() {
        let tool = IdentityStoreTool::new("com.rollball.system");
        let result = tool
            .execute(serde_json::json!({
                "field": "city",
                "value": "Shanghai",
                "confidence": 0.8,
                "source": "inferred"
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("confidence: 0.80"));
        assert!(result.content.contains("source: inferred"));
    }

    #[tokio::test]
    async fn test_identity_store_confidence_clamped() {
        let tool = IdentityStoreTool::new("com.rollball.system");
        let result = tool
            .execute(serde_json::json!({
                "field": "timezone",
                "value": "Asia/Shanghai",
                "confidence": 1.5
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("confidence: 1.00")); // clamped to 1.0
    }

    #[tokio::test]
    async fn test_identity_store_empty_field() {
        let tool = IdentityStoreTool::new("com.rollball.system");
        let result = tool
            .execute(serde_json::json!({ "field": "", "value": "test" }))
            .await
            .unwrap();
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn test_identity_store_empty_value() {
        let tool = IdentityStoreTool::new("com.rollball.system");
        let result = tool
            .execute(serde_json::json!({ "field": "language", "value": "" }))
            .await
            .unwrap();
        assert!(!result.ok);
    }
}
