//! Conversation history management (FIFO trimming + Sanitization + Emergency trim)
//!
//! Adapted from zeroclaw/src/agent/history.rs
//! Rollball deviation: uses rollball-core ChatMessage types; token estimation
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

use rollball_core::protocol::ProtocolType;
use rollball_core::providers::traits::{ChatMessage, ChatRequest, MessageRole, Provider};

use crate::error::RuntimeError;
use crate::token::counter::TokenCounter;


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

    /// Set the model name for Tier1/Tier2 token counting precision.
    /// Called when session model is determined (ADR-012).
    pub fn set_model_name(&mut self, model: String) {
        self.model_name = Some(model);
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
    /// token count for the entire prompt (system + history). This method replaces
    /// our heuristic estimate with the ground truth value, preventing cumulative
    /// estimation drift across turns.
    pub fn calibrate_from_usage(&mut self, prompt_tokens: u64) {
        let old = self.current_tokens;
        self.current_tokens = prompt_tokens;
        tracing::debug!(old, new = prompt_tokens, "History token count calibrated from API usage");
    }

    /// Append a message to history
    pub fn append(&mut self, message: ChatMessage) {
        let tokens = self.counter.count_message(&message, self.model_for_counting(), Some(&self.protocol_type));
        self.current_tokens += tokens;
        self.messages.push(message);
    }

    /// Append multiple messages
    pub fn extend(&mut self, messages: Vec<ChatMessage>) {
        for msg in &messages {
            self.current_tokens += self.counter.count_message(msg, self.model_for_counting(), Some(&self.protocol_type));
        }
        self.messages.extend(messages);
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
            .map(|m| self.counter.count_message(m, self.model_for_counting(), Some(&self.protocol_type)))
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
    
        while self.current_tokens > self.max_tokens && first_removable + removed < self.messages.len() - 1 {
            let idx = first_removable + removed;
            if idx < self.messages.len() {
                let tokens = self.counter.count_message(&self.messages[idx], self.model_for_counting(), Some(&self.protocol_type));
                self.current_tokens = self.current_tokens.saturating_sub(tokens);
                removed += 1;
            } else {
                break;
            }
        }
    
        if removed > 0 {
            // Actually remove the messages
            let end = first_removable + removed;
            self.messages.drain(first_removable..end.min(self.messages.len()));
            tracing::debug!(removed, remaining_tokens = self.current_tokens, "FIFO trimmed");
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
            msg.name.as_deref() == Some("compaction_summary")
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
                let tokens = self.counter.count_message(&self.messages[i], self.model_for_counting(), Some(&self.protocol_type));
                self.current_tokens = self.current_tokens.saturating_sub(tokens);
                self.messages.remove(i);
                removed += 1;
            }
        }

        tracing::warn!(removed, "Emergency trim performed");
        removed
    }

    /// Truncate individual messages whose content exceeds max_tokens_per_message.
    /// This prevents a single oversized tool result (e.g. shell output) from
    /// consuming the entire context window.
    /// Returns the number of messages truncated.
    pub fn truncate_large_messages(&mut self, max_tokens_per_message: u64) -> usize {
        let max_chars = (max_tokens_per_message * 4) as usize;
        let mut truncated = 0;

        // Extract model, protocol_type, and counter ref before loop
        // to avoid borrow conflicts with &mut self.messages.
        let model = self.model_for_counting().to_string();
        let pt = self.protocol_type.clone();
        let counter = &self.counter;

        for msg in &mut self.messages {
            // Skip system messages — they should never be truncated
            if matches!(msg.role, MessageRole::System) {
                continue;
            }

            if msg.content.len() > max_chars {
                let old_tokens = counter.count_message(msg, &model, Some(&pt));
                let truncation_notice = format!(
                    "\n\n[...truncated: original {} chars, showing first {} chars]",
                    msg.content.len(),
                    max_chars
                );
                msg.content.truncate(max_chars);
                msg.content.push_str(&truncation_notice);
                let new_tokens = counter.count_message(msg, &model, Some(&pt));
                self.current_tokens = self
                    .current_tokens
                    .saturating_sub(old_tokens)
                    .saturating_add(new_tokens);
                truncated += 1;
            }
        }

        if truncated > 0 {
            tracing::warn!(
                truncated,
                max_tokens_per_message,
                "Truncated oversized messages to per-message limit"
            );
        }
        truncated
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
    pub async fn compact_via_llm(
        &self,
        provider: &dyn Provider,
        model_name: &str,
        system_prompt: &str,
    ) -> std::result::Result<String, RuntimeError> {
        let messages_text = crate::episode_distill::format_messages(&self.messages);
        if messages_text.is_empty() {
            return Err(RuntimeError::Tool(
                "Cannot compact empty history".to_string(),
            ));
        }

        let prompt =
            crate::prompt::COMPACT_PROMPT.replace("{messages_text}", &messages_text);

        let request = ChatRequest {
            model: model_name.to_string(),
            messages: vec![
                ChatMessage {
                    role: MessageRole::System,
                    content: system_prompt.to_string(),
                    ..Default::default()
                },
                ChatMessage::user(prompt),
            ],
            temperature: Some(0.3),
            max_tokens: Some(2048),
            tools: None,
        };

        let response = provider
            .chat(request)
            .await
            .map_err(|e| RuntimeError::Core(e))?;

        let summary = response.content.trim().to_string();
        if summary.is_empty() {
            return Err(RuntimeError::Tool(
                "Compact model returned empty response".to_string(),
            ));
        }
        Ok(summary)
    }

    /// Replace the middle section of history with a compaction summary.
    ///
    /// Keeps system messages at the start and the last `keep_last_rounds`
    /// conversational rounds at the end. The middle is replaced with a
    /// single Assistant message carrying `name: "compaction_summary"` as
    /// a compaction marker for [`last_compaction_index`].
    ///
    /// Returns the number of messages removed.
    pub fn replace_middle_with_summary(
        &mut self,
        summary: &str,
        keep_last_rounds: usize,
    ) -> usize {
        // Count leading system messages
        let system_count = self
            .messages
            .iter()
            .take_while(|m| matches!(m.role, MessageRole::System))
            .count();

        // Find tail start: count User messages from the end.
        // Each "round" starts with a User message, so counting User messages
        // gives a more accurate round count than `keep_last_rounds * 2`.
        let tail_start = {
            let mut user_count = 0usize;
            let mut idx = self.messages.len();
            for (i, msg) in self.messages.iter().enumerate().rev() {
                if matches!(msg.role, MessageRole::User) {
                    user_count += 1;
                    if user_count >= keep_last_rounds {
                        idx = i;
                        break;
                    }
                }
            }
            // Not enough rounds: keep everything after system messages
            if user_count < keep_last_rounds {
                system_count
            } else {
                idx
            }
        };

        if tail_start <= system_count {
            return 0; // Nothing to replace
        }

        let removed_count = tail_start - system_count;

        // Subtract tokens of removed messages
        for msg in &self.messages[system_count..tail_start] {
            let tokens = self.counter.count_message(msg, self.model_for_counting(), Some(&self.protocol_type));
            self.current_tokens = self.current_tokens.saturating_sub(tokens);
        }

        // Remove middle section
        self.messages.drain(system_count..tail_start);

        // Insert compaction summary as Assistant message with marker
        let summary_msg = ChatMessage {
            role: MessageRole::Assistant,
            content: summary.to_string(),
            name: Some("compaction_summary".to_string()),
            ..Default::default()
        };
        let summary_tokens = self.counter.count_message(&summary_msg, self.model_for_counting(), Some(&self.protocol_type));
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
    /// `name == "compaction_summary"`. Returns `Some(index)` if found,
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
                    && msg.name.as_deref() == Some("compaction_summary")
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
            hm.append(make_message(MessageRole::User, &format!("Message {i} with some content to fill tokens")));
        }
        let removed = hm.trim_fifo();
        assert!(removed > 0);
        // System message should still be there
        assert!(hm.messages().iter().any(|m| matches!(m.role, MessageRole::System)));
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
            name: Some("compaction_summary".to_string()),
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
        let has_marker = hm.messages().iter().any(|m| {
            m.name.as_deref() == Some("compaction_summary")
        });
        assert!(has_marker, "Compaction marker should survive emergency trim");
    }

    #[test]
    fn test_truncate_large_messages() {
        let mut hm = HistoryManager::new(100000);
        hm.append(make_message(MessageRole::System, "System prompt"));
        // Add a message with very long content (simulating shell output)
        let long_content: String = "x".repeat(100_000); // 100K chars = ~25K tokens
        hm.append(make_message(MessageRole::Tool, &long_content));
        hm.append(make_message(MessageRole::User, "Short message"));

        // Truncate with max 1000 tokens per message (= 4000 chars)
        let truncated = hm.truncate_large_messages(1000);
        assert_eq!(truncated, 1); // Only the tool message was truncated
        assert_eq!(hm.len(), 3); // No messages removed

        // The tool message should now be truncated
        let tool_msg = hm.messages().iter().find(|m| matches!(m.role, MessageRole::Tool)).unwrap();
        assert!(tool_msg.content.len() < long_content.len());
        assert!(tool_msg.content.contains("[...truncated"));

        // System message should NOT be truncated
        let sys_msg = hm.messages().iter().find(|m| matches!(m.role, MessageRole::System)).unwrap();
        assert_eq!(sys_msg.content, "System prompt");
    }

    // ── sanitize_messages tests ─────────────────────────────────────────

    use rollball_core::providers::traits::{FunctionCall, ToolCall};

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
            ChatMessage::assistant_with_tools("", vec![
                    make_tool_call("tc_1", "read_file", "not valid json{{"),
                    make_tool_call("tc_2", "write_file", r#"{"path":"/tmp"}"#),
                ]),
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
            ChatMessage::assistant_with_tools("I'll help you", vec![
                    make_tool_call("tc_1", "read_file", "{}"),
                ]),
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
            ChatMessage::assistant_with_tools("", vec![
                    make_tool_call("tc_1", "read_file", "{}"),
                    make_tool_call("tc_2", "write_file", "{}"),
                ]),
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
            ChatMessage::assistant_with_tools("Let me check", vec![
                    make_tool_call("tc_1", "search", "{}"),
                ]),
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
            ChatMessage::assistant_with_tools("", vec![
                    make_tool_call("tc_1", "read_file", "not json"),
                ]),
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
        let mut messages = vec![
            ChatMessage::assistant_with_tools("Let me check", vec![
                    make_tool_call("tc_1", "search", "{}"),
                    make_tool_call("tc_2", "read", "{}"),
                ]),
        ];
        // No tool results at all — both tool_calls should be removed

        HistoryManager::sanitize_messages(&mut messages);

        let assistant = &messages[0];
        // tool_calls should be cleared to None since all were orphaned
        assert!(assistant.tool_calls.is_none());
        // Content should be preserved since it's non-empty
        assert_eq!(assistant.content, "Let me check");
    }
}
