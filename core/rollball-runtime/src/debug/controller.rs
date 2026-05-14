//! DebugController — shared state for the Debug Protocol.
//!
//! Manages execution control state, breakpoints, conversation snapshots,
//! and context snapshots. Wrapped in `Arc<tokio::sync::Mutex<>>` for
//! safe sharing between the WebSocket server and AgentLoop.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::protocol::{
    Breakpoint, ContextSections, DebugPhase, DebugUsage, SectionMeta,
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
    /// Create a SectionContent from the raw content string.
    pub fn new(content: String) -> Self {
        use sha2::{Digest, Sha256};

        let size_bytes = content.len();
        let token_estimate = content.len() / 4; // heuristic: ~4 bytes per token
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
    /// Registered breakpoints
    pub breakpoints: Vec<Breakpoint>,
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
    /// Breakpoint ID counter for generating unique IDs
    next_bp_id: u64,
}

impl DebugController {
    /// Create a new DebugController in Stepping state (auto-pause after first iteration).
    pub fn new() -> Self {
        Self {
            state: DebugState::Stepping,
            phase: DebugPhase::Idle,
            iteration: 0,
            breakpoints: Vec::new(),
            conversation_snapshots: Vec::new(),
            context_snapshots: HashMap::new(),
            pending_patches: None,
            rewind_target: None,
            re_execute_pending: false,
            next_bp_id: 1,
        }
    }

    /// Generate a unique breakpoint ID.
    pub fn next_breakpoint_id(&mut self) -> String {
        let id = format!("bp-{:03}", self.next_bp_id);
        self.next_bp_id += 1;
        id
    }

    /// Add a breakpoint and return its assigned ID.
    pub fn add_breakpoint(&mut self, condition: super::protocol::BreakpointCondition) -> String {
        let id = self.next_breakpoint_id();
        self.breakpoints.push(Breakpoint {
            id: id.clone(),
            enabled: true,
            condition,
        });
        id
    }

    /// Remove a breakpoint by ID. Returns true if found and removed.
    pub fn remove_breakpoint(&mut self, bp_id: &str) -> bool {
        let len_before = self.breakpoints.len();
        self.breakpoints.retain(|bp| bp.id != bp_id);
        self.breakpoints.len() < len_before
    }

    /// Check if any breakpoint matches the current phase and iteration.
    /// Returns the matching breakpoint IDs.
    pub fn check_breakpoints(&self, phase: DebugPhase, tool_name: Option<&str>) -> Vec<String> {
        self.breakpoints
            .iter()
            .filter(|bp| {
                if !bp.enabled {
                    return false;
                }
                match &bp.condition {
                    super::protocol::BreakpointCondition::OnPhase { phase: bp_phase } => {
                        let phase_str = format!("{:?}", phase);
                        phase_str.eq_ignore_ascii_case(bp_phase)
                    }
                    super::protocol::BreakpointCondition::OnIteration { iteration } => {
                        self.iteration == *iteration
                    }
                    super::protocol::BreakpointCondition::OnToolCall { tool_name_pattern } => {
                        if let Some(name) = tool_name {
                            // Simple glob: supports exact match and `*` suffix prefix
                            if let Some(prefix) = tool_name_pattern.strip_suffix('*') {
                                name.starts_with(prefix)
                            } else {
                                name == tool_name_pattern.as_str()
                            }
                        } else {
                            false
                        }
                    }
                    super::protocol::BreakpointCondition::OnToolResult { is_error: _ } => {
                        // Tool result breakpoints are handled post-execution
                        false
                    }
                }
            })
            .map(|bp| bp.id.clone())
            .collect()
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
        self.breakpoints.clear();
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
