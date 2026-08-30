//! ADR-052 context compression - shared queue contracts.
//!
//! These queue types are the **data contract** between the two sides of
//! the context-compression feature:
//!
//! - **Producer** - the `context_retrieve` builtin tool
//!   (`crate::tools::builtin::context_retrieve`). Tools have no access to
//!   `HistoryManager`; they only push intent (a `(tool_call_id,
//!   original_content)` pair) onto the queue.
//! - **Consumer** - `AgentLoop` drains the queue at the start of each
//!   iteration (`drain_retrieve_queue`) and performs the actual in-place
//!   history mutation via `HistoryManager::retrieve_tool_result()`.
//! - **Owner** - `AgentCore` creates the queue at init and hands `Arc`
//!   clones to the tool and to every `AgentLoop`.
//!
//! ADR-061 §10.2: the `AbandonQueue` contract is **deleted** —
//! LLM-autonomous tool compression is closed (`context_abandon` is no
//! longer registered; the deprecated tool keeps an internal queue that
//! nothing drains). `context_retrieve` remains the manual recall channel.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Shared queue for `context_retrieve` requests.
///
/// The tool writes `(tool_call_id, original_content)` pairs here; the
/// agent loop drains them and restores the original content in-place
/// (replacing the placeholder).
pub type RetrieveQueue = Arc<Mutex<VecDeque<(String, String)>>>;

/// Create a fresh empty retrieve queue (used at `AgentCore` init).
pub fn new_retrieve_queue() -> RetrieveQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}
