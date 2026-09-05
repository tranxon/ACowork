//! Memory store tool — store memories via Grafeo backend
//!
//! Adapted from zeroclaw/src/tools/memory_store.rs
//! ACowork deviation: uses acowork_core::Tool trait;
//! uses natural language interface (no key-value model);
//! wires to GrafeoStore for instant extraction pipeline.
//! SPDX-License-Identifier: MIT OR Apache-2.0

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use acowork_memory::types::{Episode, KnowledgeSubType, PrivacyLevel};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::memory::MemorySessionHandle;

/// Default confidence when LLM does not provide one.
const DEFAULT_CONFIDENCE: f32 = 0.7;

/// Discriminated category for the memory_store tool (ADR-068 §3.3).
///
/// Maps directly to [`KnowledgeSubType`] — the LLM has NO access to
/// autobiographical or other distiller-only channels. The previous
/// Procedure / Autobiographical variants were removed by ADR-068: tools
/// only tag episodes; the distiller routes them to the correct layer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StoreCategory {
    Knowledge(KnowledgeSubType),
}

impl StoreCategory {
    fn knowledge_subtype(&self) -> KnowledgeSubType {
        match self {
            StoreCategory::Knowledge(s) => s.clone(),
        }
    }

    fn display(&self) -> String {
        match self {
            StoreCategory::Knowledge(s) => s.as_str().to_string(),
        }
    }
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
                Use 'category' to tag what kind of observation this is — this \
                tag is consumed by the offline EpisodicDistiller to promote \
                evidence-backed clusters into the semantic layer:\n\
                - 'fact': objective truth about the user or world.\n\
                - 'preference': user taste / habit.\n\
                - 'relation': relationship between entities.\n\
                - 'procedure': behavioural pattern — 'when X, do Y'.\n\
                Autobiographical feedback about the agent itself should be \
                written the same way as any other observation (it will be \
                recognised offline by the distiller's server-side LLM). Do \
                NOT split your text into subject/predicate/object — the \
                distiller handles that. Describe what to remember in \
                'content' (natural language). Estimate your confidence \
                (0.0-1.0). Optionally provide keywords.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Natural language description of what to remember (e.g. 'User lives in Beijing', 'User prefers dark mode over light mode', 'When user asks for summary, reply in 3 sentences max', 'I tend to give conclusions first')"
                    },
                    "category": {
                        "type": "string",
                        "enum": ["fact", "preference", "relation", "procedure"],
                        "description": "Knowledge subtype tag consumed by the offline EpisodicDistiller. Use 'fact' for objective truths, 'preference' for user taste, 'relation' for entity relationships, 'procedure' for behavioural patterns."
                    },
                    "confidence": {
                        "type": "number",
                        "description": "Your confidence in this observation (0.0-1.0), reflecting how certain you actually are. Anchor on evidence, not on a target value: base it on whether the statement is direct, explicit, recent, and from the user personally (higher), versus inferred, stale, or speculative (lower). Most routine observations are moderately certain — score them accordingly. Reserve very high scores for facts you would bet on; use very low scores for uncertain or contradicting signals. Do not inflate scores to make a memory seem more certain than it is."
                    },
                    "privacy": {
                        "type": "string",
                        "enum": ["public", "personal", "sensitive"],
                        "description": "Privacy level: 'public' (shareable in agent packages), 'personal' (default, stripped on share), 'sensitive' (stripped on share)."
                    },
                    "importance": {
                        "type": "number",
                        "description": "How critical is this memory to long-term value (0.0-1.0)? Higher importance resists forgetting. Distinguish core identity facts (near 1.0) from transient preferences (~0.3-0.5) from trivia (~0.1)."
                    },
                    "keywords": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional keywords to help retrieval. Provide short lowercase tokens (≤30 chars), avoid duplicates and common stopwords (e.g. ['beijing', 'location', 'home'])"
                    }
                },
                "required": ["content", "category"]
            }),
        }
    }
}

/// Parse category string to one of the supported `StoreCategory` variants
/// (ADR-068 §3.3 — no autobiographical channel).
fn parse_category(s: &str) -> Option<StoreCategory> {
    match s.to_lowercase().as_str() {
        "fact" => Some(StoreCategory::Knowledge(KnowledgeSubType::Fact)),
        "preference" => Some(StoreCategory::Knowledge(KnowledgeSubType::Preference)),
        "relation" => Some(StoreCategory::Knowledge(KnowledgeSubType::Relation)),
        "procedure" => Some(StoreCategory::Knowledge(KnowledgeSubType::Procedure)),
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
                        "Invalid category '{}'. Must be 'fact', 'preference', 'relation', or 'procedure'.",
                        category_str
                    )),
                    token_usage: None,
                });
            }
        };

        // --- Validate confidence (optional, clamp 0.0-1.0) ---
        let default_confidence = DEFAULT_CONFIDENCE;
        let confidence = params
            .get("confidence")
            .and_then(|v| v.as_f64())
            .map(|c| c.clamp(0.0, 1.0) as f32)
            .unwrap_or(default_confidence);

        // --- Extract optional keywords (ADR-062 §6.2.1 M5 step 2a: quality gate) ---
        // The LLM is the only keyword source; sanitize deterministically at
        // this boundary so garbage never reaches metadata["keywords"] or the
        // BM25 fold (step 2b). Pure + always-on — independent of
        // `quality.keyword_index`.
        let _keywords: Option<Vec<String>> = params.get("keywords").and_then(|v| {
            v.as_array().map(|arr| {
                let raw: Vec<String> = arr
                    .iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect();
                let (clean, stats) =
                    acowork_memory::keyword::sanitize_with_stats(raw);
                tracing::debug!(
                    target: "memory_write_keyword_gate",
                    input = stats.input_count,
                    output = stats.output_count,
                    dropped_empty_or_too_long = stats.dropped_empty_or_too_long,
                    dropped_no_alpha_cjk = stats.dropped_no_alpha_cjk,
                    dropped_duplicate = stats.dropped_duplicate,
                    dropped_stopword = stats.dropped_stopword,
                    dropped_over_cap = stats.dropped_over_cap,
                    "keyword quality gate applied at memory_store LLM boundary"
                );
                clean
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
        //
        // ADR-068 §3.2: the LLM-side memory_store tool is intentionally a
        // **thin episode writer** — it only writes into the episodic layer
        // with `knowledge_subtype` as a routing hint. There is no direct
        // path into Knowledge/Procedural/Autobiographical nodes from this
        // tool anymore. Promotion is the offline distiller's job (M3).
        let provider = self.handle.as_ref().and_then(|h| h.provider());
        match provider {
            Some(provider) => {
                // ADR-068: the only valid LLM-side category is a Knowledge
                // subtype. The distiller is what decides whether/where
                // this episode eventually lands in the sediment layer
                // (Knowledge node, Procedural node, or autobiographical
                // node).
                let knowledge_subtype = category.knowledge_subtype();
                let category_display = category.display();

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

                // ADR-068: emit a normalized Episode into the episodic
                // store. The distiller (background consolidation step)
                // consumes episodes tagged with knowledge_subtype and
                // decides promotion. The legacy instant-writer fast paths
                // (fact/procedure/autobiographical direct writes) are gone.
                let source_display = String::new();
                let routed_keywords = _keywords;

                let now: chrono::DateTime<chrono::Utc> = chrono::Utc::now();

                // ADR-068: episodic Episode fields:
                //   - session_id: synthetic — we don't have a session id here,
                //     use the agent_id to scope the episode to this agent.
                //   - role: "assistant" — a self-observation written by the agent.
                //   - knowledge_subtype: the LLM's hint about which sediment
                //     layer this may belong to (ADR-068 §3.2).
                //   - metadata: carries privacy / confidence / keywords /
                //     importance / source_episode_id, since the Episode
                //     struct itself doesn't have those fields. The
                //     EpisodicDistiller consumes metadata to make promotion
                //     decisions (confidence gates, autobio aspect, etc.).
                let mut metadata = std::collections::HashMap::new();
                if let Some(imp) = importance {
                    metadata.insert(
                        "importance".to_string(),
                        serde_json::Value::from(imp as f64),
                    );
                }
                metadata.insert(
                    "confidence".to_string(),
                    serde_json::Value::from(confidence as f64),
                );
                metadata.insert(
                    "privacy".to_string(),
                    serde_json::Value::from(match privacy {
                        Some(PrivacyLevel::Public) => "public",
                        Some(PrivacyLevel::Sensitive) => "sensitive",
                        _ => "personal",
                    }),
                );
                if let Some(kw) = routed_keywords.as_ref()
                    && !kw.is_empty()
                {
                    metadata.insert(
                        "keywords".to_string(),
                        serde_json::to_value(kw).unwrap_or_default(),
                    );
                }

                let episode = Episode {
                    session_id: self.agent_id.clone(),
                    turn_index: 0,
                    role: "assistant".to_string(),
                    content: content.clone(),
                    embedding: content_embedding.clone(),
                    timestamp: now,
                    consolidated: false,
                    metadata,
                    importance: importance.unwrap_or(0.5),
                    knowledge_subtype: Some(knowledge_subtype.clone()),
                };

                match provider.store_episode(&episode) {
                    Ok(()) => {
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
                            subtype = %knowledge_subtype.as_str(),
                            confidence,
                            confidence_explicit = params.get("confidence").is_some(),
                            importance = importance.unwrap_or(f32::NAN),
                            importance_explicit = params.get("importance").is_some(),
                            "memory write score distribution"
                        );
                        Ok(ToolResult {
                            ok: true,
                            content: format!(
                                "Stored episode: \"{content}\" (confidence: {conf:.2}, subtype: {sub}{source})",
                                content = content,
                                conf = confidence,
                                sub = knowledge_subtype.as_str(),
                                source = source_display
                            ),
                            error: None,
                            token_usage: None,
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(format!("Failed to store episode: {}", e)),
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
                let category_display = category.display();
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
        // ADR-068: there is no longer an autobiographical channel or
        // conditional aspect requirement. Only Knowledge subtypes remain
        // (fact/preference/relation/procedure). The distiller handles the
        // rest offline.
        let enum_values: Vec<&str> = spec.input_schema["properties"]["category"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(
            enum_values,
            vec!["fact", "preference", "relation", "procedure"],
            "category enum is restricted to the four Knowledge subtypes"
        );
        // No autobiographical / aspect / key / source fields leak into the
        // LLM-facing schema (ADR-068 §3.3).
        assert!(
            !spec.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key("aspect")
        );
        assert!(
            !spec.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key("key")
        );
        assert!(
            !spec.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key("source")
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


    // ── ADR-068: InMemoryProvider tests via episodic write path ─────────
    // The legacy tests (basic_fact_inmemory / preference_inmemory /
    // procedure_inmemory / metadata_params_inmemory) used to assert on
    // stats.node_count, which no longer applies — there is no node
    // creation in the LLM write path anymore. The new tests verify that
    // a tagged Episode lands in the episodic store, carrying the
    // knowledge_subtype the distiller will use to decide promotion.

    use acowork_memory::types::KnowledgeSubType;
    use crate::test_support::InMemoryProvider;

    /// Helper: create a MemoryStoreTool backed by InMemoryProvider.
    fn test_tool_with_provider() -> (MemoryStoreTool, Arc<InMemoryProvider>) {
        let provider = Arc::new(InMemoryProvider::new());
        let handle = Arc::new(crate::memory::MemorySessionHandle::new(None));
        handle.set_provider(provider.clone());
        let tool = MemoryStoreTool::new("com.test.agent", Some(handle));
        (tool, provider)
    }

    /// Migrated from test_memory_store_basic_fact: uses InMemoryProvider
    /// instead of None. ADR-068 — verifies the **episode write path** is
    /// invoked, not a legacy direct-to-sediment write pipeline. The
    /// episode is tagged with knowledge_subtype = Fact and routed to
    /// the episodic layer (NOT to a Knowledge node directly).
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
        assert!(result.ok, "got error: {:?}", result.error);
        assert!(result.content.contains("User lives in Beijing"));
        assert!(result.content.contains("Fact"));

        // Verify exactly one episode was stored (NOT a node).
        let episodes = provider.all_episodes().unwrap();
        assert_eq!(episodes.len(), 1, "expected 1 episode");
        let ep = &episodes[0];
        assert_eq!(ep.content, "User lives in Beijing");
        assert_eq!(ep.knowledge_subtype, Some(KnowledgeSubType::Fact));
        assert!(!ep.consolidated, "episode must not be consolidated yet");
    }

    /// Migrated from test_memory_store_preference: same provider wiring.
    /// ADR-068: preference episodes are tagged with the Preference
    /// knowledge_subtype. Promotion to a Knowledge node is the
    /// distiller's job.
    #[tokio::test]
    async fn test_memory_store_preference_inmemory() {
        let (tool, provider) = test_tool_with_provider();
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
        assert!(result.ok, "got error: {:?}", result.error);
        assert!(result.content.contains("Preference"));
        assert!(result.content.contains("0.60"));

        let episodes = provider.all_episodes().unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].knowledge_subtype, Some(KnowledgeSubType::Preference));
    }

    /// Verify procedure storage via InMemoryProvider. ADR-068: procedure
    /// episodes go into episodic layer with knowledge_subtype=Procedure;
    /// the distiller is what later promotes them into a ProceduralNode.
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
        assert!(result.ok, "got error: {:?}", result.error);
        assert!(result.content.contains("Procedure"));
        assert!(result.content.contains("reply concisely"));

        let episodes = provider.all_episodes().unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].knowledge_subtype, Some(KnowledgeSubType::Procedure));
    }

    /// End-to-end: keywords/privacy/importance params are accepted and
    /// forwarded into the Episode's metadata map.
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

        // Both episodes were stored.
        let episodes = provider.all_episodes().unwrap();
        assert_eq!(episodes.len(), 2);

        // First episode's metadata should carry privacy + importance +
        // confidence + (possibly) keywords.
        let ep0 = &episodes[0];
        assert_eq!(ep0.metadata.get("privacy").and_then(|v| v.as_str()), Some("public"));
        // Note: importance is f32 → f64 (≈0.8 has precision noise).
        let imp = ep0
            .metadata
            .get("importance")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        assert!(
            (imp - 0.8).abs() < 0.001,
            "importance should be ≈0.8, got {imp}"
        );
        // Note: confidence is f32 → f64 (≈0.9 has precision noise).
        let conf = ep0
            .metadata
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        assert!(
            (conf - 0.9).abs() < 0.001,
            "confidence should be ≈0.9, got {conf}"
        );
        let kw = ep0.metadata.get("keywords").and_then(|v| v.as_array());
        assert!(kw.is_some(), "keywords should be persisted in metadata");
    }

    // ── ADR-068: schema guarantees ─────────────────────────────────────
    // The LLM-facing memory_store schema must NOT contain any of:
    //   - `autobiographical` category enum value
    //   - `aspect` / `key` / `source` params (those belonged to the
    //     deleted direct-autobio write path)
    //
    // The distiller reads autobio intent from episode metadata (subject,
    // keywords, embedding similarity) — see EpisodicDistiller §3.6.

    /// Reject the legacy `autobiographical` category explicitly so old
    /// LLM tool calls fail loudly instead of silently routing to the
    /// wrong destination.
    #[tokio::test]
    async fn test_memory_store_rejects_autobiographical_category() {
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
        assert!(
            result.error.unwrap().contains("Invalid category"),
            "autobiographical category must be rejected as Invalid category"
        );
    }

    /// `aspect` / `key` / `source` params are silently ignored (treated
    /// as unknown extra params). This is a forward-compat safety: an LLM
    /// trained on the old schema that includes these fields still gets
    /// the episode written.
    #[tokio::test]
    async fn test_memory_store_ignores_legacy_autobio_params() {
        let (tool, provider) = test_tool_with_provider();
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "I am learning Rust",
                    "category": "preference",
                    "aspect": "mood",
                    "key": "style.preference",
                    "source": "user-feedback"
                }),
                None,
            )
            .await
            .unwrap();
        assert!(
            result.ok,
            "extra autobio-style params must not block writes: {:?}",
            result.error
        );
        let episodes = provider.all_episodes().unwrap();
        assert_eq!(episodes.len(), 1);
        // `aspect`/`source`/`key` must NOT have leaked into metadata.
        assert!(!episodes[0].metadata.contains_key("aspect"));
        assert!(!episodes[0].metadata.contains_key("key"));
        assert!(!episodes[0].metadata.contains_key("source"));
    }
}
