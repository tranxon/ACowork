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
}

impl SessionState {
    /// Create a new SessionState with the given history parameters and budget.
    pub fn new(
        max_tokens: u64,
        keep_full_results: usize,
        budget: rollball_core::Budget,
        conversation: Option<ConversationSession>,
    ) -> Self {
        Self {
            history: HistoryManager::new(max_tokens, keep_full_results),
            conversation,
            loop_detector: LoopDetector::with_defaults(),
            budget_guard: BudgetGuard::new(budget),
            turn_counter: 0,
            deferred_inbound: Vec::new(),
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
}
