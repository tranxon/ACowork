//! Triple extraction — LLM-driven knowledge extraction from Episode text.
//!
//! Phase 3 S4.2: Extracts (subject, predicate, object) triples from
//! episodic memory content using an LLM. The LLM call is abstracted
//! behind a trait so that the grafeo crate remains independent of
//! the runtime's provider implementation.
//!
//! Design: `docs/05-memory.md` §4.3

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{GrafeoError, Result};
use crate::grafeo::GrafeoStore;
use crate::types::{KnowledgeNode, KnowledgeSubType, NodeStatus};

// ---------------------------------------------------------------------------
// LLM abstraction
// ---------------------------------------------------------------------------

/// A single message in the LLM conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    /// Role: "system", "user", or "assistant".
    pub role: String,
    /// Message content.
    pub content: String,
}

/// Response from the LLM abstraction.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    /// The text content of the assistant's reply.
    pub content: String,
    /// Token usage (if available).
    pub usage_tokens: Option<u64>,
}

/// Trait for making LLM calls. Implemented by the runtime layer
/// using the active Provider.
///
/// This trait keeps the grafeo crate independent of the provider
/// ecosystem while still supporting LLM-driven consolidation.
#[async_trait::async_trait]
pub trait TripleExtractorLlm: Send + Sync {
    /// Send a chat request and return the response text.
    async fn chat(&self, messages: Vec<LlmMessage>) -> std::result::Result<LlmResponse, String>;
}

// ---------------------------------------------------------------------------
// Extraction types
// ---------------------------------------------------------------------------

/// A single extracted triple from an Episode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractedTriple {
    /// Subject entity (e.g., "user", "project_X").
    pub subject: String,
    /// Predicate / relation (e.g., "likes", "lives_in").
    pub predicate: String,
    /// Object entity or value (e.g., "coffee", "Beijing").
    pub object: String,
    /// Confidence score [0.0, 1.0].
    pub confidence: f32,
    /// Suggested sub-type for the knowledge node.
    pub sub_type: KnowledgeSubType,
}

/// Result of extracting triples from one or more Episodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    /// Source episode IDs that were processed.
    pub source_episode_ids: Vec<String>,
    /// Extracted triples.
    pub triples: Vec<ExtractedTriple>,
    /// Number of triples that were deduplicated against existing knowledge.
    pub deduplicated: usize,
    /// Timestamp of the extraction.
    pub extracted_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Prompt template
// ---------------------------------------------------------------------------

/// System prompt for triple extraction.
const EXTRACTION_SYSTEM_PROMPT: &str = r#"You are a knowledge extraction assistant. Your task is to extract factual triples from conversation content.

Rules:
1. Extract only explicit or strongly implied facts.
2. Each triple must have (subject, predicate, object).
3. Use consistent predicates (e.g., "likes" not "is fond of").
4. Assign a confidence score (0.0-1.0).
5. Classify each triple as one of: fact, preference, relation.
6. Do NOT extract vague or uncertain information.
7. Return valid JSON only.

Output format (JSON array):
[
  {
    "subject": "...",
    "predicate": "...",
    "object": "...",
    "confidence": 0.9,
    "sub_type": "fact"
  }
]

If no triples can be extracted, return an empty array: []"#;

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

/// Check if a triple has a potential conflict with any existing knowledge node.
///
/// Uses subject+predicate match (ignoring object) to detect that the same
/// relationship already exists with a possibly different value. This is
/// intentional: a differing object (e.g. "likes coffee" vs "likes tea")
/// indicates a value update, which should be routed to the conflict
/// resolution pipeline (S4.3) rather than creating a duplicate node.
///
/// Embedding-based semantic dedup would be more robust but is deferred
/// to the quality evaluation framework (S4.5).
fn has_potential_conflict(triple: &ExtractedTriple, existing: &[KnowledgeNode]) -> bool {
    existing.iter().any(|node| {
        node.subject.eq_ignore_ascii_case(&triple.subject)
            && node.predicate.eq_ignore_ascii_case(&triple.predicate)
    })
}

// ---------------------------------------------------------------------------
// GrafeoStore methods
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Extract triples from episode content using an LLM.
    ///
    /// This method:
    /// 1. Builds the extraction prompt from episode content
    /// 2. Calls the LLM via the provided abstraction
    /// 3. Parses the JSON response into ExtractedTriple structs
    /// 4. Deduplicates against existing knowledge
    /// 5. Stores new triples as KnowledgeNode (status = Active if confidence >= 0.85, else Pending)
    pub async fn extract_triples(
        &self,
        episode_contents: &[(String, String)], // (episode_id, content)
        llm: &dyn TripleExtractorLlm,
        embedding_fn: &dyn Fn(&str) -> Vec<f32>,
    ) -> Result<ExtractionResult> {
        if episode_contents.is_empty() {
            return Ok(ExtractionResult {
                source_episode_ids: Vec::new(),
                triples: Vec::new(),
                deduplicated: 0,
                extracted_at: Utc::now(),
            });
        }

        // Step 1: Build the user message with episode content
        let combined_content: String = episode_contents
            .iter()
            .map(|(id, content)| format!("[Episode {}]: {}", id, content))
            .collect::<Vec<_>>()
            .join("\n\n");

        let messages = vec![
            LlmMessage {
                role: "system".to_string(),
                content: EXTRACTION_SYSTEM_PROMPT.to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: combined_content,
            },
        ];

        // Step 2: Call the LLM
        let response = llm.chat(messages).await.map_err(|e| {
            GrafeoError::Memory(format!("LLM call failed during triple extraction: {}", e))
        })?;

        // Step 3: Parse the JSON response
        let triples = parse_triples(&response.content)?;

        // Step 4: Deduplicate against existing knowledge
        let existing = self.get_all_active_knowledge()?;
        let mut deduplicated = 0;
        let mut new_triples = Vec::new();

        for triple in triples {
            if has_potential_conflict(&triple, &existing) {
                deduplicated += 1;
            } else {
                new_triples.push(triple);
            }
        }

        // Step 5: Store new triples as KnowledgeNodes
        let source_ids: Vec<String> = episode_contents
            .iter()
            .map(|(id, _)| id.clone())
            .collect();

        for triple in &new_triples {
            let embedding = embedding_fn(&format!(
                "{} {} {}",
                triple.subject, triple.predicate, triple.object
            ));

            let status = if triple.confidence >= 0.85 {
                NodeStatus::Active
            } else {
                NodeStatus::Pending
            };

            let node = KnowledgeNode {
                id: None,
                subject: triple.subject.clone(),
                predicate: triple.predicate.clone(),
                object: triple.object.clone(),
                sub_type: triple.sub_type.clone(),
                confidence: triple.confidence,
                source_episode_id: None, // Episode ID linkage is managed by the caller
                embedding: Some(embedding),
                status,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                metadata: HashMap::new(),
            };

            self.store_knowledge(&node)?;
        }

        Ok(ExtractionResult {
            source_episode_ids: source_ids,
            triples: new_triples,
            deduplicated,
            extracted_at: Utc::now(),
        })
    }

    /// Get all Active knowledge nodes (for dedup check).
    pub fn get_all_active_knowledge(&self) -> Result<Vec<KnowledgeNode>> {
        use crate::types::labels;
        use grafeo_common::types::Value;

        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::KNOWLEDGE);

        let mut active = Vec::new();
        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                let is_active = n
                    .get_property("status")
                    .and_then(Value::as_str)
                    .map(|s| s == "Active")
                    .unwrap_or(false);

                if !is_active {
                    continue;
                }

                let props: Vec<(String, Value)> = n
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();

                if let Ok(kn) = KnowledgeNode::from_properties(id, &props) {
                    active.push(kn);
                }
            }
        }
        Ok(active)
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse the LLM response into a list of extracted triples.
fn parse_triples(response_content: &str) -> Result<Vec<ExtractedTriple>> {
    // Try to extract JSON from the response (may be wrapped in markdown code block)
    let json_str = extract_json_array(response_content);

    let raw: Vec<RawTriple> = serde_json::from_str(&json_str).map_err(|e| {
        GrafeoError::Memory(format!("Failed to parse triple extraction response: {}", e))
    })?;

    let mut triples = Vec::new();
    for raw_triple in raw {
        let sub_type = match raw_triple.sub_type.to_lowercase().as_str() {
            "fact" => KnowledgeSubType::Fact,
            "preference" => KnowledgeSubType::Preference,
            "relation" => KnowledgeSubType::Relation,
            _ => KnowledgeSubType::Fact,
        };

        triples.push(ExtractedTriple {
            subject: raw_triple.subject,
            predicate: raw_triple.predicate,
            object: raw_triple.object,
            confidence: raw_triple.confidence.clamp(0.0, 1.0),
            sub_type,
        });
    }

    Ok(triples)
}

/// Intermediate deserialization struct.
#[derive(Debug, Deserialize)]
struct RawTriple {
    subject: String,
    predicate: String,
    object: String,
    confidence: f32,
    sub_type: String,
}

/// Extract a JSON array string from the LLM response.
/// Handles cases where the response is wrapped in ```json ... ``` blocks.
fn extract_json_array(content: &str) -> String {
    let trimmed = content.trim();

    // Case 1: Wrapped in markdown code block
    if let Some(start) = trimmed.find("```json") {
        let json_start = start + 7;
        if let Some(end) = trimmed[json_start..].find("```") {
            return trimmed[json_start..json_start + end].trim().to_string();
        }
    }

    // Case 2: Wrapped in generic code block
    if let Some(start) = trimmed.find("```") {
        let json_start = start + 3;
        if let Some(end) = trimmed[json_start..].find("```") {
            return trimmed[json_start..json_start + end].trim().to_string();
        }
    }

    // Case 3: Raw JSON array
    trimmed.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DEFAULT_EMBEDDING_DIM;

    // =====================================================================
    // Test: parse_triples with valid JSON
    // =====================================================================

    #[test]
    fn test_parse_triples_valid_json() {
        let response = r#"[{"subject":"user","predicate":"likes","object":"coffee","confidence":0.9,"sub_type":"preference"}]"#;
        let triples = parse_triples(response).unwrap();
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].subject, "user");
        assert_eq!(triples[0].predicate, "likes");
        assert_eq!(triples[0].object, "coffee");
        assert!((triples[0].confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(triples[0].sub_type, KnowledgeSubType::Preference);
    }

    // =====================================================================
    // Test: parse_triples with markdown code block
    // =====================================================================

    #[test]
    fn test_parse_triples_markdown_block() {
        let response = "Here are the extracted triples:\n```json\n[{\"subject\":\"user\",\"predicate\":\"lives_in\",\"object\":\"Beijing\",\"confidence\":0.85,\"sub_type\":\"fact\"}]\n```";
        let triples = parse_triples(response).unwrap();
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].object, "Beijing");
    }

    // =====================================================================
    // Test: parse_triples with empty array
    // =====================================================================

    #[test]
    fn test_parse_triples_empty() {
        let response = "[]";
        let triples = parse_triples(response).unwrap();
        assert!(triples.is_empty());
    }

    // =====================================================================
    // Test: parse_triples with unknown sub_type defaults to Fact
    // =====================================================================

    #[test]
    fn test_parse_triples_unknown_subtype() {
        let response = r#"[{"subject":"x","predicate":"y","object":"z","confidence":0.5,"sub_type":"unknown"}]"#;
        let triples = parse_triples(response).unwrap();
        assert_eq!(triples[0].sub_type, KnowledgeSubType::Fact);
    }

    // =====================================================================
    // Test: parse_triples confidence is clamped
    // =====================================================================

    #[test]
    fn test_parse_triples_confidence_clamped() {
        let response = r#"[{"subject":"x","predicate":"y","object":"z","confidence":1.5,"sub_type":"fact"}]"#;
        let triples = parse_triples(response).unwrap();
        assert!((triples[0].confidence - 1.0).abs() < f32::EPSILON);
    }

    // =====================================================================
    // Test: deduplication — exact match
    // =====================================================================

    #[test]
    fn test_dedup_exact_match() {
        let triple = ExtractedTriple {
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "coffee".to_string(),
            confidence: 0.9,
            sub_type: KnowledgeSubType::Preference,
        };

        let existing = vec![KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "tea".to_string(), // Different object, same subject+predicate
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.8,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
        }];

        assert!(has_potential_conflict(&triple, &existing));
    }

    // =====================================================================
    // Test: deduplication — no match
    // =====================================================================

    #[test]
    fn test_dedup_no_match() {
        let triple = ExtractedTriple {
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "coffee".to_string(),
            confidence: 0.9,
            sub_type: KnowledgeSubType::Preference,
        };

        let existing = vec![KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "lives_in".to_string(), // Different predicate
            object: "Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.8,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
        }];

        assert!(!has_potential_conflict(&triple, &existing));
    }

    // =====================================================================
    // Test: deduplication — case insensitive
    // =====================================================================

    #[test]
    fn test_dedup_case_insensitive() {
        let triple = ExtractedTriple {
            subject: "User".to_string(),
            predicate: "Likes".to_string(),
            object: "coffee".to_string(),
            confidence: 0.9,
            sub_type: KnowledgeSubType::Preference,
        };

        let existing = vec![KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "tea".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.8,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
        }];

        assert!(has_potential_conflict(&triple, &existing));
    }

    // =====================================================================
    // Test: extract_json_array — various formats
    // =====================================================================

    #[test]
    fn test_extract_json_array_raw() {
        let content = r#"[{"a":1}]"#;
        assert_eq!(extract_json_array(content), content);
    }

    #[test]
    fn test_extract_json_array_markdown() {
        let content = "```json\n[{\"a\":1}]\n```";
        assert_eq!(extract_json_array(content), r#"[{"a":1}]"#);
    }

    #[test]
    fn test_extract_json_array_generic_code_block() {
        let content = "```\n[{\"a\":1}]\n```";
        assert_eq!(extract_json_array(content), r#"[{"a":1}]"#);
    }

    // =====================================================================
    // Test: Full extraction with mock LLM
    // =====================================================================

    struct MockLlm {
        response: String,
    }

    #[async_trait::async_trait]
    impl TripleExtractorLlm for MockLlm {
        async fn chat(&self, _messages: Vec<LlmMessage>) -> std::result::Result<LlmResponse, String> {
            Ok(LlmResponse {
                content: self.response.clone(),
                usage_tokens: Some(150),
            })
        }
    }

    fn test_embedding_fn(_text: &str) -> Vec<f32> {
        vec![0.1f32; DEFAULT_EMBEDDING_DIM]
    }

    #[tokio::test]
    async fn test_extract_triples_with_mock_llm() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let llm = MockLlm {
            response: r#"[{"subject":"user","predicate":"likes","object":"coffee","confidence":0.9,"sub_type":"preference"},{"subject":"user","predicate":"lives_in","object":"Shanghai","confidence":0.85,"sub_type":"fact"}]"#.to_string(),
        };

        let result = store
            .extract_triples(
                &[("ep-1".to_string(), "I love coffee and I live in Shanghai".to_string())],
                &llm,
                &test_embedding_fn,
            )
            .await
            .unwrap();

        assert_eq!(result.triples.len(), 2);
        assert_eq!(result.deduplicated, 0);
        assert_eq!(result.source_episode_ids, vec!["ep-1"]);
    }

    // =====================================================================
    // Test: Extraction with deduplication
    // =====================================================================

    #[tokio::test]
    async fn test_extract_triples_with_dedup() {
        let store = GrafeoStore::new_in_memory().unwrap();

        // Pre-seed an existing knowledge node
        let existing = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "tea".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.9,
            source_episode_id: None,
            embedding: Some(test_embedding_fn("user likes tea")),
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
        };
        store.store_knowledge(&existing).unwrap();

        let llm = MockLlm {
            response: r#"[{"subject":"user","predicate":"likes","object":"coffee","confidence":0.9,"sub_type":"preference"},{"subject":"user","predicate":"works_at","object":"Acme","confidence":0.8,"sub_type":"fact"}]"#.to_string(),
        };

        let result = store
            .extract_triples(
                &[("ep-2".to_string(), "I like coffee now and work at Acme".to_string())],
                &llm,
                &test_embedding_fn,
            )
            .await
            .unwrap();

        // "likes" should be deduplicated (existing subject+predicate)
        assert_eq!(result.deduplicated, 1);
        // Only "works_at" should remain as new
        assert_eq!(result.triples.len(), 1);
        assert_eq!(result.triples[0].predicate, "works_at");
    }

    // =====================================================================
    // Test: Extraction with empty episodes
    // =====================================================================

    #[tokio::test]
    async fn test_extract_triples_empty_episodes() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let llm = MockLlm {
            response: "[]".to_string(),
        };

        let result = store
            .extract_triples(&[], &llm, &test_embedding_fn)
            .await
            .unwrap();

        assert!(result.triples.is_empty());
        assert_eq!(result.deduplicated, 0);
    }

    // =====================================================================
    // Test: High confidence triple → Active status
    // =====================================================================

    #[tokio::test]
    async fn test_extract_triples_high_confidence_active() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let llm = MockLlm {
            response: r#"[{"subject":"user","predicate":"name","object":"Alice","confidence":0.95,"sub_type":"fact"}]"#.to_string(),
        };

        let result = store
            .extract_triples(
                &[("ep-3".to_string(), "My name is Alice".to_string())],
                &llm,
                &test_embedding_fn,
            )
            .await
            .unwrap();

        assert_eq!(result.triples.len(), 1);
        // The stored node should be Active (confidence >= 0.85)
        let active = store.get_all_active_knowledge().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].object, "Alice");
    }

    // =====================================================================
    // Test: Low confidence triple → Pending status
    // =====================================================================

    #[tokio::test]
    async fn test_extract_triples_low_confidence_pending() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let llm = MockLlm {
            response: r#"[{"subject":"maybe","predicate":"related_to","object":"something","confidence":0.5,"sub_type":"fact"}]"#.to_string(),
        };

        let result = store
            .extract_triples(
                &[("ep-4".to_string(), "Maybe something".to_string())],
                &llm,
                &test_embedding_fn,
            )
            .await
            .unwrap();

        assert_eq!(result.triples.len(), 1);
        // The stored node should be Pending (confidence < 0.85)
        // No active nodes should exist
        let active = store.get_all_active_knowledge().unwrap();
        assert!(active.is_empty());
    }
}
