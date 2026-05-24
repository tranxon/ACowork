//! Conversation history management (FIFO trimming + Tool Result folding + Sanitization)
//!
//! Adapted from zeroclaw/src/agent/history.rs
//! Rollball deviation: uses rollball-core ChatMessage types; token estimation
//! uses char-based approximation instead of tiktoken.

use std::collections::HashSet;

use rollball_core::protocol::ProtocolType;
use rollball_core::providers::traits::{ChatMessage, ContentPart, MessageRole};


/// History manager for conversation
pub struct HistoryManager {
    /// Conversation messages
    messages: Vec<ChatMessage>,
    /// Maximum token budget for history
    max_tokens: u64,
    /// Number of full tool result iterations to keep
    keep_full_results: usize,
    /// Current estimated token count
    current_tokens: u64,
    /// LLM protocol type for image token estimation.
    /// Defaults to OpenAI; set via `set_protocol_type()` after construction.
    protocol_type: ProtocolType,
}

impl HistoryManager {
    /// Create new history manager with token budget
    pub fn new(max_tokens: u64, keep_full_results: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_tokens,
            keep_full_results,
            current_tokens: 0,
            protocol_type: ProtocolType::default(),
        }
    }

    /// Set the LLM protocol type for image token estimation.
    pub fn set_protocol_type(&mut self, pt: ProtocolType) {
        self.protocol_type = pt;
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

    /// Append a message to history
    pub fn append(&mut self, message: ChatMessage) {
        let tokens = estimate_message_tokens(&message, &self.protocol_type);
        self.current_tokens += tokens;
        self.messages.push(message);
    }

    /// Append multiple messages
    pub fn extend(&mut self, messages: Vec<ChatMessage>) {
        for msg in &messages {
            self.current_tokens += estimate_message_tokens(msg, &self.protocol_type);
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
            .map(|m| estimate_message_tokens(m, &self.protocol_type))
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
                let tokens = estimate_text_tokens(&self.messages[idx].content);
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

    /// Fold old tool results — keep last N iterations complete, summarize older ones.
    /// A "tool result iteration" is a pair of (assistant with tool_calls, tool response).
    pub fn fold_tool_results(&mut self) -> usize {
        if self.keep_full_results == 0 {
            return 0;
        }

        // Find all tool result pairs
        let tool_iterations: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m.role, MessageRole::Tool))
            .map(|(i, _)| i)
            .collect();

        if tool_iterations.len() <= self.keep_full_results {
            return 0;
        }

        let fold_count = tool_iterations.len() - self.keep_full_results;
        let mut folded = 0;

        // Fold older tool results into summaries
        for &idx in &tool_iterations[..fold_count] {
            if idx < self.messages.len() {
                let content = &self.messages[idx].content;
                let summary = if content.len() > 200 {
                    let trunc: String = content.chars().take(200).collect();
                    format!("[folded] {}", trunc)
                } else {
                    format!("[folded] {content}")
                };

                let old_tokens = estimate_text_tokens(content);
                let new_tokens = estimate_text_tokens(&summary);
                self.current_tokens = self
                    .current_tokens
                    .saturating_sub(old_tokens)
                    .saturating_add(new_tokens);

                self.messages[idx].content = summary;
                folded += 1;
            }
        }

        tracing::debug!(folded, "Tool results folded");
        folded
    }

    /// Preemptive trim — fold + FIFO if over 90% of budget
    pub fn preemptive_trim(&mut self, context_budget: u64) {
        let threshold = (context_budget as f64 * 0.9) as u64;
        if self.current_tokens > threshold {
            tracing::warn!(
                current = self.current_tokens,
                threshold,
                "Preemptive trim triggered"
            );
            self.fold_tool_results();
            if self.current_tokens > threshold {
                self.trim_fifo();
            }
        }
    }

    /// Like `preemptive_trim` but returns the messages removed by FIFO.
    ///
    /// This is used by `trim_history_to_budget` to capture evicted messages
    /// for episode distillation. The returned messages are the originals
    /// (before any folding), so they contain the full conversation content
    /// that would otherwise be lost.
    pub fn preemptive_trim_drain(&mut self, context_budget: u64) -> Vec<ChatMessage> {
        let threshold = (context_budget as f64 * 0.9) as u64;
        if self.current_tokens <= threshold {
            return Vec::new();
        }

        tracing::warn!(
            current = self.current_tokens,
            threshold,
            "Preemptive trim triggered (draining)"
        );

        // Capture messages before folding so distillation gets original content
        let first_removable = self
            .messages
            .iter()
            .position(|m| !matches!(m.role, MessageRole::System))
            .unwrap_or(0);

        self.fold_tool_results();

        if self.current_tokens <= threshold {
            return Vec::new();
        }

        // Drain FIFO — same logic as trim_fifo but returns removed messages
        self.drain_fifo(first_removable)
    }

    /// FIFO trim that returns the removed messages.
    ///
    /// `first_removable` is the index of the first non-system message.
    fn drain_fifo(&mut self, first_removable: usize) -> Vec<ChatMessage> {
        if self.current_tokens <= self.max_tokens {
            return Vec::new();
        }

        let mut removed_messages = Vec::new();
        let mut removed_count = 0;

        while self.current_tokens > self.max_tokens
            && first_removable + removed_count < self.messages.len() - 1
        {
            let idx = first_removable + removed_count;
            if idx < self.messages.len() {
                let tokens = estimate_text_tokens(&self.messages[idx].content);
                self.current_tokens = self.current_tokens.saturating_sub(tokens);
                removed_messages.push(self.messages[idx].clone());
                removed_count += 1;
            } else {
                break;
            }
        }

        if removed_count > 0 {
            let end = first_removable + removed_count;
            self.messages.drain(first_removable..end.min(self.messages.len()));
            tracing::debug!(
                removed = removed_count,
                remaining_tokens = self.current_tokens,
                "FIFO trimmed (drained)"
            );
        }

        removed_messages
    }

    /// Emergency trim — drastic measure for context overflow recovery
    /// Keeps only the last 4 non-system messages
    pub fn emergency_trim(&mut self) -> usize {
        let system_count = self
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::System))
            .count();

        let non_system_count = self.messages.len() - system_count;
        if non_system_count <= 4 {
            return 0;
        }

        let to_remove = non_system_count - 4;
        let mut removed = 0;

        // Remove oldest non-system messages
        let mut i = 0;
        while removed < to_remove && i < self.messages.len() {
            if !matches!(self.messages[i].role, MessageRole::System) {
                let tokens = estimate_text_tokens(&self.messages[i].content);
                self.current_tokens = self.current_tokens.saturating_sub(tokens);
                self.messages.remove(i);
                removed += 1;
            } else {
                i += 1;
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

        for msg in &mut self.messages {
            // Skip system messages — they should never be truncated
            if matches!(msg.role, MessageRole::System) {
                continue;
            }

            if msg.content.len() > max_chars {
                let old_tokens = estimate_text_tokens(&msg.content);
                let truncation_notice = format!(
                    "\n\n[...truncated: original {} chars, showing first {} chars]",
                    msg.content.len(),
                    max_chars
                );
                msg.content.truncate(max_chars);
                msg.content.push_str(&truncation_notice);
                let new_tokens = estimate_text_tokens(&msg.content);
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
}

/// Estimate token count for a full message, including both text content
/// and image content parts (with protocol-specific image token estimation).
fn estimate_message_tokens(message: &ChatMessage, protocol_type: &ProtocolType) -> u64 {
    let mut tokens = 0u64;

    // Count text from content_parts or fall back to .content field
    if let Some(ref parts) = message.content_parts {
        for part in parts {
            match part {
                ContentPart::Text { text } => {
                    tokens += estimate_text_tokens(text);
                }
                ContentPart::ImageUrl { image_url } => {
                    tokens += crate::token::estimate_image_tokens(
                        protocol_type,
                        image_url.width,
                        image_url.height,
                        image_url.detail.as_deref(),
                    );
                }
            }
        }
    } else {
        tokens += estimate_text_tokens(&message.content);
    }

    tokens
}

/// Estimate token count for a text string.
/// Uses a heuristic that accounts for CJK characters (which tokenize ~2 tokens each)
/// versus ASCII text (which tokenizes ~4 chars per token on average).
fn estimate_text_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let ascii_count = text.chars().filter(|c| c.is_ascii()).count() as u64;
    let cjk_count = text.chars().filter(|c| {
        ('\u{4E00}'..='\u{9FFF}').contains(c)   // CJK Unified Ideographs
            || ('\u{3040}'..='\u{309F}').contains(c) // Hiragana
            || ('\u{30A0}'..='\u{30FF}').contains(c) // Katakana
            || ('\u{AC00}'..='\u{D7AF}').contains(c) // Hangul Syllables
            || ('\u{3400}'..='\u{4DBF}').contains(c) // CJK Extension A
            || ('\u{F900}'..='\u{FAFF}').contains(c) // CJK Compatibility Ideographs
    }).count() as u64;
    let other_count = (text.chars().count() as u64).saturating_sub(ascii_count).saturating_sub(cjk_count);
    // ASCII: ~4 chars/token; CJK: ~1 char/2 tokens; other non-ASCII: ~2 chars/token
    let ascii_tokens = ascii_count.div_ceil(4);
    let cjk_tokens = (cjk_count * 2).div_ceil(1); // ~2 tokens per CJK char
    let other_tokens = other_count.div_ceil(2);
    ascii_tokens + cjk_tokens + other_tokens
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
        let mut hm = HistoryManager::new(1000, 4);
        hm.append(make_message(MessageRole::User, "Hello world"));
        assert_eq!(hm.len(), 1);
        assert!(hm.token_count() > 0);
    }

    #[test]
    fn test_fifo_trim() {
        let mut hm = HistoryManager::new(50, 4); // Very small budget
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
    fn test_fold_tool_results() {
        let mut hm = HistoryManager::new(10000, 2);
        hm.append(make_message(MessageRole::System, "System"));
        hm.append(make_message(MessageRole::User, "Query"));

        // Add 5 tool result pairs
        for i in 0..5 {
            hm.append(make_message(MessageRole::Tool, &format!("Tool result {i}: This is a long result with lots of content that should be folded when it gets old enough to save tokens in the conversation history.")));
        }

        let folded = hm.fold_tool_results();
        assert_eq!(folded, 3); // 5 - keep_full_results(2) = 3
    }

    #[test]
    fn test_emergency_trim() {
        let mut hm = HistoryManager::new(10000, 4);
        hm.append(make_message(MessageRole::System, "System"));
        for i in 0..10 {
            hm.append(make_message(MessageRole::User, &format!("Msg {i}")));
        }
        let removed = hm.emergency_trim();
        assert_eq!(removed, 6); // 10 - 4 = 6
        assert_eq!(hm.len(), 5); // 1 system + 4 remaining
    }

    #[test]
    fn test_truncate_large_messages() {
        let mut hm = HistoryManager::new(100000, 4);
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

    #[test]
    fn test_estimate_text_tokens() {
        // ASCII: 5 chars / 4 = 1.25 → ceil = 2
        assert_eq!(estimate_text_tokens("hello"), 2);
        assert_eq!(estimate_text_tokens(""), 0);
        // Pure CJK: 4 chars × 2 tokens/char = 4 tokens (rounded: (4*2+1)/2 = 4)
        assert!(estimate_text_tokens("你好世界") >= 4); // "hello world" in Chinese
        // Mixed: CJK chars get ~2 tokens each, ASCII ~1/4
        let mixed = "你好world"; // 2 CJK + 5 ASCII
        let est = estimate_text_tokens(mixed);
        assert!(est > 2, "CJK should add more tokens than pure ASCII of same len");
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
