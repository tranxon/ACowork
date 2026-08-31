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
use std::sync::{Arc, OnceLock};

use regex::Regex;

use acowork_core::protocol::ModelCapabilitiesInfo;
use acowork_core::providers::traits::{ChatMessage, ChatRequest, MessageRole, Provider, UsageInfo};

// ADR-051 P2: DistilledEpisode + Triple moved to acowork-memory.
// ADR-057: KnowledgeSubType added to the re-export so the parser can map
// the LLM-supplied `sub_type` field into the corresponding enum variant.
pub use acowork_memory::{DistilledEpisode, KnowledgeSubType, Triple};

use crate::agent::loop_session::strip_think_block;
use crate::embedding::EmbeddingProvider;
use crate::error::{Result, RuntimeError};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

// Triple and DistilledEpisode are now defined in acowork-memory (ADR-051 P2).
// Re-exported at the top of this file: pub use acowork_memory::{DistilledEpisode, Triple};

// ---------------------------------------------------------------------------
// Prompt templates
// ---------------------------------------------------------------------------

// Prompt moved to crate::prompt::COMPACT_PROMPT.

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parsed result from compact model output containing summary and triples.
///
/// ADR-057: The `<entities>` block was removed (D5 — entities are not modelled
/// as graph nodes in P0). Triples now carry `confidence` + `sub_type` per
/// field, both populated by the compact model.
#[derive(Debug, Clone)]
pub struct CompactOutput {
    pub summary: String,
    pub triples: Vec<Triple>,
}

/// Parse the compact model's raw output (which contains `<summary>` and
/// `<triples>` blocks) into structured components.
///
/// Each `<triples>` line is `subject | predicate | object | confidence | sub_type`.
/// Backwards-compatible: 3-field lines (legacy) parse with `confidence = 0.7`
/// and `sub_type = Fact`; 4-field lines (subject|predicate|object|confidence)
/// default `sub_type = Fact`; 5-field lines use the full schema.
///
/// If the output does not contain the expected block markers, the entire text
/// is treated as the summary (backwards-compatible with pre-block format).
pub fn parse_compact_output(raw: &str) -> CompactOutput {
    let summary = extract_block(raw, "summary").unwrap_or_else(|| raw.trim().to_string());
    // Sanitize so role labels / tool echoes that the LLM failed to filter do
    // not land in the episodic memory (see `sanitize_summary_text`).
    let summary = sanitize_summary_text(&summary);
    let triples_str = extract_block(raw, "triples").unwrap_or_default();

    let triples: Vec<Triple> = triples_str
        .lines()
        .filter_map(parse_triple_line)
        .collect();

    CompactOutput { summary, triples }
}

/// Quality/format gate error for LLM summary output (distillation & compaction).
///
/// Per the "LLM summaries must guarantee quality" principle, summary output
/// that fails this gate is **discarded** — it never lands in episodic memory
/// and never becomes the in-memory compaction marker:
///
/// - [`SummaryError::Empty`] / [`SummaryError::MissingBlock`] are
///   **retryable**: the model didn't follow the `<summary>` format, so a
///   different distillation target tier may do better.
/// - [`SummaryError::LowQuality`] is **not retryable**: the model produced a
///   structurally valid but unusable summary, and stepping down the fallback
///   chain only gets cheaper/weaker — the output is dropped
///   (quality-over-nothing).
#[derive(Debug, Clone)]
pub enum SummaryError {
    /// Model returned an empty response (or only thinking blocks).
    Empty(String),
    /// Output is missing the required `<summary>...</summary>` block.
    MissingBlock,
    /// Summary block exists but failed the heuristic quality gate.
    LowQuality(String),
}

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SummaryError::Empty(hint) => {
                write!(f, "compact model returned empty response{hint}")
            }
            SummaryError::MissingBlock => {
                write!(f, "compact model output is missing the <summary> block")
            }
            SummaryError::LowQuality(preview) => {
                write!(f, "compact model summary failed the quality gate: {preview:?}")
            }
        }
    }
}

impl std::error::Error for SummaryError {}

impl SummaryError {
    /// Whether retrying with the next distillation target tier may help.
    ///
    /// `LowQuality` is a model-capability problem — the fallback chain only
    /// gets cheaper/weaker, so the output is discarded instead of retried.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, SummaryError::LowQuality(_))
    }
}

/// Minimum character count for an LLM summary to be considered substantive.
///
/// Shorter output is treated as the model stalling / refusing rather than a
/// real summary. Kept small so legitimate terse summaries are not misjudged.
const MIN_SUMMARY_CHARS: usize = 20;

/// Lazily-compiled regex matching `path/to/file.ext:NNN` leaks.
///
/// Reasoning models that ignore `thinking_mode=disabled` dump their chain of
/// thought into the content; a summary that still carries tool-output shapes
/// (file:line references) after sanitization is not clean prose.
fn file_line_leak_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"[A-Za-z0-9_\-./\\]+\.(?:rs|ts|tsx|js|jsx|py|go|java|kt|toml|json|ya?ml|sh|md|c|h|cpp|hpp):\s*\d+",
        )
        .expect("file-line leak regex is valid")
    })
}

/// Lazily-compiled regex matching raw tool-call/result echo tokens that
/// survived [`sanitize_summary_text`] (mid-line echoes, bracket variants).
fn tool_echo_leak_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|\s)\[(?:tool(?:\([^)]*\))?|tool_call|tool_result|thought)\]")
            .expect("tool echo leak regex is valid")
    })
}

/// Heuristic quality gate for a `<summary>` block (valid format, bad content).
///
/// Returns `true` when the summary shows clear signs of being unusable:
/// - too short to be a real summary (model stalling / refusing), or
/// - **two or more conversation-role label lines** (`[User]:`, `[Assistant]:`,
///   `[Tool(bash)]:`, ...) echoed verbatim — the model copied the raw dialog
///   into the summary instead of summarizing it (strong copy signal), or
/// - at least **two** contamination features: table-artifact lines,
///   `file:line` references, tool-call/result echoes, or a single role-label
///   line that survived sanitization.
///
/// A single contamination feature is tolerated (a summary may legitimately
/// mention one file path); two or more strongly indicate raw tool output or
/// reasoning dumps leaked into the summary.
fn is_low_quality(summary: &str) -> bool {
    let trimmed = summary.trim();
    if trimmed.chars().count() < MIN_SUMMARY_CHARS {
        return true;
    }
    let mut contamination_flags = 0u8;
    if trimmed.lines().any(|l| l.trim_start().starts_with(['|', '│'])) {
        contamination_flags += 1;
    }
    if file_line_leak_regex().is_match(trimmed) {
        contamination_flags += 1;
    }
    if tool_echo_leak_regex().is_match(trimmed) {
        contamination_flags += 1;
    }
    // Conversation-role label lines copied verbatim into the summary mean the
    // model echoed the raw dialog instead of summarizing it. Two or more
    // labelled lines are a strong copy signal → discard outright; a single
    // line is tolerated and only counts as one contamination feature.
    let role_label_echoes = trimmed
        .lines()
        .filter(|l| {
            role_marker_prefix_regex().is_match(l) || tool_marker_line_regex().is_match(l)
        })
        .count();
    if role_label_echoes >= 2 {
        return true;
    }
    if role_label_echoes == 1 {
        contamination_flags += 1;
    }
    contamination_flags >= 2
}

/// Validate raw compact-model output against the quality gate.
///
/// The gate is **strict**: a missing `<summary>` block is an error, not a
/// reason to fall back to the raw text — that fallback is exactly how
/// reasoning dumps landed in episodic memory. Callers either step down the
/// distillation target chain (retryable errors) or discard the output.
fn validate_summary_output(raw: &str) -> std::result::Result<(), SummaryError> {
    let Some(block) = extract_block(raw, "summary") else {
        return Err(SummaryError::MissingBlock);
    };
    if block.trim().is_empty() {
        return Err(SummaryError::Empty(
            " (response contained a <summary> block with no content)".to_string(),
        ));
    }
    if is_low_quality(&block) {
        let preview: String = block.trim().chars().take(200).collect();
        return Err(SummaryError::LowQuality(preview));
    }
    Ok(())
}

/// Strict variant of [`parse_compact_output`] for production write paths.
///
/// Unlike the backwards-compatible [`parse_compact_output`] (which treats a
/// missing `<summary>` block as "the whole output is the summary"), this
/// applies the quality gate first and **fails** when the block is missing or
/// unusable — the summary is discarded (quality-over-nothing), never
/// substituted with raw text.
pub fn parse_compact_output_strict(
    raw: &str,
) -> std::result::Result<CompactOutput, SummaryError> {
    validate_summary_output(raw)?;
    let summary =
        sanitize_summary_text(&extract_block(raw, "summary").expect("validated above"));
    let triples_str = extract_block(raw, "triples").unwrap_or_default();
    let triples: Vec<Triple> = triples_str
        .lines()
        .filter_map(parse_triple_line)
        .collect();
    Ok(CompactOutput { summary, triples })
}

/// ADR-061 §8.2/§8.3/§13.3: validated compaction summary output.
///
/// `summary` is the `<summary>` block content (empty when the tag is
/// missing — production paths gate the output through [`compact_with_llm`]
/// first, so the raw text is never substituted); `user_intent` is the
/// `<user_intent>` block content, falling back to the caller-supplied raw
/// user messages when the LLM omits the block. Both are sanitized so role
/// labels / tool echoes do not land in the marker text (see
/// [`sanitize_summary_text`]).
#[derive(Debug, Clone)]
pub struct ValidatedSummary {
    pub summary: String,
    pub user_intent: String,
}

/// Parse and validate the compact model's raw output (ADR-061 §8).
///
/// - `<summary>` missing → degrades to empty (production callers run the
///   output through the quality gate in [`compact_with_llm`] first, so a
///   missing block here means the caller is on a defensive path; the raw
///   text is NEVER substituted as the summary).
/// - `<user_intent>` missing → `fallback_user_intent` (the raw user
///   messages joined by the caller, compaction markers excluded) is used;
///   when that is also absent the block degrades to empty rather than
///   failing the compaction (the marker structure must always parse).
pub fn parse_and_validate_summary(
    raw: &str,
    fallback_user_intent: Option<&str>,
) -> ValidatedSummary {
    let summary = sanitize_summary_text(&extract_block(raw, "summary").unwrap_or_default());
    let user_intent = extract_block(raw, "user_intent")
        .map(|s| sanitize_summary_text(&s))
        .or_else(|| fallback_user_intent.map(str::to_string))
        .unwrap_or_default();
    ValidatedSummary {
        summary,
        user_intent,
    }
}

/// Parse one `<triples>` line into a `Triple`, accepting 3, 4 or 5 fields.
///
/// Returns `None` for empty lines or lines with fewer than 3 fields. The
/// `confidence` field is clamped to `[0.0, 1.0]` and the `sub_type` string is
/// matched case-insensitively against `Fact` / `Preference` / `Relation`,
/// falling back to `Fact` on any unrecognized value.
fn parse_triple_line(line: &str) -> Option<Triple> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let parts: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
    if parts.len() < 3 {
        return None;
    }

    // Confidence defaults to 0.7 when the model emits only 3 fields (legacy);
    // we still surface the lower-confidence fallback so the landing pipeline
    // routes such triples through `Pending` rather than `Active`.
    let confidence = if parts.len() >= 4 {
        parts[3].parse::<f32>().unwrap_or(0.7).clamp(0.0, 1.0)
    } else {
        0.7
    };

    let sub_type = if parts.len() >= 5 {
        match parts[4].to_ascii_lowercase().as_str() {
            "fact" => KnowledgeSubType::Fact,
            "preference" => KnowledgeSubType::Preference,
            "relation" => KnowledgeSubType::Relation,
            _ => KnowledgeSubType::Fact,
        }
    } else {
        KnowledgeSubType::Fact
    };

    Some(Triple {
        subject: parts[0].to_string(),
        predicate: parts[1].to_string(),
        object: parts[2].to_string(),
        confidence,
        sub_type,
    })
}

/// Extract the text content between `<tag>` and `</tag>` markers.
fn extract_block(text: &str, tag: &str) -> Option<String> {
    let start_marker = format!("<{}>", tag);
    let end_marker = format!("</{}>", tag);
    let start = text.find(&start_marker)? + start_marker.len();
    let end = text[start..].find(&end_marker)?;
    Some(text[start..start + end].trim().to_string())
}

/// Strip triple metadata blocks from compact output, leaving only the summary.
/// Used before inserting compact model output into in-memory context (the
/// main LLM should not see the metadata blocks).
pub fn strip_metadata_blocks(raw: &str) -> String {
    let mut text = raw.to_string();
    // Remove legacy <entities>...</entities> block (no longer emitted but the
    // stripper remains defensive in case older compact-model output is replayed).
    if let Some(start) = text.find("<entities>")
        && let Some(end) = text[start..].find("</entities>") {
            let end = start + end + "</entities>".len();
            text.replace_range(start..end, "");
        }
    // Remove <triples>...</triples> block
    if let Some(start) = text.find("<triples>")
        && let Some(end) = text[start..].find("</triples>") {
            let end = start + end + "</triples>".len();
            text.replace_range(start..end, "");
        }
    sanitize_summary_text(&text)
}

/// Lazily-compiled regex for **tool-role marker lines**.
///
/// Matches a line that echoes a tool call / result back into the summary,
/// e.g. `[Tool(bash)]: ...`, `[Tool]:`, `[tool_call]: ...`, `[tool_result]: ...`.
/// Such lines are raw tool interleavings, never knowledge — the whole line is
/// dropped when sanitizing.
fn tool_marker_line_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*\[(?:tool(?:\([^\[\]]*\))?|thought|tool_call|tool_result)\]:")
            .expect("tool marker regex is valid")
    })
}

/// Lazily-compiled regex for **conversation-role markers** at line start.
///
/// Matches `[User]:`, `[Assistant]:`, `[System]:`, `[CompactionSummary]:` and
/// their lowercase JSONL variants (`[user]:`, `[assistant]:`, ...). The label
/// itself is stripped but any following text on the line is preserved.
fn role_marker_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*\[(?:user|assistant|system|compaction_summary|compactionsummary)\]:\s*")
            .expect("role marker regex is valid")
    })
}

/// Remove formatting artifacts from a compaction/distillation summary so the
/// text that lands in memory (Grafeo episodic) and in-memory context is clean
/// prose, never a verbatim echo of the conversation's role labels or tool
/// interleavings.
///
/// Two-stage cleanup:
/// 1. **Drop tool-echo lines entirely** — `[Tool(bash)]: <command>` etc. are
///    raw tool interleavings and are never knowledge worth remembering.
/// 2. **Strip conversation-role labels** (`[User]:`, `[Assistant]:`, ...) but
///    keep any following text on the line, so short-session raw-text fallback
///    (which stores the raw JSONL content as the summary) keeps the actual
///    dialogue without the labels.
///
/// The `[Tool result compressed...]` placeholder body is left untouched — it
/// is opaque tool *content* (handled by the prompt's acknowledgement rule),
/// not a role marker.
pub fn sanitize_summary_text(summary: &str) -> String {
    let tool_line = tool_marker_line_regex();
    let role_prefix = role_marker_prefix_regex();
    let mut out = Vec::with_capacity(summary.lines().count());
    for line in summary.lines() {
        if tool_line.is_match(line) {
            continue;
        }
        out.push(role_prefix.replace(line, "").to_string());
    }
    out.join("\n").trim().to_string()
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
    ///
    /// `identity_context` is the user's `UserProfile` formatted as text.
    /// When `Some`, it is embedded into the system prompt so the LLM writes
    /// the summary in the user's preferred language (see
    /// [`crate::prompt::build_compaction_system_prompt`]). Pass `None` when
    /// the session has no user profile yet (default → English summary).
    ///
    /// `compaction_prompt` is the agent-specific summarization directive from
    /// `prompts/summary.md` (see [`crate::package::prompt_builder::load_compaction_prompt`]).
    /// `None` falls back to the built-in [`crate::prompt::COMPACTION_SYSTEM_PROMPT`].
    ///
    /// Returns `(summary, usage)` per ADR-027 so callers can record raw
    /// Provider usage in [`crate::conversation::SessionTokens`].
    pub async fn compact_full_context(
        messages: &[ChatMessage],
        provider: &dyn Provider,
        model_name: &str,
        distill_max_tokens: u32,
        identity_context: Option<&str>,
        compaction_prompt: Option<&str>,
    ) -> Result<(String, UsageInfo)> {
        let messages_text = format_messages(messages);
        if messages_text.is_empty() {
            return Err(RuntimeError::Tool(
                "Cannot compact empty context".to_string(),
            ));
        }
        let prompt = crate::prompt::COMPACT_PROMPT.replace("{messages_text}", &messages_text);
        compact_with_llm(
            &prompt,
            provider,
            model_name,
            distill_max_tokens,
            identity_context,
            compaction_prompt.unwrap_or(crate::prompt::COMPACTION_SYSTEM_PROMPT),
        )
        .await
    }

    /// Compact a specific slice of in-memory messages (e.g. tail after last compaction).
    ///
    /// Same as `compact_full_context` but takes a slice reference for convenience
    /// when the caller already has the exact message range. `identity_context` is
    /// threaded through for the same language-aware reason as in
    /// [`Self::compact_full_context`], and `compaction_prompt` for the same
    /// per-agent summarization rules.
    ///
    /// Returns `(summary, usage)` per ADR-027 so callers can record raw
    /// Provider usage in [`crate::conversation::SessionTokens`].
    pub async fn compact_messages(
        messages: &[ChatMessage],
        provider: &dyn Provider,
        model_name: &str,
        distill_max_tokens: u32,
        identity_context: Option<&str>,
        compaction_prompt: Option<&str>,
    ) -> Result<(String, UsageInfo)> {
        Self::compact_full_context(
            messages,
            provider,
            model_name,
            distill_max_tokens,
            identity_context,
            compaction_prompt,
        )
        .await
    }

    /// Distill an entire conversation session upon close.
    ///
    /// Reads the JSONL file and produces a session-level natural-language summary.
    /// If the session content is shorter than `min_distill_chars`, the session
    /// is **skipped** (an episode with an empty summary is returned) — the raw
    /// conversation text is NEVER used as the summary (no raw-text fallback:
    /// role labels and tool echoes must not land in episodic memory). Callers
    /// must not write the episode when the returned summary is empty.
    ///
    /// `identity_context` is threaded into the system prompt when an LLM call
    /// is made so the summary lands in the user's preferred language. When the
    /// session is skipped, no LLM is invoked and `usage` in the returned tuple
    /// is `UsageInfo::default()`.
    ///
    /// `compaction_prompt` is the agent-specific summarization directive from
    /// `prompts/summary.md`; `None` falls back to the built-in
    /// [`crate::prompt::COMPACTION_SYSTEM_PROMPT`].
    ///
    /// Returns `(episode, usage)` per ADR-027 so callers can record raw
    /// Provider usage in [`crate::conversation::SessionTokens`].
    ///
    /// `#[allow(clippy::too_many_arguments)]` follows the project convention
    /// for thin pass-through facades (cf. `AgentCore::new_with_observer`,
    /// `SessionCore::new`): every argument is semantically independent and
    /// arrives from a different call-site context, so bundling them into a
    /// config struct would hurt readability without reducing surface area.
    #[allow(clippy::too_many_arguments)]
    pub async fn distill_on_session_end(
        session_path: &Path,
        session_id: &str,
        provider: &dyn Provider,
        model_name: &str,
        min_distill_chars: usize,
        distill_max_tokens: u32,
        identity_context: Option<&str>,
        compaction_prompt: Option<&str>,
    ) -> Result<(DistilledEpisode, UsageInfo)> {
        let messages_text = read_jsonl_content(session_path)?;
        if messages_text.is_empty() {
            return Err(RuntimeError::Tool(
                "Cannot distill empty session".to_string(),
            ));
        }

        // Short sessions are SKIPPED, not summarized with raw text: falling
        // back to the raw conversation would land role labels and tool
        // echoes in episodic memory. Skipping is not a failure — the session
        // is simply not worth remembering.
        if messages_text.len() < min_distill_chars {
            tracing::debug!(
                len = messages_text.len(),
                threshold = min_distill_chars,
                "Session content is short — skipping distillation (no raw-text fallback)"
            );
            return Ok((
                DistilledEpisode {
                    session_id: session_id.to_string(),
                    summary: String::new(),
                    source_session_id: session_id.to_string(),
                    consolidated: false,
                    triples: Vec::new(),
                },
                UsageInfo::default(),
            ));
        }

        let prompt = crate::prompt::COMPACT_PROMPT.replace("{messages_text}", &messages_text);
        let (summary, usage) = compact_with_llm(
            &prompt,
            provider,
            model_name,
            distill_max_tokens,
            identity_context,
            compaction_prompt.unwrap_or(crate::prompt::COMPACTION_SYSTEM_PROMPT),
        )
        .await?;

        Ok((
            DistilledEpisode {
                session_id: session_id.to_string(),
                summary,
                source_session_id: session_id.to_string(),
                consolidated: false,
                triples: Vec::new(),
            },
            usage,
        ))
    }

    /// Write a natural-language summary directly to Grafeo as an episodic memory.
    ///
    /// This is the unified write path for both compaction summaries and
    /// session-close tail distillations. Parses the summary and triple
    /// metadata from the compact model output and creates a DistilledEpisode.
    ///
    /// If `embedding_provider` is `Some`, generates an embedding vector
    /// from the summary text (200ms timeout) and stores it on the node
    /// for future vector-based retrieval.
    ///
    /// Returns `Err` when the summary fails the strict quality gate (the
    /// output is discarded, never stored) or when Grafeo rejects the write.
    /// Callers decide how to surface the failure (log vs. user notification).
    pub async fn write_summary_to_provider(
        summary_text: &str,
        session_id: &str,
        memory_provider: &Option<Arc<dyn acowork_memory::MemoryProvider>>,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<()> {
        let Some(provider) = memory_provider else {
            return Ok(());
        };
        let manager =
            crate::memory::MemoryManager::new(crate::memory::MemoryManagerConfig::default());
        // Strict parse: a summary that fails the quality gate is discarded —
        // parse_compact_output's raw-text fallback is intentionally NOT used.
        let parsed = parse_compact_output_strict(summary_text)?;
        let episode = DistilledEpisode {
            session_id: session_id.to_string(),
            summary: parsed.summary,
            source_session_id: session_id.to_string(),
            consolidated: false,
            triples: parsed.triples,
        };
        manager
            .record_distilled(provider.as_ref(), &episode, embedding_provider)
            .await?;
        Ok(())
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

use crate::agent::history::COMPRESSED_TOOL_PLACEHOLDER_PREFIX;
use crate::agent::history::COMPACTION_SUMMARY_NAME;

/// Format a slice of `ChatMessage` into a human-readable text block for LLM-based
/// compaction & distillation.
///
/// ## Role label convention
///
/// | Actual role        | Detected condition                                          | Emitted label                                   |
/// |--------------------|-------------------------------------------------------------|-------------------------------------------------|
/// | `System`           | —                                                           | `[System]`                                      |
/// | any                | `name == Some("compaction_summary")`                        | `[CompactionSummary]`                           |
/// | `User`             | —                                                           | `[User]`                                        |
/// | `Assistant`        | —                                                           | `[Assistant]`                                   |
/// | `Tool`             | content starts with `COMPRESSED_TOOL_PLACEHOLDER_PREFIX`    | `[Tool(name={name}, id={tool_call_id})]`        |
/// | `Tool`             | otherwise                                                   | `[Tool]` (or `[Tool(name={name})]` when name set)|
///
/// Enhanced Tool labels give the compaction LLM enough visibility to write
/// a meaningful summary even when the tool output has been compressed to a
/// ~120-char placeholder (ADR-032). The `tool_call_id` duplicate in both
/// the role label and placeholder body is intentional redundancy.
pub(crate) fn format_messages(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .map(|msg| {
            // Compaction summary marker takes precedence over the role
            // label — the marker lives at `User` role in memory (see
            // `HistoryManager::replace_middle_with_summary`), but we
            // still want the compaction LLM to recognize it as
            // previous-compaction output rather than fresh user input.
            if msg.name.as_deref() == Some(COMPACTION_SUMMARY_NAME) {
                return format!("[CompactionSummary]: {}", msg.content);
            }
            let role_label = match msg.role {
                MessageRole::System => "System".to_string(),
                MessageRole::User => "User".to_string(),
                MessageRole::Assistant => "Assistant".to_string(),
                MessageRole::Tool => {
                    // Detect compressed tool results (placeholder content)
                    // and emit structured metadata in the role label so
                    // the compaction LLM can write a name-aware summary.
                    let is_compressed = msg.content.starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX);
                    match (is_compressed, msg.name.as_deref()) {
                        (true, Some(name)) => {
                            let tc_id = msg.tool_call_id.as_deref().unwrap_or("?");
                            format!("Tool(name={}, id={})", name, tc_id)
                        }
                        (true, None) => {
                            // Compressed but no name: attach id only when known.
                            match msg.tool_call_id.as_deref() {
                                Some(tc_id) => format!("Tool(id={})", tc_id),
                                None => "Tool".to_string(),
                            }
                        }
                        (false, Some(name)) => format!("Tool({})", name),
                        (false, None) => "Tool".to_string(),
                    }
                }
            };
            format!("[{}]: {}", role_label, msg.content)
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
            let role = entry
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
            lines_vec.push(format!("[{}]: {}", role, content));
        }
    }

    Ok(lines_vec.join("\n"))
}

/// Send a compaction prompt to the LLM and return the plain-text response.
///
/// Per [ADR-011], the LLM outputs natural language — no JSON parsing needed.
///
/// Core LLM-based compaction call shared by all distillation paths.
///
/// Builds the ChatRequest, sends it to the provider, strips thinking blocks
/// from the response, and returns the summary. `thinking_mode` is explicitly
/// set to `"disabled"` so reasoning models don't silo the output into
/// `reasoning_content`.
///
/// `system_prompt` is passed through to
/// [`crate::prompt::build_compaction_system_prompt`] together with
/// `identity_context` so the summary lands in the user's preferred language.
///
/// Returns `(summary, usage)` so the caller can record raw Provider usage
/// in the session's [`SessionTokens`] accumulator (ADR-027). `usage` is the
/// raw `UsageInfo` from the response, or `UsageInfo::default()` if the
/// Provider did not return one — callers must check `usage.prompt_tokens > 0`
/// before treating the values as a real cost record.
pub(crate) async fn compact_with_llm(
    prompt: &str,
    provider: &dyn Provider,
    model_name: &str,
    max_tokens: u32,
    identity_context: Option<&str>,
    system_prompt: &str,
) -> Result<(String, UsageInfo)> {
    let system_prompt = crate::prompt::build_compaction_system_prompt(
        system_prompt,
        identity_context,
    );

    // Explicitly disable deep thinking for compaction/distillation.
    // Reasoning models may otherwise put the entire summary in
    // reasoning_content, leaving content empty and causing compaction
    // to fail silently.
    let request = ChatRequest {
        model: model_name.to_string(),
        messages: vec![
            ChatMessage {
                role: MessageRole::System,
                content: system_prompt,
                ..Default::default()
            },
            ChatMessage::user(prompt),
        ],
        temperature: None,
        max_tokens: Some(max_tokens),
        tools: None,
        reasoning_effort: None,
        thinking_mode: Some("disabled".to_string()),
    };

    let response = provider
        .chat(request)
        .await
        .map_err(RuntimeError::Core)?;

    // ADR-027: capture raw Provider usage for session token accounting.
    // Providers that omit usage (e.g. some local mocks) yield
    // `UsageInfo::default()` — callers can detect this by `prompt_tokens == 0`.
    let usage = response.usage.unwrap_or_default();

    // Strip any thinking/reasoning blocks that may have leaked into content.
    // NEVER fall back to reasoning_content — it is the model's internal
    // monologue, not a useful summary.
    let raw_content = response.content.trim();
    let summary = strip_think_block(raw_content);
    if summary.is_empty() {
        let hint = if !raw_content.is_empty() {
            " (response contained only thinking/reasoning blocks that were stripped)"
        } else {
            " (content was empty — model may have ignored thinking_mode=disabled)"
        };
        return Err(RuntimeError::Summary(SummaryError::Empty(hint.to_string())));
    }
    // Quality gate: the output MUST carry a non-trivial <summary> block.
    // A missing block or low-quality content is an error — never fall back
    // to the raw text (that fallback is how reasoning dumps polluted
    // episodic memory). Retryable errors let the caller step down the
    // distillation target chain; LowQuality discards the output.
    validate_summary_output(&summary).map_err(RuntimeError::Summary)?;
    Ok((summary, usage))
}

/// Generate a session title from the first user message using the compact model.
///
/// Unlike [`compact_with_llm`], this is a minimal single-message call:
/// - No system prompt
/// - No identity block injection (language is already resolved into `{language}`)
/// - Lower `max_tokens` (title is ≤SESSION_TITLE_MAX_CHARS chars)
///
/// `prompt` should be [`crate::prompt::TITLE_PROMPT`] with `{language}` and
/// `{user_message}` already resolved by the caller.
///
/// Returns `(title, usage)` so the caller can record raw Provider usage in
/// the session's [`SessionTokens`] accumulator (ADR-027). `usage` is the
/// raw `UsageInfo` from the response, or `UsageInfo::default()` if absent.
pub async fn compact_session_title_with_llm(
    prompt: &str,
    provider: &dyn Provider,
    model_name: &str,
    max_tokens: u32,
) -> Result<(String, UsageInfo)> {
    // Explicitly disable deep thinking for title generation.
    // Reasoning models (DeepSeek, o-series) may otherwise put the entire
    // response in reasoning_content, leaving content empty — and
    // reasoning_content is the model's internal monologue, not a title.
    // If the model ignores this hint and still returns empty content,
    // the caller falls back to truncating the user message.
    let request = ChatRequest {
        model: model_name.to_string(),
        messages: vec![ChatMessage::user(prompt)],
        temperature: Some(0.3),
        max_tokens: Some(max_tokens),
        tools: None,
        reasoning_effort: None,
        thinking_mode: Some("disabled".to_string()),
    };

    let response = provider
        .chat(request)
        .await
        .map_err(RuntimeError::Core)?;

    // ADR-027: capture raw Provider usage for session token accounting.
    let usage = response.usage.unwrap_or_default();

    // Never fall back to reasoning_content — it is the model's internal
    // monologue and unsuitable as a user-facing title.
    const MAX_RAW_LEN: usize = 500;
    let raw_content: String = response.content.chars().take(MAX_RAW_LEN).collect();
    let title = strip_think_block(&raw_content);
    if title.is_empty() {
        let hint = if !raw_content.trim().is_empty() {
            " (response contained only thinking/reasoning blocks that were stripped)"
        } else {
            " (content was empty — model may have ignored thinking_mode=disabled)"
        };
        return Err(RuntimeError::Tool(format!(
            "Title model returned empty response{hint}"
        )));
    }
    Ok((title, usage))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::providers::traits::ChatResponse;

    /// Minimal stub Provider that returns a canned `ChatResponse`.
    ///
    /// Used to verify that `compact_with_llm` / `compact_session_title_with_llm`
    /// correctly thread `UsageInfo` through their return tuple (ADR-027).
    struct StubProvider {
        content: String,
        usage: Option<UsageInfo>,
    }

    impl StubProvider {
        fn new(content: &str) -> Self {
            Self {
                content: content.to_string(),
                usage: None,
            }
        }
        fn with_usage(content: &str, usage: UsageInfo) -> Self {
            Self {
                content: content.to_string(),
                usage: Some(usage),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        async fn chat(
            &self,
            _request: ChatRequest,
        ) -> std::result::Result<ChatResponse, acowork_core::AcoworkError> {
            Ok(ChatResponse {
                content: self.content.clone(),
                usage: self.usage.clone(),
                ..Default::default()
            })
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> std::result::Result<
            Box<dyn futures_core::Stream<Item = acowork_core::providers::traits::StreamEvent> + Send>,
            acowork_core::AcoworkError,
        > {
            Err(acowork_core::AcoworkError::Unknown(
                "streaming not supported in stub".to_string(),
            ))
        }
        async fn chat_token_count(
            &self,
            _messages: &[acowork_core::providers::traits::ChatMessage],
        ) -> std::result::Result<u64, acowork_core::AcoworkError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn test_compact_with_llm_returns_usage_tuple_when_provider_supplies_it() {
        // ADR-027: Provider returning usage must thread through (String, UsageInfo).
        let provider = StubProvider::with_usage(
            "<summary>a concise summary of the conversation here</summary>",
            UsageInfo {
                prompt_tokens: 4_000,
                completion_tokens: 800,
                total_tokens: 4_800,
                ..Default::default()
            },
        );
        let result = compact_with_llm(
            "ignored",
            &provider,
            "model",
            1024,
            None,
            crate::prompt::COMPACTION_SYSTEM_PROMPT,
        )
        .await;
        let (summary, usage) = result.expect("compact_with_llm should succeed");
        assert!(
            summary.contains("a concise summary of the conversation"),
            "summary must be the raw marker-carrying output, got: {summary}"
        );
        assert_eq!(usage.prompt_tokens, 4_000);
        assert_eq!(usage.completion_tokens, 800);
        assert_eq!(usage.total_tokens, 4_800);
    }

    #[tokio::test]
    async fn test_compact_with_llm_returns_zero_usage_when_provider_omits_it() {
        // ADR-027 "宁可 miss 也不估计": when Provider returns no usage,
        // the second tuple element is UsageInfo::default() (all zeros).
        let provider = StubProvider::new("<summary>a concise summary of the conversation here</summary>");
        let result = compact_with_llm(
            "ignored",
            &provider,
            "model",
            1024,
            None,
            crate::prompt::COMPACTION_SYSTEM_PROMPT,
        )
        .await;
        let (_summary, usage) = result.expect("compact_with_llm should succeed");
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
    }

    #[tokio::test]
    async fn compact_messages_rejects_markerless_reasoning_dump() {
        // Tail-distillation entry point (used by close_session_inner) must
        // hit the same quality gate: a marker-less reasoning dump is an
        // error, never a summary (P1 — quality-over-nothing).
        let provider = StubProvider::new(
            "确认 disable 路径已清理 pending 槽位。\n开始实施。改两处：\n1. 删除 resolve_distill_model 调用\n2. 验证 fallback 链",
        );
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: "你好，请帮我分析这个问题".to_string(),
            ..Default::default()
        }];
        let err = EpisodeDistiller::compact_messages(
            &messages,
            &provider,
            "model",
            1024,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RuntimeError::Summary(SummaryError::MissingBlock)),
            "marker-less dump must fail the quality gate, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn distill_on_session_end_skips_short_sessions_without_llm() {
        // Short sessions are SKIPPED (empty summary + zero usage) — the raw
        // conversation text must never be used as the summary (no raw-text
        // fallback: role labels and tool echoes must not land in memory).
        let dir = std::env::temp_dir().join(format!(
            "acowork-distill-short-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(
            &path,
            "{}\n{\"role\":\"user\",\"content\":\"你好，帮我看看这个问题\"}\n",
        )
        .unwrap();

        // If the skip logic regressed and the LLM were called, the stub
        // would return a gate-passing summary and the empty-summary assert
        // below would fail.
        let provider = StubProvider::new(
            "<summary>should never be called for a short session</summary>",
        );
        let (episode, usage) = EpisodeDistiller::distill_on_session_end(
            &path,
            "sess-short",
            &provider,
            "model",
            10_000, // min_distill_chars — far above the file length
            1024,
            None,
            None,
        )
        .await
        .expect("short session must skip, not fail");
        assert!(
            episode.summary.is_empty(),
            "short session must yield an empty summary, got: {:?}",
            episode.summary
        );
        assert_eq!(usage.prompt_tokens, 0, "no LLM call → zero usage");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_compact_session_title_with_llm_returns_usage_tuple() {
        // Title generation also returns (title, usage) per ADR-027.
        let provider = StubProvider::with_usage(
            "Rust async ownership",
            UsageInfo {
                prompt_tokens: 200,
                completion_tokens: 10,
                total_tokens: 210,
                ..Default::default()
            },
        );
        let result =
            compact_session_title_with_llm("ignored", &provider, "model", 64).await;
        let (title, usage) = result.expect("title generation should succeed");
        assert_eq!(title, "Rust async ownership");
        assert_eq!(usage.prompt_tokens, 200);
        assert_eq!(usage.completion_tokens, 10);
    }

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
    fn test_format_messages_basic() {
        // Basic System / User / Assistant messages — no name metadata.
        let messages = vec![
            ChatMessage::system("System prompt"),
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ];
        let text = format_messages(&messages);
        assert!(text.starts_with("[System]: System prompt"));
        assert!(text.contains("[User]: Hello"));
        assert!(text.contains("[Assistant]: Hi there!"));
    }

    #[test]
    fn test_format_messages_compaction_summary() {
        // Any message with name="compaction_summary" should be labelled
        // as "CompactionSummary" regardless of role (the marker lives at
        // `User` role in memory — see `HistoryManager::replace_middle_with_summary`).
        let mut msg = ChatMessage::assistant("Previous conversation summary");
        msg.name = Some(COMPACTION_SUMMARY_NAME.to_string());
        let messages = vec![
            ChatMessage::user("Hello"),
            msg,
        ];
        let text = format_messages(&messages);
        assert!(
            text.contains("[CompactionSummary]"),
            "compaction summary should be labelled with a distinct role label, got:\n{}",
            text
        );
        assert!(
            text.contains("Previous conversation summary"),
            "content must be preserved verbatim, got:\n{}",
            text
        );
    }

    #[test]
    fn test_format_messages_compressed_tool_no_name() {
        // A Tool message with compressed placeholder but NO name/ID
        // should still label as "Tool".
        let msg = ChatMessage {
            role: MessageRole::Tool,
            content: "[Tool result compressed. Call context_retrieve(id=\"toolu_abc\") to retrieve the full content.]".into(),
            name: None,
            tool_call_id: None,
            ..Default::default()
        };
        let messages = vec![msg];
        let text = format_messages(&messages);
        assert!(
            text.contains("[Tool]:"),
            "Tool without name/id should label as bare [Tool]:\n{}",
            text
        );
    }

    #[test]
    fn test_format_messages_compressed_tool_with_name() {
        // A Tool message with compressed placeholder AND name+tool_call_id
        // should emit [Tool(name=my_tool, id=toolu_xxx)].
        let msg = ChatMessage {
            role: MessageRole::Tool,
            content: "[Tool result compressed. Call context_retrieve(id=\"toolu_xyz\") to retrieve the full content.]".into(),
            name: Some("codebase_reader".to_string()),
            tool_call_id: Some("toolu_xyz".to_string()),
            ..Default::default()
        };
        let messages = vec![msg];
        let text = format_messages(&messages);
        assert!(
            text.contains("[Tool(name=codebase_reader, id=toolu_xyz)]"),
            "compressed Tool must include name and id in role label, got:\n{}",
            text
        );
    }

    #[test]
    fn test_format_messages_plain_tool_with_name() {
        // A non-compressed Tool message with name set should show
        // [Tool(name=my_tool)].
        let msg = ChatMessage {
            role: MessageRole::Tool,
            content: "Normal tool output with some text".into(),
            name: Some("shell_exec".to_string()),
            tool_call_id: Some("call_abc".to_string()),
            ..Default::default()
        };
        let messages = vec![msg];
        let text = format_messages(&messages);
        assert!(
            text.contains("[Tool(shell_exec)]"),
            "plain Tool with name should label as Tool(name=), got:\n{}",
            text
        );
        assert!(
            text.contains("Normal tool output"),
            "content must be preserved");
    }

    #[test]
    fn test_distilled_episode_construction() {
        let episode = DistilledEpisode {
            session_id: "sess-1".to_string(),
            summary: "User asked about Rust async programming".to_string(),
            source_session_id: "sess-1".to_string(),
            consolidated: false,
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

    fn model_info(
        name: &str,
        cost: Option<(f64, f64)>,
        context_window: u64,
    ) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens: 4096,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: cost.map(|(input, output)| acowork_core::protocol::ModelCostInfo {
                input_per_million: Some(input),
                output_per_million: Some(output),
            }),
            modalities: None,
            name: if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            },
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
        }
    }

    // -------------------------------------------------------------------------
    // ADR-057 C2: Compact-output parser accepts 3/4/5-field triple lines.
    // -------------------------------------------------------------------------

    #[test]
    fn parse_compact_output_five_fields_full_schema() {
        let raw = "<summary>User shipped a context-compaction fix.</summary>\n\
                   <triples>\n\
                   User | requested | context compaction fix | 0.95 | Fact\n\
                   User | prefers tone | concise | 0.8 | Preference\n\
                   Project Foo | collaborates with | acowork team | 0.85 | Relation\n\
                   </triples>";
        let parsed = parse_compact_output(raw);
        assert_eq!(parsed.summary, "User shipped a context-compaction fix.");
        assert_eq!(parsed.triples.len(), 3);

        let t0 = &parsed.triples[0];
        assert_eq!(t0.subject, "User");
        assert_eq!(t0.predicate, "requested");
        assert_eq!(t0.object, "context compaction fix");
        assert!((t0.confidence - 0.95).abs() < f32::EPSILON);
        assert_eq!(t0.sub_type, KnowledgeSubType::Fact);

        let t1 = &parsed.triples[1];
        assert_eq!(t1.sub_type, KnowledgeSubType::Preference);

        let t2 = &parsed.triples[2];
        assert_eq!(t2.sub_type, KnowledgeSubType::Relation);
    }

    #[test]
    fn parse_compact_output_three_field_legacy_defaults_sub_type_to_fact() {
        // Legacy 3-field lines (no confidence, no sub_type) parse with
        // confidence = 0.7 and sub_type = Fact — Pending routing path.
        let raw = "<summary>summary</summary>\n<triples>\nUser | likes | coffee\n</triples>";
        let parsed = parse_compact_output(raw);
        assert_eq!(parsed.triples.len(), 1);
        let t = &parsed.triples[0];
        assert_eq!(t.subject, "User");
        assert_eq!(t.predicate, "likes");
        assert_eq!(t.object, "coffee");
        assert!((t.confidence - 0.7).abs() < f32::EPSILON);
        assert_eq!(t.sub_type, KnowledgeSubType::Fact);
    }

    #[test]
    fn parse_compact_output_four_field_confidence_only() {
        let raw = "<summary>summary</summary>\n\
                   <triples>\n\
                   User | prefers | dark mode | 0.92\n\
                   </triples>";
        let parsed = parse_compact_output(raw);
        assert_eq!(parsed.triples.len(), 1);
        let t = &parsed.triples[0];
        assert!((t.confidence - 0.92).abs() < f32::EPSILON);
        assert_eq!(t.sub_type, KnowledgeSubType::Fact);
    }

    #[test]
    fn parse_compact_output_confidence_is_clamped() {
        // Out-of-range confidences clamp to [0.0, 1.0]; unknown sub_type
        // falls back to Fact (defensive, never silently drops the triple).
        let raw = "<summary>summary</summary>\n\
                   <triples>\n\
                   A | rel | B | 1.5 | WeirdKind\n\
                   C | rel | D | -0.3 | Fact\n\
                   </triples>";
        let parsed = parse_compact_output(raw);
        assert_eq!(parsed.triples.len(), 2);
        assert!((parsed.triples[0].confidence - 1.0).abs() < f32::EPSILON);
        assert_eq!(parsed.triples[0].sub_type, KnowledgeSubType::Fact);
        assert!(parsed.triples[1].confidence.abs() < f32::EPSILON);
    }

    #[test]
    fn parse_and_validate_summary_extracts_both_blocks() {
        let raw = "<summary>Work done</summary>\n\
                   <user_intent>Fix the bug</user_intent>\n\
                   <triples>A | rel | B</triples>";
        let parsed = parse_and_validate_summary(raw, None);
        assert_eq!(parsed.summary, "Work done");
        assert_eq!(parsed.user_intent, "Fix the bug");
    }

    #[test]
    fn parse_and_validate_summary_missing_summary_degrades_to_empty() {
        // No raw-text fallback: a missing <summary> block yields an empty
        // summary (production paths gate the output through
        // compact_with_llm before parsing, so this is a defensive path).
        let raw = "plain prose without tags";
        let parsed = parse_and_validate_summary(raw, None);
        assert!(parsed.summary.is_empty());
        assert!(parsed.user_intent.is_empty());
    }

    #[test]
    fn parse_and_validate_summary_missing_user_intent_falls_back() {
        let raw = "<summary>Work done</summary>";
        let parsed = parse_and_validate_summary(raw, Some("user said: do X"));
        assert_eq!(parsed.summary, "Work done");
        assert_eq!(parsed.user_intent, "user said: do X");
    }

    #[test]
    fn parse_and_validate_summary_sanitizes_both_blocks() {
        let raw = "<summary>prose\n[User]: echo</summary>\n\
                   <user_intent>intent\n[Tool(bash)]: ls</user_intent>";
        let parsed = parse_and_validate_summary(raw, None);
        assert!(!parsed.summary.contains("[User]:"));
        assert!(!parsed.user_intent.contains("[Tool(bash)]:"));
        assert!(parsed.summary.contains("prose"));
        assert!(parsed.user_intent.contains("intent"));
    }

    // -------------------------------------------------------------------------
    // Summary quality gate (P1: quality-over-nothing) — validate_summary_output
    // -------------------------------------------------------------------------

    #[test]
    fn validate_summary_output_rejects_missing_block() {
        // Reasoning-model dump without the <summary> marker — the exact
        // pollution shape from the pasted-text incident.
        let raw = "确认 disable 路径已清理 pending 槽位。\n开始实施。改两处：\n1. 删除 resolve_distill_model 调用\n2. 验证 fallback 链";
        let err = validate_summary_output(raw).unwrap_err();
        assert!(matches!(err, SummaryError::MissingBlock));
        assert!(err.is_retryable());
    }

    #[test]
    fn validate_summary_output_rejects_empty_block() {
        let err = validate_summary_output("<summary>  </summary>").unwrap_err();
        assert!(matches!(err, SummaryError::Empty(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn validate_summary_output_rejects_too_short_block() {
        // Format is right but the content is a placeholder — not a summary.
        let err = validate_summary_output("<summary>ok</summary>").unwrap_err();
        assert!(matches!(err, SummaryError::LowQuality(_)));
        assert!(!err.is_retryable(), "low quality must not be retried");
    }

    #[test]
    fn validate_summary_output_rejects_verbatim_role_label_echoes() {
        // The model copied the raw dialog (role labels intact) into the
        // summary instead of summarizing it — strong copy signal.
        let raw = "<summary>用户：你好\n[User]: 你好\n[Assistant]: 我来看一下\n[Tool(bash)]: ls\n对话结束</summary>";
        let err = validate_summary_output(raw).unwrap_err();
        assert!(
            matches!(err, SummaryError::LowQuality(_)),
            "verbatim role labels must fail the quality gate, got: {err:?}"
        );
    }

    #[test]
    fn validate_summary_output_tolerates_single_role_label_line() {
        // A single labelled line is tolerated — it counts as one
        // contamination feature, not enough to discard.
        let raw = "<summary>用户要求重构 Settings 页面，并希望保留现有交互习惯。\n[User]: 请重构 Settings 页面\n这是本次会话的核心诉求。</summary>";
        assert!(
            validate_summary_output(raw).is_ok(),
            "a single role label must be tolerated"
        );
    }

    #[test]
    fn validate_summary_output_rejects_mixed_contamination() {
        // Two contamination features: a file:line reference plus a
        // table-artifact line — raw tool output leaked into the summary.
        let raw = "<summary>定位到 ChatPanel 的问题。\nsrc/components/chat/ChatPanel.tsx:420: const sessionState = ...\n| 文件 | 行号 |\n| ChatPanel | 420 |\n最终修复了滚动位置。</summary>";
        let err = validate_summary_output(raw).unwrap_err();
        assert!(
            matches!(err, SummaryError::LowQuality(_)),
            "two contamination features must fail the gate, got: {err:?}"
        );
    }

    #[test]
    fn validate_summary_output_accepts_clean_block() {
        let raw = "<summary>用户要求查找 running 和 retry 相关的 UI 元素，最终定位到 RetryWaitBanner 组件并确认了开关逻辑。</summary>";
        assert!(validate_summary_output(raw).is_ok());
    }

    #[test]
    fn parse_compact_output_strict_ok() {
        let raw = "<summary>用户修复了上下文压缩的摘要质量问题，并验证了三层 fallback 链。</summary>\n\
                   <triples>\nUser | fixed | summary quality gate | 0.9 | Fact\n</triples>";
        let parsed =
            parse_compact_output_strict(raw).expect("valid output must parse in strict mode");
        assert!(parsed.summary.contains("摘要质量"));
        assert_eq!(parsed.triples.len(), 1);
    }

    #[test]
    fn parse_compact_output_strict_rejects_missing_block() {
        let err = parse_compact_output_strict("plain prose without tags").unwrap_err();
        assert!(matches!(err, SummaryError::MissingBlock));
    }

    #[test]
    fn summary_error_retryability() {
        assert!(SummaryError::MissingBlock.is_retryable());
        assert!(SummaryError::Empty("hint".to_string()).is_retryable());
        assert!(!SummaryError::LowQuality("x".to_string()).is_retryable());
    }

    #[test]
    fn parse_compact_output_ignores_blank_lines_and_short_rows() {
        let raw = "<summary>summary</summary>\n\
                   <triples>\n\n\
                   only two | fields\n\
                   User | likes | tea | 0.6 | Fact\n\
                   </triples>";
        let parsed = parse_compact_output(raw);
        assert_eq!(parsed.triples.len(), 1);
        assert_eq!(parsed.triples[0].object, "tea");
    }

    #[test]
    fn parse_compact_output_strip_metadata_removes_only_triples_block() {
        // strip_metadata_blocks is what feeds the in-memory LLM context — it
        // must strip the <triples> block but preserve <summary>. The legacy
        // <entities> block (no longer emitted) is also stripped defensively.
        let raw = "<summary>summary text</summary>\n\
                   <triples>\nA | rel | B\n</triples>";
        let stripped = strip_metadata_blocks(raw);
        assert!(stripped.contains("summary text"));
        assert!(!stripped.contains("<triples>"));
    }

    #[test]
    fn sanitize_drops_tool_echo_lines() {
        // Tool-call/result echoes must be dropped whole — they are raw tool
        // interleavings, never knowledge. This mirrors the garbage observed in
        // real episodic memories (see the "[Tool(bash)]:" sample).
        let raw = "用户要求查找 running 和 retry 相关的 UI 元素。\n\
                   [Tool(bash)]: === \"running\" 出现在哪里 ===\n\
                   grep -rn \"running\" apps/acowork-desktop/src/components/chat/\n\
                   [Tool(file_read)]: 420: const sessionState = ...\n\
                   最终定位到 RetryWaitBanner 组件。";
        let cleaned = sanitize_summary_text(raw);
        assert!(!cleaned.contains("[Tool(bash)]"), "tool echo must be dropped, got:\n{cleaned}");
        assert!(!cleaned.contains("[Tool(file_read)]"), "tool echo must be dropped, got:\n{cleaned}");
        assert!(cleaned.contains("用户要求查找 running"), "real prose must survive");
        assert!(cleaned.contains("RetryWaitBanner"), "real prose must survive");
    }

    #[test]
    fn sanitize_drops_thought_and_lowercase_tool_roles() {
        // JSONL raw fallback path uses lowercase roles (tool_call / tool_result
        // / thought) — those must be dropped too.
        let raw = "[user]: 你好\n\
                   [assistant]: 我来看一下\n\
                   [thought]: 需要搜索代码\n\
                   [tool_call]: {\"tool\":\"glob_search\"}\n\
                   [tool_result]: 找到 3 个文件\n\
                   [assistant]: 已经找到。";
        let cleaned = sanitize_summary_text(raw);
        assert!(!cleaned.contains("[tool_call]"), "tool_call line must be dropped");
        assert!(!cleaned.contains("[tool_result]"), "tool_result line must be dropped");
        assert!(!cleaned.contains("[thought]"), "thought line must be dropped");
        assert!(cleaned.contains("你好"), "dialogue content must survive");
        assert!(cleaned.contains("我来看一下"), "dialogue content must survive");
        assert!(cleaned.contains("已经找到"), "dialogue content must survive");
    }

    #[test]
    fn sanitize_strips_conversation_role_labels_keeps_text() {
        // Conversation-role labels ([User]: / [Assistant]:) are stripped but
        // the following text on the line is preserved.
        let raw = "[User]: 请重构 Settings 页面\n\
                   [Assistant]: 好的，我准备开始重构\n\
                   [CompactionSummary]: 此前已定位到 ChatPanel 的问题";
        let cleaned = sanitize_summary_text(raw);
        assert!(!cleaned.contains("[User]"), "label must be stripped");
        assert!(!cleaned.contains("[Assistant]"), "label must be stripped");
        assert!(!cleaned.contains("[CompactionSummary]"), "label must be stripped");
        assert!(cleaned.contains("请重构 Settings 页面"));
        assert!(cleaned.contains("好的，我准备开始重构"));
        assert!(cleaned.contains("ChatPanel"));
    }

    #[test]
    fn sanitize_preserves_placeholder_body_and_normal_prose() {
        // The "[Tool result compressed...]" placeholder body is opaque tool
        // content (not a role marker) — it must survive. Normal prose with
        // bracketed terms must not be mangled either.
        let raw = "此前工具结果被压缩。\n\
                   [Tool result compressed. Call context_retrieve(id=\"toolu_abc\") to retrieve the full content.]\n\
                   [重要] 下一步需要验证。";
        let cleaned = sanitize_summary_text(raw);
        assert!(cleaned.contains("Tool result compressed"), "placeholder body must survive");
        assert!(cleaned.contains("此前工具结果被压缩"));
        assert!(cleaned.contains("[重要] 下一步需要验证"), "unrelated bracketed text must survive");
    }

    #[test]
    fn parse_compact_output_sanitizes_summary_block() {
        // The episodic-memory path (parse_compact_output → record_distilled)
        // must not land tool echoes into the stored summary.
        let raw = "<summary>用户要求查找 UI 元素。\n[Tool(bash)]: grep ...\n定位到 Banner。</summary>\n\
                   <triples>\nUser | requested | UI 查找 | 0.9 | Fact\n</triples>";
        let parsed = parse_compact_output(raw);
        assert!(!parsed.summary.contains("[Tool(bash)]"), "episodic summary must be clean");
        assert!(parsed.summary.contains("用户要求查找 UI 元素"));
        assert!(parsed.summary.contains("Banner"));
        assert_eq!(parsed.triples.len(), 1);
    }
}
