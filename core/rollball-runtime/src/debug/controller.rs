//! DebugController — shared state for the Debug Protocol.
//!
//! Manages execution control state, conversation snapshots,
//! and context snapshots. Wrapped in `Arc<tokio::sync::Mutex<>>` for
//! safe sharing between the WebSocket server and AgentLoop.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::protocol::{
    ContextSections, DebugPhase, DebugUsage, SectionMeta,
};

// ── Debug Execution State ─────────────────────────────────────────────

/// Current execution state of the debug session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DebugState {
    /// Running — agent loop is executing freely
    Running,
    /// Paused — agent loop is waiting for a Continue/Step command
    Paused,
    /// Stepping — agent loop will execute one step then auto-Pause
    Stepping,
    /// Stopped — agent loop has been terminated
    Stopped,
}

// ── Conversation Snapshot ─────────────────────────────────────────────

/// Lightweight conversation snapshot (per iteration).
///
/// Uses `message_count` instead of deep-copying the message array —
/// messages are append-only, so a rollback only needs to truncate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSnapshot {
    /// Snapshot ID (incrementing counter)
    pub id: String,
    /// Corresponding iteration number
    pub iteration: u32,
    /// Number of messages when this snapshot was taken
    pub message_count: usize,
    /// Cumulative LLM usage at snapshot time
    pub cumulative_usage: DebugUsage,
    /// Timestamp (milliseconds since epoch)
    pub timestamp_ms: i64,
}

// ── Context Snapshot ──────────────────────────────────────────────────

/// A snapshot of the context building result for one iteration.
///
/// Stores metadata only (size/token/hash). Section content is stored
/// separately and returned via `getSection` (lazy loading).
#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub iteration: u32,
    pub built_at: chrono::DateTime<chrono::Utc>,
    pub sections: ContextSnapshotSections,
    pub total_token_estimate: usize,
}

/// The seven control-plane sections with their content.
#[derive(Debug, Clone)]
pub struct ContextSnapshotSections {
    pub system_prompt: SectionContent,
    pub workspace_context: SectionContent,
    pub environment: SectionContent,
    pub tool_definitions: SectionContent,
    pub skill_instructions: SectionContent,
    pub retrieved_memory: SectionContent,
    pub identity_context: SectionContent,
}

/// Content of a single context section with metadata.
#[derive(Debug, Clone)]
pub struct SectionContent {
    /// Full text content
    pub content: String,
    /// Byte size of the content
    pub size_bytes: usize,
    /// Estimated token count
    pub token_estimate: usize,
    /// SHA-256 hash of the content (for diff detection)
    pub hash: String,
}

impl SectionContent {
    /// Create a SectionContent with a model-aware token estimate.
    ///
    /// Uses [`crate::token::count_text`] — the single unified entry point
    /// for all token counting in RollBall. For GPT models this uses tiktoken
    /// (< 1% error); for Claude/Qwen it uses sampling ratios (< 5% error);
    /// for unknown models it falls back to word/CJK heuristic (< 15% error).
    pub fn new(content: String, model: &str) -> Self {
        let token_estimate = crate::token::count_text(&content, model);
        Self::build(content, token_estimate)
    }

    /// Create a SectionContent with a pre-computed token estimate.
    ///
    /// Use this when the caller has already counted tokens externally
    /// (e.g. from a cached `TokenCounter` instance) to avoid redundant work.
    /// Prefer [`new`] for one-off constructions.
    pub fn with_token_count(content: String, token_estimate: usize) -> Self {
        Self::build(content, token_estimate)
    }

    /// Internal constructor shared by [`new`] and [`with_token_count`].
    fn build(content: String, token_estimate: usize) -> Self {
        use sha2::{Digest, Sha256};

        let size_bytes = content.len();
        let hash = {
            let mut hasher = Sha256::new();
            hasher.update(content.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        Self {
            content,
            size_bytes,
            token_estimate,
            hash,
        }
    }

    /// Convert to serializable metadata (without content).
    pub fn to_meta(&self) -> SectionMeta {
        SectionMeta {
            size_bytes: self.size_bytes,
            token_estimate: self.token_estimate,
            hash: self.hash.clone(),
        }
    }
}

impl From<&ContextSnapshotSections> for ContextSections {
    fn from(s: &ContextSnapshotSections) -> Self {
        Self {
            system_prompt: s.system_prompt.to_meta(),
            workspace_context: s.workspace_context.to_meta(),
            environment: s.environment.to_meta(),
            tool_definitions: s.tool_definitions.to_meta(),
            skill_instructions: s.skill_instructions.to_meta(),
            retrieved_memory: s.retrieved_memory.to_meta(),
            identity_context: s.identity_context.to_meta(),
        }
    }
}

// ── DebugController ───────────────────────────────────────────────────

/// Shared debug controller, owned by DebugProtocolServer and accessed by AgentLoop.
pub struct DebugController {
    /// Current execution state
    pub state: DebugState,
    /// Current phase of the iteration
    pub phase: DebugPhase,
    /// Current iteration number
    pub iteration: u32,
    /// Conversation snapshots (indexed by iteration)
    pub conversation_snapshots: Vec<ConversationSnapshot>,
    /// Context snapshots (indexed by iteration)
    pub context_snapshots: HashMap<u32, ContextSnapshot>,
    /// Pending patches for context re-execution
    pub pending_patches: Option<super::protocol::PatchSet>,
    /// Target iteration for rewind (set by `debugger.rewind`, consumed by SessionTask)
    pub rewind_target: Option<u32>,
    /// Flag indicating re-execute was requested (set by `debugger.reExecute`, consumed by SessionTask)
    pub re_execute_pending: bool,
    /// Notification signal for pending rewind.
    ///
    /// The RPC handler calls `notify_one()` after setting `rewind_target`.
    /// The agent loop (via `await_debug_resume`) and the SessionTask
    /// (via `tokio::select!`) await this notify to consume the rewind
    /// without polling.  This makes rewind a first-class event in the
    /// debug lifecycle instead of a polling-based side channel.
    pub rewind_notify: Arc<Notify>,
    /// Notification signal for resume requests.
    ///
    /// When the user presses resume but the agent loop has already
    /// completed (e.g. after rewind was issued post-completion),
    /// the SessionTask is blocked waiting for the next ChatMessage
    /// and cannot detect the state change.  The resume handler calls
    /// `notify_one()` so the SessionTask wakes up and re-runs the
    /// agent loop with the saved user message.
    pub resume_notify: Arc<Notify>,
    /// The model name used for the current session's token counting.
    /// Set by [`AgentLoop::capture_context_snapshot`] so that context
    /// patches (via `patchContext`) can use model-aware token estimates.
    pub current_model: Option<String>,
}

impl DebugController {
    /// Create a new DebugController in Stepping state (auto-pause after first iteration).
    pub fn new() -> Self {
        Self {
            state: DebugState::Stepping,
            phase: DebugPhase::Idle,
            iteration: 0,
            conversation_snapshots: Vec::new(),
            context_snapshots: HashMap::new(),
            pending_patches: None,
            rewind_target: None,
            re_execute_pending: false,
            rewind_notify: Arc::new(Notify::const_new()),
            resume_notify: Arc::new(Notify::const_new()),
            current_model: None,
        }
    }

    /// Create a conversation snapshot at the current state.
    pub fn create_conversation_snapshot(
        &mut self,
        message_count: usize,
        usage: DebugUsage,
    ) -> ConversationSnapshot {
        let snap = ConversationSnapshot {
            id: format!("snap-{}", self.conversation_snapshots.len()),
            iteration: self.iteration,
            message_count,
            cumulative_usage: usage,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };
        self.conversation_snapshots.push(snap.clone());
        snap
    }

    /// Store a context snapshot for the given iteration.
    pub fn store_context_snapshot(&mut self, snapshot: ContextSnapshot) {
        self.context_snapshots.insert(snapshot.iteration, snapshot);
    }

    /// Get a context snapshot by iteration.
    pub fn get_context_snapshot(&self, iteration: u32) -> Option<&ContextSnapshot> {
        self.context_snapshots.get(&iteration)
    }

    /// Take the rewind target, clearing it from the controller.
    /// Returns the target iteration if set.
    pub fn take_rewind_target(&mut self) -> Option<u32> {
        self.rewind_target.take()
    }

    /// Notify consumers that a rewind is pending.
    ///
    /// Called by the RPC handler after setting `rewind_target`.
    /// Wakes up any task waiting on `rewind_notify.notified()`.
    pub fn notify_rewind(&self) {
        self.rewind_notify.notify_one();
    }

    /// Clone the rewind notification handle.
    ///
    /// Used by `AgentCore` to pass the notify to `SessionTask`
    /// without holding the controller mutex.
    pub fn rewind_notify_handle(&self) -> Arc<Notify> {
        self.rewind_notify.clone()
    }

    /// Clone the resume notification handle.
    ///
    /// Used by `AgentCore` to pass the notify to `SessionTask`
    /// without holding the controller mutex.
    pub fn resume_notify_handle(&self) -> Arc<Notify> {
        self.resume_notify.clone()
    }

    /// Set the re-execute pending flag.
    pub fn set_re_execute_pending(&mut self) {
        self.re_execute_pending = true;
    }

    /// Take the re-execute pending flag, clearing it.
    /// Returns true if re-execute was requested.
    pub fn take_re_execute_pending(&mut self) -> bool {
        let was_pending = self.re_execute_pending;
        self.re_execute_pending = false;
        was_pending
    }

    /// Truncate conversation snapshots after the given iteration.
    /// Retains only snapshots whose iteration <= target.
    pub fn truncate_snapshots_after(&mut self, target_iteration: u32) {
        self.conversation_snapshots
            .retain(|s| s.iteration <= target_iteration);
        self.context_snapshots
            .retain(|&iter, _| iter <= target_iteration);
    }

    /// Clear all stored state (for restarting).
    pub fn reset(&mut self) {
        self.state = DebugState::Stepping;
        self.phase = DebugPhase::Idle;
        self.iteration = 0;
        self.conversation_snapshots.clear();
        self.context_snapshots.clear();
        self.pending_patches = None;
        self.rewind_target = None;
        self.re_execute_pending = false;
    }
}

impl Default for DebugController {
    fn default() -> Self {
        Self::new()
    }
}
