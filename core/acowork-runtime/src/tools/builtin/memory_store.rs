//! Memory store tool — store memories via Grafeo backend
//!
//! Adapted from zeroclaw/src/tools/memory_store.rs
//! ACowork deviation: uses acowork_core::Tool trait;
//! uses natural language interface (no key-value model);
//! wires to GrafeoStore for instant extraction pipeline.
//! SPDX-License-Identifier: MIT OR Apache-2.0

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use acowork_memory::consolidation::{AutobiographicalStoreInput, MemoryStoreInput};
use acowork_memory::types::{AutobioCategory, KnowledgeSubType, PrivacyLevel};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::memory::MemorySessionHandle;

/// Default confidence when LLM does not provide one for non-autobiographical
/// categories.
const DEFAULT_CONFIDENCE: f32 = 0.7;

/// Default confidence for autobiographical writes. Higher than the generic
/// default because the autobiographical channel is meant for high-confidence
/// signals (user explicitly said "you're too verbose", agent clearly hit a
/// milestone) — low-confidence self-knowledge should not be recorded.
const AUTOBIO_DEFAULT_CONFIDENCE: f32 = 0.85;

/// Discriminated category for the memory_store tool.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StoreCategory {
    /// Knowledge about the user or the world — goes to KnowledgeNode.
    Knowledge(KnowledgeSubType),
    /// Behavior pattern — goes to ProceduralNode.
    Procedure,
    /// Self-knowledge about the Agent — goes to AutobiographicalNode.
    Autobiographical,
}

/// Memory store tool — allows an Agent to store memories for later recall.
///
/// Design: accepts natural language content with category and confidence,
/// wires to the GrafeoStore instant extraction pipeline (dedup → conflict
/// detection → node creation).
pub struct MemoryStoreTool {
    /// Agent ID (namespace for memory isolation)
    agent_id: String,
    /// Memory session handle providing shared access to the Grafeo store.
    /// Uses late-binding via RwLock — the store may be initialized after
    /// tool construction (see `MemorySessionHandle::set_store`).
    /// `None` when no Grafeo store is available (degraded mode).
    handle: Option<Arc<MemorySessionHandle>>,
}
impl MemoryStoreTool {
    pub fn new(agent_id: &str, handle: Option<Arc<MemorySessionHandle>>) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            handle,
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "memory_store".to_string(),
            description: "Store a memory in long-term memory for later recall. \
                Use category to choose the right layer:\n\
                - 'fact': objective truth about the user or world (Knowledge).\n\
                - 'preference': user taste/habit (Knowledge).\n\
                - 'relation': entity relationship between user and others (Knowledge).\n\
                - 'procedure': behavioral pattern — 'when X, do Y' (Procedural).\n\
                - 'autobiographical': knowledge about the AGENT itself — identity, capability, \
                  limitation, self-preference, milestone, or long-term relationship. \
                  Use this whenever the user says something about the agent (e.g. 'you're too verbose', \
                  'good job on that report', 'you keep forgetting X'). Requires the 'aspect' parameter.\n\
                Describe what to remember in 'content' (natural language, no need to split triples). \
                Estimate your confidence (0.0-1.0). Optionally provide keywords.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Natural language description of what to remember (e.g. 'User lives in Beijing', 'User prefers dark mode over light mode', 'When user asks for summary, reply in 3 sentences max', 'I tend to give conclusions first')"
                    },
                    "category": {
                        "type": "string",
                        "enum": ["fact", "preference", "relation", "procedure", "autobiographical"],
                        "description": "Knowledge layer: 'fact' (objective truth), 'preference' (user taste/habit), 'relation' (entity relationship), 'procedure' (behavioral pattern: when X, do Y), 'autobiographical' (knowledge about the agent itself — requires 'aspect')."
                    },
                    "aspect": {
                        "type": "string",
                        "enum": ["identity", "capability", "limitation", "preference", "history", "relationship"],
                        "description": "REQUIRED when category='autobiographical'. Which dimension of self-knowledge: 'identity' (name/role), 'capability' (skills/tools), 'limitation' (boundaries/weaknesses), 'preference' (agent's own style), 'history' (milestone/important event), 'relationship' (long-term connection with someone)."
                    },
                    "key": {
                        "type": "string",
                        "description": "Optional idempotency key for autobiographical writes. Re-storing with the same key updates the existing node in place (use for slots like 'style', 'name', 'language'). If omitted, a key is derived from content. Ignored for non-autobiographical categories."
                    },
                    "source": {
                        "type": "string",
                        "enum": ["user_statement", "important_event", "self_evaluation"],
                        "description": "Optional provenance for autobiographical writes. Defaults to 'user_statement'. Ignored for non-autobiographical categories."
                    },
                    "confidence": {
                        "type": "number",
                        "description": "Your confidence in this knowledge (0.0-1.0), reflecting how certain you actually are. Anchor on evidence, not on a target value: base it on whether the statement is direct, explicit, recent, and from the user personally (higher), versus inferred, stale, or speculative (lower). Most routine observations are moderately certain — score them accordingly. Reserve very high scores for facts you would bet on; use very low scores for uncertain or contradicting signals. Do not inflate scores to make a memory seem more certain than it is."
                    },
                    "privacy": {
                        "type": "string",
                        "enum": ["public", "personal", "sensitive"],
                        "description": "Optional privacy level: 'public' (shareable in agent packages), 'personal' (default, stripped on share), 'sensitive' (stripped on share). Ignored for autobiographical writes."
                    },
                    "importance": {
                        "type": "number",
                        "description": "How critical is this memory to long-term value (0.0-1.0)? Higher importance resists forgetting. Distinguish core identity facts (near 1.0) from transient preferences (~0.3-0.5) from trivia (~0.1)."
                    },
                    "keywords": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional keywords to help retrieval (e.g. ['beijing', 'location', 'home'])"
                    }
                },
                "required": ["content", "category"],
                "allOf": [
                    {
                        "if": { "properties": { "category": { "const": "autobiographical" } }, "required": ["category"] },
                        "then": { "required": ["aspect"] }
                    }
                ]
            }),
        }
    }
}

/// Parse category string to one of the supported `StoreCategory` variants.
fn parse_category(s: &str) -> Option<StoreCategory> {
    match s.to_lowercase().as_str() {
        "fact" => Some(StoreCategory::Knowledge(KnowledgeSubType::Fact)),
        "preference" => Some(StoreCategory::Knowledge(KnowledgeSubType::Preference)),
        "relation" => Some(StoreCategory::Knowledge(KnowledgeSubType::Relation)),
        "procedure" => Some(StoreCategory::Procedure),
        "autobiographical" => Some(StoreCategory::Autobiographical),
        _ => None,
    }
}

/// Parse autobiographical aspect string (lowercase tool input) to the
/// canonical-case `AutobioCategory` enum.
fn parse_autobio_aspect(s: &str) -> Option<AutobioCategory> {
    match s.to_lowercase().as_str() {
        "identity" => Some(AutobioCategory::Identity),
        "capability" => Some(AutobioCategory::Capability),
        "limitation" => Some(AutobioCategory::Limitation),
        "preference" => Some(AutobioCategory::Preference),
        "history" => Some(AutobioCategory::History),
        "relationship" => Some(AutobioCategory::Relationship),
        _ => None,
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        // --- Validate content ---
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

        // --- Validate and parse category ---
        let category_str = match params.get("category").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(
                        "Missing required parameter 'category'. Must be 'fact', 'preference', 'relation', or 'procedure'."
                            .to_string(),
                    ),
                    token_usage: None,
                });
            }
        };

        let category = match parse_category(category_str) {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!(
                        "Invalid category '{}'. Must be 'fact', 'preference', 'relation', 'procedure', or 'autobiographical'.",
                        category_str
                    )),
                    token_usage: None,
                });
            }
        };

        // --- Resolve autobiographical aspect if needed ---
        let autobio_input = match &category {
            StoreCategory::Autobiographical => {
                let aspect_str = match params.get("aspect").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => {
                        return Ok(ToolResult {
                            ok: false,
                            content: String::new(),
                            error: Some(
                                "Missing required parameter 'aspect' for category='autobiographical'. \
                                 Must be one of: identity, capability, limitation, preference, history, relationship."
                                    .to_string(),
                            ),
                            token_usage: None,
                        });
                    }
                };
                let aspect = match parse_autobio_aspect(aspect_str) {
                    Some(a) => a,
                    None => {
                        return Ok(ToolResult {
                            ok: false,
                            content: String::new(),
                            error: Some(format!(
                                "Invalid aspect '{}'. Must be one of: identity, capability, limitation, preference, history, relationship.",
                                aspect_str
                            )),
                            token_usage: None,
                        });
                    }
                };
                let key = params
                    .get("key")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let source = params
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Some(AutobiographicalStoreInput { aspect, key, source })
            }
            _ => None,
        };

        // --- Validate confidence (optional, clamp 0.0-1.0) ---
        let default_confidence = if autobio_input.is_some() {
            AUTOBIO_DEFAULT_CONFIDENCE
        } else {
            DEFAULT_CONFIDENCE
        };
        let confidence = params
            .get("confidence")
            .and_then(|v| v.as_f64())
            .map(|c| c.clamp(0.0, 1.0) as f32)
            .unwrap_or(default_confidence);

        // --- Extract optional keywords ---
        let _keywords: Option<Vec<String>> = params.get("keywords").and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect()
            })
        });

        // --- Extract optional privacy (public | personal | sensitive) ---
        // Only meaningful for knowledge/procedure writes; autobiographical
        // nodes carry their own classification.
        let privacy = params.get("privacy").and_then(|v| v.as_str()).map(|s| {
            match s.to_lowercase().as_str() {
                "public" => PrivacyLevel::Public,
                "sensitive" => PrivacyLevel::Sensitive,
                _ => PrivacyLevel::Personal,
            }
        });

        // --- Extract optional importance (clamp 0.0-1.0) ---
        let importance = params
            .get("importance")
            .and_then(|v| v.as_f64())
            .map(|c| c.clamp(0.0, 1.0) as f32);

        // --- Resolve MemoryProvider via late-binding handle ---
        // The provider may be None if AgentCore::init_memory_provider hasn't
        // completed yet (Phase B of startup). We fall back to a fake
        // confirmation in that case.
        let provider = self.handle.as_ref().and_then(|h| h.provider());
        match provider {
            Some(provider) => {
                // For non-autobiographical categories, sub_type still drives
                // routing (Knowledge vs Procedural). For autobiographical, the
                // input's autobiographical field takes priority.
                let sub_type_for_knowledge = match &category {
                    StoreCategory::Knowledge(s) => s.clone(),
                    StoreCategory::Procedure => KnowledgeSubType::Procedure,
                    // Sub_type value is ignored when autobiographical is set,
                    // but we still need to satisfy the type. Use Fact as
                    // a neutral placeholder.
                    StoreCategory::Autobiographical => KnowledgeSubType::Fact,
                };
                let category_display = match &category {
                    StoreCategory::Knowledge(s) => s.as_str().to_string(),
                    // Procedure routes to ProceduralNode whose sub-type display
                    // is "Procedure" — keep the existing casing for backward
                    // compat with callers/tests parsing the result content.
                    StoreCategory::Procedure => "Procedure".to_string(),
                    StoreCategory::Autobiographical => autobio_input
                        .as_ref()
                        .map(|a| format!("autobiographical/{}", a.aspect.as_str()))
                        .unwrap_or_else(|| "autobiographical".to_string()),
                };
                // Bugfix (MEM): the handle already holds the embedding
                // provider (set once at construction) but it was never
                // wired into the write path, so every Knowledge node was
                // stored without a vector (text-only). Generate the
                // content embedding here so embedding-based dedup and
                // vector indexing actually work. Degrade gracefully to
                // text-only when no provider is available or embedding fails.
                let content_embedding: Option<Vec<f32>> =
                    match self.handle.as_ref().and_then(|h| h.embedding()) {
                        Some(ep) => match ep.embed(&content).await {
                            Ok(vec) => Some(vec),
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "memory_store: failed to embed content, storing text-only"
                                );
                                None
                            }
                        },
                        None => None,
                    };
                // Show provenance in the tool result so the LLM/audit trail can
                // see the source (G7). Only autobiographical writes carry one.
                // Extracted before `autobio_input` is moved into the input.
                let source_display = autobio_input
                    .as_ref()
                    .and_then(|a| a.source.clone())
                    .map(|s| format!(", source: {s}"))
                    .unwrap_or_default();

                let input = MemoryStoreInput {
                    content: content.clone(),
                    sub_type: sub_type_for_knowledge,
                    subject: None,
                    predicate: None,
                    object: None,
                    confidence: Some(confidence),
                    source_episode_id: None,
                    embedding: content_embedding,
                    privacy,
                    importance,
                    keywords: _keywords,
                    autobiographical: autobio_input,
                };

                match provider.process_memory_store(&input) {
                    Ok(Some(result)) => {
                        // ADR-062 M3.6: lightweight write-path distribution
                        // telemetry. One structured debug event per successful
                        // write carries the resolved confidence/importance and
                        // whether the LLM provided them explicitly (vs the
                        // fallback defaults). Log aggregation builds the
                        // confidence/importance distributions used to
                        // re-calibrate consolidation thresholds (ADR-062 §6.6).
                        tracing::debug!(
                            target: "memory_write_scores",
                            agent_id = %self.agent_id,
                            category = %category_display,
                            confidence,
                            confidence_explicit = params.get("confidence").is_some(),
                            importance = importance.unwrap_or(f32::NAN),
                            importance_explicit = params.get("importance").is_some(),
                            node_id = result.node_id,
                            "memory write score distribution"
                        );
                        Ok(ToolResult {
                            ok: true,
                            content: format!(
                                "Stored {cat}: \"{content}\" (confidence: {conf:.2}, id: {id}{source})",
                                cat = category_display,
                                content = content,
                                conf = confidence,
                                id = result.node_id,
                                source = source_display
                            ),
                            error: None,
                            token_usage: None,
                        })
                    }
                    Ok(None) => {
                        // Duplicate skipped
                        Ok(ToolResult {
                            ok: true,
                            content: format!(
                                "Skipped: content is duplicate of existing memory (similarity > 0.95). \"{content}\"",
                                content = content
                            ),
                            error: None,
                            token_usage: None,
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(format!("Failed to store memory: {}", e)),
                        token_usage: None,
                    }),
                }
            }
            None => {
                // MemoryProvider not available — return confirmation (Phase 1 fallback)
                let memory_id = format!(
                    "mem_{}",
                    &uuid::Uuid::new_v4().to_string().replace('-', "")[..12]
                );
                let category_display = match &category {
                    StoreCategory::Knowledge(s) => s.as_str().to_string(),
                    StoreCategory::Procedure => "Procedure".to_string(),
                    StoreCategory::Autobiographical => autobio_input
                        .as_ref()
                        .map(|a| format!("autobiographical/{}", a.aspect.as_str()))
                        .unwrap_or_else(|| "autobiographical".to_string()),
                };
                Ok(ToolResult {
                    ok: true,
                    content: format!(
                        "Stored {cat}: \"{content}\" (confidence: {conf:.2}, agent: {agent}, id: {id})",
                        cat = category_display,
                        content = content,
                        conf = confidence,
                        agent = self.agent_id,
                        id = memory_id
                    ),
                    error: None,
                    token_usage: None,
                })
            }
        }
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
        assert!(spec.input_schema["properties"]["content"].is_object());
        assert!(spec.input_schema["properties"]["category"].is_object());
        assert!(spec.input_schema["properties"]["confidence"].is_object());
        assert!(spec.input_schema["properties"]["keywords"].is_object());
        // Verify required fields
        let required: Vec<&str> = spec.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required.contains(&"content"));
        assert!(required.contains(&"category"));
        // aspect must be conditionally required via allOf when
        // category=autobiographical.
        assert!(
            spec.input_schema["allOf"].is_array(),
            "allOf branch should declare conditional requirement for aspect"
        );
    }

    #[tokio::test]
    async fn test_memory_store_missing_content() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(serde_json::json!({ "category": "fact" }), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("Missing required parameter 'content'")
        );
    }

    #[tokio::test]
    async fn test_memory_store_missing_category() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(serde_json::json!({ "content": "User prefers Rust" }), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("Missing required parameter 'category'")
        );
    }

    #[tokio::test]
    async fn test_memory_store_invalid_category() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "User prefers Rust",
                    "category": "daily"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Invalid category"));
    }

    #[tokio::test]
    async fn test_memory_store_empty_content() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({ "content": "", "category": "fact" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn test_memory_store_basic_fact() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "User lives in Beijing",
                    "category": "fact",
                    "confidence": 0.9
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("User lives in Beijing"));
        assert!(result.content.contains("Fact"));
    }

    #[tokio::test]
    async fn test_memory_store_preference() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "User prefers dark mode",
                    "category": "preference",
                    "confidence": 0.6
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("Preference"));
        assert!(result.content.contains("0.60"));
    }

    #[tokio::test]
    async fn test_memory_store_relation() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "Alice is the team lead of Bob",
                    "category": "relation",
                    "keywords": ["alice", "bob", "team"]
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("Relation"));
    }

    #[tokio::test]
    async fn test_memory_store_default_confidence() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "User likes coffee",
                    "category": "preference"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        // Default confidence = 0.7
        assert!(result.content.contains("0.70"));
    }

    #[tokio::test]
    async fn test_memory_store_confidence_clamped() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        // confidence > 1.0 → clamped to 1.0
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "2 + 2 = 4",
                    "category": "fact",
                    "confidence": 99.0
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("1.00"));

        // confidence < 0 → clamped to 0.0
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "Maybe it will rain",
                    "category": "fact",
                    "confidence": -5.0
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("0.00"));
    }

    #[tokio::test]
    async fn test_memory_store_procedure() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "When user asks for summary, reply concisely",
                    "category": "procedure",
                    "confidence": 0.9
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("Procedure"));
        assert!(result.content.contains("reply concisely"));
    }

    #[tokio::test]
    async fn test_memory_store_procedure_low_confidence() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "User might prefer tables",
                    "category": "procedure",
                    "confidence": 0.5
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("Procedure"));
    }

    // ── ADR-051 C5: InMemoryProvider tests ──────────────────────────────
    // These tests prove the memory_store tool can work without GrafeoStore,
    // using the actual process_memory_store() pipeline instead of the
    // degraded fallback path.

    use crate::test_support::InMemoryProvider;
    use acowork_memory::MemoryProvider;

    /// Helper: create a MemoryStoreTool backed by InMemoryProvider.
    fn test_tool_with_provider() -> (MemoryStoreTool, Arc<InMemoryProvider>) {
        let provider = Arc::new(InMemoryProvider::new());
        let handle = Arc::new(crate::memory::MemorySessionHandle::new(None));
        handle.set_provider(provider.clone());
        let tool = MemoryStoreTool::new("com.test.agent", Some(handle));
        (tool, provider)
    }

    /// Migrated from test_memory_store_basic_fact: uses InMemoryProvider
    /// instead of None. Verifies the actual storage path (process_memory_store)
    /// is invoked, not the degraded fallback.
    #[tokio::test]
    async fn test_memory_store_basic_fact_inmemory() {
        let (tool, provider) = test_tool_with_provider();
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "User lives in Beijing",
                    "category": "fact",
                    "confidence": 0.9
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("User lives in Beijing"));
        assert!(result.content.contains("Fact"));
        // Verify the result includes a real node_id (numeric, not "mem_" prefix).
        assert!(
            result.content.contains("id: "),
            "Expected content to include node id, got: {}",
            result.content
        );
        assert!(
            !result.content.contains("mem_"),
            "Should not use fallback 'mem_' id when provider is available"
        );

        // Verify the node was actually stored in the provider.
        let stats = provider.stats().unwrap();
        assert_eq!(stats.node_count, 1);
    }

    /// Verify preference storage via InMemoryProvider.
    #[tokio::test]
    async fn test_memory_store_preference_inmemory() {
        let (tool, _provider) = test_tool_with_provider();
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "User prefers dark mode",
                    "category": "preference",
                    "confidence": 0.6
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("Preference"));
        assert!(result.content.contains("0.60"));
    }

    /// Verify procedure storage via InMemoryProvider.
    #[tokio::test]
    async fn test_memory_store_procedure_inmemory() {
        let (tool, provider) = test_tool_with_provider();
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "When user asks for summary, reply concisely",
                    "category": "procedure",
                    "confidence": 0.9
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("Procedure"));
        assert!(result.content.contains("reply concisely"));

        // Verify node was stored.
        let stats = provider.stats().unwrap();
        assert_eq!(stats.node_count, 1);
    }

    /// End-to-end: keywords/privacy/importance params are accepted and
    /// forwarded into MemoryStoreInput (Gap G8 + M2). The InMemoryProvider
    /// does not persist these fields, so persistence is verified at the
    /// Grafeo layer (instant.rs test_process_memory_store_keywords_persisted);
    /// this test proves the tool wiring does not drop or reject them.
    #[tokio::test]
    async fn test_memory_store_metadata_params_inmemory() {
        let (tool, provider) = test_tool_with_provider();
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "User lives in Shanghai",
                    "category": "fact",
                    "confidence": 0.9,
                    "privacy": "public",
                    "importance": 0.8,
                    "keywords": ["shanghai", "location", "home"]
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok, "expected success, got: {:?}", result.error);
        assert!(result.content.contains("Shanghai"));

        // Invalid privacy value falls back to Personal (no hard error).
        let result2 = tool
            .execute(
                serde_json::json!({
                    "content": "User prefers tea",
                    "category": "preference",
                    "privacy": "bogus",
                    "importance": 99.0
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result2.ok, "expected success, got: {:?}", result2.error);

        // Both nodes stored.
        let stats = provider.stats().unwrap();
        assert_eq!(stats.node_count, 2);
    }
    //
    // The autobiographical category is the new entry point for "user about
    // the agent" knowledge (e.g. "you're too verbose", "good job"). It
    // requires an `aspect` parameter and routes to AutobiographicalNode.

    /// Schema declares autobiographical + aspect.
    #[test]
    fn test_memory_store_spec_includes_autobiographical() {
        let spec = MemoryStoreTool::spec_value();
        let enum_values: Vec<&str> = spec.input_schema["properties"]["category"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(enum_values.contains(&"autobiographical"));
        assert!(
            spec.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key("aspect")
        );
        let aspect_enum: Vec<&str> = spec.input_schema["properties"]["aspect"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(
            aspect_enum,
            vec![
                "identity",
                "capability",
                "limitation",
                "preference",
                "history",
                "relationship"
            ]
        );
    }

    /// autobiographical category without aspect is rejected with a clear error.
    #[tokio::test]
    async fn test_memory_store_autobiographical_missing_aspect() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "I tend to give conclusions first",
                    "category": "autobiographical"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter 'aspect'"));
    }

    /// autobiographical category with invalid aspect is rejected.
    #[tokio::test]
    async fn test_memory_store_autobiographical_invalid_aspect() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "I am a coding assistant",
                    "category": "autobiographical",
                    "aspect": "mood"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Invalid aspect 'mood'"));
    }

    /// autobiographical category with valid params falls through to degraded
    /// fallback (no provider attached) and reports the category+aspect.
    #[tokio::test]
    async fn test_memory_store_autobiographical_fallback_display() {
        let tool = MemoryStoreTool::new("com.test.agent", None);
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "I tend to give conclusions first",
                    "category": "autobiographical",
                    "aspect": "preference"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        let content = result.content;
        assert!(
            content.contains("autobiographical/Preference"),
            "Expected category+aspect in display, got: {content}"
        );
        // Default confidence for autobiographical is 0.85.
        assert!(
            content.contains("0.85"),
            "Expected default autobio confidence 0.85, got: {content}"
        );
    }

    /// End-to-end autobiographical storage + idempotent re-write on (aspect, key).
    ///
    /// Uses the real GrafeoStore in-memory backend so we exercise the full
    /// upsert path (find_autobiographical_by_key + update_autobiographical).
    #[cfg(feature = "grafeo-backend")]
    #[tokio::test]
    async fn test_memory_store_autobiographical_grafeo_idempotent() {
        use acowork_grafeo::GrafeoStore;
        use acowork_memory::types::AutobioCategory;

        let store: Arc<dyn acowork_memory::MemoryProvider> =
            Arc::new(GrafeoStore::new_in_memory().unwrap());
        let handle = Arc::new(crate::memory::MemorySessionHandle::new(None));
        handle.set_provider(store.clone());
        let tool = MemoryStoreTool::new("com.test.agent", Some(handle));

        // First write — establishes node at key "style".
        let r1 = tool
            .execute(
                serde_json::json!({
                    "content": "I tend to give conclusions first",
                    "category": "autobiographical",
                    "aspect": "preference",
                    "key": "style",
                    "source": "user_statement",
                    "confidence": 0.9
                }),
                None,
            )
            .await
            .unwrap();
        assert!(r1.ok, "first write failed: {:?}", r1.error);
        let first_id = extract_node_id(&r1.content);

        // Second write with same key — should update in place, not create new.
        let r2 = tool
            .execute(
                serde_json::json!({
                    "content": "I always give a short summary before details",
                    "category": "autobiographical",
                    "aspect": "preference",
                    "key": "style",
                    "confidence": 0.95
                }),
                None,
            )
            .await
            .unwrap();
        assert!(r2.ok, "second write failed: {:?}", r2.error);
        let second_id = extract_node_id(&r2.content);
        assert_eq!(
            first_id, second_id,
            "Idempotent update should reuse the same node id; got {first_id} vs {second_id}"
        );

        // Verify value was refreshed.
        let node = store
            .find_autobiographical_by_key("style")
            .unwrap()
            .expect("autobiographical node must exist");
        assert_eq!(node.category, AutobioCategory::Preference);
        assert_eq!(node.value, "I always give a short summary before details");
        assert!(
            node.confidence >= 0.95,
            "Confidence should track the highest input, got {}",
            node.confidence
        );
    }

    /// History aspect creates a distinct node per write — milestones are
    /// append-only, not upserted.
    #[cfg(feature = "grafeo-backend")]
    #[tokio::test]
    async fn test_memory_store_autobiographical_history_per_event() {
        use acowork_grafeo::GrafeoStore;
        use acowork_memory::types::AutobioCategory;

        let store: Arc<dyn acowork_memory::MemoryProvider> =
            Arc::new(GrafeoStore::new_in_memory().unwrap());
        let handle = Arc::new(crate::memory::MemorySessionHandle::new(None));
        handle.set_provider(store.clone());
        let tool = MemoryStoreTool::new("com.test.agent", Some(handle));

        let r1 = tool
            .execute(
                serde_json::json!({
                    "content": "Learned weekly-report skill",
                    "category": "autobiographical",
                    "aspect": "history"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(r1.ok);

        let r2 = tool
            .execute(
                serde_json::json!({
                    "content": "Learned weekly-report skill",
                    "category": "autobiographical",
                    "aspect": "history"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(r2.ok);
        let id1 = extract_node_id(&r1.content);
        let id2 = extract_node_id(&r2.content);
        assert_ne!(
            id1, id2,
            "History writes should produce distinct nodes, got {id1} == {id2}"
        );

        let history = store
            .find_autobiographical_by_category(AutobioCategory::History)
            .unwrap();
        assert_eq!(history.len(), 2, "two distinct history nodes expected");
    }

    /// Extract the node_id from a successful memory_store result's content.
    /// Format: "Stored ... (confidence: ..., id: <id>[, source: <src>])".
    /// The optional `, source:` suffix is stripped before parsing.
    #[cfg(feature = "grafeo-backend")]
    fn extract_node_id(content: &str) -> u64 {
        let after = content.rsplit("id: ").next().unwrap();
        // Drop any trailing ", source: ..." segment.
        let after = after.split(',').next().unwrap();
        let after = after.trim_end_matches(')').trim();
        after
            .parse::<u64>()
            .unwrap_or_else(|e| panic!("could not parse node_id from {content:?}: {e}"))
    }
}
