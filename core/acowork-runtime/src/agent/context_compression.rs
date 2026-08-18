//! ADR-052 context compression - shared queue contracts.
//!
//! These queue types are the **data contract** between the two sides of
//! the context-compression feature:
//!
//! - **Producers** - the `context_abandon` / `context_retrieve` builtin
//!   tools (`crate::tools::builtin::context_*`). Tools have no access to
//!   `HistoryManager`; they only push intent (a `tool_call_id`, or a
//!   `(tool_call_id, original_content)` pair) onto these queues.
//! - **Consumer** - `AgentLoop` drains both queues at the start of each
//!   iteration (`drain_abandon_queue` / `drain_retrieve_queue`) and
//!   performs the actual in-place history mutation via
//!   `HistoryManager::{abandon,retrieve}_tool_result()`.
//! - **Owner** - `AgentCore` creates the queues at init and hands `Arc`
//!   clones to the tools and to every `AgentLoop`.
//!
//! The types deliberately live here - next to their owner/consumer in
//! the `agent` module - instead of inside the tool implementation files,
//! so that core agent state (`AgentCore`, `AgentLoop`) never depends on
//! a specific builtin tool's source file for a type it owns.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Shared queue for `context_abandon` requests.
///
/// The tool writes `tool_call_id` strings here; the agent loop drains
/// them and calls `HistoryManager::abandon_tool_result()`.
pub type AbandonQueue = Arc<Mutex<VecDeque<String>>>;

/// Shared queue for `context_retrieve` requests.
///
/// The tool writes `(tool_call_id, original_content)` pairs here; the
/// agent loop drains them and restores the original content in-place
/// (replacing the placeholder).
pub type RetrieveQueue = Arc<Mutex<VecDeque<(String, String)>>>;

/// Create a fresh empty abandon queue (used at `AgentCore` init).
pub fn new_abandon_queue() -> AbandonQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Create a fresh empty retrieve queue (used at `AgentCore` init).
pub fn new_retrieve_queue() -> RetrieveQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}
