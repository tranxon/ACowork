//! Per-session state for Agent Runtime.
//!
//! `SessionState` holds all state that is scoped to a single conversation session:
//! history, conversation persistence, loop detector, and budget guard.
//! Each session gets its own independent instance, ensuring isolation
//! between sessions (e.g. loop detection does not cross session boundaries).
//!
//! Phase 1: direct ownership inside AgentLoop.
//! Phase 2: extracted into Session Actor for multi-session concurrency.

use crate::agent::budget_guard::BudgetGuard;
use crate::agent::history::HistoryManager;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_detector::LoopDetector;
use crate::conversation::ConversationSession;

/// A single item in the session-level todo list.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    /// Unique identifier for this todo item (e.g. UUID or short slug)
    pub id: String,
    /// Human-readable content of the task
    pub content: String,
    /// Current status of the task
    pub status: TodoStatus,
}

/// Status of a todo item.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Task not yet started
    Pending,
    /// Task currently being worked on
    InProgress,
    /// Task completed
    Completed,
}

/// Lifecycle status of a session, managed by Runtime as the source of truth.
///
/// ADR-014: The Runtime owns session status; the frontend is read-only.
/// State transitions are emitted as `ChunkEvent::SessionStateChanged` via
/// the on_chunk channel, so the Gateway and frontend stay in sync without
/// optimistic local writes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum SessionStatus {
    /// Session is idle — no LLM call in progress
    Idle,
    /// LLM is generating a response. `message_id` matches the streaming message.
    Streaming { message_id: Option<String> },
    /// A tool requires user approval before execution
    WaitingApproval { request_id: String },
    /// Iteration limit reached or debug pause — awaiting user decision
    Paused { iteration: Option<u32>, max_iterations: Option<u32> },

}

impl Default for SessionStatus {
    fn default() -> Self {
        Self::Idle
    }
}

impl SessionStatus {
    /// Returns true if the session is actively processing (streaming or awaiting approval).
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Streaming { .. } | Self::WaitingApproval { .. })
    }
}

/// Per-session state for the agent loop.
///
/// Each field is scoped to a single session and is not shared across sessions.
/// This ensures that loop detection, budget tracking, and history are isolated
/// per session, preventing cross-session interference.
pub struct SessionState {
    /// Conversation history manager (message list + token tracking + trimming)
    pub(crate) history: HistoryManager,
    /// Optional conversation session for JSONL persistence
    pub(crate) conversation: Option<ConversationSession>,
    /// Loop detector (per-session to avoid cross-session false positives)
    pub(crate) loop_detector: LoopDetector,
    /// Budget guard (per-session for independent token accounting)
    pub(crate) budget_guard: BudgetGuard,
    /// Turn counter for Grafeo episodic storage (P1-2 fix).
    /// Monotonically increasing per session; used as `turn_index` in
    /// ConversationRecord to preserve chronological order.
    pub(crate) turn_counter: u32,
    /// Messages deferred from `poll_interrupt()` during active execution.
    /// These are non-Interrupt messages that arrived in the AgentLoop's
    /// inbound channel while it was polling mid-iteration. They are
    /// re-injected at the next `drain_inbound_queue()` call so no
    /// message is silently lost.
    pub(crate) deferred_inbound: Vec<InboundMessage>,
    /// Current lifecycle status of the session (source of truth).
    /// ADR-014: Runtime owns this; frontend reads it via SessionStateChanged events.
    pub(crate) status: SessionStatus,
    /// Session-level todo list managed by the `todo_write` built-in tool.
    /// Memory-only; not persisted to JSONL (conversation history is the
    /// source of truth for task progress).
    pub(crate) todos: Vec<TodoItem>,
    /// Whether compaction has occurred with zero new messages since.
    ///
    /// Per [ADR-011], compaction summaries sit in the middle of history
    /// (not at the tail), so we can't use message position to detect
    /// whether new messages arrived after compaction. This boolean flag
    /// provides a clean signal:
    /// - Set to `true` when compaction completes.
    /// - Reset to `false` when a new message is appended to history.
    /// - At session close: `true` means skip distillation (no new content),
    ///   `false` means distill the tail (new messages after last compaction).
    pub(crate) is_compacted: bool,
    /// Per-session model selection (ADR-012).
    /// Initialized from JSONL metadata or Gateway LLMConfigDelivery.
    pub(crate) model: Option<String>,
    /// Per-session provider selection (ADR-012).
    pub(crate) provider: Option<String>,
}

impl SessionState {
    /// Create a new SessionState with the given history parameters and budget.
    pub fn new(
        max_tokens: u64,
        budget: rollball_core::Budget,
        conversation: Option<ConversationSession>,
    ) -> Self {
        Self {
            history: HistoryManager::new(max_tokens),
            conversation,
            loop_detector: LoopDetector::with_defaults(),
            budget_guard: BudgetGuard::new(budget),
            turn_counter: 0,
            deferred_inbound: Vec::new(),
            status: SessionStatus::Idle,
            todos: Vec::new(),
            is_compacted: false,
            model: None,
            provider: None,
        }
    }

    /// Access the history manager.
    pub fn history(&self) -> &HistoryManager {
        &self.history
    }

    /// Access the history manager (mutable).
    pub fn history_mut(&mut self) -> &mut HistoryManager {
        &mut self.history
    }

    /// Access the conversation session.
    pub fn conversation(&self) -> Option<&ConversationSession> {
        self.conversation.as_ref()
    }

    /// Access the conversation session (mutable).
    pub fn conversation_mut(&mut self) -> &mut Option<ConversationSession> {
        &mut self.conversation
    }

    /// Access the loop detector.
    pub fn loop_detector(&self) -> &LoopDetector {
        &self.loop_detector
    }

    /// Access the loop detector (mutable).
    pub fn loop_detector_mut(&mut self) -> &mut LoopDetector {
        &mut self.loop_detector
    }

    /// Access the budget guard.
    pub fn budget_guard(&self) -> &BudgetGuard {
        &self.budget_guard
    }

    /// Access the budget guard (mutable).
    pub fn budget_guard_mut(&mut self) -> &mut BudgetGuard {
        &mut self.budget_guard
    }

    /// Access the session status.
    pub fn status(&self) -> &SessionStatus {
        &self.status
    }

    /// Transition session status and return true if the status actually changed.
    /// Returns false if the new status equals the current one (no-op).
    pub fn set_status(&mut self, new_status: SessionStatus) -> bool {
        if self.status == new_status {
            return false;
        }
        tracing::info!(old = ?self.status, new = ?new_status, "Session status changed");
        self.status = new_status;
        true
    }

    /// Update the todo list from a `todo_write` tool call.
    ///
    /// * `merge`: if true, replace the entire list; if false, merge by id
    ///   (update existing items, append new items, remove items not present).
    pub fn update_todos(&mut self, items: Vec<TodoItem>, merge: bool) {
        if merge {
            // Merge: update existing by id, add new, keep items not in input
            for incoming in &items {
                if let Some(existing) = self.todos.iter_mut().find(|t| t.id == incoming.id) {
                    existing.content = incoming.content.clone();
                    existing.status = incoming.status.clone();
                } else {
                    self.todos.push(incoming.clone());
                }
            }
        } else {
            // Replace: full swap
            self.todos = items;
        }
    }

    /// Format the current todo list as a markdown text for system prompt injection.
    /// Returns `None` if the list is empty.
    pub fn format_todos(&self) -> Option<String> {
        if self.todos.is_empty() {
            return None;
        }
        let lines: Vec<String> = self
            .todos
            .iter()
            .map(|t| {
                let status_mark = match t.status {
                    TodoStatus::Pending => " ",
                    TodoStatus::InProgress => "-",
                    TodoStatus::Completed => "x",
                };
                format!("- [{}] {} ({})", status_mark, t.content, t.id)
            })
            .collect();
        Some(lines.join("\n"))
    }

    /// Get the per-session model (ADR-012).
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Set the per-session model (ADR-012).
    pub fn set_model(&mut self, model: String) {
        self.history.set_model_name(model.clone());
        self.model = Some(model);
    }

    /// Get the per-session provider (ADR-012).
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    /// Set the per-session provider (ADR-012).
    pub fn set_provider(&mut self, provider: String) {
        self.provider = Some(provider);
    }

    /// Get the per-session workspace_id (from JSONL metadata, persisted by
    /// ConversationSession::update_workspace_id).
    /// Returns None if no conversation is attached or no workspace has been set.
    pub fn workspace_id(&self) -> Option<String> {
        self.conversation.as_ref().and_then(|c| c.workspace_id())
    }
}
