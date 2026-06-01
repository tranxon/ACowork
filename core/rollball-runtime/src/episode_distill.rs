//! Episode distillation & compaction — LLM-based semantic extraction from conversations.
//!
//! ## Unified strategy (ADR-011: 摘要即蒸馏)
//!
//! Compaction and distillation are unified into a single Compact Model call.
//! The natural-language summary text serves dual purpose:
//! - Replaces the middle section of in-memory history (context compression)
//! - Written to Grafeo as an episodic memory (knowledge persistence)
//!
//! ## Trigger moments
//!
//! 1. **Context compaction** (80% token usage) — `compact_full_context()`
//!    produces a summary, replaces middle section in memory, writes to Grafeo.
//! 2. **Session close** — `distill_on_session_end()` distills the tail
//!    (everything after the last compaction) or the full session.
//!
//! Both are best-effort and non-blocking.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

use rollball_core::providers::traits::{ChatMessage, ChatRequest, MessageRole, Provider};
use rollball_core::protocol::ModelCapabilitiesInfo;
use serde::{Deserialize, Serialize};

use crate::embedding::EmbeddingProvider;
use crate::error::{Result, RuntimeError};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A simple subject-predicate-object triple extracted during compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Triple {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

/// A compacted/distilled episode — natural-language summary of a conversation segment.
///
/// Per [ADR-011], the summary is plain natural language text. No structured JSON
/// fields (intent_type, decision, keywords, etc.) — the summary text IS the
/// distillation result and is directly suitable for Grafeo semantic retrieval.
///
/// Entities and triples are extracted during compaction by the compact model
/// (replaces per-round memory_hint LLM extraction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistilledEpisode {
    /// Session that produced this episode.
    pub session_id: String,
    /// Natural-language summary of the conversation (or conversation segment).
    pub summary: String,
    /// Source session ID for traceability.
    pub source_session_id: String,
    /// Whether this episode has been consolidated to the semantic layer.
    /// Initial value is always `false`.
    pub consolidated: bool,
    /// Entities extracted during compaction (max 10, comma-separated in prompt output).
    pub entities: Vec<String>,
    /// Knowledge triples (subject|predicate|object) extracted during compaction.
    pub triples: Vec<Triple>,
}

// ---------------------------------------------------------------------------
// Prompt templates
// ---------------------------------------------------------------------------

// Prompt moved to crate::prompt::COMPACT_PROMPT.

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parsed result from compact model output containing summary, entities, and triples.
#[derive(Debug, Clone)]
pub struct CompactOutput {
    pub summary: String,
    pub entities: Vec<String>,
    pub triples: Vec<Triple>,
}

/// Parse the compact model's raw output (which may contain `<summary>`,
/// `<entities>`, and `<triples>` blocks) into structured components.
///
/// If the output does not contain the expected block markers, the entire text
/// is treated as the summary (backwards-compatible with pre-entity extraction).
pub fn parse_compact_output(raw: &str) -> CompactOutput {
    let summary = extract_block(raw, "summary").unwrap_or_else(|| raw.trim().to_string());
    let entities_str = extract_block(raw, "entities").unwrap_or_default();
    let triples_str = extract_block(raw, "triples").unwrap_or_default();

    let entities: Vec<String> = entities_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let triples: Vec<Triple> = triples_str
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let parts: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
            if parts.len() >= 3 {
                Some(Triple {
                    subject: parts[0].to_string(),
                    predicate: parts[1].to_string(),
                    object: parts[2].to_string(),
                })
            } else {
                None
            }
        })
        .collect();

    CompactOutput {
        summary,
        entities,
        triples,
    }
}

/// Extract the text content between `<tag>` and `</tag>` markers.
fn extract_block(text: &str, tag: &str) -> Option<String> {
    let start_marker = format!("<{}>", tag);
    let end_marker = format!("</{}>", tag);
    let start = text.find(&start_marker)? + start_marker.len();
    let end = text[start..].find(&end_marker)?;
    Some(text[start..start + end].trim().to_string())
}

/// Strip entity and triple metadata blocks from compact output, leaving only
/// the summary. Used before inserting compact model output into in-memory
/// context (the main LLM should not see the metadata blocks).
pub fn strip_metadata_blocks(raw: &str) -> String {
    let mut text = raw.to_string();
    // Remove <entities>...</entities> block
    if let Some(start) = text.find("<entities>") {
        if let Some(end) = text[start..].find("</entities>") {
            let end = start + end + "</entities>".len();
            text.replace_range(start..end, "");
        }
    }
    // Remove <triples>...</triples> block
    if let Some(start) = text.find("<triples>") {
        if let Some(end) = text[start..].find("</triples>") {
            let end = start + end + "</triples>".len();
            text.replace_range(start..end, "");
        }
    }
    text.trim().to_string()
}

// ---------------------------------------------------------------------------
// EpisodeDistiller
// ---------------------------------------------------------------------------

/// Distills conversation segments into semantic `DistilledEpisode` objects
/// using LLM-based extraction (natural-language output per ADR-011).
pub struct EpisodeDistiller;

impl EpisodeDistiller {
    /// Compact full conversation context into a natural-language summary.
    ///
    /// Used at 80% token usage threshold (context compaction) and for tail
    /// distillation at session close. The returned summary text is plain
    /// natural language — no JSON parsing needed.
    ///
    /// Returns the summary text (not a structured DistilledEpisode) so the
    /// caller can both write it to Grafeo and insert it into in-memory history.
    pub async fn compact_full_context(
        messages: &[ChatMessage],
        provider: &dyn Provider,
        model_name: &str,
        distill_max_tokens: u32,
    ) -> Result<String> {
        let messages_text = format_messages(messages);
        if messages_text.is_empty() {
            return Err(RuntimeError::Tool(
                "Cannot compact empty context".to_string(),
            ));
        }
        let prompt = crate::prompt::COMPACT_PROMPT.replace("{messages_text}", &messages_text);
        compact_with_llm(&prompt, provider, model_name, distill_max_tokens).await
    }

    /// Compact a specific slice of in-memory messages (e.g. tail after last compaction).
    ///
    /// Same as `compact_full_context` but takes a slice reference for convenience
    /// when the caller already has the exact message range.
    pub async fn compact_messages(
        messages: &[ChatMessage],
        provider: &dyn Provider,
        model_name: &str,
        distill_max_tokens: u32,
    ) -> Result<String> {
        Self::compact_full_context(messages, provider, model_name, distill_max_tokens).await
    }

    /// Distill an entire conversation session upon close.
    ///
    /// Reads the JSONL file and produces a session-level natural-language summary.
    /// If the session content is shorter than `min_distill_chars`, the raw text is
    /// used directly as summary — no LLM call is made.
    pub async fn distill_on_session_end(
        session_path: &Path,
        session_id: &str,
        provider: &dyn Provider,
        model_name: &str,
        min_distill_chars: usize,
        distill_max_tokens: u32,
    ) -> Result<DistilledEpisode> {
        let messages_text = read_jsonl_content(session_path)?;
        if messages_text.is_empty() {
            return Err(RuntimeError::Tool(
                "Cannot distill empty session".to_string(),
            ));
        }

        let summary = if messages_text.len() < min_distill_chars {
            tracing::debug!(
                len = messages_text.len(),
                threshold = min_distill_chars,
                "Session content is short — using raw text as summary, skipping LLM"
            );
            messages_text
        } else {
            let prompt = crate::prompt::COMPACT_PROMPT.replace("{messages_text}", &messages_text);
            compact_with_llm(&prompt, provider, model_name, distill_max_tokens).await?
        };

        Ok(DistilledEpisode {
            session_id: session_id.to_string(),
            summary,
            source_session_id: session_id.to_string(),
            consolidated: false,
            entities: Vec::new(),
            triples: Vec::new(),
        })
    }

    /// Write a natural-language summary directly to Grafeo as an episodic memory.
    ///
    /// This is the unified write path for both compaction summaries and
    /// session-close tail distillations. Parses entity and triple metadata
    /// from the compact model output and creates a DistilledEpisode.
    ///
    /// If `embedding_provider` is `Some`, generates an embedding vector
    /// from the summary text (200ms timeout) and stores it on the node
    /// for future vector-based retrieval.
    pub async fn write_summary_to_grafeo(
        summary_text: &str,
        session_id: &str,
        memory_store: &Option<Arc<rollball_grafeo::GrafeoStore>>,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) {
        let Some(store) = memory_store else {
            return;
        };
        let manager = crate::memory::MemoryManager::new(
            crate::memory::MemoryManagerConfig::default(),
        );
        let parsed = parse_compact_output(summary_text);
        let episode = DistilledEpisode {
            session_id: session_id.to_string(),
            summary: parsed.summary,
            source_session_id: session_id.to_string(),
            consolidated: false,
            entities: parsed.entities,
            triples: parsed.triples,
        };
        if let Err(e) = manager.record_distilled(store, &episode, embedding_provider).await {
            tracing::warn!(
                error = %e,
                session_id = %session_id,
                "Failed to write summary to Grafeo (non-fatal)"
            );
        }
    }

    /// Select the cheapest model from a list of `ModelCapabilitiesInfo`.
    ///
    /// Cost is estimated as `input_per_million + output_per_million`.
    /// Models without cost information are ranked last.
    /// Returns `None` if the list is empty.
    pub fn select_cheapest_model(
        models: &[ModelCapabilitiesInfo],
    ) -> Option<&ModelCapabilitiesInfo> {
        if models.is_empty() {
            return None;
        }

        models.iter().min_by(|a, b| {
            let cost_a = model_cost_score(a);
            let cost_b = model_cost_score(b);
            cost_a
                .partial_cmp(&cost_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute a cost score for a model (lower = cheaper).
///
/// Uses `input_per_million + output_per_million` as a simple heuristic.
/// Models without cost information get `f64::MAX` (ranked last).
pub fn model_cost_score(model: &ModelCapabilitiesInfo) -> f64 {
    match &model.cost {
        Some(cost) => {
            let input = cost.input_per_million.unwrap_or(0.0);
            let output = cost.output_per_million.unwrap_or(0.0);
            if input == 0.0 && output == 0.0 {
                // No meaningful cost data
                f64::MAX
            } else {
                input + output
            }
        }
        None => f64::MAX,
    }
}

/// Format a slice of `ChatMessage` into a human-readable text block.
pub(crate) fn format_messages(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .map(|msg| {
            let role = match msg.role {
                MessageRole::System => "System",
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
                MessageRole::Tool => "Tool",
            };
            format!("[{}]: {}", role, msg.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read all non-metadata lines from a JSONL conversation file.
fn read_jsonl_content(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines_vec: Vec<String> = Vec::new();
    let mut is_first_line = true;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip the first line (session metadata)
        if is_first_line {
            is_first_line = false;
            continue;
        }

        // Try to parse as ConversationEntry and extract role + content
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let role = entry.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
            let content = entry
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            lines_vec.push(format!("[{}]: {}", role, content));
        }
    }

    Ok(lines_vec.join("\n"))
}

/// Send a compaction prompt to the LLM and return the plain-text response.
///
/// Per [ADR-011], the LLM outputs natural language — no JSON parsing needed.
async fn compact_with_llm(
    prompt: &str,
    provider: &dyn Provider,
    model_name: &str,
    max_tokens: u32,
) -> Result<String> {
    let request = ChatRequest {
        model: model_name.to_string(),
        messages: vec![ChatMessage::user(prompt)],
        temperature: Some(0.3),
        max_tokens: Some(max_tokens),
        tools: None,
    };

    let response = provider.chat(request).await.map_err(|e| {
        RuntimeError::Core(e)
    })?;

    // Trim whitespace but keep the full response as-is.
    let summary = response.content.trim().to_string();
    if summary.is_empty() {
        return Err(RuntimeError::Tool(
            "Compact model returned empty response".to_string(),
        ));
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_cheapest_model_empty() {
        assert!(EpisodeDistiller::select_cheapest_model(&[]).is_none());
    }

    #[test]
    fn test_select_cheapest_model_single() {
        let models = vec![model_info("cheap-model", Some((0.5, 1.5)), 8192)];
        let selected = EpisodeDistiller::select_cheapest_model(&models);
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().name.as_deref(), Some("cheap-model"));
    }

    #[test]
    fn test_select_cheapest_model_multiple() {
        let models = vec![
            model_info("expensive-model", Some((5.0, 15.0)), 32768),
            model_info("cheap-model", Some((0.1, 0.2)), 8192),
            model_info("unknown-cost-model", None, 128000),
        ];
        let selected = EpisodeDistiller::select_cheapest_model(&models);
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().name.as_deref(), Some("cheap-model"));
    }

    #[test]
    fn test_format_messages() {
        let messages = vec![
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ];
        let text = format_messages(&messages);
        assert!(text.contains("[User]: Hello"));
        assert!(text.contains("[Assistant]: Hi there!"));
    }

    #[test]
    fn test_distilled_episode_construction() {
        let episode = DistilledEpisode {
            session_id: "sess-1".to_string(),
            summary: "User asked about Rust async programming".to_string(),
            source_session_id: "sess-1".to_string(),
            consolidated: false,
            entities: vec!["Rust".to_string()],
            triples: Vec::new(),
        };
        assert_eq!(episode.session_id, "sess-1");
        assert!(!episode.summary.is_empty());
        assert!(!episode.consolidated);
    }

    #[test]
    fn test_model_cost_score_no_cost() {
        let model = model_info("", None, 8192);
        assert_eq!(model_cost_score(&model), f64::MAX);
    }

    #[test]
    fn test_model_cost_score_with_cost() {
        let model = model_info("", Some((3.0, 6.0)), 8192);
        assert!((model_cost_score(&model) - 9.0).abs() < 0.001);
    }

    fn model_info(name: &str, cost: Option<(f64, f64)>, context_window: u64) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens: 4096,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: cost.map(|(input, output)| rollball_core::protocol::ModelCostInfo {
                input_per_million: Some(input),
                output_per_million: Some(output),
            }),
            modalities: None,
            name: if name.is_empty() { None } else { Some(name.to_string()) },
            family: None,
            knowledge_cutoff: None,
        }
    }
}
