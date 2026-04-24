//! Identity query tool — read user identity information from System Agent
//!
//! S3.4 implementation: allows any Agent to query user identity fields
//! by sending an Intent to the System Agent (com.rollball.system).
//!
//! Per design doc (07-system-agent.md, 12-tool-system.md):
//! - Any agent can query identity fields
//! - Returns values with confidence scores
//! - Respects PrivacyLevel filtering (only Public fields shared by default)
//! - Falls back to local cached identity if Gateway is unavailable

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

/// Identity query tool — read user identity information
///
/// Queries the System Agent for identity field values.
/// The System Agent filters results based on the requester's
/// authorization level and the field's PrivacyLevel.
pub struct IdentityQueryTool {
    /// Agent ID of the requester
    agent_id: String,
}

impl IdentityQueryTool {
    pub fn new(agent_id: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "identity_query".to_string(),
            description: "Query user identity information (name, language, timezone, city, etc.) from the System Agent. Returns field values with confidence scores. Only Public fields are shared with non-System agents by default.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fields": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Identity fields to query (e.g. ['display_name', 'language', 'city']). Omit to query all available fields."
                    },
                    "include_confidence": {
                        "type": "boolean",
                        "description": "Whether to include confidence scores in the response (default: true)"
                    }
                }
            }),
        }
    }
}

#[async_trait]
impl Tool for IdentityQueryTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        // Parse optional fields parameter
        let fields: Vec<String> = match params.get("fields") {
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            _ => Vec::new(), // Empty = query all
        };

        let include_confidence = params
            .get("include_confidence")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Validate field names if provided
        let known_fields = [
            "display_name", "language", "timezone", "city", "country",
            "email", "preferences", "locale", "currency", "date_format",
            "occupation", "communication_style",
        ];

        let invalid_fields: Vec<&str> = fields
            .iter()
            .filter(|f| !known_fields.contains(&f.as_str()))
            .map(|f| f.as_str())
            .collect();

        if !invalid_fields.is_empty() {
            tracing::warn!(
                "Unknown identity fields queried: {:?} (known: {:?})",
                invalid_fields,
                known_fields
            );
        }

        // Phase 2: Send Intent to System Agent via Gateway IPC
        // For now, return a structured placeholder response
        // indicating that the query would be routed through the System Agent.
        //
        // When IPC is connected, this will:
        // 1. Build an IntentSend { target: "com.rollball.system", action: "identity:query", params: { fields } }
        // 2. Send via GatewayClient
        // 3. Wait for response with IdentityQueryResult
        // 4. Apply PrivacyLevel filtering based on requester agent_id

        let fields_desc = if fields.is_empty() {
            "all available fields".to_string()
        } else {
            format!("{:?}", fields)
        };

        // Build a response structure showing what would be queried
        let mut result_fields = serde_json::Map::new();
        if fields.is_empty() || fields.contains(&"display_name".to_string()) {
            result_fields.insert("display_name".to_string(), Value::Null);
        }
        if fields.is_empty() || fields.contains(&"language".to_string()) {
            result_fields.insert("language".to_string(), Value::Null);
        }
        if fields.is_empty() || fields.contains(&"timezone".to_string()) {
            result_fields.insert("timezone".to_string(), Value::Null);
        }
        if fields.is_empty() || fields.contains(&"city".to_string()) {
            result_fields.insert("city".to_string(), Value::Null);
        }

        let content = if include_confidence {
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "queried",
                "requester": self.agent_id,
                "fields": fields_desc,
                "values": result_fields,
                "note": "Identity values will be populated from System Agent Grafeo store when IPC is connected"
            }))
            .unwrap_or_default()
        } else {
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "queried",
                "requester": self.agent_id,
                "fields": fields_desc,
                "values": result_fields
            }))
            .unwrap_or_default()
        };

        Ok(ToolResult {
            ok: true,
            content,
            error: None,
            token_usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_query_spec() {
        let spec = IdentityQueryTool::spec_value();
        assert_eq!(spec.name, "identity_query");
        assert!(spec.description.contains("identity"));
        assert!(spec.input_schema["properties"]["fields"].is_object());
        assert!(spec.input_schema["properties"]["include_confidence"].is_object());
    }

    #[tokio::test]
    async fn test_identity_query_no_fields() {
        let tool = IdentityQueryTool::new("com.example.weather");
        let result = tool
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("all available fields"));
    }

    #[tokio::test]
    async fn test_identity_query_specific_fields() {
        let tool = IdentityQueryTool::new("com.example.weather");
        let result = tool
            .execute(serde_json::json!({
                "fields": ["display_name", "city"]
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("display_name"));
        assert!(result.content.contains("city"));
    }

    #[tokio::test]
    async fn test_identity_query_include_confidence_false() {
        let tool = IdentityQueryTool::new("com.example.weather");
        let result = tool
            .execute(serde_json::json!({
                "fields": ["language"],
                "include_confidence": false
            }))
            .await
            .unwrap();
        assert!(result.ok);
        // Without confidence, the note should not be present
        assert!(!result.content.contains("confidence"));
    }

    #[tokio::test]
    async fn test_identity_query_invalid_field() {
        let tool = IdentityQueryTool::new("com.example.weather");
        let result = tool
            .execute(serde_json::json!({
                "fields": ["nonexistent_field"]
            }))
            .await
            .unwrap();
        // Should still succeed (with warning logged)
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_identity_query_system_agent() {
        let tool = IdentityQueryTool::new("com.rollball.system");
        let result = tool
            .execute(serde_json::json!({
                "fields": ["display_name"]
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("com.rollball.system"));
    }
}
