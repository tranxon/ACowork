//! Tool trait and related types

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;

/// Tool specification (name, description, input schema)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name (unique identifier)
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// JSON Schema for input parameters
    #[serde(rename = "parameters")]
    pub input_schema: Value,
}

/// Tool execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Whether the execution was successful
    pub ok: bool,
    /// Result content (text or structured data)
    pub content: String,
    /// Optional error message
    #[serde(default)]
    pub error: Option<String>,
    /// Token usage statistics
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
}

/// Token usage statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Core Tool trait that all tools must implement
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get tool specification
    fn spec(&self) -> ToolSpec;

    /// Get tool name (convenience method)
    fn name(&self) -> String {
        self.spec().name.clone()
    }

    /// Execute the tool with given parameters.
    ///
    /// `work_dir` is the caller-resolved workspace directory path.
    /// Filesystem tools (file_read, file_write, etc.) use this as the
    /// base directory for relative path resolution, overriding any
    /// construction-time default. Non-filesystem tools may ignore it.
    async fn execute(&self, params: Value, work_dir: Option<&str>) -> Result<ToolResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_spec_serialization_uses_parameters() {
        let spec = ToolSpec {
            name: "shell".to_string(),
            description: "test".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string"
                    }
                },
                "required": ["command"]
            }),
        };

        let serialized = serde_json::to_value(&spec).unwrap();

        // 验证序列化后使用 "parameters" 而不是 "input_schema"
        assert!(
            serialized.get("parameters").is_some(),
            "Should have 'parameters' field"
        );
        assert!(
            serialized.get("input_schema").is_none(),
            "Should NOT have 'input_schema' field"
        );

        // 验证 parameters 内容正确
        let params = serialized.get("parameters").unwrap();
        assert!(params
            .get("properties")
            .unwrap()
            .get("command")
            .is_some());

        println!(
            "Serialized JSON: {}",
            serde_json::to_string_pretty(&serialized).unwrap()
        );
    }
}
