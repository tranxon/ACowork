//! Conversation history management (FIFO trimming + Tool Result folding + Sanitization)
//!
//! Adapted from zeroclaw/src/agent/history.rs
//! Rollball deviation: uses rollball-core ChatMessage types; token estimation
//! uses char-based approximation instead of tiktoken.

use std::collections::HashSet;

use rollball_core::providers::traits::{ChatMessage, MessageRole};


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
}

impl HistoryManager {
    /// Create new history manager with token budget
    pub fn new(max_tokens: u64, keep_full_results: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_tokens,
            keep_full_results,
            current_tokens: 0,
        }
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
        let tokens = estimate_tokens(&message.content);
        self.current_tokens += tokens;
        self.messages.push(message);
    }

    /// Append multiple messages
    pub fn extend(&mut self, messages: Vec<ChatMessage>) {
        for msg in &messages {
            self.current_tokens += estimate_tokens(&msg.content);
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
                let tokens = estimate_tokens(&self.messages[idx].content);
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

                let old_tokens = estimate_tokens(content);
                let new_tokens = estimate_tokens(&summary);
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
                let tokens = estimate_tokens(&self.messages[i].content);
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
            if msg.role == MessageRole::Tool {
                if let Some(ref tcid) = msg.tool_call_id {
                    if !valid_tool_call_ids.contains(tcid) {
                        tracing::warn!(
                            tool_call_id = %tcid,
                            "Removing orphaned tool result message"
                        );
                        return false;
                    }
                }
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
                let has_tool_calls = msg.tool_calls.as_ref().map_or(false, |tc| !tc.is_empty());
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

/// Estimate tokens from text content (rough: 4 chars per token)
fn estimate_tokens(text: &str) -> u64 {
    (text.len() as f64 / 4.0).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
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
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello"), 2); // 5/4 = 1.25, ceil = 2
        assert_eq!(estimate_tokens(""), 0);
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
        ChatMessage {
            role: MessageRole::Tool,
            content: content.to_string(),
            name: None,
            tool_call_id: Some(tool_call_id.to_string()),
            tool_calls: None,
        }
    }

    #[test]
    fn test_sanitize_fixes_invalid_arguments() {
        let mut messages = vec![
            ChatMessage {
                role: MessageRole::Assistant,
                content: "".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    make_tool_call("tc_1", "read_file", "not valid json{{"),
                    make_tool_call("tc_2", "write_file", r#"{"path":"/tmp"}"#),
                ]),
            },
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
            ChatMessage {
                role: MessageRole::Assistant,
                content: "I'll help you".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    make_tool_call("tc_1", "read_file", "{}"),
                ]),
            },
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
            ChatMessage {
                role: MessageRole::Assistant,
                content: "".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    make_tool_call("tc_1", "read_file", "{}"),
                    make_tool_call("tc_2", "write_file", "{}"),
                ]),
            },
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
            ChatMessage {
                role: MessageRole::Assistant,
                content: "".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
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
            ChatMessage {
                role: MessageRole::Assistant,
                content: "Let me check".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    make_tool_call("tc_1", "search", "{}"),
                ]),
            },
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
            ChatMessage {
                role: MessageRole::Assistant,
                content: "".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    make_tool_call("tc_1", "read_file", "not json"),
                ]),
            },
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
            ChatMessage {
                role: MessageRole::Assistant,
                content: "Let me check".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    make_tool_call("tc_1", "search", "{}"),
                    make_tool_call("tc_2", "read", "{}"),
                ]),
            },
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
