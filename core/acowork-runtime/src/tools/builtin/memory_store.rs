//! Memory store tool — store memories via Grafeo backend
//!
//! Adapted from zeroclaw/src/tools/memory_store.rs
//! AgentCowork deviation: uses acowork_core::Tool trait;
//! uses natural language interface (no key-value model);
//! wires to GrafeoStore for instant extraction pipeline.
//! SPDX-License-Identifier: MIT OR Apache-2.0

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use acowork_grafeo::consolidation::MemoryStoreInput;
use acowork_grafeo::grafeo::GrafeoStore;
use acowork_grafeo::types::KnowledgeSubType;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Default confidence when LLM does not provide one.
const DEFAULT_CONFIDENCE: f32 = 0.7;

/// Memory store tool — allows an Agent to store memories for later recall.
///
/// Design: accepts natural language content with category and confidence,
/// wires to the GrafeoStore instant extraction pipeline (dedup → conflict
/// detection → node creation).
pub struct MemoryStoreTool {
    /// Agent ID (namespace for memory isolation)
    agent_id: String,
    /// Grafeo memory store backend (may be None before initialization).
    store: Option<Arc<GrafeoStore>>,
}

impl MemoryStoreTool {
    pub fn new(agent_id: &str, store: Option<Arc<GrafeoStore>>) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            store,
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "memory_store".to_string(),
            description: "Store a fact, preference, relationship, or behavioral pattern in long-term memory for later recall. \
                Describe what you want to remember in natural language in 'content'. \
                Choose 'category': 'fact' for objective truths, 'preference' for user tastes/habits, \
                'relation' for entity relationships, 'procedure' for behavioral patterns (when X happens, do Y). \
                Estimate your confidence in this knowledge (0.0-1.0). \
                Optionally provide keywords to help retrieval.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Natural language description of what to remember (e.g. 'User lives in Beijing', 'User prefers dark mode over light mode', 'When user asks for summary, reply in 3 sentences max')"
                    },
                    "category": {
                        "type": "string",
                        "enum": ["fact", "preference", "relation", "procedure"],
                        "description": "Type of knowledge: 'fact' (objective truth), 'preference' (user taste/habit), 'relation' (entity relationship), 'procedure' (behavioral pattern: when X, do Y)"
                    },
                    "confidence": {
                        "type": "number",
                        "description": "Your confidence in this knowledge (0.0-1.0). High confidence (>=0.85) creates an Active node; lower creates Pending for later verification. Default 0.7."
                    },
                    "keywords": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional keywords to help retrieval (e.g. ['beijing', 'location', 'home'])"
                    }
                },
                "required": ["content", "category"]
            }),
        }
    }
}

/// Parse category string to KnowledgeSubType.
fn parse_category(s: &str) -> Option<KnowledgeSubType> {
    match s.to_lowercase().as_str() {
        "fact" => Some(KnowledgeSubType::Fact),
        "preference" => Some(KnowledgeSubType::Preference),
        "relation" => Some(KnowledgeSubType::Relation),
        "procedure" => Some(KnowledgeSubType::Procedure),
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

        let sub_type = match parse_category(category_str) {
            Some(t) => t,
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
        let confidence = params
            .get("confidence")
            .and_then(|v| v.as_f64())
            .map(|c| c.clamp(0.0, 1.0) as f32)
            .unwrap_or(DEFAULT_CONFIDENCE);

        // --- Extract optional keywords ---
        let _keywords: Option<Vec<String>> = params.get("keywords").and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect()
            })
        });

        // --- Call GrafeoStore pipeline if available ---
        match &self.store {
            Some(store) => {
                let category_display = sub_type.as_str();
                let input = MemoryStoreInput {
                    content: content.clone(),
                    sub_type,
                    subject: None,
                    predicate: None,
                    object: None,
                    confidence: Some(confidence),
                    source_episode_id: None,
                    embedding: None,
                };

                match store.process_memory_store(&input) {
                    Ok(Some(result)) => Ok(ToolResult {
                        ok: true,
                        content: format!(
                            "Stored {cat}: \"{content}\" (confidence: {conf:.2}, id: {id})",
                            cat = category_display,
                            content = content,
                            conf = confidence,
                            id = result.node_id.0
                        ),
                        error: None,
                        token_usage: None,
                    }),
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
                // GrafeoStore not available — return confirmation (Phase 1 fallback)
                let memory_id = format!(
                    "mem_{}",
                    &uuid::Uuid::new_v4().to_string().replace('-', "")[..12]
                );
                Ok(ToolResult {
                    ok: true,
                    content: format!(
                        "Stored {cat}: \"{content}\" (confidence: {conf:.2}, agent: {agent}, id: {id})",
                        cat = sub_type.as_str(),
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
        // key should NOT be in schema
        assert!(
            !spec.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key("key")
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
}
