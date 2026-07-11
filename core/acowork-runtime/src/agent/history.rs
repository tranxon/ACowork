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
//! decide what to discard. The remaining strategies (trim_fifo, emergency_trim) are
//! safety nets for when the LLM-based compaction itself cannot execute.

use std::collections::HashSet;

use acowork_core::protocol::ProtocolType;
use acowork_core::providers::traits::{ChatMessage, MessageRole, Provider};
#[cfg(test)]
use acowork_core::providers::traits::ChatRequest;

use crate::error::RuntimeError;
use crate::token::counter::TokenCounter;

// ── ADR-032 placeholder format constants ────────────────────────────────
//
// Shared between `HistoryManager::compress_tool_results` (producer) and
// `episode_distill::format_messages` (consumer at LLM-compaction time).
// Both producer and consumer MUST agree on the prefix string for the
// idempotency check; centralizing it here prevents the two from drifting.

/// Stable prefix for a compressed tool-result placeholder produced by
/// [`HistoryManager::compress_tool_results`]. Content with this prefix
/// is treated as "already compressed" by both:
/// - `compress_tool_results` idempotency check
/// - `format_messages` (LLM compaction prompt builder) for richer
///   structured labelling in the summary prompt
///
/// Compression placeholder prefix used by [`HistoryManager::compress_tool_results`].
///
/// When a tool result exceeds the soft threshold, its content is replaced with:
///   `[Tool result compressed. Call context_recall(id="{tool_call_id}") to retrieve the full content.]`
///
/// ## Format contract
///
/// The exact format string is a contract shared by **three** consumers:
///
/// 1. **`context_recall` built-in tool** — parses `tool_call_id` from the placeholder
///    to fetch the original content from the in-memory inverted index.
/// 2. **`format_messages` in episode_distill** — checks `starts_with` this prefix
///    to label compressed tool results correctly in compaction prompts.
/// 3. **LLM system prompt** — instructs the model to copy the `tool_call_id` from
///    the placeholder verbatim when calling `context_recall`.
///
/// **Any change** to this prefix or the placeholder format **must** update all three
/// consumers in lockstep.
///
/// ## Format invariant
///
/// The placeholder always includes the raw `tool_call_id` embedded inline
/// (e.g. `toolu_abc123`), so the LLM can copy-paste it without parsing.
/// Format: prefix + " Call context_recall(id=\"" + tool_call_id + "\") ..."
pub(crate) const COMPRESSED_TOOL_PLACEHOLDER_PREFIX: &str = "[Tool result compressed.";

/// Stable identifier string used by [`HistoryManager::replace_middle_with_summary`]
/// to mark the synthetic Assistant message that replaces the compacted middle.
/// Detected by `format_messages` to label these as "CompactionSummary"
/// (rather than "Assistant") in the summary prompt, so the LLM knows
/// it is reading a previous compaction output rather than a fresh turn.
pub(crate) const COMPACTION_SUMMARY_NAME: &str = "compaction_summary";

/// History manager for conversation
pub struct HistoryManager {
    /// Conversation messages
    messages: Vec<ChatMessage>,
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
            messages: Vec::new(),
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
        &self.messages
    }

    /// Get mutable reference to messages
    pub fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.messages
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
        self.messages.push(message);
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
        self.messages.extend(messages);
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
        self.messages = messages;
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
    /// - The single `Assistant{name="compaction_summary"}` marker, if present
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
            matches!(msg.role, MessageRole::Assistant)
                && msg.name.as_deref() == Some(COMPACTION_SUMMARY_NAME)
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
            self.messages.drain(first_removable..round_end);
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
        self.messages.clear();
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
        self.messages.truncate(target_len);
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

    /// Trim history using FIFO strategy — removes oldest non-system messages
    /// until total tokens are within budget.
    pub fn trim_fifo(&mut self) -> usize {
        if self.current_tokens <= self.max_tokens {
            return 0;
        }

        let mut removed = 0;
        // Never remove system messages; start from first user/assistant message
        let first_removable = self
            .messages
            .iter()
            .position(|m| !matches!(m.role, MessageRole::System))
            .unwrap_or(0);

        while self.current_tokens > self.max_tokens
            && first_removable + removed < self.messages.len() - 1
        {
            let idx = first_removable + removed;
            if idx < self.messages.len() {
                let tokens = self.counter.count_message(
                    &self.messages[idx],
                    self.model_for_counting(),
                    Some(&self.protocol_type),
                );
                self.current_tokens = self.current_tokens.saturating_sub(tokens);
                removed += 1;
            } else {
                break;
            }
        }

        if removed > 0 {
            // Actually remove the messages
            let end = first_removable + removed;
            self.messages
                .drain(first_removable..end.min(self.messages.len()));
            tracing::debug!(
                removed,
                remaining_tokens = self.current_tokens,
                "FIFO trimmed"
            );
        }

        removed
    }

    /// Emergency trim — drastic measure for context overflow recovery.
    /// Keeps only the last 4 non-system messages.
    ///
    /// Compaction markers (`name == "compaction_summary"`) are protected from
    /// removal because they are needed by [`last_compaction_index`] for tail
    /// distillation at session close. Without this protection, emergency trim
    /// could delete the only compaction marker and cause the session-close
    /// distillation to fall back to full-history summarization.
    pub fn emergency_trim(&mut self) -> usize {
        fn is_compaction_marker(msg: &ChatMessage) -> bool {
            msg.name.as_deref() == Some(COMPACTION_SUMMARY_NAME)
        }

        let system_count = self
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::System))
            .count();

        let compaction_count = self
            .messages
            .iter()
            .filter(|m| is_compaction_marker(m))
            .count();

        // Non-system, non-compaction messages
        let removable_count = self.messages.len() - system_count - compaction_count;
        if removable_count <= 4 {
            return 0;
        }

        let to_remove = removable_count - 4;
        let mut removed = 0;

        // Remove oldest removable messages, skipping system + compaction markers
        let mut i = 0;
        while removed < to_remove && i < self.messages.len() {
            if matches!(self.messages[i].role, MessageRole::System)
                || is_compaction_marker(&self.messages[i])
            {
                i += 1;
            } else {
                let tokens = self.counter.count_message(
                    &self.messages[i],
                    self.model_for_counting(),
                    Some(&self.protocol_type),
                );
                self.current_tokens = self.current_tokens.saturating_sub(tokens);
                self.messages.remove(i);
                removed += 1;
            }
        }

        tracing::warn!(removed, "Emergency trim performed");
        removed
    }

    /// ADR-032: Replace oversized tool result content with compact placeholders.
    ///
    /// Scope is intentional and permanent within this ADR: v1 only processes
    /// `MessageRole::Tool`. Other large messages (User/Assistant) are handled
    /// by L2 LLM summarization (history > 80%) and L3 emergency_trim (> 95%)
    /// in `loop_context.rs` — placeholder + recall is one compression tier,
    /// not a "cover all large messages" mechanism.
    ///
    /// ## Behavior
    ///
    /// - Iterates `self.messages` and replaces each `MessageRole::Tool` entry
    ///   whose `content.len() > soft_threshold_chars` with a fixed ~120-char
    ///   placeholder embedding the tool_call_id for later recall.
    /// - **Excludes the most recent `keep_recent_n` tool messages** from
    ///   compression (uniform N rule per ADR-032 core principle #7). Pass
    ///   `keep_recent_n = 0` to compress all eligible; pass
    ///   `keep_recent_n >= tool_count` for a no-op.
    /// - **Idempotent** via two self-describing checks (no persistence flags):
    ///   1. `content.len() <= soft_threshold_chars` → skip
    ///   2. `content.starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX)` → skip
    ///      (safety net against misconfigured threshold; placeholder prefix
    ///      is unique)
    /// - **Does NOT modify `name` field** — preserves tool_use.name ↔
    ///   tool_result.name protocol pairing.
    /// - **Does NOT modify `tool_call_id` field** — embedded in the
    ///   placeholder string, but the field stays intact for restorer.
    /// - **Pure in-memory**: does not touch JSONL.
    /// - **Does NOT update `current_tokens`**: caller MUST call
    ///   [`Self::recalibrate_tokens`] after this returns.
    ///
    /// ## Returns
    ///
    /// Number of messages whose content was replaced with a placeholder.
    ///
    /// ## Do NOT
    ///
    /// - Do NOT extend this to other roles without opening a new ADR
    ///   (planned as ADR-033) with proper id strategy for non-tool messages.
    /// - Do NOT write a `compressed: bool` field — persistence layer must
    ///   stay free of derived runtime state (see ADR-032 core principle #6).
    pub fn compress_tool_results(
        &mut self,
        soft_threshold_chars: usize,
        keep_recent_n: usize,
    ) -> usize {
        // Collect indices of Tool-role messages, in order. We exclude the last
        // `keep_recent_n` from compression; the rest are candidates.
        let tool_indices: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                if matches!(m.role, MessageRole::Tool) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();

        // Slice the candidate set: drop the trailing `keep_recent_n` entries.
        let candidates_end = tool_indices.len().saturating_sub(keep_recent_n);

        let mut compressed = 0usize;
        for &i in &tool_indices[..candidates_end] {
            // Idempotency check 1: skip messages already at or below threshold.
            // (After first pass, all Tool messages are ≤ ~120 chars placeholder,
            // so they naturally fall into this branch on subsequent invocations.)
            if self.messages[i].content.len() <= soft_threshold_chars {
                continue;
            }

            // Idempotency check 2: skip messages already compressed via the
            // placeholder prefix. Defends against misconfigured thresholds
            // (< 100 chars) and accidental double-processing.
            if self.messages[i]
                .content
                .starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX)
            {
                continue;
            }

            // Need tool_call_id to embed in the placeholder. If absent, the
            // message is malformed (should have been cleaned by sanitize_messages);
            // skip silently — no-op is safer than corrupting the message.
            let Some(tc_id) = self.messages[i].tool_call_id.as_deref() else {
                tracing::warn!(
                    history_index = i,
                    "Tool message missing tool_call_id; skipping compression"
                );
                continue;
            };

            // Replace content with placeholder. `name` field is intentionally
            // preserved to keep tool_use.name ↔ tool_result.name protocol pairing.
            self.messages[i].content = format!(
                "[Tool result compressed. Call context_recall(id=\"{}\") to retrieve the full content.]",
                tc_id
            );
            compressed += 1;
        }

        if compressed > 0 {
            tracing::debug!(
                compressed,
                soft_threshold_chars,
                keep_recent_n,
                candidates = candidates_end,
                "ADR-032: compressed oversized tool result messages into placeholders"
            );
        }
        compressed
    }

    /// ADR-032: Recompute `current_tokens` from scratch.
    ///
    /// Must be called after `compress_tool_results` (which mutates content in
    /// place but cannot update `current_tokens` under borrow rules).
    /// O(N) over messages with constant-time token estimation each.
    pub fn recalibrate_tokens(&mut self) {
        let model = self.model_for_counting().to_string();
        let pt = self.protocol_type.clone();
        let mut total = 0u64;
        for msg in &self.messages {
            total = total.saturating_add(self.counter.count_message(msg, &model, Some(&pt)));
        }
        self.current_tokens = total;
    }

    /// ADR-032: Return cloned copies of all `MessageRole::Tool` messages
    /// EXCEPT the most recent `n` (which are protected from compression to
    /// preserve LLM's current-reasoning input context).
    ///
    /// Walks newest-to-oldest, copies the first `tool_count - n` Tool
    /// messages into the result (in chronological order). Returns an empty
    /// Vec if `tool_count <= n`.
    ///
    /// Note: not used by the in-place `compress_tool_results` (which filters
    /// and mutates `self.messages` directly), but exposed for testing,
    /// debugging, and future read-only inspection needs.
    pub fn tool_results_excluding_recent(&self, n: usize) -> Vec<ChatMessage> {
        let mut collected = 0usize;
        let mut result = Vec::new();
        // Newest → oldest, skip the first `n` Tool messages we encounter.
        let mut to_skip = n;
        for m in self.messages.iter().rev() {
            if !matches!(m.role, MessageRole::Tool) {
                continue;
            }
            if to_skip > 0 {
                to_skip -= 1;
                continue;
            }
            result.push(m.clone());
            collected += 1;
        }
        // Reverse to restore chronological order (oldest first).
        result.reverse();
        tracing::trace!(
            collected,
            n,
            "ADR-032: tool_results_excluding_re Recent returned slice"
        );
        result
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
        let messages_text = crate::episode_distill::format_messages(&self.messages);
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
            2048,
            identity_context,
            system_prompt,
        )
        .await
    }

    /// Replace the middle section of history with a compaction summary.
    ///
    /// Keeps system messages at the start and the last `keep_last_rounds`
    /// conversational rounds at the end. The middle is replaced with a
    /// single Assistant message carrying `name: "compaction_summary"` as
    /// a compaction marker for [`last_compaction_index`].
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
        self.messages.drain(system_count..tail_start);

        // Insert compaction summary as Assistant message with marker
        let summary_msg = ChatMessage {
            role: MessageRole::Assistant,
            content: summary.to_string(),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        };
        let summary_tokens = self.counter.count_message(
            &summary_msg,
            self.model_for_counting(),
            Some(&self.protocol_type),
        );
        self.messages.insert(system_count, summary_msg);
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
    /// Scans messages from the end, looking for an Assistant message with
    /// `name == COMPACTION_SUMMARY_NAME`. Returns `Some(index)` if found,
    /// `None` if no compaction has occurred in this session.
    ///
    /// Used at session close to determine the tail distillation start point:
    /// tail = `messages[last_compaction_index + 1 ..]`.
    pub fn last_compaction_index(&self) -> Option<usize> {
        self.messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, msg)| {
                msg.role == MessageRole::Assistant
                    && msg.name.as_deref() == Some(COMPACTION_SUMMARY_NAME)
            })
            .map(|(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
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
    fn test_fifo_trim() {
        let mut hm = HistoryManager::new(50); // Very small budget
        hm.append(make_message(MessageRole::System, "System prompt"));
        for i in 0..10 {
            hm.append(make_message(
                MessageRole::User,
                &format!("Message {i} with some content to fill tokens"),
            ));
        }
        let removed = hm.trim_fifo();
        assert!(removed > 0);
        // System message should still be there
        assert!(
            hm.messages()
                .iter()
                .any(|m| matches!(m.role, MessageRole::System))
        );
    }

    #[test]
    fn test_emergency_trim() {
        let mut hm = HistoryManager::new(10000);
        hm.append(make_message(MessageRole::System, "System"));
        for i in 0..10 {
            hm.append(make_message(MessageRole::User, &format!("Msg {i}")));
        }
        let removed = hm.emergency_trim();
        assert_eq!(removed, 6); // 10 - 4 = 6
        assert_eq!(hm.len(), 5); // 1 system + 4 remaining
    }

    #[test]
    fn test_emergency_trim_protects_compaction_markers() {
        let mut hm = HistoryManager::new(10000);
        hm.append(make_message(MessageRole::System, "System"));
        // Insert a compaction marker (Assistant with name="compaction_summary")
        hm.append(ChatMessage {
            role: MessageRole::Assistant,
            content: "Compaction summary".to_string(),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        });
        for i in 0..10 {
            hm.append(make_message(MessageRole::User, &format!("Msg {i}")));
        }
        let removed = hm.emergency_trim();
        // Should remove 6 of the 10 user messages (keeps last 4),
        // but NOT the compaction marker
        assert_eq!(removed, 6);
        // Compaction marker should still be present
        let has_marker = hm
            .messages()
            .iter()
            .any(|m| m.name.as_deref() == Some(COMPACTION_SUMMARY_NAME));
        assert!(
            has_marker,
            "Compaction marker should survive emergency trim"
        );
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
            role: MessageRole::Assistant,
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

    // ── compress_tool_results tests (ADR-032) ─────────────────────────

    /// Helper: make a Tool-role message with a tool_call_id and optional `name`.
    fn make_tool_message(content: &str, tool_call_id: &str, name: Option<&str>) -> ChatMessage {
        ChatMessage {
            role: MessageRole::Tool,
            content: content.to_string(),
            tool_call_id: Some(tool_call_id.to_string()),
            name: name.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn test_compress_tool_results_below_threshold_no_op() {
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_tool_message("short result", "toolu_a", Some("content_search")));
        let n = hm.compress_tool_results(2048, 0);
        assert_eq!(n, 0, "messages below threshold must not be compressed");
        assert_eq!(hm.messages()[0].content, "short result");
    }

    #[test]
    fn test_compress_tool_results_above_threshold_replaced() {
        let mut hm = HistoryManager::new(100_000);
        let big = "x".repeat(5000); // 5KB, above 2KB threshold
        hm.append(make_tool_message(&big, "toolu_xyz", Some("content_search")));

        let n = hm.compress_tool_results(2048, 0);
        assert_eq!(n, 1);
        let new_content = &hm.messages()[0].content;
        assert!(
            new_content.contains("[Tool result compressed."),
            "content should be replaced with placeholder: got={}",
            &new_content[..new_content.len().min(80)]
        );
        assert!(new_content.contains("toolu_xyz"), "placeholder must embed tool_call_id");
        assert!(
            new_content.len() < big.len(),
            "placeholder must be smaller than original (got {} vs {})",
            new_content.len(),
            big.len()
        );
    }

    #[test]
    fn test_compress_tool_results_preserves_name_field() {
        // ADR-032 core invariant: `name` field must NOT be modified by the
        // compression function. It carries the original tool name for the
        // provider's tool_use.name ↔ tool_result.name protocol pairing.
        let mut hm = HistoryManager::new(100_000);
        let big = "y".repeat(5000);
        hm.append(make_tool_message(&big, "toolu_q", Some("content_search")));

        hm.compress_tool_results(2048, 0);
        assert_eq!(
            hm.messages()[0].name.as_deref(),
            Some("content_search"),
            "name field must be preserved verbatim"
        );
        // tool_call_id also preserved
        assert_eq!(
            hm.messages()[0].tool_call_id.as_deref(),
            Some("toolu_q"),
            "tool_call_id field must be preserved verbatim"
        );
    }

    #[test]
    fn test_compress_tool_results_skips_non_tool_roles() {
        let mut hm = HistoryManager::new(100_000);
        let big_user = "u".repeat(5000);
        let big_assistant = "a".repeat(5000);
        let big_system = "s".repeat(5000);
        let big_tool = "t".repeat(5000);
        hm.append(make_message(MessageRole::User, &big_user));
        hm.append(make_message(MessageRole::Assistant, &big_assistant));
        hm.append(make_message(MessageRole::System, &big_system));
        hm.append(make_tool_message(&big_tool, "toolu_w", Some("shell")));

        let n = hm.compress_tool_results(2048, 0);
        assert_eq!(n, 1, "only the Tool message should be compressed");

        // The non-Tool messages must be byte-identical
        assert_eq!(hm.messages()[0].content, big_user);
        assert_eq!(hm.messages()[1].content, big_assistant);
        assert_eq!(hm.messages()[2].content, big_system);
        // Tool was compressed
        assert!(hm.messages()[3].content.contains("[Tool result compressed."));
    }

    #[test]
    fn test_compress_tool_results_idempotent_by_content_length() {
        // After first compression, the placeholder is ~120 chars. A second
        // pass with the same threshold must be a no-op (the natural
        // self-describing idempotency).
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_tool_message(&"z".repeat(5000), "toolu_idem", Some("file_read")));

        let n1 = hm.compress_tool_results(2048, 0);
        assert_eq!(n1, 1);
        let after_first = hm.messages()[0].content.clone();

        let n2 = hm.compress_tool_results(2048, 0);
        assert_eq!(n2, 0, "second call must not re-compress");
        assert_eq!(hm.messages()[0].content, after_first, "content unchanged");
    }

    #[test]
    fn test_compress_tool_results_idempotent_by_prefix() {
        // Safety net: if threshold is misconfigured (< 120 chars), the prefix
        // check must still prevent double-processing.
        let mut hm = HistoryManager::new(100_000);
        // Pre-load a message that LOOKS like a placeholder but is slightly over
        // an absurdly low threshold. The prefix check should skip it.
        let placeholder = "[Tool result compressed. Call context_recall(id=\"toolu_p\") to retrieve the full content.] plus_extra_chars_to_be_above_threshold".to_string();
        hm.append(make_tool_message(&placeholder, "toolu_p", Some("content_search")));

        let n = hm.compress_tool_results(100, 0); // threshold < placeholder length
        assert_eq!(n, 0, "prefix check must skip already-compressed messages even if threshold is below their length");
    }

    #[test]
    fn test_compress_tool_results_keep_recent_n_protects_tail() {
        // N=3 means: keep the 3 most recent Tool messages raw; compress the older ones.
        let mut hm = HistoryManager::new(100_000);
        let big = "k".repeat(5000);
        // 5 tool messages, all oversized
        for i in 0..5 {
            hm.append(make_tool_message(&big, &format!("toolu_{i}"), Some("content_search")));
        }
        assert_eq!(hm.messages().len(), 5);

        let n = hm.compress_tool_results(2048, 3);
        assert_eq!(n, 2, "exactly the 2 oldest Tool messages should be compressed");
        // Verify: oldest 2 are compressed, newest 3 are raw
        assert!(hm.messages()[0].content.contains("[Tool result compressed."));
        assert!(hm.messages()[1].content.contains("[Tool result compressed."));
        // Last 3 unchanged
        for i in 2..5 {
            assert_eq!(
                hm.messages()[i].content, big,
                "Tool message #{i} (within keep_recent_n=3 window) must remain raw"
            );
        }
    }

    #[test]
    fn test_compress_tool_results_keep_recent_n_zero_compresses_all() {
        let mut hm = HistoryManager::new(100_000);
        let big = "z".repeat(5000);
        hm.append(make_tool_message(&big, "toolu_1", Some("content_search")));
        hm.append(make_tool_message(&big, "toolu_2", Some("content_search")));
        hm.append(make_tool_message(&big, "toolu_3", Some("content_search")));

        let n = hm.compress_tool_results(2048, 0);
        assert_eq!(n, 3, "N=0 must compress every oversized Tool message");
        for m in hm.messages() {
            assert!(m.content.contains("[Tool result compressed."));
        }
    }

    #[test]
    fn test_compress_tool_results_keep_recent_n_exceeds_total_no_op() {
        // If N >= total tool_count, nothing is compressed.
        let mut hm = HistoryManager::new(100_000);
        let big = "z".repeat(5000);
        hm.append(make_tool_message(&big, "toolu_1", Some("content_search")));
        hm.append(make_tool_message(&big, "toolu_2", Some("content_search")));

        let n = hm.compress_tool_results(2048, 100);
        assert_eq!(n, 0, "keep_recent_n >= tool_count must be a no-op");
        // All messages still raw
        for m in hm.messages() {
            assert_eq!(m.content, big);
        }
    }

    #[test]
    fn test_compress_tool_results_skips_tool_without_tool_call_id() {
        // A Tool message with no tool_call_id can't be safely compressed
        // (we can't embed a recall key in the placeholder). Skip silently.
        let mut hm = HistoryManager::new(100_000);
        let big = "o".repeat(5000);
        let msg = ChatMessage {
            role: MessageRole::Tool,
            content: big.clone(),
            tool_call_id: None, // explicitly missing
            ..Default::default()
        };
        hm.append(msg);

        let n = hm.compress_tool_results(2048, 0);
        assert_eq!(n, 0, "Tool message without tool_call_id must be skipped");
        assert_eq!(hm.messages()[0].content, big, "raw content must be preserved");
    }

    #[test]
    fn test_compress_tool_results_threshold_boundary_equality() {
        // Per ADR: `content.len() > soft_threshold_chars` is the primary
        // check. So a message equal to the threshold (in chars) is NOT
        // compressed. This is the boundary case.
        let mut hm = HistoryManager::new(100_000);
        // Build a message whose content.len() equals the threshold exactly
        let threshold = 100usize;
        let exact = "x".repeat(threshold);
        assert_eq!(exact.len(), threshold);
        hm.append(make_tool_message(&exact, "toolu_eq", Some("shell")));

        let n = hm.compress_tool_results(threshold, 0);
        assert_eq!(
            n, 0,
            "content.len() == threshold is NOT strictly greater, so not compressed"
        );
        assert_eq!(hm.messages()[0].content, exact);
    }

    #[test]
    fn test_recalibrate_tokens_after_compress() {
        // recompute current_tokens after compress — should be a smaller value
        // reflecting the placeholder sizes.
        let mut hm = HistoryManager::new(1_000_000);
        let big = "r".repeat(20_000); // ~5K tokens char/4
        hm.append(make_tool_message(&big, "toolu_rec", Some("content_search")));
        // Force a manual token count by calling existing API
        let original_tokens = hm.token_count();
        assert!(original_tokens > 0);

        hm.compress_tool_results(2048, 0);
        hm.recalibrate_tokens();

        let new_tokens = hm.token_count();
        assert!(
            new_tokens < original_tokens,
            "token count should decrease after compress + recalibrate (was {original_tokens}, now {new_tokens})"
        );
        // Note: it WON'T be 0 — placeholder is ~120 chars ≈ 30 tokens
        assert!(new_tokens < original_tokens / 10);
    }

    #[test]
    fn test_tool_results_excluding_recent_basic() {
        // Sanity check on the helper: newest-to-oldest, skip N recent Tool messages,
        // return the rest in chronological order.
        let mut hm = HistoryManager::new(100_000);
        for i in 0..5 {
            hm.append(make_tool_message(&format!("raw_{i}"), &format!("toolu_{i}"), Some("content_search")));
        }
        hm.append(make_message(MessageRole::User, "user question"));
        hm.append(make_tool_message("raw_5", "toolu_5", Some("content_search")));

        let older = hm.tool_results_excluding_recent(2);
        // Should skip the 2 most recent Tool messages: toolu_5 and toolu_4.
        // Return toolu_0..toolu_3 in chronological order.
        assert_eq!(older.len(), 4);
        assert_eq!(older[0].tool_call_id.as_deref(), Some("toolu_0"));
        assert_eq!(older[3].tool_call_id.as_deref(), Some("toolu_3"));
        // toolu_4 / toolu_5 must NOT be present
        assert!(!older.iter().any(|m| m.tool_call_id.as_deref() == Some("toolu_4")));
        assert!(!older.iter().any(|m| m.tool_call_id.as_deref() == Some("toolu_5")));
    }

    #[test]
    fn test_tool_results_excluding_recent_n_exceeds_total_empty() {
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_tool_message("only", "toolu_one", Some("content_search")));
        let older = hm.tool_results_excluding_recent(100);
        assert!(older.is_empty());
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

        // Should have: [System] [compaction_summary] [Assistant Round 4] [Tool tc_4]
        //              [Assistant Round 5] [Tool tc_5]

        // Verify compaction summary exists
        let has_summary = messages.iter().any(|m| {
            m.role == MessageRole::Assistant && m.name.as_deref() == Some(COMPACTION_SUMMARY_NAME)
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
        let provider = CaptureProvider::new("<summary>hello</summary>");

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
        // User message keeps the unmodified COMPACT_PROMPT template
        assert!(req.messages[1].content.contains("<summary>"));
    }

    #[tokio::test]
    async fn compact_via_llm_with_identity_embeds_language_directive_into_system() {
        let hm = build_history_with_messages();
        let provider = CaptureProvider::new("<summary>summary text</summary>");

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
        // The original COMPACT_PROMPT body must NOT be polluted — it stays
        // in the user message untouched.
        assert!(req.messages[1].content.contains("You are a conversation summarization assistant"));
        assert!(!req.messages[0].content.contains("You are a conversation summarization assistant"),
            "the inner COMPACT_PROMPT instructions must remain in the user message, not leak into system");
    }

    #[tokio::test]
    async fn compact_via_llm_with_empty_identity_keeps_system_prompt_unchanged() {
        let hm = build_history_with_messages();
        let provider = CaptureProvider::new("ok");

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
}
