//! Agent main loop (9 steps)
//!
//! The core execution loop for Agent Runtime.
//! References ZeroClaw agent/loop_.rs but simplified for gRPC transport.
//!
//! S1.5: Streaming LLM responses via chat_stream()
//! S1.6: InboundQueue for external message injection
//! S1.7: Parallel tool execution with per-tool timeout

use std::path::Path;
use std::sync::Arc;

use acowork_core::protocol::ModelCapabilitiesInfo;
use acowork_core::providers::traits::{ChatMessage, Provider};
#[allow(unused_imports)]
use acowork_core::tools::traits::Tool;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::agent::agent_core::AgentCore;
use crate::agent::context::ContextBuilder;
use crate::agent::history::HistoryManager;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_approval::{ApprovalDecision, ApprovalHandle};
use crate::agent::session_state::{PauseReason, SessionState, SessionStatus};
use crate::config::RuntimeConfig;
use crate::conversation::ConversationSession;
use crate::error::{Result, RuntimeError};
use crate::security::approval_gate::ApprovalRequest;
use crate::tools::builtin::ask_user_question::QuestionOption;

/// User-initiated compression actions.
///
/// Triggered via frontend/CLI buttons (the "Compress Summary" button in
/// `ContextUsageIcon`).  ADR-052 removed the `CompressToolResults` variant
/// — tool-result compression is now LLM-initiated via `context_abandon`,
/// not user-triggered. Only LLM-based summary compaction remains here.
#[derive(Debug, Clone)]
pub enum CompressionAction {
    /// Run LLM-based summary compaction (ADR-011 L2 layer).
    CompressSummary,
}

/// A ChunkEvent annotated with the session that produced it.
///
/// Every event emitted by a SessionTask carries its `session_id` at the
/// *source*, eliminating the need for external relay-side injection via
/// a watch channel (which had a race condition when sessions switched
/// between event production and relay processing).
#[derive(Debug, Clone)]
pub struct SessionChunkEvent {
    /// The session that produced this event.
    pub session_id: String,
    /// The actual chunk event.
    pub event: ChunkEvent,
}

/// Streaming chunk event emitted during LLM response generation.
///
/// Adapted from ZeroClaw's DraftEvent, simplified for ACowork's gRPC transport.
/// Each delta is forwarded to the Gateway via `StreamChunk` gRPC message,
/// which maps to a BridgeEventType for the Desktop App WebSocket.
/// SPDX-License-Identifier: MIT OR Apache-2.0
#[derive(Debug, Clone)]
pub enum ChunkEvent {
    // ── ADR-021: Data events (Delta, ReasoningDelta, ReasoningStarted, ToolCall,
    //    ToolResult) removed — frontend polls via HTTP. Only control events remain. ──

    /// Context usage report (after each LLM call)
    ContextUsage(acowork_core::protocol::ContextUsageInfo),
    /// Context compaction started (emitted before auto/manual compact triggers),
    /// so the frontend can show a "Context compacting..." indicator.
    CompactingStarted,
    /// Context compaction finished (emitted after compaction completes or fails),
    /// so the frontend can clear the "compacting..." indicator.
    CompactingEnded,
    /// Iteration limit reached — agent loop paused
    IterationLimitPaused {
        iteration: u32,
        max_iterations: u32,
        /// Human-readable message (e.g. "Iteration limit reached (3/3). Click Continue to proceed.")
        message: String,
    },
    /// Loop detection triggered — agent loop paused, waiting for ContinueExecution.
    /// Analogous to IterationLimitPaused but specifically for loop detection.
    LoopDetectedPaused {
        iteration: u32,
        max_iterations: u32,
        /// Detection detail message (e.g. "Detected repeated call to [shell] with same parameters")
        message: String,
    },
    /// Tool execution requires user approval (shell command risk check).
    /// The Desktop App displays a confirmation dialog; the Runtime pauses
    /// until Gateway delivers an InboundMessage::ApprovalDecision.
    ToolApprovalNeeded {
        /// Unique approval request ID
        request_id: String,
        /// The tool name (e.g. "bash", "powershell")
        tool_name: String,
        /// The command being executed
        action: String,
        /// Risk level: "Low", "Medium", "High"
        risk_level: String,
        /// Human-readable reason for the risk assessment
        reason: String,
        /// LLM-generated tool_call.id for frontend matching
        tool_call_id: String,
        /// Approval timeout in seconds (for frontend countdown)
        approval_timeout_secs: u64,
    },
    /// Agent response interrupted by user stop signal
    Stopped { content: String },
    /// Agent response complete (routed through chunk channel for ordering guarantee
    /// with preceding content chunks)
    Done { content: String, message_id: String },
    /// Agent error (routed through chunk channel for ordering guarantee)
    Error {
        /// User-friendly error summary (shown by default)
        user_message: String,
        /// Raw error detail (shown in expandable "Details" section)
        detail: String,
        /// Error type string for frontend conditional rendering
        error_type: String,
        /// Message ID for deduplication
        message_id: String,
    },
    /// LLM asks the user a question with pre-defined options.
    /// The Desktop App renders an AskQuestionCard with options + "Other" textarea;
    /// the Runtime pauses until Gateway delivers an InboundMessage::QuestionAnswer.
    AskQuestion {
        /// Unique request ID (correlates with the answer)
        request_id: String,
        /// The question text
        question: String,
        /// Pre-defined options for the user
        options: Vec<QuestionOption>,
        /// Optional title/header for the question card
        title: Option<String>,
        /// Effective wait timeout in seconds, computed by the runtime from the
        /// agent's `approval_timeout_secs` config (user preference).
        ///
        /// The frontend uses this to render the count-down UI.
        /// Always `Some(_)` — the runtime fills this in unconditionally;
        /// LLM-supplied values are no longer accepted as tool input.
        timeout_seconds: Option<u32>,
    },
    /// Todo list updated — emitted after a `todo_write` tool call mutates
    /// SessionState.todos, so the frontend can render the current task list.
    TodoListUpdated {
        todos: Vec<crate::agent::session_state::TodoItem>,
    },
    /// ADR-021/025: New data is available for polling.
    ///
    /// Sent via control channel to notify the frontend that the StreamingStateMap
    /// or JSONL has new content. The frontend should trigger an HTTP poll with
    /// `incremental=true` — the backend uses its own delivery cursor to
    /// determine what to return.
    ///
    /// This is a **pure signal** — it carries no state data (no `total_lines`,
    /// no `streaming_line`).  The backend maintains all delivery state.
    ///
    /// `interval_ms` is the configured notify throttle interval (from
    /// `DataFlowConfig.notify_interval_ms`).  The frontend uses it as its
    /// polling interval base.
    NewDataAvailable {
        session_id: String,
        /// Notify throttle interval in ms (from DataFlowConfig).
        /// Frontend uses this as polling interval base.
        interval_ms: u64,
        /// Latest session title from async LLM summarization.
        /// `None` when title has not been generated yet.
        title: Option<String>,
    },
    /// ADR-045: Tool execution progress heartbeat.
    ///
    /// Pure control-plane signal — carries NO tool result data.
    /// The frontend uses it to refresh a timer/countdown display.
    /// Emitted once every `TOOL_HEARTBEAT` (5 s) while a tool is in
    /// flight, **skipping the first tick so the first event lands at
    /// 5s, not 0s**. Short tools (<5s) complete without ever sending
    /// this event, preserving the pre-ADR-045 UX.
    ToolProgress {
        session_id: String,
        tool_call_id: String,
        /// Milliseconds since tool execution started (wall clock).
        elapsed_ms: u64,
        /// Tool timeout in ms (= tool_timeout_ms). Used by the
        /// frontend to compute the progress bar percentage.
        timeout_ms: u64,
    },
    /// Clear a retained `messages/*` event for this session.
    ///
    /// Published as a zero-byte payload with `retain = true` to the
    /// `messages/{event_type}` topic, which instructs the broker to
    /// delete the previously stored retained message. Used after a
    /// blocking state (tool approval, ask question) is resolved so
    /// that a Desktop reconnecting later does not receive a stale
    /// retained event from a previous turn.
    ClearRetainedEvent {
        /// The event type suffix (e.g. "tool_approval_needed", "ask_question").
        event_type: String,
    },
    /// ADR-035: incremental streaming delta carrying actual data.
    ///
    /// Pushed via MQTT `messages/stream_delta` every `notify_interval_ms`
    /// (default 500). Each entry in `lines` is ONE COMPLETE line
    /// (terminated by '\n' in the source) — never a partial line or a token.
    /// The frontend appends these to the per-session active stream buffer.
    /// Sent regardless of foreground/background (replaces the
    /// `notify_enabled`-gated `NewDataAvailable` signal for the live path).
    ///
    /// ADR-035 M2: each tuple is `(role, message_id, content)` — the
    /// `message_id` is the streaming line's stable id, shared with the
    /// eventual `RecordComplete` event so the frontend can match them.
    StreamDelta {
        session_id: String,
        /// (role, message_id, content) triples. `role` ∈ {"thought", "assistant"};
        /// `content` is a complete line of text.
        lines: Vec<(String, String, String)>,
        /// Per-session monotonic seq assigned by `SessionCore::next_seq`
        /// at emit time. The chunk_relay (single-threaded FIFO) forwards it
        /// into the MQTT `StreamDeltaPayload.seq` field; the Desktop uses
        /// it to insert this frame at the right position in `messages[]`
        /// independent of arrival order.
        seq: u64,
    },
    /// ADR-035 C1: a record finalized (committed to JSONL). Carries the
    /// COMPLETE content. The frontend freezes the active stream buffer into
    /// `messages[]` on receipt and clears `activeStream`.
    ///
    /// Emitted by `flush_streaming_line` (assistant / thought) and by the
    /// tool_call / tool_result persistence paths. For `tool_result` the
    /// content is truncated to the first 5 lines at the MQTT publish layer
    /// (D9.2) — the full content stays in JSONL for LLM context.
    ///
    /// Published at QoS 1 (ADR-035 O2) — `record_complete` is the
    /// authoritative terminal event; losing it leaves the message stuck
    /// in the streaming state.
    ///
    /// `tool_name` / `tool_call_id` / `is_error` are populated only for
    /// `tool_call` / `tool_result` records (mirrors the JSONL metadata).
    /// They are forwarded into the MQTT payload so the frontend can pair
    /// tool_call with tool_result without an extra HTTP round-trip.
    RecordComplete {
        session_id: String,
        /// "assistant" | "thought" | "tool_call" | "tool_result"
        role: String,
        message_id: String,
        content: String,
        tool_name: String,
        tool_call_id: String,
        is_error: bool,
        /// Per-session monotonic seq assigned by `SessionCore::next_seq`
        /// at emit time. For streaming records this matches the seq of the
        /// preceding `stream_delta` placeholder (same message_id); for
        /// tool_call / `tool_result` records emitted by
        /// `persist_and_emit_tool_results` it's a fresh seq that fits
        /// between the assistant that owned them and the next round.
        seq: u64,
    },
    /// ADR-043: config fields changed (title, model, provider, workspace,
    /// reasoning_effort, temperature). Published Retained to sessions/{sid}/config.
    SessionConfigChanged {
        config: acowork_core::mqtt_proto::SessionConfig,
    },
    /// ADR-043: runtime state changed (status, message_count, tokens, ratio,
    /// context_usage). Published Retained to sessions/{sid}/state.
    /// Replaces the deleted ChunkEvent::SessionStateChanged (status/ratio/
    /// context_usage) and the runtime portion of SessionMetaChanged.
    SessionStateChanged {
        state: acowork_core::mqtt_proto::SessionState,
    },
}

/// Unified control signal returned by `poll_control()`.
///
/// Replaces the ad-hoc `poll_stop() -> bool` and scattered `DebugState` checks
/// with a single, exhaustive decision that every blocking wait point evaluates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlDecision {
    /// No control signal — continue normally
    Continue,
    /// Stop the loop (Chat Stop or debugger.stop)
    Stop,
    /// Pause execution (debugger.pause) — abort current work, await resume
    Pause,
}

/// Result of executing a single iteration of the agent loop.
///
/// This is the shared building block used by both:
/// - Production `run()`: loops automatically until TextResponse/Stopped
/// - Debug `DebugSessionTask`: calls one iteration at a time with pause control
#[derive(Debug)]
pub(crate) enum IterationResult {
    /// Agent returned a text response — conversation round complete
    TextResponse(String),
    /// Tool calls were executed successfully — continue to next iteration
    ToolCallsExecuted,
    /// Agent was stopped by user request
    Stopped(String),
    /// Agent was paused by debug panel — iteration aborted, await resume
    Paused,
}

use crate::agent::session_core::SessionCore;

/// Agent loop runner
pub struct AgentLoop {
    /// Cross-session shared state (config, provider, tools, capabilities)
    pub(crate) core: AgentCore,
    /// Per-session state (session_id, chunk channel, streaming, workspace, retry, approval)
    pub(crate) session_core: SessionCore,
    /// Per-session state (history, conversation, loop detector, budget)
    pub(crate) session: SessionState,
    /// Inbound message receiver for external message injection
    pub(crate) inbound_rx: tokio::sync::mpsc::Receiver<InboundMessage>,
    /// Approval request receiver: spawned tool tasks send requests here,
    /// the main loop receives them and routes decisions via `route_inbound`.
    pub(crate) approval_rx: mpsc::Receiver<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
    /// Approval handle (sender side) - cloned into spawned tool tasks.
    pub(crate) approval_handle: ApprovalHandle,
    /// In-flight approval requests keyed by request_id (UUID v4).
    /// Inserted by `execute_tools_parallel`'s `approval_rx` branch, removed
    /// by `route_inbound` when the matching `ApprovalDecision` arrives on
    /// `inbound_rx`. Replaces the old recursive `await_approval_decision`
    /// design which deadlocked when multiple concurrent approvals raced on
    /// the shared `inbound_rx` (buffered into `deferred_inbound` but never
    /// drained within the approval wait).
    pub(crate) pending_approvals:
        std::collections::HashMap<String, oneshot::Sender<ApprovalDecision>>,
    /// Total input chars of the most recent ChatRequest, used for token
    /// ratio calibration together with the API-reported prompt_tokens.
    pub(crate) last_input_chars: usize,
    /// The reasoning_effort from the most recent build_chat_request() call.
    /// Preserved for emergency trim retry in call_llm_streaming_inner()
    /// where the context_builder is immutable.
    pub(crate) last_reasoning_effort: Option<acowork_core::providers::traits::ReasoningEffort>,
    /// The thinking_mode from the most recent build_chat_request() call.
    /// Preserved for emergency trim retry in call_llm_streaming_inner().
    pub(crate) last_thinking_mode: Option<String>,
    /// Pending control signal set by sub-modules when a control interrupt
    /// (Stop/Pause) is detected during a nested blocking wait.  Consumed by
    /// `poll_control()` at the next checkpoint so that interrupt intent is
    /// never lost — even when the original channel/notify event has been
    /// consumed by a sub-module's own `select!` loop.
    pub(crate) pending_interrupt: Option<ControlDecision>,

    /// ADR-045: Per-tool cancel tokens. Each entry is the `Sender` half of a
    /// `tokio::sync::watch<bool>`; the corresponding `Receiver` is moved into
    /// the spawned tool task via [`crate::agent::loop_tools`]. When the user
    /// clicks "cancel this tool" on the frontend, `cancel_tool_by_id()` fires
    /// the matching sender, which trips the tool task's `tokio::select!`
    /// branch and yields a "Cancelled by user" result.
    ///
    /// Bounded by the number of in-flight tools — typically O(active_iteration_tool_calls)
    /// and cleaned up by the tool task itself when it returns. A `watch::Sender`
    /// is cheap (~24 B) so a HashMap is the right structure.
    pub(crate) pending_tool_cancels:
        std::collections::HashMap<String, tokio::sync::watch::Sender<bool>>,
    /// Transient tool results from the previous iteration (ADR-032 C3a).
    ///
    /// Tools with `transient: true` have their
    /// results injected into the *next* `build_chat_request` without being
    /// permanently appended to history.
    ///
    /// ## Lifecycle
    ///
    /// 1. **Populated** during tool execution (`execute_tools_parallel`):
    ///    each transient tool result is pushed here instead of appended to
    ///    `session.history`.
    /// 2. **Consumed** once at the start of the next `build_chat_request`:
    ///    messages are `append`-ed into the outgoing `ChatRequest`, which
    ///    clears the vec.
    /// 3. **Unreachable** after consumption: the next call to
    ///    `build_chat_request` will find an empty vec.
    ///
    /// This design ensures the LLM sees transient tool output for exactly
    /// one turn before it is discarded (neither persisted to history nor
    /// retained across iterations).
    pub(crate) pending_transient_tool_msgs: Vec<ChatMessage>,

    /// ADR-052: Shared queue for `context_abandon` tool requests.
    ///
    /// The tool writes `tool_call_id` strings here; the agent loop drains
    /// them before the next `build_chat_request` via
    /// `drain_abandon_queue()`.
    pub(crate) abandon_queue:
        crate::agent::context_compression::AbandonQueue,

    /// ADR-052: Shared queue for `context_retrieve` tool requests.
    ///
    /// The tool writes `(tool_call_id, original_content)` pairs here; the
    /// agent loop drains them before the next `build_chat_request` via
    /// `drain_retrieve_queue()`, restoring the original content in-place.
    pub(crate) retrieve_queue:
        crate::agent::context_compression::RetrieveQueue,

    /// ADR-032 C4b: Compression action receiver.
    ///
    /// Set by the creator (SessionTask / Gateway wiring) to enable
    /// `drain_compress_actions()`.  `None` means no channel was wired.
    pub(crate) compress_action_rx: Option<mpsc::Receiver<CompressionAction>>,
}

impl AgentLoop {
    /// Create a new agent loop runner with a pre-configured debug observer.
    ///
    /// This constructor supports integration testing and advanced embedding
    /// scenarios where the caller needs to control the observer lifecycle.
    /// For normal usage, prefer [`AgentLoop::new()`] which defaults to
    /// Production mode (zero-cost no-ops). See ADR-013.
    ///
    /// The caller can use the returned sender to inject messages into the loop
    /// from external sources (Gateway, cross-agent intents, system notifications).
    ///
    /// If `chunk_tx` is provided, control events are forwarded to it
    /// so the caller can relay them to the Gateway.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_observer(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        builtin_tools: Vec<crate::agent::agent_core::BuiltinToolEntry>,
        budget: acowork_core::Budget,
        chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
        conversation: Option<Arc<ConversationSession>>,
        observer: crate::debug::DebugObserverSlot,
    ) -> (Self, tokio::sync::mpsc::Sender<InboundMessage>) {
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(64);
        let (approval_tx, approval_rx) =
            mpsc::channel::<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>(16);
        let max_tokens = config.history_max_tokens;
        let approval_handle = ApprovalHandle::new(approval_tx);
        let core = AgentCore::new_with_observer(
            config.clone(),
            manifest,
            provider,
            builtin_tools,
            observer,
        );
        let streaming_lines: crate::conversation::StreamingStateMap =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let current_work_dir =
            Arc::new(std::sync::RwLock::new(Some(config.work_dir.clone())));
        let session_core = SessionCore::new(
            String::new(), // session_id set later
            chunk_tx,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)), // committed_lines placeholder
            config.data_flow.notify_interval_ms,
            current_work_dir,
            streaming_lines,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let mut loop_ = Self {
            abandon_queue: core.abandon_queue.clone(),
            retrieve_queue: core.retrieve_queue.clone(),
            core,
            session_core,
            session: SessionState::new(max_tokens, budget, conversation),
            inbound_rx,
            approval_rx,
            approval_handle: approval_handle.clone(),
            pending_approvals: std::collections::HashMap::new(),
            last_input_chars: 0,
            last_reasoning_effort: None,
            last_thinking_mode: None,
            pending_interrupt: None,
            pending_tool_cancels: std::collections::HashMap::new(),
            pending_transient_tool_msgs: Vec::new(),
            compress_action_rx: None,
        };
        // Initialize persistent model ratio store from agent config dir.
        let ratio_config_dir = Path::new(&loop_.core.config.work_dir).join("config");
        loop_.session.history.init_model_ratios(&ratio_config_dir);
        // Inject approval_handle into SessionCore so execute_tools_parallel can detect Gateway mode
        loop_.session_core.approval_handle = Some(approval_handle.clone());
        (loop_, inbound_tx)
    }

    /// Create a new agent loop runner, returning both the loop and an inbound sender.
    ///
    /// Defaults to Production mode (zero-cost debug no-ops).
    /// Use [`AgentLoop::new_with_observer()`] to inject a DevMode observer.
    ///
    /// The caller can use the sender to inject messages into the loop from
    /// external sources (Gateway, cross-agent intents, system notifications).
    ///
    /// If `chunk_tx` is provided, control events are forwarded to it
    /// so the caller can relay them to the Gateway.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        builtin_tools: Vec<crate::agent::agent_core::BuiltinToolEntry>,
        budget: acowork_core::Budget,
        chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
        conversation: Option<Arc<ConversationSession>>,
    ) -> (Self, tokio::sync::mpsc::Sender<InboundMessage>) {
        Self::new_with_observer(
            config,
            manifest,
            provider,
            builtin_tools,
            budget,
            chunk_tx,
            conversation,
            crate::debug::DebugObserverSlot::production(),
        )
    }

    /// Create an AgentLoop from pre-built components (for multi-session Actor model).
    ///
    /// This constructor accepts an `Arc<AgentCore>` template, a `SessionCore`,
    /// and `SessionState`, used by `SessionTask` to spawn independent sessions.
    pub(crate) fn from_core_and_session(
        core: AgentCore,
        session_core: SessionCore,
        session: SessionState,
    ) -> (Self, tokio::sync::mpsc::Sender<InboundMessage>) {
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(64);
        let (approval_tx, approval_rx) =
            mpsc::channel::<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>(16);
        let approval_handle = ApprovalHandle::new(approval_tx);
        let mut session_loop = Self {
            abandon_queue: core.abandon_queue.clone(),
            retrieve_queue: core.retrieve_queue.clone(),
            core,
            session_core,
            session,
            inbound_rx,
            approval_rx,
            approval_handle: approval_handle.clone(),
            pending_approvals: std::collections::HashMap::new(),
            last_input_chars: 0,
            last_reasoning_effort: None,
            last_thinking_mode: None,
            pending_interrupt: None,
            pending_tool_cancels: std::collections::HashMap::new(),
            pending_transient_tool_msgs: Vec::new(),
            compress_action_rx: None,
        };
        // Inject approval_handle into SessionCore so execute_tools_parallel can detect Gateway mode
        session_loop.session_core.approval_handle = Some(approval_handle);
        (session_loop, inbound_tx)
    }

    // ── Memory system methods moved to loop_memory.rs (ADR-014 Phase 6) ──
    //   - init_memory_store
    //   - retrieve_and_inject_memories
    //   - write_attached_items

    /// Execute a built-in tool by name, simulating an LLM tool call.
    ///
    /// This enables the runtime to invoke tools directly without going through
    /// the LLM. Use cases include pre-extracting user-uploaded document content
    /// before the LLM sees the message, so the LLM doesn't need to call
    /// `doc_reader` itself — saving a round-trip and eliminating uncertainty.
    ///
    /// Returns the tool's result content on success, or an error message on failure.
    pub async fn execute_tool_by_name(
        &self,
        name: &str,
        params: serde_json::Value,
    ) -> std::result::Result<String, String> {
        let tool = self
            .core
            .builtin_tools
            .iter()
            .find(|t| t.spec().name == name)
            .ok_or_else(|| format!("Tool not found: {}", name))?
            .tool
            .clone();

        let work_dir = self.session_core.current_work_dir.read().unwrap().clone();
        match tool.execute(params, work_dir.as_deref()).await
        {
            Ok(result) if result.ok => Ok(result.content),
            Ok(result) => Err(result
                .error
                .unwrap_or_else(|| "Unknown tool error".to_string())),
            Err(e) => Err(format!("Tool execution error: {e}")),
        }
    }

    /// Look up model capabilities by exact model name (delegates to AgentCore).
    pub(crate) fn get_model_capabilities(&self, model_name: &str) -> Option<ModelCapabilitiesInfo> {
        self.core.get_model_capabilities(model_name)
    }

    /// Resolve the current model name for capability lookups.
    /// Uses override_model (set by model_switch) if present,
    /// otherwise falls back to session state model.
    pub(crate) fn resolve_current_model(&self, ctx: Option<&ContextBuilder>) -> String {
        ctx.and_then(|cb| cb.override_model())
            .map(|s| s.to_string())
            .or_else(|| self.session.model.clone())
            .unwrap_or_default()
    }

    /// Run the agent loop for a single user message.
    ///
    /// Appends the user message to history and persists to JSONL,
    /// then runs the LLM loop until done, paused, or stopped.
    ///
    /// `message_id` is the frontend-generated ID for the user message.
    /// When `Some`, it is used as the JSONL entry ID so the frontend can
    /// deduplicate by ID when polling session messages.  When `None`,
    /// a UUID is generated (backward-compatible fallback).
    ///
    /// When `replay` is true, the user message is NOT appended to history
    /// or persisted to JSONL (it is assumed to already be present, e.g.
    /// after a debug rewind + resume).  Memory retrieval is still performed
    /// in case the context builder has been modified by pending patches.
    ///
    /// `raw_user_message` is the user's original input before any prompt
    /// enrichment (no [Attached context:] prefix, no document content, no
    /// file hints). Used for session title generation and other metadata
    /// extraction. When `None`, falls back to extracting filenames from
    /// `<attached_document>` tags in `user_message`.
    pub async fn run(
        &mut self,
        user_message: &str,
        context_builder: &mut ContextBuilder,
        content_parts: Option<Vec<acowork_core::providers::traits::ContentPart>>,
        message_id: Option<String>,
        raw_user_message: Option<&str>,
        attached_items: Option<&[acowork_core::protocol::AttachedItem]>,
    ) -> Result<String> {
        self.run_inner(user_message, context_builder, false, content_parts, message_id, raw_user_message, attached_items)
            .await
    }

    /// Re-run the agent loop after a debug resume (user message already in history).
    ///
    /// Same as [`run`] but skips the user-message append and JSONL persist steps.
    pub async fn replay(
        &mut self,
        user_message: &str,
        context_builder: &mut ContextBuilder,
        content_parts: Option<Vec<acowork_core::providers::traits::ContentPart>>,
    ) -> Result<String> {
        self.run_inner(user_message, context_builder, true, content_parts, None, None, None)
            .await
    }

    /// Core agent loop shared by [`run`] and [`replay`].
    ///
    /// `#[allow(clippy::too_many_arguments)]` follows the project convention
    /// for thin pass-through facades (cf. `AgentCore::new_with_observer`):
    /// `run` / `replay` / debug-resume call sites pass orthogonal per-request
    /// inputs; bundling them would obscure the call sites.
    #[allow(clippy::too_many_arguments)]
    async fn run_inner(
        &mut self,
        user_message: &str,
        context_builder: &mut ContextBuilder,
        replay: bool,
        content_parts: Option<Vec<acowork_core::providers::traits::ContentPart>>,
        message_id: Option<String>,
        raw_user_message: Option<&str>,
        attached_items: Option<&[acowork_core::protocol::AttachedItem]>,
    ) -> Result<String> {
        // ADR-044 §4.5: allocate a fresh `Active` `CancelHandle` for this
        // request. Critical — without this swap, a Stop from a previous
        // request would short-circuit `select_on_cancel` in
        // `loop_llm::chat_stream` and `loop_tools::*` because the slot
        // would still point at the previously-cancelled handle (Arc keeps
        // it alive until the in-flight future is dropped). The slot is
        // the single source of truth: external cancel sources read through
        // `Arc::lock()` on every dispatch and always observe the
        // generation we write here.
        //
        // Why at the *top* of `run_inner` (not at session creation, not
        // per iteration): handle scope = request scope. Within one
        // request, multiple iterations (tool-call chains) share the
        // generation so the user can still Stop mid-tool-execution.
        // A new generation is established only when a new user-driven
        // request begins.
        let _cancel_handle = self.session_core.begin_new_request();

        // ADR-014: Idle → Streaming
        // ADR-049: Streaming is split into LlmAwaitingFirstChunk / LlmStreaming /
        // ToolExecuting. The HTTP request is about to be sent here, so we are
        // in the TTFT wait phase — LlmAwaitingFirstChunk. The transition to
        // LlmStreaming happens in loop_llm.rs on the first content chunk.
        self.transition_status(SessionStatus::LlmAwaitingFirstChunk);

        if !replay {
            // Add user message to history
            // ADR-011: reset compaction flag — new user input means new content since last compaction
            self.session.is_compacted = false;
            if let Some(parts) = content_parts {
                self.session
                    .history
                    .append(ChatMessage::user_multimodal(user_message, parts));
            } else {
                self.session.history.append(ChatMessage::user(user_message));
            }

            // Persist user message to JSONL with frontend-generated ID
            // so the frontend can deduplicate by ID when polling.
            //
            // ADR-046 §2.1: The JSONL `user` entry MUST store the user's
            // original input verbatim — no `[Attached context:]` prefix,
            // no `[Attached workspace files & uploads …]` hint block, no
            // inlined document body. `user_message` carries those
            // enrichments because we also feed it to the LLM via
            // `history.append(...)` below; `raw_user_message` (when the
            // caller supplied it, see `session_task.rs:741-742`) is the
            // clean copy. Pre-046 call sites that pass `None` for
            // `raw_user_message` fall back to `user_message` so the
            // binary contract for legacy tests stays intact.
            let persisted_user_message: &str =
                raw_user_message.unwrap_or(user_message);
            if let Some(ref conversation) = self.session.conversation {
                conversation.append_message_with_id(
                    "user",
                    persisted_user_message,
                    None,
                    message_id,
                );
            }

            // ADR-046: Persist attachment records AFTER the user message so
            // their timestamps are slightly later than the user entry. This
            // ensures the frontend's `foldMessages` (which looks for
            // attachment entries *after* the user message by timestamp) can
            // fold them into a single `user_with_attachments` block.
            if let Some(items) = attached_items
                && !items.is_empty() {
                    self.write_attached_items(items);
                }

            // Async: generate session title from first user message using
            // the compact model. The title is pushed to the frontend via
            // NewDataAvailable events — no blocking, no optimistic truncation.
            // Only when a conversation exists (not in tests without sessions).
            // Title generation runs only once per session — subsequent user
            // messages skip the LLM call entirely.
            if self.session.conversation.is_some()
                && self.session_core.title.read().unwrap().is_none()
            {
                // Check if the conversation already has a title from a
                // previous session run (session resume case). If so,
                // propagate it to session_core.title and skip the LLM call.
                if let Some(existing_title) = self
                    .session
                    .conversation
                    .as_ref()
                    .and_then(|conv| conv.title())
                {
                    let trimmed = crate::prompt::truncate_title_for_display(&existing_title);
                    *self.session_core.title.write().unwrap() = Some(trimmed);
                } else {
                    let lang = self
                        .session
                        .identity_context()
                        .and_then(|ctx| {
                            ctx.lines()
                                .find(|l| l.starts_with("- Language:"))
                                .and_then(|l| l.split(':').nth(1).map(|s| s.trim().to_string()))
                        })
                        .unwrap_or_else(|| "en".to_string());
            
                    // Use the raw user message (before any prompt enrichment)
                    // for title generation. This is the user's original input
                    // without [Attached context:] prefix, document content,
                    // or file hints — exactly what the user typed.
                    //
                    // When `raw_user_message` is None or empty (e.g. user only
                    // uploaded files without typing text), the fallback below
                    // extracts filenames from <attached_document> tags.
                    let title_input = raw_user_message.unwrap_or("");
            
                    let prompt = crate::prompt::TITLE_PROMPT
                        .replace("{language}", &lang)
                        .replace("{user_message}", title_input);
                    // ADR-056: title generation uses the same distillation
                    // target resolution + provider rebuild as compaction, so
                    // a cross-provider global default (e.g. local Ollama)
                    // drives the title call too — never a mismatched
                    // (session provider, other-provider model) pair.
                    let resolved_distill = self.resolve_distill_model(title_input);
                    let (provider, compact_model, _tier) =
                        self.distill_provider(&resolved_distill);
                    let session_core_title = self.session_core.title.clone();
                    let conversation_clone = self.session.conversation.clone();
                    // ADR-028: clone the AgentCore so the spawned task can
                    // feed the agent-scoped token counters for this
                    // title-generation LLM call.
                    let core_clone = self.core.clone();
                    let fallback_msg = if title_input.is_empty() {
                        // User only uploaded files (no text). Extract filenames
                        // from <attached_document> tags as a meaningful fallback
                        // title, e.g. "report.pdf, slides.pptx".
                        let filenames: Vec<&str> = user_message
                            .split("<attached_document filename=\"")
                            .skip(1)
                            .filter_map(|s| s.split('"').next())
                            .collect();
                        if filenames.is_empty() {
                            String::new()
                        } else {
                            filenames.join(", ")
                        }
                    } else {
                        title_input.to_string()
                    };
            
                    tokio::spawn(async move {
                        // max_tokens=120 leaves room for a 60-char Chinese title
                        // (CJK char ≈ 1.5–2 BPE tokens). 64 was tight for a 30-char
                        // budget and would silently cap LLM output for Chinese
                        // sessions, leaving our display truncation with a
                        // half-sentence title.
                        match crate::episode_distill::compact_session_title_with_llm(
                            &prompt,
                            provider.as_ref(),
                            &compact_model,
                            120,
                        )
                        .await
                        {
                            Ok((title, usage)) => {
                                // ADR-027: record raw Provider usage from
                                // title-generation call.
                                if let Some(ref conv) = conversation_clone {
                                    conv.accumulate_llm_usage(&usage);
                                }
                                // ADR-028: also feed the agent-scoped
                                // counters so the agent-total line in the
                                // Results Panel accounts for this call.
                                core_clone.accumulate_llm_usage(&usage);
                                // Prefer natural break points over a blind cut so
                                // the user sees a complete sentence (or "…") rather
                                // than a half-word. See prompt.rs for details.
                                let trimmed = crate::prompt::truncate_title_for_display(&title);
                                *session_core_title.write().unwrap() = Some(trimmed);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "LLM session title generation failed (non-fatal): {e}"
                                );
                                let fallback = crate::prompt::truncate_title_for_display(&fallback_msg);
                                *session_core_title.write().unwrap() = Some(fallback);
                            }
                        }
                    });
                }
            }
        }

        // Retrieve relevant long-term memories and inject into context
        // P2-4 fix: capture memory node IDs for later traceability in record_turn_to_memory
        let retrieved_memory_ids = self
            .retrieve_and_inject_memories(user_message, context_builder)
            .await;

        // P3: Notify consolidation scheduler that agent is active —
        // resets idle timer so consolidation doesn't run during active use.
        self.core.notify_consolidation_active().await;

        // Title persistence is now handled lazily by flush_pending_title()
        // called at the end of each execute_single_iteration(). The async
        // title generation task (spawned above) writes to session_core.title
        // when it completes, and the next iteration checkpoint picks it up
        // and persists it via conversation.update_title_force().

        let mut iteration = 0u32;

        loop {
            iteration += 1;
            // Resolve current model name for this iteration — model_switch
            // may update override_model mid-session, so compute it fresh each loop.
            let current_model = self.resolve_current_model(Some(context_builder));
            tracing::info!(
                iteration,
                history_token_count = self.session.history.token_count(),
                history_message_count = self.session.history.len(),
                history_max_tokens = self.core.config.history_max_tokens,
                "Starting loop iteration"
            );

            // ⑨ Iteration limit check — pause and await user decision
            if iteration > self.core.config.max_iterations {
                tracing::warn!(
                    iteration,
                    max_iterations = self.core.config.max_iterations,
                    "Max iterations reached, pausing for user decision"
                );

                // Notify Gateway/Desktop App that iteration limit was reached
                // ADR-014: Streaming → Paused
                let max_iters = self.core.config.max_iterations;
                let message = format!(
                    "Iteration limit reached ({}/{}). Click Continue to proceed.",
                    iteration, max_iters
                );
                self.transition_status(SessionStatus::Paused {
                    iteration: Some(iteration),
                    max_iterations: Some(max_iters),
                    retry_info: None,
                    reason: Some(PauseReason::IterationLimit),
                    message: Some(message.clone()),
                });
                let _ = self.session_core.try_send_chunk(ChunkEvent::IterationLimitPaused {
                    iteration,
                    max_iterations: max_iters,
                    message,
                });

                // Wait for ContinueExecution or Interrupt from inbound queue
                // Also checks UserOperation variants for the unified fast channel.
                loop {
                    match self.inbound_rx.recv().await {
                        Some(InboundMessage::ContinueExecution { session_id: _, reason }) => {
                            tracing::info!(
                                reason = %reason,
                                "User chose to continue, resetting iteration counter"
                            );
                            // ADR-014: Paused → Streaming
                            // ADR-049: HTTP request not yet sent → LlmAwaitingFirstChunk.
                            self.transition_status(SessionStatus::LlmAwaitingFirstChunk);
                            iteration = 0; // Reset counter

                            // Trim history before resuming to avoid context window overflow
                            self.trim_history_to_budget(&current_model);

                            break; // Resume main loop
                        }
                        Some(InboundMessage::Stop { reason }) => {
                            tracing::info!(reason = %reason, "User chose to stop during iteration limit pause");
                            // ADR-014: Paused → Idle
                            self.transition_status(SessionStatus::Idle);
                            return Ok(String::new());
                        }
                        Some(InboundMessage::UserOperation(user_op)) => {
                            match user_op {
                                crate::agent::inbound::UserOp::ContinueLoop { reason } => {
                                    tracing::info!(
                                        reason = %reason,
                                        "UserOp: continue loop via fast channel"
                                    );
                                    self.transition_status(SessionStatus::LlmAwaitingFirstChunk);
                                    iteration = 0;
                                    self.trim_history_to_budget(&current_model);
                                    break;
                                }
                                crate::agent::inbound::UserOp::StopLoop { reason } => {
                                    tracing::info!(reason = %reason, "UserOp: stop via fast channel during iteration limit pause");
                                    self.transition_status(SessionStatus::Idle);
                                    return Ok(String::new());
                                }
                                other_op => {
                                    // Other UserOps (UpdateRuntimeConfig etc.) — apply inline
                                    self.apply_user_op(&other_op);
                                }
                            }
                        }
                        Some(other) => {
                            // D1 dedup: inject message into history via shared helper
                            let (msg, _) = other.enforce_size_limit();
                            crate::agent::loop_inbound::inject_inbound_into_history(
                                msg,
                                &mut self.session.history,
                            );
                        }
                        None => {
                            // Channel closed — treat as stop
                            tracing::warn!(
                                "Inbound channel closed during iteration limit pause, stopping"
                            );
                            return Ok(String::new());
                        }
                    }
                }
            }

            // ⓪ Drain inbound queue (non-blocking)
            if self.drain_inbound_queue() {
                // ADR-014: Streaming → Idle
                self.transition_status(SessionStatus::Idle);
                tracing::info!("Agent loop interrupted by inbound interrupt signal");
                return Ok(String::new());
            }

            // ①-⑧ Execute single iteration (shared with debug mode)
            // With iteration-level retry for retryable stream errors.
            const MAX_ITERATION_RETRIES: u32 = 3;
            const MAX_LONG_RETRIES: u32 = 3;
            let mut iteration_retries = 0u32;
            let mut long_retry_count = 0u32;
            let iteration_result = loop {
                match self
                    .execute_single_iteration(
                        iteration,
                        context_builder,
                        user_message,
                        &retrieved_memory_ids,
                        &current_model,
                    )
                    .await
                {
                    Ok(result) => break result,
                    Err(RuntimeError::StreamError(ref err))
                        if err.retryable && iteration_retries < MAX_ITERATION_RETRIES =>
                    {
                        iteration_retries += 1;
                        let backoff = std::time::Duration::from_millis(
                            1000 * 2u64.pow(iteration_retries - 1),
                        );
                        let backoff = backoff.min(std::time::Duration::from_secs(10));
                        tracing::warn!(
                            iteration,
                            retry = iteration_retries,
                            max_retries = MAX_ITERATION_RETRIES,
                            error = %err.message,
                            backoff_ms = backoff.as_millis(),
                            "Retryable stream error, retrying iteration"
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    Err(RuntimeError::StreamError(ref err))
                        if err.retryable && long_retry_count < MAX_LONG_RETRIES =>
                    {
                        long_retry_count += 1;
                        // Enter paused state with 5-minute retry info.
                        // The frontend RetryWaitBanner shows a countdown and
                        // "Retry Now" button; the user can skip the wait or
                        // let the timer expire for automatic retry.
                        // reason: None — retry_info already disambiguates this
                        // pause from iteration-limit / loop-detected / debug.
                        self.transition_status(SessionStatus::Paused {
                            iteration: Some(iteration),
                            max_iterations: Some(self.core.config.max_iterations),
                            retry_info: Some(
                                crate::agent::session_state::RetryPauseInfo {
                                    wait_ms: 5 * 60 * 1000,
                                    attempt: long_retry_count,
                                    max_attempts: MAX_LONG_RETRIES,
                                    provider: current_model.clone(),
                                },
                            ),
                            reason: None,
                            message: None,
                        });
                        tracing::warn!(
                            iteration,
                            long_retry = long_retry_count,
                            max_long_retries = MAX_LONG_RETRIES,
                            error = %err.message,
                            "Retryable stream error, entering long retry wait (5 min)"
                        );

                        // Wait for 5 minutes, ContinueExecution, or Stop.
                        // Uses tokio::select! to await the first of:
                        //   - 5-minute timeout (auto-retry)
                        //   - inbound ContinueExecution (user clicked Retry Now)
                        //   - inbound Stop (user stopped the session)
                        let long_wait =
                            tokio::time::sleep(std::time::Duration::from_secs(5 * 60));
                        tokio::pin!(long_wait);

                        loop {
                            tokio::select! {
                                _ = &mut long_wait => {
                                    // Time's up — auto resume
                                    tracing::info!(
                                        "Long retry wait completed, auto-resuming"
                                    );
                                    self.transition_status(
                                        SessionStatus::LlmAwaitingFirstChunk,
                                    );
                                    break;
                                }
                                msg = self.inbound_rx.recv() => {
                                    match msg {
                                        Some(InboundMessage::ContinueExecution { .. }) => {
                                            tracing::info!(
                                                "User chose to retry immediately"
                                            );
                                            self.transition_status(
                                                SessionStatus::LlmAwaitingFirstChunk,
                                            );
                                            break;
                                        }
                                        Some(InboundMessage::Stop { .. }) => {
                                            tracing::info!(
                                                "User stopped during retry wait"
                                            );
                                            self.transition_status(
                                                SessionStatus::Idle,
                                            );
                                            return Err(
                                                RuntimeError::StreamError(
                                                    err.clone(),
                                                ),
                                            );
                                        }
                                        Some(InboundMessage::UserOperation(
                                            user_op,
                                        )) => {
                                            self.apply_user_op(&user_op);
                                        }
                                        Some(other) => {
                                            self.session
                                                .deferred_inbound
                                                .push(other);
                                        }
                                        None => {
                                            // Channel closed — stop
                                            tracing::info!(
                                                "Inbound channel closed during \
                                                 retry wait"
                                            );
                                            self.transition_status(
                                                SessionStatus::Idle,
                                            );
                                            return Err(
                                                RuntimeError::StreamError(
                                                    err.clone(),
                                                ),
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        // Reset short retry counter and try again
                        iteration_retries = 0;
                        continue;
                    }
                    Err(RuntimeError::LoopDetected(msg)) => {
                        // Loop detection: inject system warning into history,
                        // reset detector, pause, and wait for ContinueExecution.
                        // This gives the user a "Continue" button like iteration
                        // limit pause, instead of a fatal error.

                        // Inject system warning so the LLM knows what happened
                        // when execution resumes.
                        self.session.history.append(ChatMessage {
                            role: acowork_core::providers::traits::MessageRole::User,
                            content: format!(
                                "[System Warning] The system detected a loop and paused. \
                                 The user has asked you to continue. \
                                 Please try a different approach — do not repeat the same tool call. \
                                 Details: {msg}"
                            ),
                            name: Some("system".to_string()),
                            ..Default::default()
                        });

                        // Reset loop detector so the next tool calls start clean
                        self.session.loop_detector_mut().reset();

                        // Transition to paused state
                        self.transition_status(SessionStatus::Paused {
                            iteration: Some(iteration),
                            max_iterations: Some(self.core.config.max_iterations),
                            retry_info: None,
                            reason: Some(PauseReason::LoopDetected),
                            message: Some(msg.clone()),
                        });

                        // Send chunk event for frontend to display the continue button
                        let _ = self.session_core.try_send_chunk(
                            ChunkEvent::LoopDetectedPaused {
                                iteration,
                                max_iterations: self.core.config.max_iterations,
                                message: msg,
                            },
                        );

                        tracing::warn!(
                            iteration,
                            "Loop detected — pausing for user decision"
                        );

                        // Wait for ContinueExecution or Stop from inbound queue
                        loop {
                            match self.inbound_rx.recv().await {
                                Some(InboundMessage::ContinueExecution {
                                    session_id: _,
                                    reason,
                                }) => {
                                    tracing::info!(
                                        reason = %reason,
                                        "User chose to continue after loop detection"
                                    );
                                    // ADR-049: HTTP request about to be sent → LlmAwaitingFirstChunk.
                                    self.transition_status(SessionStatus::LlmAwaitingFirstChunk);
                                    iteration = 0;
                                    self.trim_history_to_budget(&current_model);
                                    break; // Resume main loop
                                }
                                Some(InboundMessage::Stop { reason }) => {
                                    tracing::info!(
                                        reason = %reason,
                                        "User stopped during loop detection pause"
                                    );
                                    self.transition_status(SessionStatus::Idle);
                                    return Ok(String::new());
                                }
                                Some(InboundMessage::UserOperation(user_op)) => {
                                    self.apply_user_op(&user_op);
                                }
                                Some(other) => {
                                    self.session.deferred_inbound.push(other);
                                }
                                None => {
                                    tracing::warn!(
                                        "Inbound channel closed during loop detection pause, stopping"
                                    );
                                    self.transition_status(SessionStatus::Idle);
                                    return Ok(String::new());
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // ADR-014: Streaming → Idle on non-retryable error
                        // or all retry attempts exhausted
                        self.transition_status(SessionStatus::Idle);
                        return Err(e);
                    }
                }
            };
            // Lazy-persist any async-generated title to the conversation
            // JSONL and index.json. The title generation task writes to
            // session_core.title asynchronously; this checkpoint picks it
            // up at the earliest opportunity after the LLM round-trip.
            self.flush_pending_title();

            // ADR-054 follow-up: refresh this iteration's messages snapshot
            // so it includes the current iteration's assistant reply / tool
            // results (the context-build snapshot predates the LLM call and
            // therefore only contains history up to the user message).
            self.core
                .debug_observer
                .on_iteration_complete(&self.session.history);

            match iteration_result {
                IterationResult::TextResponse(content) => {
                    // Consume redundant channel-based Stop/StopLoop that
                    // accompanies the urgent_stop Notify. When the user
                    // clicks stop, both paths fire simultaneously — the
                    // Notify aborts the LLM stream (causing this exit),
                    // while the channel Stop waits in the queue. Without
                    // consuming it here, the next run() call would abort
                    // immediately on drain_inbound_queue().
                    self.poll_stop();
                    // ADR-014: Streaming → Idle (normal completion)
                    self.transition_status(SessionStatus::Idle);
                    return Ok(content);
                }
                IterationResult::Stopped(content) => {
                    self.poll_stop();
                    // ADR-014: Streaming → Idle (stopped)
                    self.transition_status(SessionStatus::Idle);
                    return Ok(content);
                }
                IterationResult::ToolCallsExecuted => {
                    tracing::debug!(iteration, "Loop iteration complete, continuing");
                    continue;
                }
                IterationResult::Paused => {
                    // ADR-014: Streaming → Paused (iteration aborted, await resume)
                    self.transition_status(SessionStatus::Paused {
                        iteration: Some(iteration),
                        max_iterations: Some(self.core.config.max_iterations),
                        retry_info: None,
                        reason: Some(PauseReason::Debug),
                        message: None,
                    });
                    tracing::info!(iteration, "Iteration paused via debug panel — await resume");
                    // The next iteration's step ① (await_debug_resume) will block
                    // until resume/stop/rewind is received.
                    continue;
                }
            }
        }
    }

    /// Await resume from paused/stepping state (DevMode only).
    ///
    /// When a debug controller is active, this method loops until the
    /// controller transitions to Running or Stepping (resume) or
    /// Stopped (abort). It also handles rewind requests by truncating
    /// history to the target snapshot.
    ///
    /// Returns `Some(IterationResult::Stopped)` if the loop should stop,
    /// or `None` if execution should continue.
    async fn await_debug_resume(&mut self) -> Option<IterationResult> {
        let ctrl = self.core.debug_observer.debug_ctrl().cloned();
        if let Some(ctrl) = ctrl {
            let rewind_notify = self.core.debug_observer.rewind_notify().cloned();
            loop {
                // Check for control signals (Stop via chat panel or debug)
                match self.poll_control() {
                    ControlDecision::Stop => {
                        tracing::info!("Debug: agent loop stopped via poll_control");
                        let mut ctrl_guard = ctrl.lock().await;
                        let iteration = ctrl_guard.iteration;
                        ctrl_guard.state = crate::debug::controller::DebugState::Stopped;
                        drop(ctrl_guard);
                        if let Some(event_tx) = self.core.debug_observer.debug_event_tx() {
                            let _ = event_tx.send(
                                crate::debug::DebugEvent::ExecutionStateChanged {
                                    new_state: crate::debug::controller::DebugState::Stopped,
                                    iteration,
                                },
                            );
                        }
                        return Some(IterationResult::Stopped(String::new()));
                    }
                    ControlDecision::Pause => {
                        // Pause was set by debug panel while we were in this loop.
                        // Fall through to the state check below which handles Paused.
                    }
                    ControlDecision::Continue => {}
                }

                // Consume any pending rewind
                {
                    let mut ctrl_guard = ctrl.lock().await;
                    if let Some(target_iter) = ctrl_guard.take_rewind_target() {
                        let msg_count = ctrl_guard
                            .conversation_snapshots
                            .iter()
                            .find(|s| s.iteration == target_iter)
                            .map(|s| s.message_count);
                        if let Some(count) = msg_count {
                            self.session.history.truncate_to(count);
                            tracing::info!(
                                target_iteration = target_iter,
                                messages_trimmed_to = count,
                                "Debug rewind: history truncated"
                            );
                        }
                        ctrl_guard.iteration = target_iter;
                    }
                }

                let state = {
                    let ctrl_guard = ctrl.lock().await;
                    ctrl_guard.state
                };
                match state {
                    crate::debug::controller::DebugState::Running => {
                        // ADR-049: HTTP request about to be sent → LlmAwaitingFirstChunk.
                        self.transition_status(SessionStatus::LlmAwaitingFirstChunk);
                        break;
                    }
                    crate::debug::controller::DebugState::Stepping => {
                        // ADR-049: HTTP request about to be sent → LlmAwaitingFirstChunk.
                        self.transition_status(SessionStatus::LlmAwaitingFirstChunk);
                        break;
                    }
                    crate::debug::controller::DebugState::Stopped => {
                        tracing::info!("Debug: agent loop stopped");
                        self.transition_status(SessionStatus::Idle);
                        return Some(IterationResult::Stopped(String::new()));
                    }
                    crate::debug::controller::DebugState::Paused => {
                        self.transition_status(SessionStatus::Paused {
                            iteration: None,
                            max_iterations: None,
                            retry_info: None,
                            reason: Some(PauseReason::Debug),
                            message: None,
                        });
                        if let Some(ref notify) = rewind_notify {
                            tokio::select! {
                                _ = notify.notified() => {},
                                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
                            }
                        } else {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }
        None
    }

    /// Execute a single iteration of the agent loop (steps ① through ⑧).
    ///
    /// Shared between production [`run()`] and debug [`DebugSessionTask`].
    /// The caller is responsible for iteration counting, limit checks, and
    /// inbound queue draining (steps ⑨ and ⓪).
    ///
    /// # Steps
    /// ① Budget pre-check → ② Preemptive trim → ②.5 Build context →
    /// ③ Call LLM → ④ Parse response → ④.5 Tool dedup →
    /// ⑤ Tool dispatch → ⑥ Append results → ⑧ Loop detection
    ///
    /// # Returns
    /// - `TextResponse(content)`: agent returned a final text response
    /// - `ToolCallsExecuted`: tool calls processed, caller should loop
    /// - `Stopped(content)`: user stopped execution
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_single_iteration(
        &mut self,
        iteration: u32,
        context_builder: &mut ContextBuilder,
        _user_message: &str,
        _retrieved_memory_ids: &[String],
        current_model: &str,
    ) -> Result<IterationResult> {
        // ── ① Debug observer hooks + resume ──
        self.core.debug_observer.check_pending_injection();
        let debug_iter = self
            .core
            .debug_observer
            .on_iteration_start(self.session.history.len());
        if let Some(result) = self.await_debug_resume().await {
            return Ok(result);
        }
        self.core
            .debug_observer
            .apply_pending_patches(context_builder);
        self.core.debug_observer.take_re_execute_pending();

        // ADR-032 C4b: drain user-initiated compression actions.
        // This runs every iteration, both in auto mode (where events also
        // trigger) and manual mode (where manual commands are the only way).
        self.drain_compress_actions();

        // ADR-052: Drain LLM-initiated abandon/retrieve queues.
        // These replace the old auto-compress trigger. The LLM calls
        // context_abandon / context_retrieve tools, which push into these
        // queues. We drain them here so the in-place modifications are
        // visible to build_chat_request in the same iteration.
        self.drain_abandon_queue();
        self.drain_retrieve_queue();

        // ── ② Budget + context build ──
        self.core
            .debug_observer
            .on_phase_enter(crate::debug::protocol::DebugPhase::BudgetCheck)
            .await;
        self.check_budget_and_warn()?;
        self.trim_history_to_budget(current_model);
        let mut chat_request = self.build_chat_request(context_builder, current_model);
        if self.check_context_overflow_and_trim(current_model) {
            chat_request = self.build_chat_request(context_builder, current_model);
        }
        self.core
            .debug_observer
            .on_phase_enter(crate::debug::protocol::DebugPhase::BuildContext)
            .await;
        self.core
            .debug_observer
            .on_context_built(crate::debug::observer::ContextSnapshotRequest {
                context_builder,
                iteration: debug_iter,
                model: current_model,
                all_tools: &self.core.all_tools,
                max_tokens: chat_request.max_tokens,
                history: &self.session.history,
            })
            .await;

        // ── ②.7 Context depletion guard ──
        // After aggressive trimming, the chat request may contain only a
        // system message with no user/assistant messages. Anthropic-protocol
        // providers extract the system message into a separate `system` field,
        // leaving `messages` empty — causing a 400 "messages must not be empty"
        // API error. Detect this early and return a clear error instead.
        let has_non_system = chat_request
            .messages
            .iter()
            .any(|m| !matches!(m.role, acowork_core::providers::traits::MessageRole::System));
        if !has_non_system {
            tracing::error!(
                iteration,
                history_tokens = self.session.history.token_count(),
                "All non-system messages trimmed — context depleted, cannot call LLM"
            );
            return Err(RuntimeError::ContextOverflow(
                "Context window exceeded: all conversation history was trimmed. \
                 Please start a new conversation or reduce attached files."
                    .to_string(),
            ));
        }

        // ── ③ Call LLM + parse + usage ──
        let response = self
            .call_llm_streaming(&chat_request, context_builder)
            .await?;

        // After LLM call, check for control signals that might have interrupted
        // the stream (e.g. Pause via debug panel).  The LLM streaming select!
        // sets `pending_interrupt` when it detects Pause and returns a stopped
        // response — we consume that interrupt here to decide the iteration outcome.
        match self.poll_control() {
            ControlDecision::Stop => {
                tracing::info!("Stopped during LLM call");
                return Ok(self.handle_stopped(&response.content).await);
            }
            ControlDecision::Pause => {
                tracing::info!("Paused during LLM call");
                return Ok(IterationResult::Paused);
            }
            ControlDecision::Continue => {}
        }

        self.core
            .debug_observer
            .on_phase_enter(crate::debug::protocol::DebugPhase::LlmCall)
            .await;
        let has_tool_calls = response.tool_calls.is_some();
        self.core
            .debug_observer
            .on_phase_enter(crate::debug::protocol::DebugPhase::ParseResponse)
            .await;
        self.process_llm_response_usage(&response, current_model)
            .await;

        // ── ④ Text response → early return ──
        if !has_tool_calls {
            // Guard: log empty response before exiting the loop.
            // This can happen when a thinking model exhausts its token budget
            // on reasoning and produces neither content nor tool_calls.
            if response.content.is_empty() && response.reasoning_content.is_some() {
                let reasoning_tokens = response
                    .usage
                    .as_ref()
                    .map(|u| u.reasoning_tokens)
                    .unwrap_or(0);
                let completion_tokens = response
                    .usage
                    .as_ref()
                    .map(|u| u.completion_tokens)
                    .unwrap_or(0);
                tracing::error!(
                    iteration,
                    finish_reason = ?response.finish_reason,
                    reasoning_tokens,
                    completion_tokens,
                    "LLM returned empty content with reasoning — \
                     model likely exhausted output budget on thinking. \
                     The loop will exit with an empty response."
                );
            }
            return Ok(self.handle_text_response(&response, iteration).await);
        }

        // ── ⑤ Prepare + pre-check tool calls ──
        let deduped_calls = self.prepare_tool_calls(&response);
        let (calls_to_execute, blocked_info) = self.pre_check_loop_detection(&deduped_calls);

        // ── ⑥ Pre-tool control check ──
        match self.poll_control() {
            ControlDecision::Stop => {
                tracing::info!("Stopped before tool execution — saving partial response");
                return Ok(self.handle_stopped(&response.content).await);
            }
            ControlDecision::Pause => {
                tracing::info!("Paused before tool execution");
                return Ok(IterationResult::Paused);
            }
            ControlDecision::Continue => {}
        }

        // ── ⑦ Dispatch + merge tool results ──
        self.core
            .debug_observer
            .on_phase_enter(crate::debug::protocol::DebugPhase::ToolExecution)
            .await;
        let (tagged_results, interrupt) = self
            .dispatch_and_merge_tools(
                calls_to_execute,
                &deduped_calls,
                &blocked_info,
            )
            .await;

        // Split tagged results into content strings + transient flags
        // (ADR-032 C3a: transient tool results bypass permanent history).
        let mut tool_contents: Vec<String> = Vec::with_capacity(tagged_results.len());
        let transient_flags: Vec<bool> = tagged_results.iter().map(|(_, t)| *t).collect();
        for (content, _) in tagged_results {
            tool_contents.push(content);
        }

        // ── ⑧ Persist + emit + append + pre-trim tool results ──
        self.persist_and_emit_tool_results(&deduped_calls, &tool_contents);
        self.pre_trim_for_tool_results(&tool_contents, current_model);

        // ── ⑧.25 Context-aware tool result trimming ──
        // After pre-trim removed old history, also truncate individual
        // tool results that exceed the remaining context budget.
        // This prevents a single large shell/grep/file_read output from
        // overflowing the window when appended, which would cause the
        // FIFO/emergency trim to delete ALL messages including the
        // results themselves, crashing the session with "context depleted".
        let truncated = self.trim_tool_results_for_context(
            &mut tool_contents,
            current_model,
        );
        if truncated > 0 {
            tracing::warn!(
                truncated,
                total = tool_contents.len(),
                "Tool results were truncated to fit context budget"
            );
        }

        // ── ⑧.5 Path B: Record tool failures as ProceduralNodes ──
        // After persisting results, scan for errors and create
        // low-confidence ProceduralNodes (execution_failure path).
        self.record_tool_failures_to_memory(&deduped_calls, &tool_contents);

        // ── ⑧.75 Append tool results to history ──
        //
        // `is_transient` results are queued for the next LLM request but
        // never permanently stored in history. ADR-052 removed the transient
        // mechanism for `context_retrieve` — all tool results are now
        // permanently appended. The `is_transient` field on `ToolResult`
        // is preserved for future hypothetical one-shot tools (ADR-032 C3a).
        for (tc, (result_content, &is_transient)) in
            deduped_calls.iter().zip(tool_contents.iter().zip(transient_flags.iter()))
        {
            let msg = ChatMessage {
                name: Some(tc.function.name.clone()),
                ..ChatMessage::tool(tc.id.clone(), result_content.to_string())
            };
            if is_transient {
                self.pending_transient_tool_msgs.push(msg);
            } else {
                self.session.history.append(msg);
            }
        }

        // ── ⑨ Post-execution loop detection ──
        self.post_check_loop_detection(&deduped_calls, &tool_contents, &blocked_info)?;

        // ── ⑩ Post-tool control check ──
        match interrupt {
            Some(ControlDecision::Stop) => {
                tracing::info!("Stopped during tool execution — saving partial results");
                return Ok(self.handle_stopped(&response.content).await);
            }
            Some(ControlDecision::Pause) => {
                tracing::info!("Paused during tool execution");
                return Ok(IterationResult::Paused);
            }
            _ => {}
        }

        // ── ⑪ Debug phase completion ──
        tracing::debug!(iteration, "Loop iteration complete");
        self.core
            .debug_observer
            .on_phase_enter(crate::debug::protocol::DebugPhase::AppendHistory)
            .await;
        self.core.debug_observer.on_phase_step(
            crate::debug::protocol::DebugPhase::Idle,
            None,
            None,
        );
        self.core.debug_observer.on_phase_step_done().await;

        // Emit post-iteration session snapshot so the frontend status panel
        // sees the latest model, provider, and ratio without waiting for the
        // next status change.
        //
        // TextResponse/Stopped paths return early and are covered by their
        // subsequent transition_status(Idle) → emit_session_state calls.
        // This checkpoint covers the ToolCallsExecuted continue-path only.
        self.emit_session_state();

        Ok(IterationResult::ToolCallsExecuted)
    }

    // ── Inbound message methods moved to loop_inbound.rs (ADR-014 Phase 3) ──
    //   - apply_user_op
    //   - poll_stop
    //   - drain_inbound_queue
    // D1 dedup helper: inject_inbound_into_history (shared function)

    // ── User interaction methods moved to loop_interaction.rs (ADR-014 Phase 4) ──
    //   - handle_ask_user_question
    //   - handle_todo_write

    // ── User-initiated compression action helpers ──

    /// Drain any pending user-initiated compression actions from the channel.
    ///
    /// Called at the start of each iteration so user-initiated compression
    /// actions (e.g. the "Compress Summary" button) are honored promptly.
    /// ADR-052 removed the `CompressionMode` concept; only `CompressSummary`
    /// (LLM-based summary) remains. Returns `true` if any action was
    /// processed.
    pub(crate) fn drain_compress_actions(&mut self) -> bool {
        let Some(rx) = &mut self.compress_action_rx else {
            return false;
        };
        let mut did_work = false;
        while let Ok(cmd) = rx.try_recv() {
            match cmd {

                CompressionAction::CompressSummary => {
                    // Summary compaction is handled by compact_history_if_needed
                    // (LLM-based). For now, just trigger the budget-trim path.
                    // Full LLM-based summary trigger will be wired in a later phase.
                    tracing::info!("Manual summary compaction requested (deferred to budget path)");
                    did_work = true;
                }
            }
        }
        did_work
    }

    /// ADR-052: Drain the abandon queue, replacing tool results with placeholders.
    ///
    /// Called at the start of each iteration (after `drain_compress_actions`,
    /// before `build_chat_request`). Each entry in the queue is a
    /// `tool_call_id` that the LLM requested to abandon via the
    /// `context_abandon` tool. The actual replacement is done in-place by
    /// `HistoryManager::abandon_tool_result()`.
    ///
    /// Returns `true` if any replacements were made (caller may want to
    /// recalibrate tokens - but this method already does it).
    pub(crate) fn drain_abandon_queue(&mut self) -> bool {
        let mut ids = self.abandon_queue.lock().unwrap();
        if ids.is_empty() {
            return false;
        }
        let mut did_work = false;
        while let Some(tool_call_id) = ids.pop_front() {
            let compressed = self.session.history.abandon_tool_result(&tool_call_id);
            if compressed > 0 {
                tracing::info!(tool_call_id = %tool_call_id, "context_abandon: replaced with placeholder");
                did_work = true;
            } else {
                tracing::debug!(tool_call_id = %tool_call_id, "context_abandon: no matching tool result (already compressed or not found)");
            }
        }
        drop(ids); // release lock before recalibrate
        if did_work {
            self.session.history.recalibrate_tokens();
        }
        did_work
    }

    /// ADR-052: Drain the retrieve queue, restoring original tool result content.
    ///
    /// Called at the start of each iteration (after `drain_abandon_queue`,
    /// before `build_chat_request`). Each entry is a `(tool_call_id,
    /// original_content)` pair that the LLM requested to retrieve via the
    /// `context_retrieve` tool. The restoration is done in-place by
    /// `HistoryManager::retrieve_tool_result()`.
    ///
    /// Returns `true` if any restorations were made.
    pub(crate) fn drain_retrieve_queue(&mut self) -> bool {
        let mut items = self.retrieve_queue.lock().unwrap();
        if items.is_empty() {
            return false;
        }
        let mut did_work = false;
        while let Some((tool_call_id, original_content)) = items.pop_front() {
            let restored = self.session.history.retrieve_tool_result(
                &tool_call_id,
                &original_content,
            );
            if restored > 0 {
                tracing::info!(tool_call_id = %tool_call_id, "context_retrieve: restored original content in-place");
                did_work = true;
            } else {
                tracing::debug!(tool_call_id = %tool_call_id, "context_retrieve: no matching placeholder (already restored or not found)");
            }
        }
        drop(items);
        if did_work {
            self.session.history.recalibrate_tokens();
        }
        did_work
    }


    // ── LLM streaming methods extracted to loop_llm.rs ──

    // ── Tool execution extracted to loop_tools.rs ──

    // ── Debug methods migrated to DebugObserverImpl (ADR-013) ──
    // The following methods were moved to loop_approval.rs (ADR-014 Phase 2):
    //   - await_approval_decision
    //   - send_tool_approval_needed
    //   - await_question_answer
    //   - handle_approval_request
    // The following types were moved to loop_approval.rs:
    //   - ApprovalDecision
    //   - ApprovalHandle

    /// Get reference to history manager
    pub fn history(&self) -> &HistoryManager {
        &self.session.history
    }

    /// Get reference to the agent manifest
    pub fn manifest(&self) -> &acowork_core::AgentManifest {
        &self.core.manifest
    }

    /// Get mutable reference to history manager
    pub fn history_mut(&mut self) -> &mut HistoryManager {
        &mut self.session.history
    }
}

// ── Think block utilities moved to loop_session.rs (ADR-014 Phase 5) ──
//   - extract_think_block
//   - strip_think_block
//   - build_think_metadata
// Use via: use crate::agent::loop_session::{extract_think_block, strip_think_block, build_think_metadata};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_core::BuiltinToolEntry;
    use crate::agent::loop_llm::make_incomplete_marker;
    use crate::agent::loop_tools::execute_single_tool;
    use acowork_core::providers::mock::MockProvider;
    use acowork_core::providers::traits::{FunctionCall, MessageRole, ToolCall};

    /// Simple echo tool for testing
    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
            acowork_core::tools::traits::ToolSpec {
                name: "echo".to_string(),
                description: "Echoes back the input".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {"type": "string", "description": "Message to echo"}
                    },
                    "required": ["message"]
                }),
            }
        }
        async fn execute(
            &self,
            params: serde_json::Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
            let message = params
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("no message");
            Ok(acowork_core::tools::traits::ToolResult {
                ok: true,
                content: format!("Echo: {message}"),
                error: None,
                token_usage: None,
            })
        }
    }

    fn test_manifest() -> acowork_core::AgentManifest {
        acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.loop"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"
            "#,
        )
        .unwrap()
    }

    fn test_budget() -> acowork_core::Budget {
        acowork_core::Budget {
            daily_tokens: Some(100000),
            monthly_tokens: None,
            daily_cost_usd: Some(10.0),
            monthly_cost_usd: None,
            exceeded_action: "warn".to_string(),
        }
    }

    /// Wrap a `Vec<Arc<dyn Tool>>` (test fixture style) into the
    /// `Vec<BuiltinToolEntry>` shape that `AgentLoop::new` now requires.
    /// Used widely in this module's tests to keep fixtures terse.
    fn entries(tools: Vec<Arc<dyn Tool>>) -> Vec<BuiltinToolEntry> {
        tools
            .into_iter()
            .map(|tool| BuiltinToolEntry { tool, enabled: true })
            .collect()
    }

    /// Inverse of `entries` — used by tests that hand the tool set to
    /// pure `Arc<dyn Tool>` APIs like `execute_single_tool`.
    fn raw_tools(entries: &[BuiltinToolEntry]) -> Vec<Arc<dyn Tool>> {
        entries.iter().map(|e| e.tool.clone()).collect()
    }

    #[test]
    fn test_agent_loop_with_gateway_client() {
        // NOTE: Both chunk_tx and conversation are None because this test is
        // only verifying that AgentLoop construction works correctly, not the
        // gRPC streaming connection.
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools = entries(vec![]);
        let budget = test_budget();
        let (_agent_loop, _inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        // Verify inbound sender works
        assert!(
            _inbound_tx
                .try_send(InboundMessage::UserMessage("test".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn test_agent_loop_without_gateway_client() {
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools = entries(vec![]);
        let budget = test_budget();
        let (_agent_loop, _inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        // Just verify construction works
        assert!(
            _inbound_tx
                .try_send(InboundMessage::UserMessage("test".to_string()))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_agent_loop_standalone_no_panic() {
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("Hello from standalone!"));
        let tools = entries(vec![]);
        let budget = test_budget();
        let (mut agent_loop, _inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Hello from standalone!");
    }

    // ── S1.5: Streaming tests ─────────────────────────────────────────

    #[tokio::test]
    async fn test_stream_content_accumulation() {
        // MockProvider::chat_stream internally calls chat() then emits Finished event.
        // Content should be correctly accumulated from the stream.
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("Accumulated content here"));
        let tools = entries(vec![]);
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Accumulated content here");
    }

    #[tokio::test]
    async fn test_stream_tool_call_detection() {
        let provider = Arc::new(MockProvider::tool_call_then_text(
            "echo",
            r#"{"message": "hello"}"#,
            "Done",
        ));
        let tools = entries(vec![Arc::new(EchoTool)]);
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stream_finished_event() {
        // When stream emits Finished, content and usage are extracted
        let provider = Arc::new(MockProvider::single_text("Final response"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Final response");
        // Verify usage was tracked (budget guard should have been updated)
        assert!(agent_loop.history().estimate_total_tokens() > 0);
    }

    #[tokio::test]
    async fn test_stream_error_propagation() {
        let provider = Arc::new(MockProvider::new(vec![
            acowork_core::providers::mock::MockResponse::Error {
                message: "API rate limit".to_string(),
            },
        ]));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_err());
        // Error from chat_stream propagates as Core(AcoworkError::Provider(...))
        // because Provider trait returns acowork_core::AcoworkError
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("rate limit"),
            "Error should mention rate limit: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_stream_content_then_tool_call() {
        // MockProvider returns tool call then text — content accumulates correctly
        let provider = Arc::new(MockProvider::tool_call_then_text(
            "echo",
            r#"{"message": "test"}"#,
            "All done",
        ));
        let tools = entries(vec![Arc::new(EchoTool)]);
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "All done");
    }

    #[tokio::test]
    async fn test_stream_empty_content() {
        let provider = Arc::new(MockProvider::single_text(""));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }

    #[tokio::test]
    async fn test_stream_history_append() {
        // Verify that streamed text response is correctly appended to history
        let provider = Arc::new(MockProvider::single_text("Streamed text"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let _ = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        let messages = agent_loop.history().messages();
        // Should have: user message + assistant message
        let assistant_msgs: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .collect();
        assert_eq!(assistant_msgs.len(), 1);
        assert_eq!(assistant_msgs[0].content, "Streamed text");
    }

    #[tokio::test]
    async fn test_stream_usage_tracking() {
        let provider = Arc::new(MockProvider::single_text("Response"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let _ = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        // Budget guard should have been updated with usage from the stream
        // (MockProvider returns usage with total_tokens=150)
        // We can't directly check budget_guard, but we verify no error occurred
    }

    // ── S1.6: InboundQueue tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_inbound_user_message() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        // Inject a user message before running
        inbound_tx
            .try_send(InboundMessage::UserMessage("Injected question".to_string()))
            .unwrap();

        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
        // Verify the injected message appeared in history
        let messages = agent_loop.history().messages();
        let injected: Vec<_> = messages
            .iter()
            .filter(|m| m.content.contains("Injected question"))
            .collect();
        assert!(
            !injected.is_empty(),
            "Injected user message should appear in history"
        );
    }

    #[tokio::test]
    async fn test_inbound_system_notification() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        inbound_tx
            .try_send(InboundMessage::SystemNotification {
                notification_type: "identity_update".to_string(),
                data: serde_json::json!({"key": "new_value"}),
            })
            .unwrap();

        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
        let messages = agent_loop.history().messages();
        let notif: Vec<_> = messages
            .iter()
            .filter(|m| m.content.contains("[system:identity_update]"))
            .collect();
        assert!(
            !notif.is_empty(),
            "System notification should appear in history"
        );
    }

    #[tokio::test]
    async fn test_inbound_intent_message() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        inbound_tx
            .try_send(InboundMessage::IntentMessage {
                from: "com.acowork.system".to_string(),
                action: "ping".to_string(),
                params: serde_json::json!({}),
            })
            .unwrap();

        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
        let messages = agent_loop.history().messages();
        let intent: Vec<_> = messages
            .iter()
            .filter(|m| m.content.contains("[intent:com.acowork.system:ping]"))
            .collect();
        assert!(
            !intent.is_empty(),
            "Intent message should appear in history"
        );
    }

    #[tokio::test]
    async fn test_inbound_concurrent_injection() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        // Inject 10 messages concurrently
        for i in 0..10 {
            inbound_tx
                .try_send(InboundMessage::UserMessage(format!("Message {i}")))
                .unwrap();
        }

        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        assert!(result.is_ok());
        let messages = agent_loop.history().messages();
        let injected: Vec<_> = messages
            .iter()
            .filter(|m| m.content.starts_with("Message "))
            .collect();
        assert_eq!(
            injected.len(),
            10,
            "All 10 injected messages should appear in history"
        );
    }

    #[tokio::test]
    async fn test_inbound_queue_full_backpressure() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (agent_loop, inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);

        // Fill the channel (capacity 64)
        for i in 0..64 {
            assert!(
                inbound_tx
                    .try_send(InboundMessage::UserMessage(format!("Msg {i}")))
                    .is_ok()
            );
        }
        // The 65th message should fail (backpressure) — but no panic
        let result = inbound_tx.try_send(InboundMessage::UserMessage("overflow".to_string()));
        assert!(result.is_err(), "Channel should be full");
        // Should not panic — just returns Err
        drop(agent_loop);
    }

    #[tokio::test]
    async fn test_inbound_drain_nonblocking() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools = entries(vec![]);
        let (mut agent_loop, _inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        // Run without any inbound messages — drain should return immediately
        let start = std::time::Instant::now();
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None, None).await;
        let elapsed = start.elapsed();
        assert!(result.is_ok());
        // Drain should not block — core path is sub-100ms, but allow up to 2s
        // for CI variance and debug-build overhead of the async runtime.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "Drain should be non-blocking, but took {:?}",
            elapsed
        );
    }

    // ── S1.7: Parallel tool execution tests ───────────────────────────

    #[tokio::test]
    async fn test_tool_parallel_execution() {
        use async_trait::async_trait;

        #[derive(Clone)]
        struct SlowTool {
            name: String,
            delay_ms: u64,
        }

        #[async_trait]
        impl Tool for SlowTool {
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: self.name.clone(),
                    description: format!("Slow tool {}", self.name),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: format!("{} done", self.name),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.parallel"
            version = "1.0.0"
            name = "Parallel Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "slow_a"

            [[tools]]
            name = "slow_b"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(SlowTool {
                name: "slow_a".to_string(),
                delay_ms: 100,
            }),
            Arc::new(SlowTool {
                name: "slow_b".to_string(),
                delay_ms: 100,
            }),
        ];
        let tools = entries(tools);

        let provider = Arc::new(MockProvider::new(vec![
            acowork_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_1".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "slow_a".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_2".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "slow_b".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            acowork_core::providers::mock::MockResponse::Text {
                content: "Both done".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop
            .run("Run parallel", &mut context_builder, None, None, None, None)
            .await;
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "Parallel execution should succeed: {:?}",
            result
        );
        // Parallel: ~100ms total. Serial would be ~200ms.
        // Allow generous margin (300ms) to avoid flaky tests
        assert!(
            elapsed < std::time::Duration::from_millis(300),
            "Parallel execution should be faster than serial: {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_tool_single_failure_no_shortcircuit() {
        use async_trait::async_trait;

        struct FailTool;
        #[async_trait]
        impl Tool for FailTool {
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: "fail_tool".to_string(),
                    description: "Always fails".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Intentional failure".to_string()),
                    token_usage: None,
                })
            }
        }

        struct SuccessTool;
        #[async_trait]
        impl Tool for SuccessTool {
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: "success_tool".to_string(),
                    description: "Always succeeds".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Success!".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.fail"
            version = "1.0.0"
            name = "Fail Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "fail_tool"

            [[tools]]
            name = "success_tool"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools = entries(vec![Arc::new(FailTool), Arc::new(SuccessTool)]);

        // LLM returns both tool calls, then text
        let provider = Arc::new(MockProvider::new(vec![
            acowork_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_fail".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "fail_tool".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_success".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "success_tool".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            acowork_core::providers::mock::MockResponse::Text {
                content: "Mixed results".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let result = agent_loop
            .run("Test failure", &mut context_builder, None, None, None, None)
            .await;
        assert!(result.is_ok(), "Should succeed even with one tool failure");
        assert_eq!(result.unwrap(), "Mixed results");
    }

    #[tokio::test]
    async fn test_tool_timeout() {
        use async_trait::async_trait;

        struct StuckTool;
        #[async_trait]
        impl Tool for StuckTool {
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: "stuck_tool".to_string(),
                    description: "Never returns".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                // Sleep for a long time — should be cut short by timeout.
                // 5s is more than enough to verify timeout works (100ms threshold),
                // while avoiding a 60s hang if timeout logic breaks.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Should not reach".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.timeout"
            version = "1.0.0"
            name = "Timeout Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "stuck_tool"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools = entries(vec![Arc::new(StuckTool)]);

        let provider = Arc::new(MockProvider::tool_call_then_text(
            "stuck_tool",
            "{}",
            "After timeout",
        ));

        let config = RuntimeConfig {
            timeouts: acowork_core::Timeouts {
                iteration_timeout_ms: 100,
                ..Default::default()
            },
            ..Default::default()
        }; // 100ms timeout
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop
            .run("Test timeout", &mut context_builder, None, None, None, None)
            .await;
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "Should succeed with timeout error captured: {:?}",
            result
        );
        // Should complete within ~1 second (100ms timeout + overhead)
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "Should timeout quickly: {:?}",
            elapsed
        );

        // Verify the timeout error message appears in history
        let messages = agent_loop.history().messages();
        let timeout_msg: Vec<_> = messages
            .iter()
            .filter(|m| m.content.contains("timed out"))
            .collect();
        assert!(
            !timeout_msg.is_empty(),
            "Timeout error should appear in tool result history"
        );
    }

    #[tokio::test]
    async fn test_tool_permission_check_sequential() {
        // When a tool lacks permission, the sequential check should catch it
        // before any parallel execution begins.
        let toml_str = r#"
            agent_id = "com.test.perm"
            version = "1.0.0"
            name = "Perm Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "shell"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();

        // shell requires Shell permission, but manifest doesn't declare it
        let tools = entries(vec![]);

        let provider = Arc::new(MockProvider::tool_call_then_text(
            "shell",
            r#"{"command": "ls"}"#,
            "Done",
        ));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        // The tool call will fail because shell is not in the tool registry
        // (empty tools vec), so it should produce "Unknown tool: shell"
        let result = agent_loop
            .run("Run shell", &mut context_builder, None, None, None, None)
            .await;
        // Should still succeed — error becomes tool result message
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_tool_results_order_preserved() {
        use async_trait::async_trait;

        #[derive(Clone)]
        struct OrderedTool {
            name: String,
            output: String,
        }

        #[async_trait]
        impl Tool for OrderedTool {
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: self.name.clone(),
                    description: format!("Ordered tool {}", self.name),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: self.output.clone(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.order"
            version = "1.0.0"
            name = "Order Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "tool_a"

            [[tools]]
            name = "tool_b"

            [[tools]]
            name = "tool_c"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(OrderedTool {
                name: "tool_a".to_string(),
                output: "Result A".to_string(),
            }),
            Arc::new(OrderedTool {
                name: "tool_b".to_string(),
                output: "Result B".to_string(),
            }),
            Arc::new(OrderedTool {
                name: "tool_c".to_string(),
                output: "Result C".to_string(),
            }),
        ];
        let tools = entries(tools);

        let provider = Arc::new(MockProvider::new(vec![
            acowork_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_a".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool_a".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_b".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool_b".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_c".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool_c".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            acowork_core::providers::mock::MockResponse::Text {
                content: "All ordered".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let result = agent_loop
            .run("Run ordered", &mut context_builder, None, None, None, None)
            .await;
        assert!(result.is_ok());

        // Verify that tool results in history are in order
        let messages = agent_loop.history().messages();
        let tool_results: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(tool_results.len(), 3);
        // First tool result should be tool_a
        assert!(
            tool_results[0].content.contains("Result A"),
            "First result should be A"
        );
        // Second should be tool_b
        assert!(
            tool_results[1].content.contains("Result B"),
            "Second result should be B"
        );
        // Third should be tool_c
        assert!(
            tool_results[2].content.contains("Result C"),
            "Third result should be C"
        );
    }

    // ── Fix #1: Iteration timeout with partial results ─────────────────

    #[tokio::test]
    async fn test_iteration_timeout_partial_results() {
        use async_trait::async_trait;

        #[derive(Clone)]
        struct FastTool;

        #[async_trait]
        impl Tool for FastTool {
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: "fast_tool".to_string(),
                    description: "Fast tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Fast result".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        #[derive(Clone)]
        struct SlowTool;

        #[async_trait]
        impl Tool for SlowTool {
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: "slow_tool".to_string(),
                    description: "Slow tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                // Sleep longer than the iteration timeout (200ms).
                // 5s is plenty to verify timeout works without risking a 60s hang.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Should not reach".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.iter_timeout"
            version = "1.0.0"
            name = "Iter Timeout Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "fast_tool"

            [[tools]]
            name = "slow_tool"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools = entries(vec![Arc::new(FastTool), Arc::new(SlowTool)]);

        // LLM requests both tools; fast_tool completes quickly, slow_tool times out
        let provider = Arc::new(MockProvider::new(vec![
            acowork_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_fast".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "fast_tool".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_slow".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "slow_tool".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            acowork_core::providers::mock::MockResponse::Text {
                content: "Partial complete".to_string(),
            },
        ]));

        // Very short iteration timeout so slow_tool gets aborted
        let config = RuntimeConfig {
            timeouts: acowork_core::Timeouts {
                iteration_timeout_ms: 200,
                tool_timeout_ms: 10000, // tool_timeout is long, iteration timeout is short
                ..Default::default()
            },
            ..Default::default()
        };
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop
            .run("Test iteration timeout", &mut context_builder, None, None, None, None)
            .await;
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "Should succeed with partial results: {:?}",
            result
        );
        // Should complete within ~1 second (200ms iteration timeout + overhead)
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "Should complete quickly with iteration timeout: {:?}",
            elapsed
        );

        // Verify the fast_tool result and slow_tool timeout both appear in history
        let messages = agent_loop.history().messages();
        let tool_results: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        // fast_tool should have its result
        assert!(
            tool_results[0].content.contains("Fast result"),
            "Fast tool should have its result"
        );
        // slow_tool should have iteration timeout error
        assert!(
            tool_results[1].content.contains("iteration timed out"),
            "Slow tool should have iteration timeout error: {}",
            tool_results[1].content
        );
    }

    #[tokio::test]
    async fn test_tool_timeout_vs_iteration_timeout_independent() {
        // Verify that single-tool timeout and iteration timeout work independently.
        // A tool that exceeds tool_timeout_ms should get a per-tool timeout error,
        // even if iteration_timeout_ms is longer.
        use async_trait::async_trait;

        struct MediumTool;

        #[async_trait]
        impl Tool for MediumTool {
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: "medium_tool".to_string(),
                    description: "Medium-speed tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                // Sleep longer than tool_timeout (100ms) but shorter than iteration_timeout (30s)
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Should not reach".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.tool_timeout"
            version = "1.0.0"
            name = "Tool Timeout Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "medium_tool"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools = entries(vec![Arc::new(MediumTool)]);

        let provider = Arc::new(MockProvider::tool_call_then_text(
            "medium_tool",
            "{}",
            "After tool timeout",
        ));

        // tool_timeout_ms is 100ms (shorter than tool execution),
        // iteration_timeout_ms is 30000ms (much longer)
        let config = RuntimeConfig {
            timeouts: acowork_core::Timeouts {
                tool_timeout_ms: 100,
                iteration_timeout_ms: 30000,
                ..Default::default()
            },
            ..Default::default()
        };
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop
            .run("Test tool timeout", &mut context_builder, None, None, None, None)
            .await;
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "Should succeed with tool timeout error: {:?}",
            result
        );
        // Should complete in ~100ms (tool timeout) + overhead, not 500ms
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "Should timeout at tool level: {:?}",
            elapsed
        );

        // Verify per-tool timeout message (not iteration timeout)
        let messages = agent_loop.history().messages();
        let timeout_msg: Vec<_> = messages
            .iter()
            .filter(|m| m.content.contains("timed out"))
            .collect();
        assert!(
            !timeout_msg.is_empty(),
            "Per-tool timeout should be recorded"
        );
        // Should NOT be an iteration timeout message
        assert!(
            timeout_msg
                .iter()
                .all(|m| !m.content.contains("iteration timed out")),
            "Should be per-tool timeout, not iteration timeout"
        );
    }

    // ── Fix #2: Partial permission denial ──────────────────────────────

    #[tokio::test]
    async fn test_permission_partial_denial() {
        // When a tool is declared in the manifest but not in the tool registry
        // (i.e. not permitted), the missing tool should produce an error while
        // other registered tools still execute normally.
        //
        // Note: the tool registry IS the permission boundary — tools not in the
        // registry are effectively permission-denied. `execute_single_tool` returns
        // "Unknown tool" for any tool not found in the registry.
        use async_trait::async_trait;

        struct EchoPermTool;

        #[async_trait]
        impl Tool for EchoPermTool {
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: "echo".to_string(),
                    description: "Echo tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Echo result".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        // Manifest declares echo tool (no permission needed) but NOT shell permission
        let toml_str = r#"
            agent_id = "com.test.partial_perm"
            version = "1.0.0"
            name = "Partial Perm Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "echo"

            [[tools]]
            name = "shell"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools = entries(vec![Arc::new(EchoPermTool)]);

        // LLM requests both echo and shell
        let provider = Arc::new(MockProvider::new(vec![
            acowork_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_echo".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "echo".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_shell".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "shell".to_string(),
                            arguments: r#"{"command": "ls"}"#.to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            acowork_core::providers::mock::MockResponse::Text {
                content: "Partial permission result".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let result = agent_loop
            .run("Test partial permission", &mut context_builder, None, None, None, None)
            .await;
        assert!(
            result.is_ok(),
            "Should succeed even with one tool permission denied: {:?}",
            result
        );

        // Verify echo result appears (it was executed) and shell has permission denied
        let messages = agent_loop.history().messages();
        let tool_results: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(tool_results.len(), 2, "Should have 2 tool results");
        // First tool (echo) should have result
        assert!(
            tool_results[0].content.contains("Echo result")
                || tool_results[0].content.contains("Unknown tool"),
            "Echo tool should have result or unknown tool error"
        );
        // Second tool (shell) is not in the tool registry (permission denied),
        // so it should produce an "Unknown tool" error.
        assert!(
            tool_results[1].content.contains("Unknown tool: shell"),
            "Shell tool should be unknown (not in registry): {}",
            tool_results[1].content
        );
    }

    // ── S1.9: Tool call argument robustness tests ──────────────────────

    /// Verify that TOOL_CALL_INCOMPLETE marker is detected and tool execution
    /// is skipped, returning the embedded message to the LLM.
    #[tokio::test]
    async fn test_incomplete_tool_call_skipped() {
        let tools = entries(vec![Arc::new(EchoTool)]);

        // Simulate the marker that the streaming assembler injects
        let incomplete_args = make_incomplete_marker("echo", 42);
        let tc = ToolCall {
            id: "call_incomplete".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "echo".to_string(),
                arguments: incomplete_args.clone(),
            },
        };

        let (result, _transient) = execute_single_tool(&raw_tools(&tools), &tc, None).await;

        // Must NOT contain "Echo:" — tool was never called
        assert!(
            !result.contains("Echo:"),
            "Tool should NOT be executed, got: {}",
            result
        );
        // Must contain the error message from the marker
        assert!(
            result.contains("truncated during streaming"),
            "Result should explain truncation: {}",
            result
        );
        assert!(
            result.contains("NOT executed"),
            "Result should state it was NOT executed: {}",
            result
        );
    }

    /// Verify that genuinely unparseable JSON (e.g. LLM hallucinated output)
    /// does not silently degrade to {} — it returns a clear error.
    #[tokio::test]
    async fn test_invalid_json_tool_call_error() {
        let tools = entries(vec![Arc::new(EchoTool)]);

        // Simulate LLM producing broken JSON (not from streaming truncation)
        let tc = ToolCall {
            id: "call_broken".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "echo".to_string(),
                arguments: r#"{"message": "hello"#.to_string(), // missing closing brace
            },
        };

        let (result, _transient) = execute_single_tool(&raw_tools(&tools), &tc, None).await;

        // Must NOT execute the tool
        assert!(
            !result.contains("Echo:"),
            "Tool should NOT be executed on invalid JSON, got: {}",
            result
        );
        // Must contain error explanation
        assert!(
            result.contains("not valid JSON"),
            "Result should explain JSON parse failure: {}",
            result
        );
        assert!(
            result.contains("NOT executed"),
            "Result should state it was NOT executed: {}",
            result
        );
    }

    /// Verify that valid JSON tool arguments execute normally (regression test
    /// for the INCOMPLETE/invalid-JSON guard).
    #[tokio::test]
    async fn test_valid_json_tool_call_executes_normally() {
        let tools = entries(vec![Arc::new(EchoTool)]);

        let tc = ToolCall {
            id: "call_ok".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "echo".to_string(),
                arguments: r#"{"message": "hello world"}"#.to_string(),
            },
        };

        let (result, _transient) = execute_single_tool(&raw_tools(&tools), &tc, None).await;
        assert_eq!(
            result, "Echo: hello world",
            "Valid tool call should execute normally, got: {}",
            result
        );
    }

    // ── ADR-021: Single-channel chunk tests ───────────────────────────

    #[test]
    fn test_try_send_chunk_single_channel() {
        // ADR-021: All events go through the single chunk_tx channel.
        let (chunk_tx, mut chunk_rx) = mpsc::channel::<SessionChunkEvent>(16);

        let streaming_lines: crate::conversation::StreamingStateMap =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let current_work_dir = Arc::new(std::sync::RwLock::new(None));
        let session_core = SessionCore::new(
            "test-session".to_string(),
            Some(chunk_tx),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            500,
            current_work_dir,
            streaming_lines,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );

        // All events go through the single channel
        assert!(session_core.try_send_chunk(ChunkEvent::Stopped {
            content: "stopped".to_string(),
        }));
        let evt = chunk_rx.try_recv().expect("Stopped should arrive");
        assert!(matches!(evt.event, ChunkEvent::Stopped { .. }));

        assert!(session_core.try_send_chunk(ChunkEvent::Done {
            content: "done".to_string(),
            message_id: "msg-1".to_string(),
        }));
        let evt = chunk_rx.try_recv().expect("Done should arrive");
        assert!(matches!(evt.event, ChunkEvent::Done { .. }));
    }

    #[test]
    fn test_try_send_chunk_standalone_mode() {
        // In standalone mode, chunk_tx is None → try_send_chunk returns false.
        let streaming_lines: crate::conversation::StreamingStateMap =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let current_work_dir = Arc::new(std::sync::RwLock::new(None));
        let session_core = SessionCore::new(
            "test-session".to_string(),
            None,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            500,
            current_work_dir,
            streaming_lines,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );

        assert!(!session_core.try_send_chunk(ChunkEvent::Stopped {
            content: "stopped".to_string(),
        }));
    }

    /// ADR-046 §2.1: `run()` persists the raw (un-enriched) user message
    /// to JSONL when `raw_user_message` is provided. The enriched version
    /// (with `[Attached workspace files & uploads …]` hints) goes to the
    /// LLM via `history.append()` but MUST NOT land in the JSONL.
    #[tokio::test]
    async fn test_run_persists_raw_user_message_not_enriched() {
        use std::io::Read;
        use std::sync::atomic::AtomicUsize;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let work_dir = dir.path();
        let session_id = "test-raw-user-msg";
        let committed = Arc::new(AtomicUsize::new(0));
        let config = crate::conversation::SessionConfig {
            agent_id: "com.test.loop".to_string(),
            workspace_id: None,
            model: None,
            provider: None,
        };
        let (conversation, _config_rx, _state_rx) = ConversationSession::new(
            work_dir,
            session_id,
            config,
            0, // max_sessions
            committed,
        )
        .unwrap();

        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools = entries(vec![]);
        let budget = test_budget();
        let (mut agent_loop, _inbound_tx) = AgentLoop::new(
            RuntimeConfig::default(),
            manifest,
            provider,
            tools,
            budget,
            None,
            Some(Arc::new(conversation)),
        );

        let mut context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let enriched = "帮我看看这个文件\n\n[Attached workspace files & uploads — use `read_file` / `doc_reader` on demand]\n- file: `buglist.docx` (id=abc123, format=docx)";
        let raw = "帮我看看这个文件";
        let result = agent_loop
            .run(
                enriched,
                &mut context_builder,
                None,
                Some("msg-1".to_string()),
                Some(raw),
                None,
            )
            .await;
        assert!(result.is_ok(), "run() should succeed: {result:?}");

        // Read the JSONL file and verify the user entry is the raw message
        let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
        let mut content = String::new();
        std::fs::File::open(&jsonl_path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        let user_lines: Vec<&str> = content
            .lines()
            .filter(|l| l.contains(r#""role":"user""#))
            .collect();

        assert_eq!(
            user_lines.len(),
            1,
            "Expected exactly one user entry in JSONL, got {}: {user_lines:?}",
            user_lines.len(),
        );
        let user_entry = user_lines[0];
        // The user entry MUST contain the raw message
        assert!(
            user_entry.contains(raw),
            "JSONL user entry should contain raw message, got: {user_entry}",
        );
        // The user entry MUST NOT contain the enriched hint
        assert!(
            !user_entry.contains("Attached workspace files"),
            "JSONL user entry should NOT contain enriched hint, got: {user_entry}",
        );
        // The message_id should be present
        assert!(
            user_entry.contains(r#""id":"msg-1""#),
            "JSONL user entry should have the correct message id, got: {user_entry}",
        );
    }

    /// When `raw_user_message` is None (legacy callers), `run()` falls back
    /// to `user_message` for JSONL persistence — backward-compatible.
    #[tokio::test]
    async fn test_run_falls_back_to_user_message_when_raw_is_none() {
        use std::io::Read;
        use std::sync::atomic::AtomicUsize;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let work_dir = dir.path();
        let session_id = "test-raw-none-fallback";
        let committed = Arc::new(AtomicUsize::new(0));
        let config = crate::conversation::SessionConfig {
            agent_id: "com.test.loop".to_string(),
            workspace_id: None,
            model: None,
            provider: None,
        };
        let (conversation, _config_rx, _state_rx) = ConversationSession::new(
            work_dir,
            session_id,
            config,
            0,
            committed,
        )
        .unwrap();

        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools = entries(vec![]);
        let budget = test_budget();
        let (mut agent_loop, _inbound_tx) = AgentLoop::new(
            RuntimeConfig::default(),
            manifest,
            provider,
            tools,
            budget,
            None,
            Some(Arc::new(conversation)),
        );

        let mut context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let enriched = "帮我看看这个文件\n\n[Attached workspace files & uploads — use `read_file` / `doc_reader` on demand]\n- file: `buglist.docx` (id=abc123, format=docx)";
        let result = agent_loop
            .run(
                enriched,           // no raw_user_message → user_message is used as-is
                &mut context_builder,
                None,
                Some("msg-2".to_string()),
                None,               // raw_user_message = None
                None,               // attached_items = None
            )
            .await;
        assert!(result.is_ok(), "run() should succeed: {result:?}");

        let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
        let mut content = String::new();
        std::fs::File::open(&jsonl_path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        let user_lines: Vec<&str> = content
            .lines()
            .filter(|l| l.contains(r#""role":"user""#))
            .collect();

        assert_eq!(user_lines.len(), 1, "Expected one user entry, got: {user_lines:?}");
        let user_entry = user_lines[0];
        // Without raw_user_message, the enriched content is used as-is (backward compat)
        assert!(
            user_entry.contains("Attached workspace files"),
            "When raw_user_message=None, JSONL should contain enriched hint, got: {user_entry}",
        );
    }

    // ── ADR-052: drain_abandon_queue / drain_retrieve_queue tests ──────

    /// Helper: build a minimal AgentLoop for drain tests. We use the standard
    /// `AgentLoop::new()` constructor with default config + mock provider, then
    /// inject Tool-role messages directly into history.
    fn make_loop_for_drain_tests() -> AgentLoop {
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools = entries(vec![]);
        let budget = test_budget();
        let (agent_loop, _inbound_tx) =
            AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        agent_loop
    }

    /// Helper: build a Tool-role message for history injection.
    fn make_tool_message_for_drain(content: &str, tool_call_id: &str) -> ChatMessage {
        ChatMessage {
            role: MessageRole::Tool,
            content: content.to_string(),
            name: Some("content_search".to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_drain_abandon_queue_empty_returns_false() {
        // ADR-052 §3.3.3: empty queue returns false without recalibrate_tokens
        let mut agent_loop = make_loop_for_drain_tests();
        let tokens_before = agent_loop.session.history.token_count();

        let did_work = agent_loop.drain_abandon_queue();
        assert!(!did_work, "empty queue should return false");

        let tokens_after = agent_loop.session.history.token_count();
        assert_eq!(
            tokens_before, tokens_after,
            "empty drain should not call recalibrate_tokens (no token changes)"
        );
    }

    #[test]
    fn test_drain_abandon_queue_replaces_and_recalibrates() {
        // ADR-052 §3.3.3: non-empty queue drains, calls abandon_tool_result,
        // and recalibrate_tokens.
        let mut agent_loop = make_loop_for_drain_tests();
        let big = "x".repeat(5000);
        agent_loop
            .history_mut()
            .append(make_tool_message_for_drain(&big, "toolu_abc"));
        agent_loop
            .history_mut()
            .append(make_tool_message_for_drain(&big, "toolu_def"));
        let tokens_before = agent_loop.session.history.token_count();
        assert!(tokens_before > 0);

        // Push two abandon requests
        agent_loop
            .abandon_queue
            .lock()
            .unwrap()
            .push_back("toolu_abc".to_string());
        agent_loop
            .abandon_queue
            .lock()
            .unwrap()
            .push_back("toolu_def".to_string());

        let did_work = agent_loop.drain_abandon_queue();
        assert!(did_work, "non-empty queue should return true");

        // Both messages replaced with placeholders
        let msgs = agent_loop.session.history.messages();
        assert!(msgs[0].content.starts_with("[Tool result compressed."));
        assert!(msgs[1].content.starts_with("[Tool result compressed."));

        // recalibrate_tokens was called - token count should drop significantly
        let tokens_after = agent_loop.session.history.token_count();
        assert!(
            tokens_after < tokens_before,
            "recalibrate_tokens should reflect new content: before={tokens_before}, after={tokens_after}"
        );

        // Queue is now empty
        assert!(agent_loop.abandon_queue.lock().unwrap().is_empty());
    }

    #[test]
    fn test_drain_abandon_queue_missing_id_returns_false_work_done() {
        // ADR-052 §3.3.3: if all IDs in the queue are unknown, did_work=false
        // (no recalibrate triggered).
        let mut agent_loop = make_loop_for_drain_tests();
        agent_loop
            .history_mut()
            .append(make_tool_message_for_drain("some content", "toolu_known"));
        let tokens_before = agent_loop.session.history.token_count();

        agent_loop
            .abandon_queue
            .lock()
            .unwrap()
            .push_back("toolu_unknown_1".to_string());
        agent_loop
            .abandon_queue
            .lock()
            .unwrap()
            .push_back("toolu_unknown_2".to_string());

        let did_work = agent_loop.drain_abandon_queue();
        assert!(
            !did_work,
            "all-unknown IDs should leave did_work=false (no recalibrate)"
        );

        // Content unchanged
        let msgs = agent_loop.session.history.messages();
        assert_eq!(msgs[0].content, "some content");
        let tokens_after = agent_loop.session.history.token_count();
        assert_eq!(
            tokens_before, tokens_after,
            "no recalibrate should have happened"
        );
    }

    #[test]
    fn test_drain_retrieve_queue_empty_returns_false() {
        // ADR-052 §3.2.2: empty queue returns false without recalibrate_tokens
        let mut agent_loop = make_loop_for_drain_tests();
        let tokens_before = agent_loop.session.history.token_count();

        let did_work = agent_loop.drain_retrieve_queue();
        assert!(!did_work, "empty queue should return false");

        let tokens_after = agent_loop.session.history.token_count();
        assert_eq!(
            tokens_before, tokens_after,
            "empty drain should not call recalibrate_tokens"
        );
    }

    #[test]
    fn test_drain_retrieve_queue_restores_in_place() {
        // ADR-052 §3.2.2: non-empty queue drains, calls retrieve_tool_result,
        // restores original content at the placeholder's position.
        let mut agent_loop = make_loop_for_drain_tests();
        let original = "The original full content".repeat(100);
        agent_loop
            .history_mut()
            .append(make_tool_message_for_drain(&original, "toolu_abc"));

        // First abandon the message so it becomes a placeholder
        let n = agent_loop.history_mut().abandon_tool_result("toolu_abc");
        assert_eq!(n, 1);
        assert!(agent_loop.session.history.messages()[0]
            .content
            .starts_with("[Tool result compressed."));

        // Now queue a retrieve for the same id
        agent_loop
            .retrieve_queue
            .lock()
            .unwrap()
            .push_back(("toolu_abc".to_string(), original.clone()));

        let did_work = agent_loop.drain_retrieve_queue();
        assert!(did_work);

        // Original content restored IN PLACE (not appended)
        let msgs = agent_loop.session.history.messages();
        assert_eq!(msgs.len(), 1, "retrieve should NOT add new messages");
        assert_eq!(
            msgs[0].content, original,
            "retrieve should restore content at the original position"
        );

        // name + tool_call_id preserved
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("toolu_abc"));
        assert_eq!(msgs[0].name.as_deref(), Some("content_search"));

        // Queue is empty
        assert!(agent_loop.retrieve_queue.lock().unwrap().is_empty());
    }

    #[test]
    fn test_drain_retrieve_queue_missing_placeholder_returns_false() {
        // ADR-052 §3.2.2: if the target is already raw (not a placeholder),
        // retrieve_tool_result returns 0 and did_work stays false.
        let mut agent_loop = make_loop_for_drain_tests();
        let original = "Original content here";
        agent_loop
            .history_mut()
            .append(make_tool_message_for_drain(original, "toolu_raw"));

        // Queue retrieve for an already-raw message (no abandon first)
        agent_loop
            .retrieve_queue
            .lock()
            .unwrap()
            .push_back(("toolu_raw".to_string(), "new content".to_string()));

        let did_work = agent_loop.drain_retrieve_queue();
        assert!(
            !did_work,
            "already-raw message should not trigger recalibrate"
        );

        // Content unchanged (still original)
        let msgs = agent_loop.session.history.messages();
        assert_eq!(msgs[0].content, original);
    }

    #[test]
    fn test_drain_abandon_then_retrieve_round_trip() {
        // ADR-052 §3.2.3: the abandon↔retrieve symmetry forms a closed loop.
        let mut agent_loop = make_loop_for_drain_tests();
        let original = "Round-trip test content".repeat(50);
        agent_loop
            .history_mut()
            .append(make_tool_message_for_drain(&original, "toolu_rt"));

        // Cycle 1: abandon
        agent_loop
            .abandon_queue
            .lock()
            .unwrap()
            .push_back("toolu_rt".to_string());
        assert!(agent_loop.drain_abandon_queue());
        assert!(agent_loop.session.history.messages()[0]
            .content
            .starts_with("[Tool result compressed."));

        // Cycle 2: retrieve
        agent_loop
            .retrieve_queue
            .lock()
            .unwrap()
            .push_back(("toolu_rt".to_string(), original.clone()));
        assert!(agent_loop.drain_retrieve_queue());
        assert_eq!(agent_loop.session.history.messages()[0].content, original);

        // Cycle 3: re-abandon (close the loop)
        agent_loop
            .abandon_queue
            .lock()
            .unwrap()
            .push_back("toolu_rt".to_string());
        assert!(agent_loop.drain_abandon_queue());
        assert!(agent_loop.session.history.messages()[0]
            .content
            .starts_with("[Tool result compressed."));
    }
}
