//! Memory recall tool — retrieve memories from Grafeo backend
//!
//! Adapted from zeroclaw/src/tools/memory_recall.rs
//! ACowork deviation: uses acowork_core::Tool trait; replaces Memory trait
//! with GrafeoStore backend; adds agent_id isolation. The retrieval strategy
//! (hybrid/BM25/vector) is decided by the engine based on embedding
//! availability — NOT exposed to the LLM (ADR-062: strategy is an engine
//! concern, see memory_recall.md).
//! SPDX-License-Identifier: MIT OR Apache-2.0

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use acowork_memory::MemoryQuery;

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
    pub fn new(agent_id: &str, handle: Option<Arc<crate::memory::MemorySessionHandle>>) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            handle,
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "memory_recall".to_string(),
            description: "Search long-term memory for relevant facts, preferences, or context. Returns scored results ranked by relevance. Supports keyword search, time-only query (since/until), or both. NOTE: if the conversation context already includes auto-injected memories (a retrieved-memory block is present in the system prompt), do NOT re-run the same query — use this tool only for deeper or targeted recall (different keywords, graph neighbors, or time-filtered) to avoid duplicating already-present context.".to_string(),
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

    async fn execute(
        &self,
        params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let since = params.get("since").and_then(|v| v.as_str());
        let until = params.get("until").and_then(|v| v.as_str());

        // Must have at least one filter criterion (query or time range)
        if query.trim().is_empty() && since.is_none() && until.is_none() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(
                    "Provide at least 'query' (keywords) or time range ('since'/'until')"
                        .to_string(),
                ),
                token_usage: None,
            });
        }

        // Validate date strings
        if let Some(s) = since
            && chrono::DateTime::parse_from_rfc3339(s).is_err()
        {
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
            && chrono::DateTime::parse_from_rfc3339(u).is_err()
        {
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
            && s_dt >= u_dt
        {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("'since' must be before 'until'".to_string()),
                token_usage: None,
            });
        }

        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(5, |v| v.min(20)) as usize;

        // Resolve provider and session context.
        // ADR-051 C3: Use grafeo_store() compat accessor for MemoryManager
        // (which still takes &GrafeoStore in C3; C4 will migrate to trait).
        let provider = match self.handle.as_ref().and_then(|h| h.provider()) {
            Some(p) => p,
            None => {
                return Ok(ToolResult {
                    ok: true,
                    content: "Memory store not available.".to_string(),
                    error: None,
                    token_usage: None,
                });
            }
        };

        let exclude_session_id = self.handle.as_ref().and_then(|h| h.current_session_id());

        // Build memory query with deep recall strategy.
        // LLM can override the limit via the 'limit' parameter.
        let mut memory_query = MemoryQuery::deep_recall(query.to_string(), exclude_session_id);
        memory_query.limit = limit;

        // since/until → time_range filter. The dates were validated above;
        // this finally wires them into the query (previously validated but
        // never applied — ADR-062 M5). Single-sided ranges get a sane bound:
        // `since` only → [since, now]; `until` only → [epoch, until].
        if let Some(s) = since {
            if let Ok(s_dt) = chrono::DateTime::parse_from_rfc3339(s) {
                let until_dt = until
                    .and_then(|u| chrono::DateTime::parse_from_rfc3339(u).ok())
                    .map(|u| u.with_timezone(&chrono::Utc))
                    .unwrap_or_else(chrono::Utc::now);
                memory_query.filters.time_range =
                    Some((s_dt.with_timezone(&chrono::Utc), until_dt));
            }
        } else if let Some(u) = until
            && let Ok(u_dt) = chrono::DateTime::parse_from_rfc3339(u)
        {
            let epoch = chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
                .unwrap_or_else(chrono::Utc::now);
            memory_query.filters.time_range =
                Some((epoch, u_dt.with_timezone(&chrono::Utc)));
        }

        // Config consistency (ADR-062 M5): use the agent's MemoryManagerConfig
        // (quality min_score / graph_expand / …) so `memory_recall` behaves
        // identically to auto-inject for the same agent. Falls back to
        // defaults when the handle has no config (tests, degraded mode).
        let config = self
            .handle
            .as_ref()
            .and_then(|h| h.memory_config())
            .unwrap_or_default();
        let manager = crate::memory::MemoryManager::new(config);

        // Pass embedding provider from session handle so retrieve() can
        // auto-generate query embeddings (Ollama → Remote fallback).
        let emb_provider = self.handle.as_ref().and_then(|h| h.embedding());
        let emb_deref = emb_provider.as_deref();

        match manager
            .retrieve(provider.as_ref(), &mut memory_query, emb_deref)
            .await
        {
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
    use acowork_grafeo::GrafeoStore;

    /// Helper: create a MemoryRecallTool backed by an in-memory GrafeoStore.
    fn test_tool() -> MemoryRecallTool {
        let store = Arc::new(GrafeoStore::new_in_memory().unwrap());
        let handle = Arc::new(crate::memory::MemorySessionHandle::new(None));
        handle.set_provider(store);
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
        assert!(
            spec.description.contains("do NOT re-run the same query"),
            "spec must warn the LLM against duplicating auto-injected memories"
        );
        assert!(spec.input_schema["properties"]["query"].is_object());
        // Schema must match the actual default used in execute() (map_or(5)).
        assert_eq!(
            spec.input_schema["properties"]["limit"]["description"],
            "Max results to return (default: 5)"
        );
        // since/until must be documented (they are applied via filters.time_range).
        assert!(
            spec.input_schema["properties"]["since"]["description"]
                .as_str()
                .unwrap()
                .contains("created at or after")
        );
        assert!(
            spec.input_schema["properties"]["until"]["description"]
                .as_str()
                .unwrap()
                .contains("created at or before")
        );
    }

    #[tokio::test]
    async fn test_memory_recall_no_filters() {
        let tool = test_tool();
        let result = tool.execute(serde_json::json!({}), None).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("at least"));
    }

    #[tokio::test]
    async fn test_memory_recall_empty_query_no_store() {
        let tool = test_tool_no_store();
        let result = tool
            .execute(serde_json::json!({ "query": "user preferences" }), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("not available"));
    }

    #[tokio::test]
    async fn test_memory_recall_empty_result() {
        let tool = test_tool();
        let result = tool
            .execute(serde_json::json!({ "query": "nonexistent content" }), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("No relevant memories found"));
    }

    #[tokio::test]
    async fn test_memory_recall_with_since() {
        let tool = test_tool();
        let result = tool
            .execute(serde_json::json!({ "since": "2025-01-01T00:00:00Z" }), None)
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_memory_recall_with_time_range() {
        let tool = test_tool();
        let result = tool
            .execute(
                serde_json::json!({
                    "since": "2025-01-01T00:00:00Z",
                    "until": "2025-12-31T23:59:59Z"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_memory_recall_invalid_since() {
        let tool = test_tool();
        let result = tool
            .execute(serde_json::json!({ "since": "not-a-date" }), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Invalid 'since'"));
    }

    #[tokio::test]
    async fn test_memory_recall_since_after_until() {
        let tool = test_tool();
        let result = tool
            .execute(
                serde_json::json!({
                    "since": "2026-01-01T00:00:00Z",
                    "until": "2025-01-01T00:00:00Z"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("'since' must be before 'until'")
        );
    }

    #[tokio::test]
    async fn test_memory_recall_limit_capped() {
        let tool = test_tool();
        let result = tool
            .execute(serde_json::json!({ "query": "test", "limit": 100 }), None)
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_memory_recall_combined() {
        let tool = test_tool();
        let result = tool
            .execute(
                serde_json::json!({
                    "query": "project status",
                    "since": "2025-01-01T00:00:00Z",
                    "until": "2025-12-31T23:59:59Z",
                    "limit": 5
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    // ── ADR-051 C5: InMemoryProvider tests ──────────────────────────────
    // These tests prove the Runtime can work without GrafeoStore.

    use crate::test_support::InMemoryProvider;
    use acowork_memory::{MemoryProvider, MemoryStoreInput};

    /// Helper: create a MemoryRecallTool backed by InMemoryProvider.
    fn test_tool_inmemory() -> (MemoryRecallTool, Arc<InMemoryProvider>) {
        let provider = Arc::new(InMemoryProvider::new());
        let handle = Arc::new(crate::memory::MemorySessionHandle::new(None));
        handle.set_provider(provider.clone());
        let tool = MemoryRecallTool {
            agent_id: "com.test.agent".to_string(),
            handle: Some(handle),
        };
        (tool, provider)
    }
    /// Migrated from test_memory_recall_empty_result: uses InMemoryProvider
    /// instead of GrafeoStore to verify empty retrieval works.
    #[tokio::test]
    async fn test_memory_recall_empty_result_inmemory() {
        let (tool, _provider) = test_tool_inmemory();
        let result = tool
            .execute(serde_json::json!({ "query": "nonexistent content" }), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("No relevant memories found"));
    }

    /// Full cycle: store a memory via InMemoryProvider, then recall it
    /// through the memory_recall tool. Proves the retrieval pipeline
    /// works end-to-end without GrafeoStore.
    #[tokio::test]
    async fn test_memory_recall_store_and_recall_inmemory() {
        let (tool, provider) = test_tool_inmemory();

        // Store a fact via the provider directly.
        let input = MemoryStoreInput {
            content: "User lives in Shanghai".to_string(),
            sub_type: acowork_memory::KnowledgeSubType::Fact,
            subject: None,
            predicate: None,
            object: None,
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: None,
            privacy: None,
            importance: None,
            keywords: None,
            autobiographical: None,
        };
        let result = provider.process_memory_store(&input).unwrap();
        assert!(result.is_some());

        // Recall it through the tool.
        let result = tool
            .execute(serde_json::json!({ "query": "Shanghai" }), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.content.contains("Shanghai"),
            "Expected recall result to contain 'Shanghai', got: {}",
            result.content
        );
    }

    /// Verify that a query with no search_mode works with InMemoryProvider
    /// (the engine picks the strategy automatically — search_mode was removed
    /// as an LLM-facing knob; strategy is an engine concern).
    #[tokio::test]
    async fn test_memory_recall_plain_query_inmemory() {
        let (tool, _provider) = test_tool_inmemory();
        let result = tool
            .execute(
                serde_json::json!({
                    "query": "test"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    /// Time-range filtering (`since`/`until`) must actually filter by
    /// created_at — previously validated but never applied (ADR-062 M5).
    /// InMemoryProvider variant covers the manager post-filter + tool wiring.
    #[tokio::test]
    async fn test_memory_recall_since_filters_by_created_at_inmemory() {
        use acowork_memory::types::{
            KnowledgeNode, KnowledgeSubType, NodeStatus, PrivacyLevel,
        };
        use chrono::{Duration, Utc};

        let (tool, provider) = test_tool_inmemory();
        let now = Utc::now();

        let old = KnowledgeNode {
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "coffee".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: now - Duration::days(30),
            updated_at: now - Duration::days(30),
            metadata: Default::default(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        provider.store_knowledge(&old).unwrap();

        let mut recent = old.clone();
        recent.object = "rust".to_string();
        recent.created_at = now - Duration::days(1);
        recent.updated_at = now - Duration::days(1);
        provider.store_knowledge(&recent).unwrap();

        // No time filter: both memories are recalled.
        let result = tool
            .execute(serde_json::json!({ "query": "likes" }), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("coffee"));
        assert!(result.content.contains("rust"));

        // since = 7 days ago: the 30-day-old memory must be filtered out.
        let since = (now - Duration::days(7)).to_rfc3339();
        let result = tool
            .execute(serde_json::json!({ "query": "likes", "since": since }), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.content.contains("rust"),
            "recent memory must be recalled, got: {}",
            result.content
        );
        assert!(
            !result.content.contains("coffee"),
            "old memory must be filtered by since, got: {}",
            result.content
        );
    }

    /// GrafeoStore variant of the time-range filter test — covers
    /// `GrafeoProvider::get_node_created_at` (real property read path).
    #[tokio::test]
    async fn test_memory_recall_since_filters_by_created_at_grafeo() {
        use acowork_memory::types::{
            KnowledgeNode, KnowledgeSubType, NodeStatus, PrivacyLevel,
        };
        use chrono::{Duration, Utc};

        let store: Arc<dyn acowork_memory::MemoryProvider> =
            Arc::new(GrafeoStore::new_in_memory().unwrap());
        let handle = Arc::new(crate::memory::MemorySessionHandle::new(None));
        handle.set_provider(store.clone());
        let tool = MemoryRecallTool {
            agent_id: "com.test.agent".to_string(),
            handle: Some(handle),
        };

        let now = Utc::now();
        let old = KnowledgeNode {
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "coffee".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: now - Duration::days(30),
            updated_at: now - Duration::days(30),
            metadata: Default::default(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        store.store_knowledge(&old).unwrap();

        // NOTE: use a different `subject` than `old` — GrafeoStore's semantic
        // dedup (semantic/knowledge.rs) merges same (subject, predicate) nodes
        // when embeddings are absent, keeping the *old* created_at. That would
        // collapse both nodes into one 30-day-old node and `since` would
        // correctly filter it out (not what this test targets).
        let mut recent = old.clone();
        recent.subject = "colleague".to_string();
        recent.object = "rust".to_string();
        recent.created_at = now - Duration::days(1);
        recent.updated_at = now - Duration::days(1);
        store.store_knowledge(&recent).unwrap();

        let since = (now - Duration::days(7)).to_rfc3339();
        let result = tool
            .execute(serde_json::json!({ "query": "likes", "since": since }), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.content.contains("rust"),
            "recent memory must be recalled, got: {}",
            result.content
        );
        assert!(
            !result.content.contains("coffee"),
            "old memory must be filtered by since, got: {}",
            result.content
        );
    }

    /// `until` variant of the time-range filter — only memories created at or
    /// before the boundary survive.
    #[tokio::test]
    async fn test_memory_recall_until_filters_by_created_at_inmemory() {
        use acowork_memory::types::{KnowledgeNode, KnowledgeSubType, NodeStatus, PrivacyLevel};
        use chrono::{Duration, Utc};

        let (tool, provider) = test_tool_inmemory();
        let now = Utc::now();

        let old = KnowledgeNode {
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "coffee".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: now - Duration::days(30),
            updated_at: now - Duration::days(30),
            metadata: Default::default(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        provider.store_knowledge(&old).unwrap();

        let mut recent = old.clone();
        recent.subject = "colleague".to_string();
        recent.object = "rust".to_string();
        recent.created_at = now - Duration::days(1);
        recent.updated_at = now - Duration::days(1);
        provider.store_knowledge(&recent).unwrap();

        // until = 7 days ago: only the 30-day-old memory survives.
        let until = (now - Duration::days(7)).to_rfc3339();
        let result = tool
            .execute(serde_json::json!({ "query": "likes", "until": until }), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.content.contains("coffee"),
            "old memory must be recalled, got: {}",
            result.content
        );
        assert!(
            !result.content.contains("rust"),
            "recent memory must be filtered by until, got: {}",
            result.content
        );
    }

    /// Direct trait test: `get_node_created_at` round-trips the stored
    /// `created_at` timestamp from the provider (InMemoryProvider impl).
    #[test]
    fn test_get_node_created_at_inmemory() {
        use acowork_memory::types::{KnowledgeNode, KnowledgeSubType, NodeStatus, PrivacyLevel};
        use acowork_memory::MemoryProvider;
        use chrono::{Duration, Utc};

        let provider = Arc::new(InMemoryProvider::new());
        let created = Utc::now() - Duration::days(3);
        let node = KnowledgeNode {
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "coffee".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: created,
            updated_at: created,
            metadata: Default::default(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        provider.store_knowledge(&node).unwrap();

        // Locate the node via retrieval to obtain its id.
        let results = provider
            .hybrid_search(&MemoryQuery::new("user likes coffee"))
            .unwrap();
        assert_eq!(results.len(), 1, "node must be searchable");
        let ts = provider.get_node_created_at(results[0].node_id).unwrap();
        assert_eq!(ts, Some(created));

        // Unknown id -> None (defensive).
        assert_eq!(provider.get_node_created_at(999_999).unwrap(), None);
    }

    /// Manager-layer test: `filters.time_range` is honored directly by
    /// `MemoryManager::retrieve` (independent of the tool layer), covering
    /// the post-filter at manager.rs (kept when timestamp is unknown).
    #[tokio::test]
    async fn test_manager_time_range_filter_inmemory() {
        use acowork_memory::types::{KnowledgeNode, KnowledgeSubType, NodeStatus, PrivacyLevel};
        use acowork_memory::{MemoryManager, MemoryManagerConfig, MemoryProvider};
        use chrono::{Duration, Utc};

        let provider = Arc::new(InMemoryProvider::new());
        let now = Utc::now();

        let old = KnowledgeNode {
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "coffee".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: now - Duration::days(30),
            updated_at: now - Duration::days(30),
            metadata: Default::default(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        provider.store_knowledge(&old).unwrap();

        let mut recent = old.clone();
        recent.subject = "colleague".to_string();
        recent.object = "rust".to_string();
        recent.created_at = now - Duration::days(1);
        recent.updated_at = now - Duration::days(1);
        provider.store_knowledge(&recent).unwrap();

        let mut query = MemoryQuery::new("likes");
        query.filters.time_range = Some((now - Duration::days(7), now + Duration::days(1)));
        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let retrieval = manager
            .retrieve(provider.as_ref(), &mut query, None)
            .await
            .unwrap();

        assert!(
            retrieval.memories.iter().any(|m| m.content.contains("rust")),
            "recent memory must survive the time-range filter"
        );
        assert!(
            !retrieval.memories.iter().any(|m| m.content.contains("coffee")),
            "old memory must be filtered by time_range at manager layer"
        );
    }
}
