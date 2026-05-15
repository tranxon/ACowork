//! Episode distillation — LLM-based semantic extraction from conversations.
//!
//! Distillation is triggered at two moments:
//! 1. **Context trim** — when `trim_history_to_budget()` evicts messages, the
//!    evicted batch is distilled into a `DistilledEpisode` so the information
//!    is not lost.
//! 2. **Session close** — when a conversation session ends, the entire session
//!    is distilled into a summary episode.
//!
//! Both triggers are best-effort and non-blocking: failures are logged but
//! never panic or interrupt the main conversation flow.
//!
//! The cheapest available model is selected for distillation to minimise cost
//! while still obtaining meaningful semantic compression.

use std::io::{BufRead, BufReader};
use std::path::Path;

use rollball_core::providers::traits::{ChatMessage, ChatRequest, MessageRole, Provider};
use rollball_core::protocol::ModelCapabilitiesInfo;
use serde::{Deserialize, Serialize};

use crate::error::{Result, RuntimeError};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// LLM-distilled episode — semantic summary of a conversation segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistilledEpisode {
    /// Session that produced this episode.
    pub session_id: String,
    /// One-sentence summary of the conversation core.
    pub summary: String,
    /// User intent classification (e.g. coding, debugging, planning, research).
    pub intent_type: String,
    /// Key decision made during the conversation, if any.
    pub decision: Option<String>,
    /// Summary of tool usage, if any tools were invoked.
    pub tool_summary: Option<String>,
    /// Keywords for later retrieval.
    pub keywords: Vec<String>,
    /// Importance score [0.0, 1.0] assigned by the LLM.
    pub importance: f32,
    /// Source session ID for traceability.
    pub source_session_id: String,
    /// Whether this episode has been consolidated to the semantic layer.
    /// Initial value is always `false`.
    pub consolidated: bool,
    /// Line number in the JSONL file up to which distillation has been applied.
    /// Used to prevent re-distilling already processed content.
    pub distill_offset: u32,
}

/// Intermediate JSON structure returned by the LLM for distillation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DistillationResponse {
    summary: String,
    intent_type: String,
    decision: Option<String>,
    tool_summary: Option<String>,
    keywords: Vec<String>,
    importance: f32,
}

// ---------------------------------------------------------------------------
// Prompt templates
// ---------------------------------------------------------------------------

/// Prompt for distilling a batch of messages trimmed from context.
const TRIM_DISTILL_PROMPT: &str = r#"You are a conversation analysis assistant. Analyze the following conversation segment and extract key information.

Conversation content:
{messages_text}

Please return a JSON object with the following fields:
{
  "summary": "A one-sentence summary of the core content of this conversation",
  "intent_type": "User intent classification, e.g. coding/debugging/planning/research/configuration",
  "decision": "If an important decision was made, describe it; otherwise null",
  "tool_summary": "If tools were used, summarize their usage; otherwise null",
  "keywords": ["keyword1", "keyword2", ...],
  "importance": 0.0-1.0 importance score
}

Return ONLY the JSON object, no other text."#;

/// Prompt for distilling an entire session.
const SESSION_DISTILL_PROMPT: &str = r#"You are a conversation analysis assistant. Analyze the following complete conversation session and extract a comprehensive summary.

Session content:
{messages_text}

Please return a JSON object with the following fields:
{
  "summary": "A one-sentence summary of the core content of this conversation session",
  "intent_type": "Overall user intent classification, e.g. coding/debugging/planning/research/configuration",
  "decision": "If important decisions were made, describe them; otherwise null",
  "tool_summary": "If tools were used, summarize their overall usage; otherwise null",
  "keywords": ["keyword1", "keyword2", ...],
  "importance": 0.0-1.0 importance score for the entire session
}

Return ONLY the JSON object, no other text."#;

// ---------------------------------------------------------------------------
// EpisodeDistiller
// ---------------------------------------------------------------------------

/// Distills conversation segments into semantic `DistilledEpisode` objects
/// using LLM-based extraction.
pub struct EpisodeDistiller;

impl EpisodeDistiller {
    /// Distill a batch of messages that were trimmed from the context window.
    ///
    /// This is called from `trim_history_to_budget()` **before** the messages
    /// are actually removed, so they can be captured for distillation.
    ///
    /// The cheapest available model is selected for cost efficiency.
    pub async fn distill_on_trim(
        trimmed_messages: &[ChatMessage],
        session_id: &str,
        provider: &dyn Provider,
        model_name: &str,
    ) -> Result<DistilledEpisode> {
        let messages_text = format_messages(trimmed_messages);
        let prompt = TRIM_DISTILL_PROMPT.replace("{messages_text}", &messages_text);
        distill_with_llm(&prompt, session_id, session_id, provider, model_name).await
    }

    /// Distill an entire conversation session upon close.
    ///
    /// Reads the JSONL file and produces a session-level summary.
    pub async fn distill_on_session_end(
        session_path: &Path,
        session_id: &str,
        provider: &dyn Provider,
        model_name: &str,
    ) -> Result<DistilledEpisode> {
        let messages_text = read_jsonl_content(session_path)?;
        if messages_text.is_empty() {
            return Err(RuntimeError::Tool(
                "Cannot distill empty session".to_string(),
            ));
        }
        let prompt = SESSION_DISTILL_PROMPT.replace("{messages_text}", &messages_text);
        distill_with_llm(&prompt, session_id, session_id, provider, model_name).await
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
fn format_messages(messages: &[ChatMessage]) -> String {
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

/// Core distillation logic: send the prompt to the LLM and parse the response.
async fn distill_with_llm(
    prompt: &str,
    session_id: &str,
    source_session_id: &str,
    provider: &dyn Provider,
    model_name: &str,
) -> Result<DistilledEpisode> {
    let request = ChatRequest {
        model: model_name.to_string(),
        messages: vec![ChatMessage::user(prompt)],
        temperature: Some(0.3),
        max_tokens: Some(1024),
        tools: None,
    };

    let response = provider.chat(request).await.map_err(|e| {
        RuntimeError::Core(e)
    })?;

    parse_distillation_response(response.content, session_id, source_session_id)
}

/// Parse the LLM response into a `DistilledEpisode`.
///
/// The LLM is instructed to return JSON, but we handle malformed responses
/// gracefully by logging a warning and returning a fallback episode.
fn parse_distillation_response(
    content: String,
    session_id: &str,
    source_session_id: &str,
) -> Result<DistilledEpisode> {
    // Try to extract JSON from the response (LLM may wrap it in markdown)
    let json_str = extract_json(&content);

    match serde_json::from_str::<DistillationResponse>(json_str) {
        Ok(distilled) => {
            let importance = distilled.importance.clamp(0.0, 1.0);
            Ok(DistilledEpisode {
                session_id: session_id.to_string(),
                summary: distilled.summary,
                intent_type: distilled.intent_type,
                decision: distilled.decision,
                tool_summary: distilled.tool_summary,
                keywords: distilled.keywords,
                importance,
                source_session_id: source_session_id.to_string(),
                consolidated: false,
                distill_offset: 0,
            })
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                raw_response = %content.chars().take(200).collect::<String>(),
                "Failed to parse distillation LLM response, using fallback"
            );
            // Fallback: create a minimal episode from the raw content
            Ok(DistilledEpisode {
                session_id: session_id.to_string(),
                summary: content.chars().take(200).collect(),
                intent_type: "unknown".to_string(),
                decision: None,
                tool_summary: None,
                keywords: vec![],
                importance: 0.3,
                source_session_id: source_session_id.to_string(),
                consolidated: false,
                distill_offset: 0,
            })
        }
    }
}

/// Extract JSON content from a string that may be wrapped in markdown code fences.
fn extract_json(content: &str) -> &str {
    let trimmed = content.trim();

    // Check for markdown code fence wrapping
    if trimmed.starts_with("```json") {
        // Find the closing ``` after the opening ```json
        let inner_start = 7; // len of "```json\n"
        if let Some(rel_end) = trimmed[inner_start..].find("```") {
            let end = inner_start + rel_end;
            return trimmed[inner_start..end].trim();
        }
    }
    if trimmed.starts_with("```") {
        let inner_start = 3; // len of "```\n"
        if let Some(rel_end) = trimmed[inner_start..].find("```") {
            let end = inner_start + rel_end;
            return trimmed[inner_start..end].trim();
        }
    }

    // Try to find the JSON object boundaries
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
    {
        return &trimmed[start..=end];
    }

    trimmed
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
        let models = vec![ModelCapabilitiesInfo {
            context_window: 8192,
            max_output_tokens: 4096,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: Some(rollball_core::protocol::ModelCostInfo {
                input_per_million: Some(0.5),
                output_per_million: Some(1.5),
            }),
            modalities: None,
            name: Some("cheap-model".to_string()),
            family: None,
            knowledge_cutoff: None,
        }];
        let selected = EpisodeDistiller::select_cheapest_model(&models);
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().name.as_deref(), Some("cheap-model"));
    }

    #[test]
    fn test_select_cheapest_model_multiple() {
        let models = vec![
            ModelCapabilitiesInfo {
                context_window: 32768,
                max_output_tokens: 4096,
                max_input_tokens: None,
                supports_tool_calling: true,
                supports_reasoning: None,
                supports_attachment: None,
                supports_temperature: None,
                cost: Some(rollball_core::protocol::ModelCostInfo {
                    input_per_million: Some(5.0),
                    output_per_million: Some(15.0),
                }),
                modalities: None,
                name: Some("expensive-model".to_string()),
                family: None,
                knowledge_cutoff: None,
            },
            ModelCapabilitiesInfo {
                context_window: 8192,
                max_output_tokens: 2048,
                max_input_tokens: None,
                supports_tool_calling: true,
                supports_reasoning: None,
                supports_attachment: None,
                supports_temperature: None,
                cost: Some(rollball_core::protocol::ModelCostInfo {
                    input_per_million: Some(0.1),
                    output_per_million: Some(0.2),
                }),
                modalities: None,
                name: Some("cheap-model".to_string()),
                family: None,
                knowledge_cutoff: None,
            },
            ModelCapabilitiesInfo {
                context_window: 128000,
                max_output_tokens: 8192,
                max_input_tokens: None,
                supports_tool_calling: true,
                supports_reasoning: None,
                supports_attachment: None,
                supports_temperature: None,
                cost: None, // No cost info — should be ranked last
                modalities: None,
                name: Some("unknown-cost-model".to_string()),
                family: None,
                knowledge_cutoff: None,
            },
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
    fn test_parse_distillation_response_valid() {
        let json = r#"{"summary":"User asked about Rust","intent_type":"coding","decision":null,"tool_summary":null,"keywords":["rust","programming"],"importance":0.7}"#;
        let episode = parse_distillation_response(json.to_string(), "sess-1", "sess-1").unwrap();
        assert_eq!(episode.session_id, "sess-1");
        assert_eq!(episode.summary, "User asked about Rust");
        assert_eq!(episode.intent_type, "coding");
        assert!(episode.decision.is_none());
        assert_eq!(episode.keywords, vec!["rust", "programming"]);
        assert!((episode.importance - 0.7).abs() < 0.001);
        assert!(!episode.consolidated);
    }

    #[test]
    fn test_parse_distillation_response_with_decision() {
        let json = r#"{"summary":"Decided to use Tokio","intent_type":"planning","decision":"Use Tokio for async runtime","tool_summary":"file_read(2 times)","keywords":["tokio","async"],"importance":0.9}"#;
        let episode = parse_distillation_response(json.to_string(), "sess-2", "sess-2").unwrap();
        assert_eq!(episode.decision.as_deref(), Some("Use Tokio for async runtime"));
        assert_eq!(
            episode.tool_summary.as_deref(),
            Some("file_read(2 times)")
        );
    }

    #[test]
    fn test_parse_distillation_response_malformed_fallback() {
        let bad_response = "This is not JSON at all".to_string();
        let episode = parse_distillation_response(bad_response, "sess-3", "sess-3").unwrap();
        // Fallback should still produce a valid episode
        assert_eq!(episode.session_id, "sess-3");
        assert!(!episode.summary.is_empty());
        assert_eq!(episode.intent_type, "unknown");
    }

    #[test]
    fn test_parse_distillation_response_markdown_wrapped() {
        let wrapped = r#"```json
{"summary":"Test","intent_type":"debugging","decision":null,"tool_summary":null,"keywords":[],"importance":0.5}
```"#;
        let episode =
            parse_distillation_response(wrapped.to_string(), "sess-4", "sess-4").unwrap();
        assert_eq!(episode.summary, "Test");
        assert_eq!(episode.intent_type, "debugging");
    }

    #[test]
    fn test_importance_clamped() {
        let json = r#"{"summary":"Test","intent_type":"coding","decision":null,"tool_summary":null,"keywords":[],"importance":1.5}"#;
        let episode = parse_distillation_response(json.to_string(), "sess-5", "sess-5").unwrap();
        assert!((episode.importance - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_extract_json_plain() {
        let input = r#"{"key": "value"}"#;
        assert_eq!(extract_json(input), input);
    }

    #[test]
    fn test_extract_json_with_surrounding_text() {
        let input = r#"Here is the result: {"key": "value"} and that's it."#;
        let extracted = extract_json(input);
        assert!(extracted.starts_with('{'));
        assert!(extracted.ends_with('}'));
    }

    #[test]
    fn test_extract_json_markdown_fenced() {
        let input = "```json\n{\"key\": \"value\"}\n```";
        let extracted = extract_json(input);
        assert_eq!(extracted, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_model_cost_score_no_cost() {
        let model = ModelCapabilitiesInfo {
            context_window: 8192,
            max_output_tokens: 2048,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
        };
        assert_eq!(model_cost_score(&model), f64::MAX);
    }

    #[test]
    fn test_model_cost_score_with_cost() {
        let model = ModelCapabilitiesInfo {
            context_window: 8192,
            max_output_tokens: 2048,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: Some(rollball_core::protocol::ModelCostInfo {
                input_per_million: Some(3.0),
                output_per_million: Some(6.0),
            }),
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
        };
        assert!((model_cost_score(&model) - 9.0).abs() < 0.001);
    }
}
