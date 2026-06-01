//! Memory recall tool — retrieve memories from Grafeo backend
//!
//! Adapted from zeroclaw/src/tools/memory_recall.rs
//! Rollball deviation: uses rollball_core::Tool trait; replaces Memory trait
//! with GrafeoStore backend; adds agent_id isolation; supports search_mode
//! parameter for future embedding/hybrid search.
//! SPDX-License-Identifier: MIT OR Apache-2.0

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::sync::Arc;

use rollball_memory::MemoryQuery;

/// Memory recall tool — allows an Agent to recall stored memories.
///
/// Queries the Grafeo backend with real semantic/text search.
/// Automatically excludes nodes from the current session to avoid
/// re-injecting data already present in the conversation context.
pub struct MemoryRecallTool {
    /// Agent ID (namespace for memory isolation).
    /// Kept for future per-agent query filtering; Grafeo currently isolates at store level.
    #[allow(dead_code)]
    agent_id: String,
    /// Memory session handle providing store + current session context.
    /// None when no Grafeo store is available (degraded mode).
    handle: Option<Arc<crate::memory::MemorySessionHandle>>,
}

impl MemoryRecallTool {
    pub fn new(
        agent_id: &str,
        handle: Option<Arc<crate::memory::MemorySessionHandle>>,
    ) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            handle,
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

        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(10, |v| v.min(20)) as usize;

        // Resolve store and session context.
        let store = match self.handle.as_ref().and_then(|h| h.store()) {
            Some(s) => s,
            None => {
                return Ok(ToolResult {
                    ok: true,
                    content: "Memory store not available.".to_string(),
                    error: None,
                    token_usage: None,
                });
            }
        };

        let exclude_session_id = self
            .handle
            .as_ref()
            .and_then(|h| h.current_session_id());

        // Build memory query with deep recall strategy.
        // LLM can override the limit via the 'limit' parameter.
        let mut memory_query = MemoryQuery::deep_recall(query.to_string(), exclude_session_id);
        memory_query.limit = limit;

        let manager = crate::memory::MemoryManager::new(
            crate::memory::MemoryManagerConfig::default(),
        );

        // Pass embedding provider from session handle so retrieve() can
        // auto-generate query embeddings (Ollama → Remote fallback).
        let emb_provider = self
            .handle
            .as_ref()
            .and_then(|h| h.embedding());
        let emb_deref = emb_provider.as_deref();

        match manager.retrieve(&*store, &mut memory_query, emb_deref).await {
            Ok(retrieval) => {
                if retrieval.memories.is_empty() {
                    return Ok(ToolResult {
                        ok: true,
                        content: "No relevant memories found.".to_string(),
                        error: None,
                        token_usage: None,
                    });
                }

                // Format results as structured text.
                let mut lines: Vec<String> = Vec::new();
                for m in &retrieval.memories {
                    lines.push(format!(
                        "- [{}] (score={:.2}) {}",
                        m.label, m.score, m.content
                    ));
                }
                let content = lines.join("\n");

                Ok(ToolResult {
                    ok: true,
                    content,
                    error: None,
                    token_usage: None,
                })
            }
            Err(e) => Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!("Memory retrieval failed: {e}")),
                token_usage: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rollball_grafeo::GrafeoStore;

    /// Helper: create a MemoryRecallTool backed by an in-memory GrafeoStore.
    fn test_tool() -> MemoryRecallTool {
        let store = Arc::new(GrafeoStore::new_in_memory().unwrap());
        let handle = Arc::new(crate::memory::MemorySessionHandle::new(None));
        handle.set_store(store);
        MemoryRecallTool {
            agent_id: "com.test.agent".to_string(),
            handle: Some(handle),
        }
    }

    /// Helper: create a tool with no store (degraded mode).
    fn test_tool_no_store() -> MemoryRecallTool {
        MemoryRecallTool {
            agent_id: "com.test.agent".to_string(),
            handle: None,
        }
    }

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
        let tool = test_tool();
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("at least"));
    }

    #[tokio::test]
    async fn test_memory_recall_empty_query_no_store() {
        let tool = test_tool_no_store();
        let result = tool
            .execute(serde_json::json!({ "query": "user preferences" }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("not available"));
    }

    #[tokio::test]
    async fn test_memory_recall_empty_result() {
        let tool = test_tool();
        let result = tool
            .execute(serde_json::json!({ "query": "nonexistent content" }))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("No relevant memories found"));
    }

    #[tokio::test]
    async fn test_memory_recall_with_since() {
        let tool = test_tool();
        let result = tool
            .execute(serde_json::json!({ "since": "2025-01-01T00:00:00Z" }))
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_memory_recall_with_time_range() {
        let tool = test_tool();
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
        let tool = test_tool();
        let result = tool
            .execute(serde_json::json!({ "since": "not-a-date" }))
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Invalid 'since'"));
    }

    #[tokio::test]
    async fn test_memory_recall_since_after_until() {
        let tool = test_tool();
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
        let tool = test_tool();
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
            let tool = test_tool();
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
        let tool = test_tool();
        let result = tool
            .execute(serde_json::json!({ "query": "test", "limit": 100 }))
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_memory_recall_combined() {
        let tool = test_tool();
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
    }
}
