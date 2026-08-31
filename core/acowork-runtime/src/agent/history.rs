//! Conversation history management (FIFO trimming + Sanitization + Emergency trim)
//!
//! Adapted from zeroclaw/src/agent/history.rs
//! ACowork deviation: uses acowork-core ChatMessage types; token estimation
//! uses char-based approximation instead of tiktoken.
//! SPDX-License-Identifier: MIT OR Apache-2.0
//!
//! ## Design note (2026-05-28)
//!
//! Programmatic folding strategies (Tool Result folding, content folding) have been
//! removed per [ADR-010](../../../../docs/adr/ADR-010-context-compression-simplification.md).
//! Context compression is a semantic understanding task — only an LLM can reliably
//! decide what to discard. Per ADR-061, the FIFO safety nets (trim_fifo,
//! emergency_trim) are deleted too: the 8-level plan (level 8 floor) covers every
//! within-budget case, and LLM failure is an explicit `ChunkEvent::Error` (§11.3).

use std::collections::HashSet;
use std::sync::Arc;

use acowork_core::protocol::ProtocolType;
use acowork_core::providers::traits::{ChatMessage, MessageRole, Provider};
#[cfg(test)]
use acowork_core::providers::traits::ChatRequest;

use crate::agent::compression_constants::SUMMARY_TOKEN_BUDGET;
use crate::error::RuntimeError;
use crate::token::counter::TokenCounter;

// ── ADR-052 placeholder format constants ────────────────────────────────
//
// Shared between `HistoryManager::abandon_tool_result` (producer) and
// `episode_distill::format_messages` (consumer at LLM-compaction time).
// Both producer and consumer MUST agree on the prefix string for the
// idempotency check; centralizing it here prevents the two from drifting.

/// Stable prefix for a compressed tool-result placeholder produced by
/// [`HistoryManager::abandon_tool_result`]. Content with this prefix
/// is treated as "already compressed" by both:
/// - `abandon_tool_result` idempotency check
/// - `retrieve_tool_result` round-trip check (placeholder → restored)
/// - `format_messages` (LLM compaction prompt builder) for richer
///   structured labelling in the summary prompt
///
/// ADR-052: When the LLM invokes `context_abandon`, the tool result
/// content is replaced with:
///   `[Tool result compressed. Call context_retrieve(id="{tool_call_id}") to retrieve the full content.]`
///
/// ## Format contract
///
/// The exact format string is a contract shared by **three** consumers:
///
/// 1. **`context_retrieve` built-in tool** — parses `tool_call_id` from the placeholder
///    to fetch the original content from the in-memory inverted index.
/// 2. **`format_messages` in episode_distill** — checks `starts_with` this prefix
///    to label compressed tool results correctly in compaction prompts.
/// 3. **LLM system prompt** — instructs the model to copy the `tool_call_id` from
///    the placeholder verbatim when calling `context_retrieve`.
///
/// **Any change** to this prefix or the placeholder format **must** update all three
/// consumers in lockstep.
///
/// ## Format invariant
///
/// The placeholder always includes the raw `tool_call_id` embedded inline
/// (e.g. `toolu_abc123`), so the LLM can copy-paste it without parsing.
/// Format: prefix + " Call context_retrieve(id=\"" + tool_call_id + "\") ..."
pub(crate) const COMPRESSED_TOOL_PLACEHOLDER_PREFIX: &str = "[Tool result compressed.";

/// Stable identifier string used by [`HistoryManager::replace_middle_with_summary`]
/// to mark the synthetic Assistant message that replaces the compacted middle.
/// Detected by `format_messages` to label these as "CompactionSummary"
/// (rather than "Assistant") in the summary prompt, so the LLM knows
/// it is reading a previous compaction output rather than a fresh turn.
pub(crate) const COMPACTION_SUMMARY_NAME: &str = "compaction_summary";

// ── ADR-061: 8-level degradation compression types ─────────────────────

/// ADR-061: error from planning / applying an 8-level compression.
#[derive(Debug)]
pub(crate) enum CompressError {
    /// The plan failed its acceptance check at apply time (defensive;
    /// plan-time validation normally catches this first).
    InsufficientCompression { projected_ratio: f64 },
    /// Level 8 (the sole budget-only level) still cannot fit the
    /// summary + retained skeleton within the budget (§19.5).
    UnrecoverableOverflow { projected: u64, budget: u64 },
}

impl std::fmt::Display for CompressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientCompression { projected_ratio } => write!(
                f,
                "compression plan failed acceptance check (projected ratio {:.1}%)",
                projected_ratio * 100.0
            ),
            Self::UnrecoverableOverflow { projected, budget } => write!(
                f,
                "level 8 cannot fit within budget: projected {} > budget {}",
                projected, budget
            ),
        }
    }
}

impl std::error::Error for CompressError {}

/// ADR-061: result of a successful 8-level compression apply.
#[derive(Debug, Clone)]
pub(crate) struct CompressionOutcome {
    pub level: u8,
    pub original_tokens: u64,
    pub new_tokens: u64,
    pub compression_ratio: f64,
    pub removed_messages: usize,
}

/// ADR-061: retention statistics per level — persisted inside the summary
/// marker's level-metadata block (§9) for post-hoc debugging.
#[derive(Debug, Clone)]
struct RetentionStats {
    user_messages: usize,
    assistant_messages: usize,
    tool_messages: usize,
    user_desc: String,
    assistant_desc: String,
    tool_desc: String,
}

/// ADR-061: how tool messages are retained for levels 1-4.
#[derive(Clone, Copy)]
enum ToolKeep {
    /// Keep tool messages from (and after) the K-th assistant from the end.
    WithinLastAssistants(usize),
    /// Keep no tool messages.
    None,
}

/// ADR-061: an 8-level degradation compression plan (levels 1-8).
///
/// Captures exactly which messages survive at the chosen level (system +
/// user skeleton + selected assistant/tool tail) plus the token projection.
/// Created by [`HistoryManager::plan_compression`], consumed by
/// [`HistoryManager::apply_compression`].
#[derive(Debug)]
pub(crate) struct CompressionPlan {
    /// Selected level (1-8).
    pub level: u8,
    /// Retained messages in original order (excluding the new summary
    /// marker).
    retained: Vec<ChatMessage>,
    /// History token count at plan time.
    original_tokens: u64,
    /// Estimated tokens of `retained` (via `count_message`).
    retained_tokens: u64,
    /// Projected total = retained + summary marker (exact: the summary
    /// size is already known at plan time, §19.1).
    projected_tokens: u64,
    /// Retention stats for the level metadata block.
    stats: RetentionStats,
}

/// Build the summary marker content: level metadata block (§9) prepended
/// to the LLM summary. Metadata is runtime-generated (not LLM output) and
/// machine-parseable.
fn build_summary_marker(
    level: u8,
    original_tokens: u64,
    new_tokens: u64,
    ratio: f64,
    stats: &RetentionStats,
    summary: &str,
) -> String {
    format!(
        "[compressed: level={}]\n  user_messages: {} ({})\n  assistant_messages: {} ({})\n  tool_messages: {} ({})\n  tokens: {} -> {} (ratio {:.1}%)\n\n{}",
        level,
        stats.user_desc,
        stats.user_messages,
        stats.assistant_desc,
        stats.assistant_messages,
        stats.tool_desc,
        stats.tool_messages,
        original_tokens,
        new_tokens,
        ratio * 100.0,
        summary
    )
}

/// History manager for conversation
pub struct HistoryManager {
    /// Conversation messages.
    ///
    /// ADR-054: held behind `Arc` so debug context snapshots can retain a
    /// shallow reference (`Arc::clone`) to the exact history of their build
    /// iteration instead of deep-copying the whole conversation per iteration.
    /// All mutations go through `Arc::make_mut` (copy-on-write): when no
    /// snapshot holds the buffer (non-debug mode) mutations are in-place
    /// zero-copy; when snapshots share it, the first mutation after a
    /// snapshot clones once and subsequent mutations reuse that buffer.
    messages: Arc<Vec<ChatMessage>>,
    /// Maximum token budget for history
    max_tokens: u64,
    /// Current estimated token count for the conversation prompt.
    ///
    /// Initially tracks conversation history tokens only (via `count_message`).
    /// After each LLM call, [`calibrate_from_usage`] replaces this with the
    /// API-reported `prompt_tokens` (which includes system prompt), preventing
    /// cumulative estimation drift across turns.
    current_tokens: u64,
    /// LLM protocol type for image token estimation.
    /// Defaults to OpenAI; set via `set_protocol_type()` after construction.
    protocol_type: ProtocolType,
    /// Tiered token counter for unified token estimation.
    counter: TokenCounter,
    /// Model name for Tier1/Tier2 token counting precision.
    /// When `None` (not yet set), falls back to Tier3 heuristic.
    model_name: Option<String>,
}

impl HistoryManager {
    /// Create new history manager with token budget.
    pub fn new(max_tokens: u64) -> Self {
        Self {
            messages: Arc::new(Vec::new()),
            max_tokens,
            current_tokens: 0,
            protocol_type: ProtocolType::default(),
            counter: TokenCounter::new(),
            model_name: None,
        }
    }

    /// Set the LLM protocol type for image token estimation.
    pub fn set_protocol_type(&mut self, pt: ProtocolType) {
        self.protocol_type = pt;
    }

    /// Get the current model chars/token ratio from the calibrated ratio store.
    /// Returns `None` if no model is set or no calibration has occurred yet.
    pub fn model_ratio(&self) -> Option<f64> {
        let model = self.model_name.as_deref()?;
        if model.is_empty() {
            return None;
        }
        Some(self.counter.model_ratios().get(model))
    }

    /// Set the model name for token counting precision.
    /// Called when session model is determined (ADR-012).
    pub fn set_model_name(&mut self, model: String) {
        self.model_name = Some(model);
    }

    /// Initialize the token counter with a persistent ratio store.
    ///
    /// Called once during AgentLoop startup with the agent's config directory
    /// path. Loads previously calibrated ratios from `{config_dir}/model_ratios.json`
    /// and auto-saves after each calibration.
    pub fn init_model_ratios(&mut self, config_dir: &std::path::Path) {
        let path = config_dir.join("model_ratios.json");
        self.counter = TokenCounter::new_with_ratios(
            crate::token::ratio_store::ModelRatioStore::with_persistence(path),
        );
    }

    /// Dynamically update the max token budget for FIFO trimming.
    ///
    /// This should be called whenever the model changes (session creation,
    /// model switch), so that [`trim_fifo`] uses the correct
    /// [`ModelCapabilitiesInfo::effective_input_budget`] instead of
    /// the static config default.
    pub fn set_max_tokens(&mut self, max_tokens: u64) {
        tracing::info!(
            old = self.max_tokens,
            new = max_tokens,
            "HistoryManager max_tokens updated"
        );
        self.max_tokens = max_tokens;
    }

    /// Get the model name for token counting, falling back to empty string (Tier3).
    fn model_for_counting(&self) -> &str {
        self.model_name.as_deref().unwrap_or("")
    }

    /// Get the current protocol type.
    pub fn protocol_type(&self) -> &ProtocolType {
        &self.protocol_type
    }

    /// Get reference to messages
    pub fn messages(&self) -> &[ChatMessage] {
        self.messages.as_slice()
    }

    /// Get an `Arc` clone of the messages (O(1), no copy).
    ///
    /// ADR-054: debug context snapshots hold this shallow reference so
    /// `messages_by_iteration` shares the underlying buffer instead of
    /// deep-copying per iteration. Mutations after this clone go through
    /// [`Arc::make_mut`], so the clone retains the exact history as of the
    /// snapshot's build iteration.
    pub fn messages_arc(&self) -> Arc<Vec<ChatMessage>> {
        Arc::clone(&self.messages)
    }

    /// Get mutable reference to messages (copy-on-write).
    ///
    /// When other `Arc` clones exist (e.g. a debug snapshot), this clones
    /// the buffer once; subsequent mutations reuse that buffer. When this
    /// is the sole owner, the buffer is mutated in place with zero copies.
    pub fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        Arc::make_mut(&mut self.messages)
    }

    /// Get current estimated token count
    pub fn token_count(&self) -> u64 {
        self.current_tokens
    }

    /// Calibrate the history token count from actual API usage feedback.
    ///
    /// LLM API responses include `usage.prompt_tokens` which is the authoritative
    /// token count for the entire prompt (system + history + tool definitions).
    /// This method:
    /// 1. Replaces our heuristic estimate with the API ground truth for budget tracking
    /// 2. Computes the chars/token ratio and feeds it back into TokenCounter
    ///
    /// ## Calibration formula
    ///
    /// ```text
    /// ratio = total_input_chars / prompt_tokens
    /// ```
    ///
    /// Both `total_input_chars` and `prompt_tokens` represent the same LLM request
    /// payload — they are **same-source** (分子分母同源), avoiding the calibration
    /// distortion that plagued previous versions.
    ///
    /// ## Safety
    ///
    /// When `prompt_tokens` is 0, the API response is considered unreliable
    /// (observed with some Anthropic-protocol providers like MiniMax that
    /// occasionally omit `message_start` usage fields). Calibration is skipped
    /// entirely to prevent corrupting the ratio store with a bogus value.
    pub fn calibrate_from_usage(&mut self, prompt_tokens: u64, total_input_chars: usize) {
        if prompt_tokens == 0 {
            tracing::warn!(
                current_tokens = self.current_tokens,
                "Skipping calibration: API returned prompt_tokens=0 (unreliable usage data)"
            );
            return;
        }

        // Store API ground truth for budget tracking.
        let prior = self.current_tokens;
        self.current_tokens = prompt_tokens;

        // Calibrate the chars/token ratio from same-source data.
        // Both total_input_chars and prompt_tokens represent the same LLM request,
        // so the computed ratio is a precise measurement of the model's chars/token.
        if total_input_chars > 500 && prompt_tokens > 500 {
            let ratio = total_input_chars as f64 / prompt_tokens as f64;
            if let Some(ref model) = self.model_name {
                self.counter.model_ratios_mut().update(model, ratio);
            }
        }

        tracing::debug!(
            prior,
            api = prompt_tokens,
            total_input_chars,
            delta = prompt_tokens as i64 - prior as i64,
            "History token count calibrated from API usage"
        );
    }

    /// Append a message to history
    pub fn append(&mut self, message: ChatMessage) {
        let tokens = self.counter.count_message(
            &message,
            self.model_for_counting(),
            Some(&self.protocol_type),
        );
        self.current_tokens += tokens;
        self.messages_mut().push(message);
    }

    /// Append multiple messages
    pub fn extend(&mut self, messages: Vec<ChatMessage>) {
        for msg in &messages {
            self.current_tokens += self.counter.count_message(
                msg,
                self.model_for_counting(),
                Some(&self.protocol_type),
            );
        }
        self.messages_mut().extend(messages);
    }

    /// Bulk-load a pre-built message sequence from session resume.
    ///
    /// Replaces any existing messages and recomputes the token count once.
    /// Used by [`crate::agent::session::restorer`] to install the JSONL-derived
    /// history before the session starts processing new inbound messages.
    ///
    /// Unlike [`Self::append`], this is intended for trusted, already-sanitized
    /// input (the restorer guarantees tool_call/tool_result pairing and
    /// system/compaction-marker ordering invariants).
    pub fn load_restored(&mut self, messages: Vec<ChatMessage>) {
        self.messages = Arc::new(messages);
        self.current_tokens = self
            .messages
            .iter()
            .map(|m| {
                self.counter
                    .count_message(m, self.model_for_counting(), Some(&self.protocol_type))
            })
            .sum();
        tracing::info!(
            count = self.messages.len(),
            tokens = self.current_tokens,
            "HistoryManager: loaded restored history"
        );
    }

    /// Lossless trim after restore: drop the oldest **complete rounds** until
    /// the history token count is at or below 80% of `max_tokens`.
    ///
    /// A "round" here is the maximal contiguous tail starting at a non-system,
    /// non-compaction-marker message and extending up to (but not including)
    /// the next User message. This guarantees we never split an
    /// `Assistant{tool_calls}` from its matching `Tool` results.
    ///
    /// Preserved across all trims:
    /// - Leading `MessageRole::System` messages
    /// - The single compaction summary marker (identified by
    ///   `name == "compaction_summary"`; stored as a `User` message — see
    ///   [`Self::replace_middle_with_summary`]), if present
    ///
    /// Returns the number of messages dropped. Does not invoke any LLM.
    ///
    /// This is the safety net for the "model swap on resume → smaller token
    /// budget" case: even faithful replay can overflow if the user resumed the
    /// session under a model with a smaller context window.
    pub fn fit_to_budget_lossless(&mut self) -> usize {
        if self.max_tokens == 0 {
            return 0;
        }
        let target = (self.max_tokens as f64 * 0.80) as u64;
        if self.current_tokens <= target {
            return 0;
        }

        fn is_compaction_marker(msg: &ChatMessage) -> bool {
            // Identify by `name` only — the compaction summary lives at
            // `User` role in memory (see `replace_middle_with_summary`),
            // not `Assistant`.  A role-based check here would misclassify
            // the marker as a regular user/assistant turn and let
            // `lossless_trim` remove it.
            msg.name.as_deref() == Some(COMPACTION_SUMMARY_NAME)
        }

        // Locate the first removable index: skip leading System and the
        // contiguous compaction marker that follows them (if any).
        let mut first_removable = self
            .messages
            .iter()
            .position(|m| !matches!(m.role, MessageRole::System))
            .unwrap_or(self.messages.len());
        if first_removable < self.messages.len()
            && is_compaction_marker(&self.messages[first_removable])
        {
            first_removable += 1;
        }

        let mut removed = 0;
        while self.current_tokens > target && first_removable < self.messages.len() {
            // Find the end of the next "round": from first_removable up to
            // (but not including) the next User message, OR end of history.
            let mut round_end = first_removable + 1;
            while round_end < self.messages.len()
                && !matches!(self.messages[round_end].role, MessageRole::User)
            {
                round_end += 1;
            }

            // If dropping this round would empty everything tail-side, stop:
            // we always want at least one tail round to remain.
            if round_end >= self.messages.len() {
                break;
            }

            // Drop [first_removable .. round_end)
            let dropped_tokens: u64 = self.messages[first_removable..round_end]
                .iter()
                .map(|m| {
                    self.counter
                        .count_message(m, self.model_for_counting(), Some(&self.protocol_type))
                })
                .sum();
            self.messages_mut().drain(first_removable..round_end);
            self.current_tokens = self.current_tokens.saturating_sub(dropped_tokens);
            removed += round_end - first_removable;
        }

        if removed > 0 {
            tracing::warn!(
                removed,
                remaining = self.messages.len(),
                tokens = self.current_tokens,
                target_budget = target,
                "HistoryManager: lossless trim after restore"
            );
        }
        removed
    }

    /// Clear all messages
    pub fn clear(&mut self) {
        self.messages = Arc::new(Vec::new());
        self.current_tokens = 0;
    }

    /// Get message count
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Check if history is empty
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Truncate history to the specified number of messages.
    ///
    /// Keeps only the first `target_len` messages and recalculates
    /// the token count. Used by debug rewind to roll back history
    /// to a specific conversation snapshot.
    pub fn truncate_to(&mut self, target_len: usize) {
        if target_len >= self.messages.len() {
            return;
        }
        self.messages_mut().truncate(target_len);
        // Recalculate token count
        self.current_tokens = self
            .messages
            .iter()
            .map(|m| {
                self.counter
                    .count_message(m, self.model_for_counting(), Some(&self.protocol_type))
            })
            .sum();
        tracing::info!(
            target_len,
            new_token_count = self.current_tokens,
            "History truncated for debug rewind"
        );
    }

    /// Estimate total tokens for all messages (for pre-check)
    pub fn estimate_total_tokens(&self) -> u64 {
        self.current_tokens
    }

    /// Returns 1 if replaced, 0 if not found or already compressed.
    pub fn abandon_tool_result(&mut self, tool_call_id: &str) -> usize {
        for msg in self.messages_mut() {
            if !matches!(msg.role, MessageRole::Tool) {
                continue;
            }
            if msg.tool_call_id.as_deref() != Some(tool_call_id) {
                continue;
            }
            // Idempotency: skip already-compressed messages
            if msg.content.starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX) {
                return 0;
            }
            msg.content = format!(
                "[Tool result compressed. Call context_retrieve(id=\"{}\") to retrieve the full content.]",
                tool_call_id
            );
            return 1;
        }
        0
    }

    /// ADR-052: Restore a Tool message's content from placeholder back to original.
    /// Called by `drain_retrieve_queue` after the LLM invokes `context_retrieve`.
    ///
    /// Idempotent: if the message is already raw (not a placeholder), returns 0.
    ///
    /// Returns 1 if restored, 0 if not found or already raw.
    pub fn retrieve_tool_result(&mut self, tool_call_id: &str, original_content: &str) -> usize {
        for msg in self.messages_mut() {
            if !matches!(msg.role, MessageRole::Tool) {
                continue;
            }
            if msg.tool_call_id.as_deref() != Some(tool_call_id) {
                continue;
            }
            // Idempotency: skip already-restored messages
            if !msg.content.starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX) {
                return 0;
            }
            msg.content = original_content.to_string();
            return 1;
        }
        0
    }

    /// Recompute `current_tokens` from scratch.
    ///
    /// Must be called after any in-place content mutation (e.g. after
    /// `abandon_tool_result` or `retrieve_tool_result`) since these mutate
    /// content in place but cannot update `current_tokens` under borrow
    /// rules. O(N) over messages with constant-time token estimation each.
    pub fn recalibrate_tokens(&mut self) {
        let model = self.model_for_counting().to_string();
        let pt = self.protocol_type.clone();
        let mut total = 0u64;
        for msg in self.messages.iter() {
            total = total.saturating_add(self.counter.count_message(msg, &model, Some(&pt)));
        }
        self.current_tokens = total;
    }

    /// Sanitize message history to remove or fix corrupted entries.
    ///
    /// This prevents LLM 400 errors caused by invalid tool_call data when
    /// conversation history is replayed after an agent restart.
    ///
    /// Cleaning rules (applied in order):
    /// 1. Fix invalid tool_call arguments — replace non-JSON with `{}`
    /// 2. Remove orphaned tool result messages — no matching tool_call
    /// 3. Remove orphaned tool_calls — no matching tool result
    /// 4. Remove empty assistant messages — no content and no tool_calls
    /// 5. Remove non-first system messages — some LLM providers only allow
    ///    system role at the first position (e.g. MiniMax)
    ///
    /// This method is idempotent: calling it multiple times produces the same result.
    pub fn sanitize_messages(messages: &mut Vec<ChatMessage>) {
        // Step 1: Fix invalid tool_call arguments
        for msg in messages.iter_mut() {
            if let Some(ref mut tool_calls) = msg.tool_calls {
                for tc in tool_calls.iter_mut() {
                    if serde_json::from_str::<serde_json::Value>(&tc.function.arguments).is_err() {
                        tracing::warn!(
                            tool_call_id = %tc.id,
                            tool_name = %tc.function.name,
                            invalid_args = %tc.function.arguments,
                            "Sanitizing invalid tool_call arguments to empty object"
                        );
                        tc.function.arguments = "{}".to_string();
                    }
                }
            }
        }

        // Step 2: Collect valid tool_call_ids from assistant messages
        let valid_tool_call_ids: HashSet<String> = messages
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .flat_map(|tcs| tcs.iter().map(|tc| tc.id.clone()))
            .collect();

        // Step 3: Remove orphaned tool result messages
        messages.retain(|msg| {
            if msg.role == MessageRole::Tool
                && let Some(ref tcid) = msg.tool_call_id
                && !valid_tool_call_ids.contains(tcid)
            {
                tracing::warn!(
                    tool_call_id = %tcid,
                    "Removing orphaned tool result message"
                );
                return false;
            }
            true
        });

        // Step 4: Collect tool result IDs to find orphaned tool_calls
        let tool_result_ids: HashSet<String> = messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .filter_map(|m| m.tool_call_id.clone())
            .collect();

        // Remove tool_calls without corresponding tool results
        for msg in messages.iter_mut() {
            if let Some(ref mut tool_calls) = msg.tool_calls {
                let before = tool_calls.len();
                tool_calls.retain(|tc| {
                    if !tool_result_ids.contains(&tc.id) {
                        tracing::warn!(
                            tool_call_id = %tc.id,
                            tool_name = %tc.function.name,
                            "Removing tool_call without corresponding result"
                        );
                        return false;
                    }
                    true
                });
                // If all tool_calls were removed, clear the field
                if tool_calls.is_empty() && before > 0 {
                    msg.tool_calls = None;
                }
            }
        }

        // Step 5: Remove empty assistant messages (no content + no tool_calls)
        messages.retain(|msg| {
            if msg.role == MessageRole::Assistant {
                let has_content = !msg.content.is_empty();
                let has_tool_calls = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
                if !has_content && !has_tool_calls {
                    tracing::warn!("Removing empty assistant message");
                    return false;
                }
            }
            true
        });

        // Step 6: Remove system messages that are not at position 0
        // Some LLM providers only allow system role at the first position.
        let before_len = messages.len();
        let mut first_system_seen = false;
        messages.retain(|m| {
            if matches!(m.role, MessageRole::System) {
                if !first_system_seen {
                    first_system_seen = true;
                    true
                } else {
                    tracing::warn!(
                        content_preview = %m.content.chars().take(80).collect::<String>(),
                        "sanitize: removing non-first system message"
                    );
                    false
                }
            } else {
                true
            }
        });
        if messages.len() < before_len {
            tracing::warn!(
                removed = before_len - messages.len(),
                "sanitize: removed non-first system messages"
            );
        }
    }

    // ── Compaction methods (ADR-011: 摘要即蒸馏) ─────────────────────

    /// Compact full conversation history into a natural-language summary
    /// via LLM. Used at 80% token usage threshold (context compaction).
    ///
    /// Formats all messages as text, wraps them in the COMPACT_PROMPT
    /// template, and sends to the configured Compact Model.
    /// Returns the plain-text summary (no JSON parsing).
    ///
    /// `identity_context` is the user's `UserProfile` formatted as text
    /// (see [`super::session::session_manager::format_user_profile_context`]).
    /// When `Some`, it is embedded into the system prompt so the LLM writes
    /// the summary in the user's preferred language. Pass `None` when the
    /// session has no user profile yet (default → English summary).
    ///
    /// Returns `(summary, usage)` per ADR-027 so callers can record raw
    /// Provider usage in [`crate::conversation::SessionTokens`].
    pub async fn compact_via_llm(
        &self,
        provider: &dyn Provider,
        model_name: &str,
        system_prompt: &str,
        identity_context: Option<&str>,
    ) -> std::result::Result<(String, acowork_core::providers::traits::UsageInfo), RuntimeError> {
        let messages_text = crate::episode_distill::format_messages(self.messages.as_slice());
        if messages_text.is_empty() {
            return Err(RuntimeError::Tool(
                "Cannot compact empty history".to_string(),
            ));
        }

        let prompt = crate::prompt::COMPACT_PROMPT.replace("{messages_text}", &messages_text);
        crate::episode_distill::compact_with_llm(
            &prompt,
            provider,
            model_name,
            SUMMARY_TOKEN_BUDGET as u32,
            identity_context,
            system_prompt,
        )
        .await
    }

    /// ADR-061 §8.2/§19.4: join the original user messages as the
    /// fallback `<user_intent>` when the compaction LLM omits the block.
    ///
    /// Compaction markers (`name == COMPACTION_SUMMARY_NAME`) are
    /// excluded — they are compression artifacts, not original user
    /// input, and must never masquerade as user intent (ADR-061 §19.4).
    pub fn user_intent_fallback_text(&self) -> String {
        self.messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User))
            .filter(|m| m.name.as_deref() != Some(COMPACTION_SUMMARY_NAME))
            .map(|m| m.content.trim())
            .filter(|c| !c.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Replace the middle section of history with a compaction summary.
    ///
    /// Keeps system messages at the start and the last `keep_last_rounds`
    /// conversational rounds at the end. The middle is replaced with a
    /// single `User`-role message carrying `name: "compaction_summary"` as
    /// a compaction marker for [`last_compaction_index`].
    ///
    /// NOTE: The marker is stored as `User` (not `Assistant`) so that
    /// providers which reject two consecutive `Assistant` messages in the
    /// request payload (e.g. glm-5.2 on Volcano Ark) do not 400 when the
    /// preserved tail's first round is also `Assistant{tool_calls}`.
    /// Consumers that need to recognize the marker must filter by
    /// `name == COMPACTION_SUMMARY_NAME`, not by role.
    ///
    /// Returns the number of messages removed.
    pub fn replace_middle_with_summary(&mut self, summary: &str, keep_last_rounds: usize) -> usize {
        // Count leading system messages
        let system_count = self
            .messages
            .iter()
            .take_while(|m| matches!(m.role, MessageRole::System))
            .count();

        // Find tail start: count User or Tool messages from the end.
        // Each "round" starts with a User message (human input) or a Tool
        // message (tool result that feeds the next assistant turn). Counting
        // both ensures correct round detection in tool-calling scenarios
        // where the only User messages are at the conversation start.
        let tail_start = {
            let mut round_count = 0usize;
            let mut idx = self.messages.len();
            for (i, msg) in self.messages.iter().enumerate().rev() {
                if matches!(msg.role, MessageRole::User | MessageRole::Tool) {
                    round_count += 1;
                    if round_count >= keep_last_rounds {
                        idx = i;
                        break;
                    }
                }
            }
            // Not enough rounds: keep everything after system messages
            if round_count < keep_last_rounds {
                system_count
            } else {
                // ── Fix: expand tail boundary to include Assistant messages
                // that own tool_calls referenced by Tool messages in the tail.
                // Without this, sanitize_messages removes orphaned Tool results
                // and the "kept" rounds become empty, defeating compaction.
                //
                // Collect tool_call_ids from Tool messages in [idx, end).
                let tail_tool_ids: HashSet<String> = self.messages[idx..]
                    .iter()
                    .filter(|m| m.role == MessageRole::Tool)
                    .filter_map(|m| m.tool_call_id.clone())
                    .collect();

                // Walk backward from idx-1 to expand tail_start to include
                // any Assistant whose tool_calls match tail_tool_ids.
                // Stop when hitting a User message (natural round boundary).
                let mut expanded = idx;
                if !tail_tool_ids.is_empty() {
                    for j in (system_count..idx).rev() {
                        match self.messages[j].role {
                            MessageRole::User => break,
                            MessageRole::Assistant | MessageRole::Tool => {
                                if let Some(ref tcs) = self.messages[j].tool_calls
                                    && tcs.iter().any(|tc| tail_tool_ids.contains(&tc.id)) {
                                        expanded = j;
                                    }
                            }
                            _ => {}
                        }
                    }
                }
                expanded
            }
        };

        if tail_start <= system_count {
            return 0; // Nothing to replace
        }

        let removed_count = tail_start - system_count;

        // Subtract tokens of removed messages
        for msg in &self.messages[system_count..tail_start] {
            let tokens = self.counter.count_message(
                msg,
                self.model_for_counting(),
                Some(&self.protocol_type),
            );
            self.current_tokens = self.current_tokens.saturating_sub(tokens);
        }

        // Remove middle section
        self.messages_mut().drain(system_count..tail_start);

        // Insert compaction summary as a User message with marker.
        //
        // We use `User` (not `Assistant`) on purpose: a compaction summary
        // represents the conversation's prior content narrated as context, not
        // a prior assistant turn.  Crucially, this avoids producing two
        // consecutive `Assistant` messages in the request payload when the
        // tail's first preserved round is also `Assistant{tool_calls}`.  Some
        // providers (notably glm-5.2 on Volcano Ark) reject that exact pattern
        // with `400 InvalidParameter`, even though the OpenAI spec treats it
        // permissively.  The marker is still identified by
        // `name == COMPACTION_SUMMARY_NAME`, so consumers that filter by role
        // must check `name` instead.
        let summary_msg = ChatMessage {
            role: MessageRole::User,
            content: summary.to_string(),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        };
        let summary_tokens = self.counter.count_message(
            &summary_msg,
            self.model_for_counting(),
            Some(&self.protocol_type),
        );
        self.messages_mut().insert(system_count, summary_msg);
        self.current_tokens += summary_tokens;

        tracing::debug!(
            removed = removed_count,
            inserted_tokens = summary_tokens,
            remaining_tokens = self.current_tokens,
            "Middle history replaced with compaction summary"
        );

        removed_count
    }

    /// Find the index of the last compaction summary message.
    ///
    /// Scans messages from the end, looking for any message with
    /// `name == COMPACTION_SUMMARY_NAME`. Returns `Some(index)` if found,
    /// `None` if no compaction has occurred in this session.
    ///
    /// Identification is by `name` only — the marker is a `User` message
    /// in memory (see [`Self::replace_middle_with_summary`]), not
    /// `Assistant`.
    ///
    /// Used at session close to determine the tail distillation start point:
    /// tail = `messages[last_compaction_index + 1 ..]`.
    pub fn last_compaction_index(&self) -> Option<usize> {
        self.messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, msg)| msg.name.as_deref() == Some(COMPACTION_SUMMARY_NAME))
            .map(|(i, _)| i)
    }

    // ── ADR-061: 8-level degradation compression ──────────────────────

    /// ADR-061 §19.1: select the 8-level degradation plan.
    ///
    /// `summary` is the LLM output (post `strip_metadata_blocks`); its token
    /// size is known here, so the projection is exact — the 8-level walk
    /// happens **after** the summary (§19.1 先摘要后 plan).
    ///
    /// Selection rule (§19.2):
    /// - Levels 1-7: the **first** level satisfying
    ///   `ratio >= min_ratio && projected <= budget` wins
    ///   (stop at the first sufficient one — more aggressive levels are
    ///   never tried).
    /// - Level 8: sole fallback, **exempt** from the ratio bar; only requires
    ///   `projected <= budget` (T > budget overflow lands here in one shot).
    ///
    /// `min_ratio` is the per-agent compression ratio threshold (default
    /// [`MIN_COMPRESSION_RATIO`] = 0.90, i.e. "compress until at most 10%
    /// remains", e.g. 200K → 20K).
    ///
    /// Errors with [`CompressError::UnrecoverableOverflow`] when level 8
    /// still cannot fit — the caller must not touch history (§19.5).
    pub(crate) fn plan_compression(
        &self,
        summary: &str,
        min_ratio: f64,
    ) -> std::result::Result<CompressionPlan, CompressError> {
        if self.current_tokens == 0 || self.max_tokens == 0 {
            return Err(CompressError::UnrecoverableOverflow {
                projected: self.current_tokens,
                budget: self.max_tokens,
            });
        }

        let msgs = self.messages.as_slice();
        let mut level8_projected = 0u64;
        for level in 1..=8u8 {
            let plan = self.build_level_plan(level, msgs, summary);
            let projected = plan.projected_tokens;
            if level < 8 {
                let ratio = 1.0 - (projected as f64 / self.current_tokens as f64);
                if ratio >= min_ratio && projected <= self.max_tokens {
                    return Ok(plan);
                }
            } else {
                level8_projected = projected;
                if projected <= self.max_tokens {
                    return Ok(plan);
                }
            }
        }

        Err(CompressError::UnrecoverableOverflow {
            projected: level8_projected,
            budget: self.max_tokens,
        })
    }

    /// Build the retention plan for a single level (pure function over the
    /// message snapshot; no mutation).
    fn build_level_plan(&self, level: u8, msgs: &[ChatMessage], summary: &str) -> CompressionPlan {
        let system_count = msgs
            .iter()
            .take_while(|m| matches!(m.role, MessageRole::System))
            .count();

        // Level 8: only system + latest real user message survive; all the
        // rest goes to the summary. "Current user message" = last non-marker
        // User message (§19.1 note — compaction markers are User-role too,
        // so they must be excluded here).
        if level == 8 {
            let last_user = msgs.iter().rposition(|m| {
                matches!(m.role, MessageRole::User)
                    && m.name.as_deref() != Some(COMPACTION_SUMMARY_NAME)
            });
            let mut retained: Vec<ChatMessage> = msgs[..system_count].to_vec();
            let mut user_kept = 0usize;
            if let Some(ui) = last_user {
                retained.push(msgs[ui].clone());
                user_kept = 1;
            }
            let retained_tokens = self.count_slice_tokens(&retained);
            return CompressionPlan {
                level,
                retained,
                original_tokens: self.current_tokens,
                retained_tokens,
                projected_tokens: retained_tokens + self.estimate_marker_tokens(summary),
                stats: RetentionStats {
                    user_messages: user_kept,
                    assistant_messages: 0,
                    tool_messages: 0,
                    user_desc: if user_kept > 0 {
                        "last 1".to_string()
                    } else {
                        "none".to_string()
                    },
                    assistant_desc: "none".to_string(),
                    tool_desc: "none".to_string(),
                },
            };
        }

        // Levels 1-7: user messages (incl. prior compaction markers) are
        // always preserved (§19.4); assistant/tool retention tightens per
        // the level table (ADR-061 §6.2).
        let (assistant_keep, tool_keep) = match level {
            1 => (None, ToolKeep::WithinLastAssistants(5)),
            2 => (None, ToolKeep::WithinLastAssistants(3)),
            3 => (None, ToolKeep::WithinLastAssistants(1)),
            4 => (Some(5), ToolKeep::WithinLastAssistants(1)),
            5 => (Some(5), ToolKeep::None),
            6 => (Some(3), ToolKeep::None),
            7 => (Some(1), ToolKeep::None),
            _ => unreachable!("level 8 handled above"),
        };

        let assistant_indices: Vec<usize> = (0..msgs.len())
            .filter(|&i| matches!(msgs[i].role, MessageRole::Assistant))
            .collect();
        // Index of the K-th assistant from the end; `None` when fewer than K
        // assistants exist → that dimension drops nothing.
        let kth_from_end = |k: usize| -> Option<usize> {
            (assistant_indices.len() >= k)
                .then(|| assistant_indices[assistant_indices.len() - k])
        };
        let assistant_threshold = assistant_keep.and_then(kth_from_end);
        let tool_threshold = match tool_keep {
            // ADR-061 §6.2: tools associated with the last K assistant
            // messages survive. A round is `Assistant{tool_calls} → Tool →
            // Assistant`, so the K-th assistant's own tool result precedes
            // it — threshold at the (K+1)-th assistant from the end to
            // include that window (K=1 keeps exactly the last round's tool).
            ToolKeep::WithinLastAssistants(k) => kth_from_end(k + 1),
            ToolKeep::None => Some(usize::MAX),
        };

        let mut retained: Vec<ChatMessage> = Vec::with_capacity(msgs.len());
        let mut user_kept = 0usize;
        let mut assistant_kept = 0usize;
        let mut tool_kept = 0usize;
        for (i, msg) in msgs.iter().enumerate() {
            let keep = match msg.role {
                MessageRole::System => true,
                MessageRole::User => {
                    user_kept += 1;
                    true
                }
                MessageRole::Assistant => {
                    let k = assistant_threshold.is_none_or(|t| i >= t);
                    if k {
                        assistant_kept += 1;
                    }
                    k
                }
                MessageRole::Tool => {
                    let k = tool_threshold.is_none_or(|t| i >= t);
                    if k {
                        tool_kept += 1;
                    }
                    k
                }
            };
            if keep {
                retained.push(msg.clone());
            }
        }

        let retained_tokens = self.count_slice_tokens(&retained);
        let stats = RetentionStats {
            user_messages: user_kept,
            assistant_messages: assistant_kept,
            tool_messages: tool_kept,
            user_desc: "all".to_string(),
            assistant_desc: match assistant_keep {
                Some(n) => format!("last {}", n.min(assistant_kept)),
                None => "all".to_string(),
            },
            tool_desc: match tool_keep {
                ToolKeep::WithinLastAssistants(k) => {
                    format!("within last {} assistants", k)
                }
                ToolKeep::None => "none".to_string(),
            },
        };
        CompressionPlan {
            level,
            retained,
            original_tokens: self.current_tokens,
            retained_tokens,
            projected_tokens: retained_tokens + self.estimate_marker_tokens(summary),
            stats,
        }
    }

    /// ADR-061 §19.2: apply the plan — rebuild history as
    /// `[system][summary marker][retained]`, with the level metadata block
    /// (§9) prepended to the summary inside the marker.
    ///
    /// The marker keeps `User` role + `name = "compaction_summary"` so the
    /// restorer / `last_compaction_index` contracts are unchanged, and no
    /// `Assistant → Assistant{tool_calls}` adjacency can appear (glm-5.2
    /// on Volcano Ark rejects that pattern).
    pub(crate) fn apply_compression(
        &mut self,
        plan: CompressionPlan,
        summary: &str,
        min_ratio: f64,
    ) -> std::result::Result<CompressionOutcome, CompressError> {
        let original_tokens = plan.original_tokens;
        let ratio = 1.0 - (plan.projected_tokens as f64 / original_tokens as f64);

        // Defensive re-check (plan-time validation already ran; this guards
        // against callers bypassing `plan_compression`). Level 8 is exempt
        // from the ratio bar (§19.2).
        if plan.projected_tokens > self.max_tokens
            || (plan.level < 8 && ratio < min_ratio)
        {
            return Err(CompressError::InsufficientCompression { projected_ratio: ratio });
        }

        let marker = ChatMessage {
            role: MessageRole::User,
            content: build_summary_marker(
                plan.level,
                original_tokens,
                plan.projected_tokens,
                ratio,
                &plan.stats,
                summary,
            ),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        };

        // Marker sits right after the leading system block (middle-replace
        // semantics preserved; restorer anchors replay after it).
        let split = plan
            .retained
            .iter()
            .take_while(|m| matches!(m.role, MessageRole::System))
            .count();
        let mut new_msgs = Vec::with_capacity(plan.retained.len() + 1);
        new_msgs.extend_from_slice(&plan.retained[..split]);
        new_msgs.push(marker);
        new_msgs.extend_from_slice(&plan.retained[split..]);

        let removed_messages = self.messages.len().saturating_sub(new_msgs.len());
        *self.messages_mut() = new_msgs;
        self.recalibrate_tokens();

        tracing::info!(
            level = plan.level,
            removed_messages,
            original_tokens,
            retained_tokens = plan.retained_tokens,
            new_tokens = self.current_tokens,
            ratio = ?ratio,
            "ADR-061: 8-level compression applied"
        );

        Ok(CompressionOutcome {
            level: plan.level,
            original_tokens,
            new_tokens: self.current_tokens,
            compression_ratio: ratio,
            removed_messages,
        })
    }

    /// Sum `count_message` over a slice (kept messages at plan time).
    fn count_slice_tokens(&self, msgs: &[ChatMessage]) -> u64 {
        msgs.iter()
            .map(|m| {
                self.counter
                    .count_message(m, self.model_for_counting(), Some(&self.protocol_type))
            })
            .sum()
    }

    /// Estimate the to-be-inserted summary marker's tokens: precise summary
    /// text count plus a fixed overhead for the level metadata block (§9).
    fn estimate_marker_tokens(&self, summary: &str) -> u64 {
        crate::token::count_text(summary, "") as u64 + 64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::compression_constants::MIN_COMPRESSION_RATIO;

    fn make_message(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            ..Default::default()
        }
    }

    /// Helper: make a Tool-role message with a tool_call_id and optional `name`.
    fn make_tool_message(content: &str, tool_call_id: &str, name: Option<&str>) -> ChatMessage {
        ChatMessage {
            role: MessageRole::Tool,
            content: content.to_string(),
            name: name.map(|s| s.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_append_and_count() {
        let mut hm = HistoryManager::new(1000);
        hm.append(make_message(MessageRole::User, "Hello world"));
        assert_eq!(hm.len(), 1);
        assert!(hm.token_count() > 0);
    }

    #[test]
    fn messages_arc_is_shallow_and_copy_on_write() {
        let mut hm = HistoryManager::new(100_000);
        hm.append(ChatMessage::user("hello".to_string()));
        hm.append(ChatMessage::assistant("hi".to_string()));

        // Snapshot A: shallow Arc clone — O(1), shares the live buffer.
        let snap_a = hm.messages_arc();
        assert_eq!(snap_a.len(), 2);

        // Mutate after the clone: COW clones once; the snapshot retains the
        // exact build-time history while the live history advances.
        hm.append(ChatMessage::user("second turn".to_string()));
        assert_eq!(snap_a.len(), 2, "snapshot must retain build-time history");
        assert_eq!(hm.messages().len(), 3, "live history advances");

        // Snapshot B taken after the mutation sees the new state.
        let snap_b = hm.messages_arc();
        assert_eq!(snap_b.len(), 3);

        // Multiple snapshots without intervening mutation share one buffer.
        let snap_c = hm.messages_arc();
        assert!(
            Arc::ptr_eq(&snap_b, &snap_c),
            "no mutation between clones => same underlying buffer"
        );
        assert!(
            !Arc::ptr_eq(&snap_a, &snap_b),
            "mutation between clones => distinct buffers"
        );
    }

    #[test]
    fn messages_arc_survives_rewind_truncate() {
        let mut hm = HistoryManager::new(100_000);
        for i in 0..5 {
            hm.append(ChatMessage::user(format!("m{i}")));
        }
        let snap = hm.messages_arc();
        assert_eq!(snap.len(), 5);

        // Debug rewind truncates live history; the snapshot keeps the
        // pre-rewind buffer (lazy getSection(iteration, "messages") still
        // returns the full conversation as of that build).
        hm.truncate_to(3);
        assert_eq!(snap.len(), 5, "snapshot keeps pre-rewind history");
        assert_eq!(hm.messages().len(), 3, "live history truncated");
    }

    #[test]
    fn test_fit_to_budget_lossless_drops_oldest_rounds() {
        // Tiny budget so 5 user messages will overflow 80%.
        // Tier3 char-based estimator gives ~content.len()/4 tokens per message;
        // we use long content to make accounting predictable.
        let mut hm = HistoryManager::new(100);
        hm.append(make_message(MessageRole::System, "Sys"));
        for i in 0..5 {
            // ~50 chars each → ~12 tokens × 5 = ~60 tokens of user content,
            // plus assistants → easily over 80 (= 80% of 100).
            hm.append(make_message(
                MessageRole::User,
                &format!("user msg number {i} with some padding text"),
            ));
            hm.append(make_message(
                MessageRole::Assistant,
                &format!("assistant reply number {i} with padding"),
            ));
        }
        assert!(hm.token_count() > 80, "precondition: should overflow 80% of 100");

        let dropped = hm.fit_to_budget_lossless();
        assert!(dropped > 0, "should have dropped at least one round");
        // System always preserved.
        assert!(matches!(hm.messages()[0].role, MessageRole::System));
        // At least one trailing round must remain.
        assert!(
            hm.messages().iter().any(|m| matches!(m.role, MessageRole::User)),
            "at least one User message must survive"
        );
        // Final budget should be ≤ 80% of max.
        assert!(
            hm.token_count() <= 80,
            "after trim, current_tokens ({}) must be ≤ 80",
            hm.token_count()
        );
    }

    #[test]
    fn test_fit_to_budget_lossless_preserves_compaction_marker() {
        let mut hm = HistoryManager::new(100);
        hm.append(make_message(MessageRole::System, "Sys"));
        hm.append(ChatMessage {
            role: MessageRole::User,
            content: "summary of earlier conversation that we want to keep".to_string(),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        });
        for i in 0..6 {
            hm.append(make_message(
                MessageRole::User,
                &format!("user message {i} with some additional text padding"),
            ));
            hm.append(make_message(
                MessageRole::Assistant,
                &format!("assistant reply {i} with extra padding"),
            ));
        }

        let _ = hm.fit_to_budget_lossless();

        // Compaction marker must still be present (in addition to System).
        let has_marker = hm
            .messages()
            .iter()
            .any(|m| m.name.as_deref() == Some("compaction_summary"));
        assert!(has_marker, "compaction marker must survive lossless trim");
        // System must still be at index 0.
        assert!(matches!(hm.messages()[0].role, MessageRole::System));
    }

    #[test]
    fn test_fit_to_budget_lossless_noop_when_under_budget() {
        let mut hm = HistoryManager::new(10000);
        hm.append(make_message(MessageRole::System, "Sys"));
        hm.append(make_message(MessageRole::User, "hi"));
        hm.append(make_message(MessageRole::Assistant, "hello"));
        let before = hm.len();
        let dropped = hm.fit_to_budget_lossless();
        assert_eq!(dropped, 0);
        assert_eq!(hm.len(), before);
    }

    #[test]
    fn test_load_restored_replaces_and_recounts() {
        let mut hm = HistoryManager::new(10000);
        hm.append(make_message(MessageRole::User, "old data"));
        let before_tokens = hm.token_count();
        assert!(before_tokens > 0);

        let new_msgs = vec![
            make_message(MessageRole::System, "Sys"),
            make_message(MessageRole::User, "fresh"),
            make_message(MessageRole::Assistant, "fresh reply"),
        ];
        hm.load_restored(new_msgs);
        assert_eq!(hm.len(), 3);
        assert!(matches!(hm.messages()[0].role, MessageRole::System));
        // Token count must be recomputed (not stale "old data" + new).
        let recomputed = hm.token_count();
        assert!(recomputed > 0);
    }

    // ── abandon_tool_result / retrieve_tool_result tests (ADR-052) ────

    #[test]
    fn test_abandon_tool_result_replaces_content() {
        let mut hm = HistoryManager::new(100_000);
        let big = "x".repeat(5000);
        hm.append(make_tool_message(&big, "toolu_abc", Some("content_search")));

        let n = hm.abandon_tool_result("toolu_abc");
        assert_eq!(n, 1);
        assert!(
            hm.messages()[0].content.starts_with("[Tool result compressed."),
            "content should be replaced with placeholder"
        );
        assert!(
            hm.messages()[0].content.contains("context_retrieve(id=\"toolu_abc\")"),
            "placeholder should contain context_retrieve and the tool_call_id"
        );
    }

    #[test]
    fn test_abandon_tool_result_not_found_returns_zero() {
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_tool_message("some content", "toolu_a", Some("shell")));

        let n = hm.abandon_tool_result("toolu_nonexistent");
        assert_eq!(n, 0);
        assert_eq!(hm.messages()[0].content, "some content");
    }

    #[test]
    fn test_abandon_tool_result_idempotent() {
        let mut hm = HistoryManager::new(100_000);
        let big = "x".repeat(5000);
        hm.append(make_tool_message(&big, "toolu_idem", Some("file_read")));

        let n1 = hm.abandon_tool_result("toolu_idem");
        assert_eq!(n1, 1);
        let n2 = hm.abandon_tool_result("toolu_idem");
        assert_eq!(n2, 0, "already-compressed message should not be re-processed");
    }

    #[test]
    fn test_abandon_preserves_name_and_tool_call_id() {
        let mut hm = HistoryManager::new(100_000);
        let big = "y".repeat(5000);
        hm.append(make_tool_message(&big, "toolu_q", Some("content_search")));

        hm.abandon_tool_result("toolu_q");
        assert_eq!(
            hm.messages()[0].name.as_deref(),
            Some("content_search"),
            "name field must be preserved"
        );
        assert_eq!(
            hm.messages()[0].tool_call_id.as_deref(),
            Some("toolu_q"),
            "tool_call_id must be preserved"
        );
    }

    #[test]
    fn test_abandon_skips_non_tool_roles() {
        let mut hm = HistoryManager::new(100_000);
        let big_user = "u".repeat(5000);
        let big_tool = "t".repeat(5000);
        hm.append(make_message(MessageRole::User, &big_user));
        hm.append(make_tool_message(&big_tool, "toolu_w", Some("shell")));

        let n = hm.abandon_tool_result("toolu_w");
        assert_eq!(n, 1, "only the Tool message should be compressed");
        // Non-Tool message must be byte-identical
        assert_eq!(hm.messages()[0].content, big_user);
    }

    #[test]
    fn test_retrieve_tool_result_restores_content() {
        let mut hm = HistoryManager::new(100_000);
        let original = "This is the original content".repeat(100);
        hm.append(make_tool_message(&original, "toolu_abc", Some("file_read")));

        // Abandon first
        hm.abandon_tool_result("toolu_abc");
        assert!(hm.messages()[0].content.starts_with("[Tool result compressed."));

        // Retrieve
        let n = hm.retrieve_tool_result("toolu_abc", &original);
        assert_eq!(n, 1);
        assert_eq!(hm.messages()[0].content, original);
    }

    #[test]
    fn test_retrieve_tool_result_not_found_returns_zero() {
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_tool_message("content", "toolu_a", Some("shell")));

        let n = hm.retrieve_tool_result("toolu_nonexistent", "some content");
        assert_eq!(n, 0);
    }

    #[test]
    fn test_retrieve_tool_result_idempotent() {
        let mut hm = HistoryManager::new(100_000);
        let original = "Original content here".repeat(50);
        hm.append(make_tool_message(&original, "toolu_r", Some("file_read")));

        // Abandon then retrieve
        hm.abandon_tool_result("toolu_r");
        let n1 = hm.retrieve_tool_result("toolu_r", &original);
        assert_eq!(n1, 1);

        // Second retrieve should be a no-op (already raw)
        let n2 = hm.retrieve_tool_result("toolu_r", &original);
        assert_eq!(n2, 0, "already-restored message should not be re-processed");
    }

    #[test]
    fn test_retrieve_preserves_name_and_tool_call_id() {
        // ADR-052 §3.3.4 + §3.2.3: retrieve is symmetric to abandon — both
        // mutate `content` in-place but preserve `name` and `tool_call_id`
        // to maintain tool_use ↔ tool_result protocol pairing.
        let mut hm = HistoryManager::new(100_000);
        let big = "z".repeat(5000);
        hm.append(make_tool_message(&big, "toolu_preserve", Some("file_read")));

        // Abandon then retrieve
        hm.abandon_tool_result("toolu_preserve");
        assert!(hm.messages()[0].content.starts_with("[Tool result compressed."));
        hm.retrieve_tool_result("toolu_preserve", &big);

        assert_eq!(
            hm.messages()[0].name.as_deref(),
            Some("file_read"),
            "name field must be preserved through retrieve"
        );
        assert_eq!(
            hm.messages()[0].tool_call_id.as_deref(),
            Some("toolu_preserve"),
            "tool_call_id must be preserved through retrieve"
        );
    }

    #[test]
    fn test_abandon_retrieve_cycle() {
        let mut hm = HistoryManager::new(100_000);
        let original = "Cycle test content".repeat(50);
        hm.append(make_tool_message(&original, "toolu_cycle", Some("shell")));

        // Abandon
        assert_eq!(hm.abandon_tool_result("toolu_cycle"), 1);
        assert!(hm.messages()[0].content.starts_with("[Tool result compressed."));

        // Retrieve
        assert_eq!(hm.retrieve_tool_result("toolu_cycle", &original), 1);
        assert_eq!(hm.messages()[0].content, original);

        // Re-abandon (close the loop)
        assert_eq!(hm.abandon_tool_result("toolu_cycle"), 1);
        assert!(hm.messages()[0].content.starts_with("[Tool result compressed."));
    }

    // ── sanitize_messages tests ─────────────────────────────────────────

    use acowork_core::providers::traits::{FunctionCall, ToolCall};

    fn make_tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn make_tool_result(tool_call_id: &str, content: &str) -> ChatMessage {
        ChatMessage::tool(tool_call_id, content)
    }

    #[test]
    fn test_sanitize_fixes_invalid_arguments() {
        let mut messages = vec![
            ChatMessage::assistant_with_tools(
                "",
                vec![
                    make_tool_call("tc_1", "read_file", "not valid json{{"),
                    make_tool_call("tc_2", "write_file", r#"{"path":"/tmp"}"#),
                ],
            ),
            make_tool_result("tc_1", "result 1"),
            make_tool_result("tc_2", "result 2"),
        ];

        HistoryManager::sanitize_messages(&mut messages);

        let assistant = &messages[0];
        let tool_calls = assistant.tool_calls.as_ref().unwrap();
        // Invalid arguments should be fixed to `{}`
        assert_eq!(tool_calls[0].function.arguments, "{}");
        // Valid arguments should be unchanged
        assert_eq!(tool_calls[1].function.arguments, r#"{"path":"/tmp"}"#);
    }

    #[test]
    fn test_sanitize_removes_orphaned_tool_result() {
        let mut messages = vec![
            ChatMessage::assistant_with_tools(
                "I'll help you",
                vec![make_tool_call("tc_1", "read_file", "{}")],
            ),
            make_tool_result("tc_1", "result 1"),
            make_tool_result("tc_orphan", "orphaned result"),
        ];

        HistoryManager::sanitize_messages(&mut messages);

        // Only tc_1's result should remain
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].tool_call_id, Some("tc_1".to_string()));
    }

    #[test]
    fn test_sanitize_removes_orphaned_tool_call() {
        let mut messages = vec![
            ChatMessage::assistant_with_tools(
                "",
                vec![
                    make_tool_call("tc_1", "read_file", "{}"),
                    make_tool_call("tc_2", "write_file", "{}"),
                ],
            ),
            make_tool_result("tc_1", "result 1"),
            // tc_2 has no result
        ];

        HistoryManager::sanitize_messages(&mut messages);

        let assistant = &messages[0];
        let tool_calls = assistant.tool_calls.as_ref().unwrap();
        // Only tc_1 should remain
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "tc_1");
    }

    #[test]
    fn test_sanitize_removes_empty_assistant_message() {
        let mut messages = vec![
            make_message(MessageRole::User, "Hello"),
            ChatMessage::assistant(""),
            make_message(MessageRole::User, "World"),
        ];

        HistoryManager::sanitize_messages(&mut messages);

        // Empty assistant message should be removed
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[1].role, MessageRole::User);
    }

    #[test]
    fn test_sanitize_preserves_order() {
        let mut messages = vec![
            make_message(MessageRole::System, "System"),
            make_message(MessageRole::User, "Hello"),
            ChatMessage::assistant_with_tools(
                "Let me check",
                vec![make_tool_call("tc_1", "search", "{}")],
            ),
            make_tool_result("tc_1", "Found it"),
            make_message(MessageRole::Assistant, "Here's the answer"),
        ];

        HistoryManager::sanitize_messages(&mut messages);

        // All messages should be preserved in order
        assert_eq!(messages.len(), 5);
        assert!(matches!(messages[0].role, MessageRole::System));
        assert!(matches!(messages[1].role, MessageRole::User));
        assert!(matches!(messages[2].role, MessageRole::Assistant));
        assert!(matches!(messages[3].role, MessageRole::Tool));
        assert!(matches!(messages[4].role, MessageRole::Assistant));
    }

    #[test]
    fn test_sanitize_is_idempotent() {
        let mut messages = vec![
            ChatMessage::assistant_with_tools(
                "",
                vec![make_tool_call("tc_1", "read_file", "not json")],
            ),
            make_tool_result("tc_1", "result 1"),
        ];

        HistoryManager::sanitize_messages(&mut messages);
        let first_result = messages.clone();

        HistoryManager::sanitize_messages(&mut messages);

        // Second call should produce same result
        assert_eq!(messages.len(), first_result.len());
        for (a, b) in messages.iter().zip(first_result.iter()) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.content, b.content);
        }
    }

    #[test]
    fn test_sanitize_clears_tool_calls_when_all_orphaned() {
        let mut messages = vec![ChatMessage::assistant_with_tools(
            "Let me check",
            vec![
                make_tool_call("tc_1", "search", "{}"),
                make_tool_call("tc_2", "read", "{}"),
            ],
        )];
        // No tool results at all — both tool_calls should be removed

        HistoryManager::sanitize_messages(&mut messages);

        let assistant = &messages[0];
        // tool_calls should be cleared to None since all were orphaned
        assert!(assistant.tool_calls.is_none());
        // Content should be preserved since it's non-empty
        assert_eq!(assistant.content, "Let me check");
    }

    // ── replace_middle_with_summary tests ────────────────────────────────

    #[test]
    fn test_replace_middle_keeps_complete_tool_call_rounds() {
        // Scenario: 4 user messages, each followed by Assistant tc + Tool result.
        // With keep_last_rounds=3, Q4 should be complete, Q1 should be compacted.
        // The core fix ensures any Tool message kept in tail has its matching
        // Assistant preserved (no orphaned tool results that sanitize would remove).
        let mut hm = HistoryManager::new(100000);
        hm.append(make_message(MessageRole::System, "System prompt"));

        // Q1
        hm.append(make_message(MessageRole::User, "Question 1"));
        hm.append(ChatMessage::assistant_with_tools(
            "Searching",
            vec![make_tool_call("tc_1", "search", "{}")],
        ));
        hm.append(make_tool_result("tc_1", "Result for Q1"));
        hm.append(make_message(MessageRole::Assistant, "Answer 1"));

        // Q2
        hm.append(make_message(MessageRole::User, "Question 2"));
        hm.append(ChatMessage::assistant_with_tools(
            "Searching again",
            vec![make_tool_call("tc_2", "search", "{}")],
        ));
        hm.append(make_tool_result("tc_2", "Result for Q2"));
        hm.append(make_message(MessageRole::Assistant, "Answer 2"));

        // Q3
        hm.append(make_message(MessageRole::User, "Question 3"));
        hm.append(ChatMessage::assistant_with_tools(
            "Searching third",
            vec![make_tool_call("tc_3", "search", "{}")],
        ));
        hm.append(make_tool_result("tc_3", "Result for Q3"));
        hm.append(make_message(MessageRole::Assistant, "Answer 3"));

        // Q4
        hm.append(make_message(MessageRole::User, "Question 4"));
        hm.append(ChatMessage::assistant_with_tools(
            "Searching fourth",
            vec![make_tool_call("tc_4", "search", "{}")],
        ));
        hm.append(make_tool_result("tc_4", "Result for Q4"));
        hm.append(make_message(MessageRole::Assistant, "Answer 4"));

        let removed = hm.replace_middle_with_summary("Summary Q1", 3);
        assert!(removed > 0, "Should compact some messages");

        let messages = hm.messages();

        // Q1 (tc_1) should be compacted
        let has_tc1 = messages.iter().any(|m| {
            m.tool_calls
                .as_ref()
                .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == "tc_1"))
        });
        assert!(!has_tc1, "Q1 should be compacted");

        // Q4 must be complete (User + Assistant tc + Tool result)
        let has_tc4_call = messages.iter().any(|m| {
            m.tool_calls
                .as_ref()
                .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == "tc_4"))
        });
        assert!(has_tc4_call, "Q4 tool_call should be preserved");
        let has_tc4_result = messages
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("tc_4"));
        assert!(has_tc4_result, "Q4 tool result should be preserved");

        // Key assertion: sanitize should NOT remove any messages from the tail.
        // Before the fix, orphaned Tool results (preserved without their
        // Assistant) would be cleaned up here.
        let mut messages_clone = messages.to_vec();
        let len_before = messages_clone.len();
        HistoryManager::sanitize_messages(&mut messages_clone);
        assert_eq!(messages_clone.len(), len_before, "No orphans after fix");

        // All Tool messages still present after sanitize must have matching
        // Assistant with tool_calls.
        for msg in &messages_clone {
            if msg.role == MessageRole::Tool
                && let Some(ref tcid) = msg.tool_call_id {
                    let has_call = messages_clone.iter().any(|m| {
                        m.tool_calls
                            .as_ref()
                            .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == *tcid))
                    });
                    assert!(has_call, "Tool result {tcid} has matching Assistant");
                }
        }
    }

    #[test]
    fn test_replace_middle_single_user_many_tools() {
        // Scenario: 1 user message followed by many tool-calling rounds.
        // With keep_last_rounds=2, tail should keep the last 2 complete
        // Assistant+Tool pairs (expanded from idx).
        let mut hm = HistoryManager::new(100000);
        hm.append(make_message(MessageRole::System, "System"));
        hm.append(make_message(MessageRole::User, "Complex task"));

        // 5 rounds of tool calls
        for i in 1..=5 {
            hm.append(ChatMessage::assistant_with_tools(
                format!("Round {i}"),
                vec![make_tool_call(&format!("tc_{i}"), "tool", "{}")],
            ));
            hm.append(make_tool_result(&format!("tc_{i}"), &format!("Result {i}")));
        }

        let removed = hm.replace_middle_with_summary("Summary of rounds 1-3", 2);
        assert!(removed > 0);

        let messages = hm.messages();

        // Should have: [System] [User compaction_summary] [Assistant Round 4] [Tool tc_4]
        //              [Assistant Round 5] [Tool tc_5]

        // Verify compaction summary exists (User role, marker by name)
        let has_summary = messages.iter().any(|m| {
            m.role == MessageRole::User && m.name.as_deref() == Some(COMPACTION_SUMMARY_NAME)
        });
        assert!(has_summary, "Compaction summary should be present");

        // Verify rounds 4 and 5 are complete (no orphans)
        for i in 4..=5 {
            let tc_id = format!("tc_{i}");
            let has_call = messages.iter().any(|m| {
                m.tool_calls
                    .as_ref()
                    .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == tc_id))
            });
            assert!(has_call, "Tool call {tc_id} should be preserved");

            let has_result = messages
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some(&tc_id));
            assert!(has_result, "Tool result {tc_id} should be preserved");
        }

        // Verify rounds 1-3 are NOT present (compacted)
        for i in 1..=3 {
            let tc_id = format!("tc_{i}");
            let has_call = messages.iter().any(|m| {
                m.tool_calls
                    .as_ref()
                    .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == tc_id))
            });
            assert!(!has_call, "Tool call {tc_id} should be compacted");
        }

        // sanitize should not remove anything
        let mut messages_clone = messages.to_vec();
        let len_before = messages_clone.len();
        HistoryManager::sanitize_messages(&mut messages_clone);
        assert_eq!(messages_clone.len(), len_before, "No orphans after fix");
    }

    // ─────────────────────────────────────────────────────────────────────
    // compact_via_llm language-aware system-prompt tests
    //
    // These verify that the user's identity_context (containing the
    // `Language: zh-CN` field) is embedded into the system message sent to
    // the compact model, so the LLM writes the summary in the user's
    // preferred language.
    // ─────────────────────────────────────────────────────────────────────

    use acowork_core::providers::traits::ChatResponse;
    use std::sync::{Arc, Mutex};

    /// Minimal Provider that captures the most recent `ChatRequest` and
    /// returns a canned summary. Used to assert what `compact_via_llm`
    /// actually sends to the LLM.
    struct CaptureProvider {
        captured: Arc<Mutex<Option<ChatRequest>>>,
        canned: String,
    }

    impl CaptureProvider {
        fn new(canned: impl Into<String>) -> Self {
            Self {
                captured: Arc::new(Mutex::new(None)),
                canned: canned.into(),
            }
        }
        fn last_request(&self) -> ChatRequest {
            self.captured
                .lock()
                .unwrap()
                .take()
                .expect("provider was never called")
        }
    }

    #[async_trait::async_trait]
    impl Provider for CaptureProvider {
        fn name(&self) -> &str {
            "capture"
        }

        async fn chat(
            &self,
            request: ChatRequest,
        ) -> acowork_core::error::Result<ChatResponse> {
            *self.captured.lock().unwrap() = Some(request);
            Ok(ChatResponse {
                content: self.canned.clone(),
                ..Default::default()
            })
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> acowork_core::error::Result<
            Box<dyn futures_core::Stream<Item = acowork_core::providers::traits::StreamEvent> + Send>,
        > {
            Err(acowork_core::error::AcoworkError::Provider(
                acowork_core::providers::traits::ProviderError::unknown(
                    "CaptureProvider does not support streaming".to_string(),
                ),
            ))
        }

        async fn chat_token_count(
            &self,
            _messages: &[ChatMessage],
        ) -> acowork_core::error::Result<u64> {
            Ok(0)
        }
    }

    fn build_history_with_messages() -> HistoryManager {
        let mut hm = HistoryManager::new(10_000);
        hm.append(make_message(MessageRole::User, "用户：你好"));
        hm.append(make_message(
            MessageRole::Assistant,
            "你好！有什么可以帮你的吗？",
        ));
        hm
    }

    #[tokio::test]
    async fn compact_via_llm_without_identity_keeps_system_prompt_unchanged() {
        let hm = build_history_with_messages();
        // Stub output must pass the compact_with_llm quality gate (≥20 chars).
        let provider = CaptureProvider::new("<summary>a valid compact model summary output here</summary>");

        let result = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                None,
            )
            .await;
        assert!(result.is_ok(), "compact_via_llm should succeed");

        let req = provider.last_request();
        // Two messages: system + user
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, MessageRole::System);
        assert_eq!(
            req.messages[0].content,
            crate::prompt::COMPACTION_SYSTEM_PROMPT,
            "with identity=None, system prompt must be the base prompt unchanged"
        );
        // User message keeps the unmodified COMPACT_PROMPT template — body
        // only, no summarization instructions (those live in
        // COMPACTION_SYSTEM_PROMPT now per the prompt.rs architecture note).
        assert!(req.messages[1].content.contains("<conversation>"));
        assert!(req.messages[1].content.contains("</conversation>"));
    }

    #[tokio::test]
    async fn compact_via_llm_with_identity_embeds_language_directive_into_system() {
        let hm = build_history_with_messages();
        // Stub output must pass the compact_with_llm quality gate (≥20 chars).
        let provider = CaptureProvider::new("<summary>a valid compact model summary output here</summary>");

        let identity =
            "- Display Name: 大鱼\n- Language: zh-CN\n- Timezone: Asia/Shanghai\n- City: 上海";

        let result = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                Some(identity),
            )
            .await;
        assert!(result.is_ok());

        let req = provider.last_request();
        assert_eq!(req.messages.len(), 2);
        let system = &req.messages[0].content;
        assert_eq!(system[0..crate::prompt::COMPACTION_SYSTEM_PROMPT.len()].to_string(), crate::prompt::COMPACTION_SYSTEM_PROMPT,
            "system prompt must start with the original base prompt");
        // Identity text embedded verbatim — the LLM reads it directly
        assert!(system.contains(identity), "identity text must be embedded verbatim");
        assert!(system.contains("Language"), "language directive must be present");
        assert!(system.contains("preferred language"), "language directive must be present");
        // COMPACT_PROMPT (user) is now body-only — summarization role lives
        // in COMPACTION_SYSTEM_PROMPT (system). Assert the split:
        // - system carries the summarization role/instructions
        // - user stays focused on the conversation body, free of role leakage
        assert!(req.messages[0].content.contains("summarizes conversations"),
            "system prompt must carry the summarization role/instructions");
        assert!(!req.messages[1].content.contains("summarizes conversations"),
            "user message must stay focused on conversation body, not leak system role instructions");
    }

    #[tokio::test]
    async fn compact_via_llm_with_empty_identity_keeps_system_prompt_unchanged() {
        let hm = build_history_with_messages();
        // Stub output must carry a valid <summary> block — the quality gate in
        // compact_with_llm rejects marker-less output (no raw-text fallback).
        let provider = CaptureProvider::new("<summary>a valid compact model summary here</summary>");

        let result = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                Some("   \n\t  "),
            )
            .await;
        assert!(result.is_ok());

        let req = provider.last_request();
        assert_eq!(
            req.messages[0].content,
            crate::prompt::COMPACTION_SYSTEM_PROMPT,
            "whitespace-only identity must not append the directive"
        );
    }

    // ── Quality gate rejection paths (P1: quality-over-nothing) ───────────

    #[tokio::test]
    async fn compact_via_llm_rejects_too_short_summary() {
        let hm = build_history_with_messages();
        // A placeholder summary (< MIN_SUMMARY_CHARS) must fail the quality
        // gate — the output is discarded, never stored.
        let provider = CaptureProvider::new("<summary>ok</summary>");
        let err = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeError::Summary(crate::episode_distill::SummaryError::LowQuality(_))
            ),
            "placeholder summary must fail the quality gate, got: {err}"
        );
    }

    #[tokio::test]
    async fn compact_via_llm_rejects_markerless_reasoning_dump() {
        let hm = build_history_with_messages();
        // Reasoning-model dump without the <summary> marker — the exact
        // pollution shape from the pasted-text incident. Must error, never
        // fall back to the raw text as a summary.
        let provider = CaptureProvider::new(
            "确认 disable 路径已清理 pending 槽位。\n开始实施。改两处：\n1. 删除 resolve_distill_model 调用\n2. 验证 fallback 链",
        );
        let err = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeError::Summary(crate::episode_distill::SummaryError::MissingBlock)
            ),
            "marker-less dump must fail the quality gate, got: {err}"
        );
    }

    #[tokio::test]
    async fn compact_via_llm_rejects_empty_content() {
        let hm = build_history_with_messages();
        // Empty content (model ignored thinking_mode=disabled and put its
        // output in reasoning_content) must surface as SummaryError::Empty.
        let provider = CaptureProvider::new("");
        let err = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeError::Summary(crate::episode_distill::SummaryError::Empty(_))
            ),
            "empty content must surface as SummaryError::Empty, got: {err}"
        );
    }

    // ── ADR-061: 8-level compression plan/apply tests ─────────────────────

    /// Build 7 full rounds: System + (User → Assistant{tool_calls} → Tool →
    /// Assistant) × 7. Tool results carry extra text so level 1's tool drops
    /// clear a 10% ratio bar (tests that want a weak level selected pass
    /// `0.10` explicitly — the default bar is 0.90).
    fn build_7_round_history() -> HistoryManager {
        let mut hm = HistoryManager::new(1_000_000);
        hm.append(make_message(MessageRole::System, "System prompt"));
        for i in 1..=7 {
            hm.append(make_message(MessageRole::User, &format!("Question {i}")));
            hm.append(ChatMessage::assistant_with_tools(
                format!("Searching {i}"),
                vec![make_tool_call(&format!("tc_{i}"), "search", "{}")],
            ));
            hm.append(make_tool_result(
                &format!("tc_{i}"),
                &format!(
                    "Tool result for round {i} with a long payload so this \
                     message carries real token weight in every level projection"
                ),
            ));
            hm.append(make_message(MessageRole::Assistant, &format!("Answer {i}")));
        }
        hm
    }

    fn user_count(msgs: &[ChatMessage]) -> usize {
        msgs.iter()
            .filter(|m| matches!(m.role, MessageRole::User))
            .count()
    }

    fn has_tool_call(msgs: &[ChatMessage], id: &str) -> bool {
        msgs.iter().any(|m| {
            m.tool_calls
                .as_ref()
                .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == id))
        })
    }

    fn has_tool_result(msgs: &[ChatMessage], id: &str) -> bool {
        msgs.iter().any(|m| m.tool_call_id.as_deref() == Some(id))
    }

    #[test]
    fn test_plan_level1_keeps_users_all_and_tools_of_last_5_assistants() {
        let hm = build_7_round_history();
        // Explicit 10% bar: this fixture's total is small (~1K tokens), so
        // level 1 only clears a weak bar — under the default 0.90 bar it
        // would be skipped (see test_plan_default_ratio_skips_weak_levels).
        let plan = hm.plan_compression("Summary", 0.10).expect("level 1 must fit");
        assert_eq!(plan.level, 1, "with a huge budget level 1 is selected first");

        // Level 1: every User and Assistant survives; tool window = the last
        // 5 assistants → rounds 5-7 keep their tool results, rounds 1-4 drop
        // theirs (ADR-061 §6.2). Dropped Tool results leave orphaned
        // `tool_calls` declarations on kept assistants — sanitized at build
        // time (accepted deviation, §19.6).
        assert_eq!(user_count(&plan.retained), 7, "all user messages preserved");
        assert_eq!(
            plan.stats.assistant_messages, 14,
            "all assistants preserved at level 1"
        );
        for id in ["tc_5", "tc_6", "tc_7"] {
            assert!(has_tool_call(&plan.retained, id), "{id} call kept");
            assert!(has_tool_result(&plan.retained, id), "{id} result kept");
        }
        for id in ["tc_1", "tc_2", "tc_3", "tc_4"] {
            assert!(
                has_tool_call(&plan.retained, id),
                "{id} call kept (assistant survives at level 1)"
            );
            assert!(!has_tool_result(&plan.retained, id), "{id} result dropped");
        }
        assert!(plan.projected_tokens < plan.original_tokens);
    }

    #[test]
    fn test_plan_projections_decrease_monotonically_and_stop_at_first_fit() {
        let hm = build_7_round_history();
        let projections: Vec<u64> = (1..=8)
            .map(|l| hm.build_level_plan(l, hm.messages(), "Summary").projected_tokens)
            .collect();
        // Core 8-level invariant: more aggressive levels never retain more.
        for w in projections.windows(2) {
            assert!(
                w[0] >= w[1],
                "level projections must be monotonic non-increasing: {w:?}"
            );
        }
        assert!(projections[0] > projections[7], "level 8 must be strictly smaller");

        // Budget = level-5 projection, 10% bar: levels 1-4 fail the budget
        // check, level 5 is the first fit → selected (§19.1: stop, never try 6-8).
        let mut hm = hm;
        hm.set_max_tokens(projections[4]);
        let plan = hm.plan_compression("Summary", 0.10).expect("level 5 must fit");
        assert_eq!(plan.level, 5, "first sufficient level wins");
        // §19.2/D5: after apply the real count stays within budget (S known
        // at plan time + level 8 backstop).
        let outcome = hm
            .apply_compression(plan, "Summary", 0.10)
            .expect("apply must succeed");
        assert_eq!(outcome.level, 5);
        assert!(
            hm.token_count() <= hm.max_tokens,
            "post-compression count must fit budget"
        );
    }

    #[test]
    fn test_plan_level8_exempt_from_ratio_keeps_last_non_marker_user() {
        // Conversation dominated by the system block: levels 1-7 drop nothing
        // (ratio ~0% < 10%) yet level 8 must still be accepted on the budget
        // check alone (§19.2 ratio exemption).
        let mut hm = HistoryManager::new(1_000_000);
        hm.append(make_message(MessageRole::System, &"System prompt ".repeat(600)));
        hm.append(ChatMessage {
            role: MessageRole::User,
            content: "Previous compaction summary".to_string(),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        });
        hm.append(make_message(MessageRole::User, "Question 1"));
        hm.append(make_message(MessageRole::Assistant, "Answer 1"));
        hm.append(make_message(MessageRole::User, "Question 2"));

        let p8 = hm
            .build_level_plan(8, hm.messages(), "Summary")
            .projected_tokens;
        hm.set_max_tokens(p8);
        let plan = hm
            .plan_compression("Summary", MIN_COMPRESSION_RATIO)
            .expect("level 8 must fit");
        assert_eq!(plan.level, 8, "only level 8 fits; ratio exemption required");

        // Level 8 retains system + the LAST non-marker User (the earlier
        // marker is User-role too and must be excluded, §19.1).
        assert_eq!(user_count(&plan.retained), 1, "only the current user message");
        assert_eq!(
            plan.retained.last().map(|m| m.content.as_str()),
            Some("Question 2"),
            "last real user message survives, marker does not"
        );
        assert_eq!(plan.stats.assistant_messages, 0);
        assert_eq!(plan.stats.tool_messages, 0);
    }

    #[test]
    fn test_plan_unrecoverable_overflow_when_level8_exceeds_budget() {
        let mut hm = build_7_round_history();
        let p8 = hm.build_level_plan(8, hm.messages(), "Summary").projected_tokens;
        hm.set_max_tokens(p8 - 1);
        let err = hm
            .plan_compression("Summary", MIN_COMPRESSION_RATIO)
            .unwrap_err();
        assert!(
            matches!(err, CompressError::UnrecoverableOverflow { .. }),
            "level 8 cannot fit => explicit failure, history untouched: {err}"
        );
        assert_eq!(hm.messages().len(), 29, "planning must never mutate history");
    }

    #[test]
    fn test_apply_compression_marker_contract_and_user_preservation() {
        let mut hm = build_7_round_history();
        let plan = hm
            .plan_compression("A solid summary of everything so far", 0.10)
            .expect("plan");
        assert_eq!(plan.level, 1);
        let outcome = hm
            .apply_compression(plan, "A solid summary of everything so far", 0.10)
            .expect("apply");

        assert!(outcome.removed_messages > 0);
        assert!(outcome.new_tokens < outcome.original_tokens);
        assert!(outcome.compression_ratio >= 0.10);

        let msgs = hm.messages();
        let system_count = msgs
            .iter()
            .take_while(|m| matches!(m.role, MessageRole::System))
            .count();
        // Marker contract: User role + name=compaction_summary, right after
        // the leading system block (restorer anchors replay after it).
        let marker = &msgs[system_count];
        assert!(matches!(marker.role, MessageRole::User));
        assert_eq!(marker.name.as_deref(), Some(COMPACTION_SUMMARY_NAME));
        assert!(
            marker.content.starts_with("[compressed: level=1]"),
            "level metadata block (§9)"
        );
        assert!(marker.content.contains("user_messages: "), "retention stats block");
        assert!(marker.content.contains("tokens: "), "token delta line");
        assert!(marker.content.ends_with("A solid summary of everything so far"));

        // §13.2: levels 1-7 keep every original User message (7 + new marker).
        assert_eq!(user_count(msgs), 8, "all user messages preserved plus the new marker");
    }

    #[test]
    fn test_apply_rejects_insufficient_plan_without_touching_history() {
        let mut hm = build_7_round_history();
        let plan = hm.build_level_plan(1, hm.messages(), "Summary");
        let before = format!("{:?}", hm.messages());
        hm.set_max_tokens(1); // projected >> budget → defensive re-check fails
        let err = hm
            .apply_compression(plan, "Summary", MIN_COMPRESSION_RATIO)
            .unwrap_err();
        assert!(matches!(err, CompressError::InsufficientCompression { .. }));
        assert_eq!(
            format!("{:?}", hm.messages()),
            before,
            "history must be untouched on reject"
        );
    }

    #[test]
    fn test_plan_default_ratio_skips_weak_levels() {
        // ADR-061 §19.3: the default bar is 0.90 ("compress until at most
        // 10% remains"). The small 7-round fixture never removes 90% at
        // levels 1-7, so plan must fall through to level 8's ratio
        // exemption instead of stopping at a weak level.
        let hm = build_7_round_history();
        let plan = hm
            .plan_compression("Summary", MIN_COMPRESSION_RATIO)
            .expect("level 8 must still fit");
        assert_eq!(
            plan.level, 8,
            "under the default 0.90 bar every level 1-7 is skipped"
        );
    }

    #[test]
    fn test_build_summary_marker_metadata_format() {
        let stats = RetentionStats {
            user_messages: 7,
            assistant_messages: 5,
            tool_messages: 3,
            user_desc: "all".to_string(),
            assistant_desc: "last 5".to_string(),
            tool_desc: "within last 5 assistants".to_string(),
        };
        let marker = build_summary_marker(3, 1000, 400, 0.6, &stats, "body summary");
        assert!(marker.starts_with("[compressed: level=3]\n"));
        assert!(marker.contains("user_messages: all (7)"));
        assert!(marker.contains("assistant_messages: last 5 (5)"));
        assert!(marker.contains("tool_messages: within last 5 assistants (3)"));
        assert!(marker.contains("tokens: 1000 -> 400 (ratio 60.0%)"));
        assert!(marker.ends_with("body summary"));
    }
}
