//! Memory recall tool — retrieve memories from Grafeo backend
//!
//! Adapted from zeroclaw/src/tools/memory_recall.rs
//! Rollball deviation: uses rollball_core::Tool trait; replaces Memory trait
//! with Phase 1 placeholder; adds agent_id isolation; supports search_mode
//! parameter for future embedding/hybrid search.
//! SPDX-License-Identifier: MIT OR Apache-2.0

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

/// Memory recall tool — allows an Agent to recall stored memories.
///
/// Supports keyword search, time-range filtering, and multiple search
/// strategies (bm25, embedding, hybrid). In Phase 1, this returns a
/// placeholder response. In Phase 2+, this queries the Grafeo backend
/// with real semantic search.
pub struct MemoryRecallTool {
    /// Agent ID (namespace for memory isolation)
    agent_id: String,
}

impl MemoryRecallTool {
    pub fn new(agent_id: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "memory_recall".to_string(),
            description: "Search long-term memory for relevant facts, preferences, or context. Returns scored results ranked by relevance. Supports keyword search, time-only query (since/until), or both.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords or phrase to search for in memory (optional if since/until provided)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results to return (default: 5)"
                    },
                    "since": {
                        "type": "string",
                        "description": "Filter memories created at or after this time (RFC 3339, e.g. 2025-03-01T00:00:00Z)"
                    },
                    "until": {
                        "type": "string",
                        "description": "Filter memories created at or before this time (RFC 3339)"
                    },
                    "search_mode": {
                        "type": "string",
                        "enum": ["bm25", "embedding", "hybrid"],
                        "description": "Search strategy: bm25 (keyword), embedding (semantic), or hybrid (both). Defaults to bm25."
                    }
                }
            }),
        }
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let since = params.get("since").and_then(|v| v.as_str());
        let until = params.get("until").and_then(|v| v.as_str());

        // Must have at least one filter criterion (query or time range)
        if query.trim().is_empty() && since.is_none() && until.is_none() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(
                    "Provide at least 'query' (keywords) or time range ('since'/'until')".to_string(),
                ),
                token_usage: None,
            });
        }

        // Validate date strings
        if let Some(s) = since
            && chrono::DateTime::parse_from_rfc3339(s).is_err() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "Invalid 'since' date: {s}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
                )),
                token_usage: None,
            });
        }
        if let Some(u) = until
            && chrono::DateTime::parse_from_rfc3339(u).is_err() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "Invalid 'until' date: {u}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
                )),
                token_usage: None,
            });
        }
        if let (Some(s), Some(u)) = (since, until)
            && let (Ok(s_dt), Ok(u_dt)) = (
                chrono::DateTime::parse_from_rfc3339(s),
                chrono::DateTime::parse_from_rfc3339(u),
            )
            && s_dt >= u_dt {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("'since' must be before 'until'".to_string()),
                token_usage: None,
            });
        }

        // Validate search_mode
        if let Some(mode) = params.get("search_mode").and_then(|v| v.as_str())
            && !["bm25", "embedding", "hybrid"].contains(&mode) {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "Invalid search_mode '{}'. Must be bm25, embedding, or hybrid",
                    mode
                )),
                token_usage: None,
            });
        }

        let _limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(5, |v| v.min(20)) as usize;

        // Phase 1: Placeholder response
        // Phase 2+: Query Grafeo backend with real search
        let mut filter_desc = Vec::new();
        if !query.is_empty() {
            filter_desc.push(format!("query='{}'", query));
        }
        if let Some(s) = since {
            filter_desc.push(format!("since={s}"));
        }
        if let Some(u) = until {
            filter_desc.push(format!("until={u}"));
        }

        Ok(ToolResult {
            ok: true,
            content: format!(
                "No memories found for agent '{}' with filters: {}. Grafeo backend not yet available in Phase 1.",
                self.agent_id,
                filter_desc.join(", ")
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
    fn test_memory_recall_spec() {
        let spec = MemoryRecallTool::spec_value();
        assert_eq!(spec.name, "memory_recall");
        assert!(spec.description.contains("long-term memory"));
        assert!(spec.input_schema["properties"]["query"].is_object());
        assert!(spec.input_schema["properties"]["search_mode"].is_object());
    }

    #[tokio::test]
    async fn test_memory_recall_no_filters() {
        let tool = MemoryRecallTool::new("com.test.agent");
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("at least"));
    }

    #[tokio::test]
    async fn test_memory_recall_with_query() {
        let tool = MemoryRecallTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({ "query": "user preferences" }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("query='user preferences'"));
    }

    #[tokio::test]
    async fn test_memory_recall_with_since() {
        let tool = MemoryRecallTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({ "since": "2025-01-01T00:00:00Z" }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("since=2025"));
    }

    #[tokio::test]
    async fn test_memory_recall_with_time_range() {
        let tool = MemoryRecallTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({
                "since": "2025-01-01T00:00:00Z",
                "until": "2025-12-31T23:59:59Z"
            }))
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_memory_recall_invalid_since() {
        let tool = MemoryRecallTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({ "since": "not-a-date" }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Invalid 'since'"));
    }

    #[tokio::test]
    async fn test_memory_recall_since_after_until() {
        let tool = MemoryRecallTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({
                "since": "2026-01-01T00:00:00Z",
                "until": "2025-01-01T00:00:00Z"
            }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("'since' must be before 'until'"));
    }

    #[tokio::test]
    async fn test_memory_recall_invalid_search_mode() {
        let tool = MemoryRecallTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({
                "query": "test",
                "search_mode": "invalid"
            }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Invalid search_mode"));
    }

    #[tokio::test]
    async fn test_memory_recall_valid_search_modes() {
        for mode in &["bm25", "embedding", "hybrid"] {
            let tool = MemoryRecallTool::new("com.test.agent");
            let result = tool
                .execute(serde_json::json!({
                    "query": "test",
                    "search_mode": mode
                }))
                .await
                .unwrap();
            assert!(result.ok, "search_mode '{}' should be valid", mode);
        }
    }

    #[tokio::test]
    async fn test_memory_recall_limit_capped() {
        let tool = MemoryRecallTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({ "query": "test", "limit": 100 }))
            .await
            .unwrap();
        assert!(result.ok); // limit is capped to 20 internally
    }

    #[tokio::test]
    async fn test_memory_recall_combined() {
        let tool = MemoryRecallTool::new("com.test.agent");
        let result = tool
            .execute(serde_json::json!({
                "query": "project status",
                "since": "2025-01-01T00:00:00Z",
                "until": "2025-12-31T23:59:59Z",
                "search_mode": "hybrid",
                "limit": 10
            }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("query='project status'"));
    }
}
