//! Conversation history management (FIFO trimming + Tool Result folding)
//!
//! Adapted from zeroclaw/src/agent/history.rs
//! Rollball deviation: uses rollball-core ChatMessage types; token estimation
//! uses char-based approximation instead of tiktoken.

use rollball_core::providers::traits::{ChatMessage, MessageRole, ToolCall};

use crate::error::{Result, RuntimeError};

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
                    format!("[folded] {}", &content[..200])
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
}
