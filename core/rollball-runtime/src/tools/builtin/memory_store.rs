//! Memory store tool — store memories via Grafeo backend
//!
//! Adapted from zeroclaw/src/tools/memory_store.rs
//! Rollball deviation: uses rollball_core::Tool trait;
//! adds agent_id isolation via Grafeo backend.

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

/// Memory store tool — allows an Agent to store memories for later recall.
///
/// Memories are keyed by a unique identifier and categorized for
/// efficient retrieval. In Phase 1, memories are stored via Grafeo
/// placeholder. In Phase 2+, this uses the Grafeo backend for
/// persistent storage with semantic search.
pub struct MemoryStoreTool {
    /// Agent ID (namespace for memory isolation)
    agent_id: String,
}

impl MemoryStoreTool {
    pub fn new(agent_id: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "memory_store".to_string(),
            description: "Store a fact, preference, or note in long-term memory. Use category 'core' for permanent facts, 'daily' for session notes, 'conversation' for chat context, or a custom category name.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Unique key for this memory (e.g. 'user_lang', 'project_stack')"
                    },
                    "content": {
                        "type": "string",
                        "description": "The information to remember"
                    },
                    "category": {
                        "type": "string",
                        "description": "Memory category: 'core' (permanent), 'daily' (session), 'conversation' (chat), or a custom category name. Defaults to 'core'."
                    }
                },
                "required": ["key", "content"]
            }),
        }
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let key = match params.get("key").and_then(|v| v.as_str()) {
            Some(k) if !k.trim().is_empty() => k.trim().to_string(),
            _ => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'key'".to_string()),
                    token_usage: None,
                });
            }
        };

        let content = match params.get("content").and_then(|v| v.as_str()) {
            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
            _ => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'content'".to_string()),
                    token_usage: None,
                });
            }
        };

        let category = params
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("core");

        // Validate category
        let valid_category = match category {
            "core" | "daily" | "conversation" => category.to_string(),
            other => {
                // Allow custom categories but validate the name
                if other.is_empty() || other.len() > 64 {
                    return Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(format!(
                            "Invalid category '{}'. Must be 'core', 'daily', 'conversation', or a custom name (1-64 chars)",
                            other
                        )),
                        token_usage: None,
                    });
                }
                other.to_string()
            }
        };

        // Phase 1: Return a confirmation with a generated memory ID
        // Phase 2+: Store in Grafeo backend
        let memory_id = format!(
            "mem_{}",
            &uuid::Uuid::new_v4().to_string().replace("-", "")[..12]
        );

        Ok(ToolResult {
            ok: true,
            content: format!(
                "Stored memory: {key} = {content} (category: {valid_category}, agent: {}, id: {memory_id})",
                self.agent_id
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
    fn test_memory_store_spec() {
        let spec = MemoryStoreTool::spec_value();
        assert_eq!(spec.name, "memory_store");
        assert!(spec.description.contains("long-term memory"));
        assert!(spec.input_schema["properties"]["key"].is_object());
        assert!(spec.input_schema["properties"]["content"].is_object());
        assert!(spec.input_schema["properties"]["category"].is_object());
    }

    #[tokio::test]
    async fn test_memory_store_missing_key() {
        let tool = MemoryStoreTool::new("com.test.agent");
        let result = tool.execute(serde_json::json!({ "content": "test" })).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter 'key'"));
    }

    #[tokio::test]
    async fn test_memory_store_missing_content() {
        let tool = MemoryStoreTool::new("com.test.agent");
        let result = tool.execute(serde_json::json!({ "key": "lang" })).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter 'content'"));
    }

    #[tokio::test]
    async fn test_memory_store_empty_content() {
        let tool = MemoryStoreTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({ "key": "lang", "content": "" }))
            .await
            .unwrap();
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn test_memory_store_basic() {
        let tool = MemoryStoreTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({ "key": "lang", "content": "User prefers Rust" }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("lang"));
        assert!(result.content.contains("core")); // default category
    }

    #[tokio::test]
    async fn test_memory_store_with_category_daily() {
        let tool = MemoryStoreTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({
                "key": "standup",
                "content": "Meeting notes from standup",
                "category": "daily"
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("daily"));
    }

    #[tokio::test]
    async fn test_memory_store_with_custom_category() {
        let tool = MemoryStoreTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({
                "key": "proj_note",
                "content": "Uses async runtime",
                "category": "project"
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("project"));
    }

    #[tokio::test]
    async fn test_memory_store_invalid_category() {
        let tool = MemoryStoreTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({
                "key": "test",
                "content": "test",
                "category": ""
            }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Invalid category"));
    }
}
