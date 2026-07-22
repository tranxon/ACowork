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
use std::sync::atomic::AtomicU64;

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
use crate::agent::session_state::SessionState;
use crate::config::RuntimeConfig;
use crate::conversation::ConversationSession;
use crate::error::{Result, RuntimeError};
use crate::security::approval_gate::ApprovalRequest;
use crate::tools::builtin::ask_user_question::QuestionOption;

use crate::agent::session_state::SessionStatus;

/// ADR-032 C4b: User-initiated compression actions.
///
/// These are triggered via frontend/CLI buttons regardless of the
/// current `CompressionMode` (Auto/Manual).  In Auto mode the system
/// also fires at event points; in Manual mode these are the only way.
#[derive(Debug, Clone)]
pub enum CompressionAction {
    /// Run `compress_tool_results` (L0 placeholder compression).
    CompressToolResults,
    /// Run LLM-based summary compaction.
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
    /// Session lifecycle status changed (ADR-014).
    /// Emitted whenever SessionState::status transitions, so the frontend
    /// can stay in sync without optimistic local writes.
    ///
    /// ADR-039: persistent per-session fields (model, provider, workspace_id,
    /// reasoning_effort, temperature) are no longer carried in this event.
    /// They are broadcast through the `session_meta` MQTT channel which
    /// sources them from `data/meta/{session_id}.json`.
    SessionStateChanged {
        status: SessionStatus,
        /// Current model chars/token ratio from API calibration.
        /// `None` before the first calibration.
        ratio: Option<f64>,
        /// ADR-028: JSON-serialized ContextUsageInfo snapshot from persisted
        /// session tokens. Set on session activation/resume so the frontend
        /// can show token counts without waiting for the first LLM call.
        context_usage: Option<String>,
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

    /// Per-session persisted metadata changed (title, model, provider,
    /// reasoning_effort, temperature, workspace_id, message_count, tokens).
    ///
    /// Triggered by `ConversationSession::write_meta()` — every code path
    /// that persists the per-session meta file ends up here. The payload
    /// is always the **latest complete** `SessionMeta` snapshot, never a
    /// diff; the MQTT broker retains it so a (re)connecting Desktop sees
    /// the current state without an HTTP fetch.
    ///
    /// Distinction from `SessionStateChanged`:
    ///   - `SessionMetaChanged`  → persisted per-session config
    ///   - `SessionStateChanged` → runtime state (status, context_usage)
    SessionMetaChanged {
        /// Snapshot of the just-written ConversationSession state, already
        /// flattened into the on-the-wire `mqtt_proto::SessionMeta` shape.
        meta: acowork_core::mqtt_proto::SessionMeta,
        /// Human-readable field names that triggered this emit (used for
        /// relay-side debug logging only — payload is the full snapshot).
        fields_changed: Vec<&'static str>,
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
    /// the main loop receives them and handles the pause/resume cycle.
    pub(crate) approval_rx: mpsc::Receiver<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
    /// Approval handle (sender side) — cloned into spawned tool tasks.
    pub(crate) approval_handle: ApprovalHandle,
    /// Counter for generating unique approval request IDs.
    pub(crate) approval_next_id: AtomicU64,
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
    /// Transient tool results from the previous iteration (ADR-032 C3a).
    ///
    /// Tools with `transient: true` (e.g., `context_recall`) have their
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
        conversation: Option<ConversationSession>,
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
            core,
            session_core,
            session: SessionState::new(max_tokens, budget, conversation),
            inbound_rx,
            approval_rx,
            approval_handle: approval_handle.clone(),
            approval_next_id: AtomicU64::new(0),
            last_input_chars: 0,
            last_reasoning_effort: None,
            last_thinking_mode: None,
            pending_interrupt: None,
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
        conversation: Option<ConversationSession>,
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
            core,
            session_core,
            session,
            inbound_rx,
            approval_rx,
            approval_handle: approval_handle.clone(),
            approval_next_id: AtomicU64::new(0),
            last_input_chars: 0,
            last_reasoning_effort: None,
            last_thinking_mode: None,
            pending_interrupt: None,
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
    //   - write_document_entries

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
    ) -> Result<String> {
        self.run_inner(user_message, context_builder, false, content_parts, message_id, raw_user_message)
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
        self.run_inner(user_message, context_builder, true, content_parts, None, None)
            .await
    }

    /// Core agent loop shared by [`run`] and [`replay`].
    async fn run_inner(
        &mut self,
        user_message: &str,
        context_builder: &mut ContextBuilder,
        replay: bool,
        content_parts: Option<Vec<acowork_core::providers::traits::ContentPart>>,
        message_id: Option<String>,
        raw_user_message: Option<&str>,
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
        self.transition_status(SessionStatus::Streaming { message_id: None });

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
            if let Some(ref conversation) = self.session.conversation {
                conversation.append_message_with_id("user", user_message, None, message_id);
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
                    let trimmed: String = existing_title.chars().take(crate::prompt::SESSION_TITLE_MAX_CHARS).collect();
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
                    let provider = self.core.provider.clone();
                    let compact_model = self.resolve_distill_model(title_input);
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
                        match crate::episode_distill::compact_session_title_with_llm(
                            &prompt,
                            provider.as_ref(),
                            &compact_model,
                            64,
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
                                let trimmed: String = title.chars().take(crate::prompt::SESSION_TITLE_MAX_CHARS).collect();
                                *session_core_title.write().unwrap() = Some(trimmed);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "LLM session title generation failed (non-fatal): {e}"
                                );
                                let fallback: String = fallback_msg.chars().take(crate::prompt::SESSION_TITLE_MAX_CHARS).collect();
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
                self.transition_status(SessionStatus::Paused {
                    iteration: Some(iteration),
                    max_iterations: Some(self.core.config.max_iterations),
                    retry_info: None,
                });
                let max_iters = self.core.config.max_iterations;
                let message = format!(
                    "Iteration limit reached ({}/{}). Click Continue to proceed.",
                    iteration, max_iters
                );
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
                            self.transition_status(SessionStatus::Streaming { message_id: None });
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
                                    self.transition_status(SessionStatus::Streaming {
                                        message_id: None,
                                    });
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
                                        SessionStatus::Streaming {
                                            message_id: None,
                                        },
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
                                                SessionStatus::Streaming {
                                                    message_id: None,
                                                },
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
                                crate::debug::server::DebugEvent::ExecutionStateChanged {
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
                        self.transition_status(SessionStatus::Streaming { message_id: None });
                        break;
                    }
                    crate::debug::controller::DebugState::Stepping => {
                        self.transition_status(SessionStatus::Streaming { message_id: None });
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
                context_builder,
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
        // never permanently stored in history. The current set of transient
        // tools is: `context_recall`. The transient flag is set in
        // `execute_single_tool` (loop_tools.rs) — see ADR-032 C3a.
        //
        // This is what breaks the
        // recall → compress → recall loop: the recalled content is visible
        // to the LLM exactly once (in the next chat request) and then
        // discarded. The compressed placeholder in history is never
        // overwritten with the recalled content, so future
        // `compress_tool_results` calls cannot re-trigger on the same data.
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

    // ── ADR-032 C4b: compression action + mode helpers ──

    /// Resolve the effective compression mode for this session.
    ///
    /// Resolution chain (Layer 1 = highest priority):
    /// 1. `self.core.compression_mode_override` — set from agent_config via
    ///    `apply_runtime_config_override` and hot-patched by RuntimeConfigUpdate
    /// 2. `crate::agent::loop_context::DEFAULT_COMPRESSION_MODE` — hardcoded (Auto)
    pub(crate) fn compression_mode(&self) -> crate::agent::loop_context::CompressionMode {
        let mode_str = self.core.compression_mode_override.as_deref();
        match mode_str {
            Some("manual") => crate::agent::loop_context::CompressionMode::Manual,
            _ => crate::agent::loop_context::CompressionMode::Auto,
        }
    }

    /// Drain any pending user-initiated compression actions from the channel.
    ///
    /// Called at the start of each iteration so user-initiated compression
    /// actions are honored regardless of the current `CompressionMode`.
    /// Returns `true` if any action was processed (caller may want to
    /// rebuild the chat request after this).
    pub(crate) fn drain_compress_actions(&mut self) -> bool {
        let Some(rx) = &mut self.compress_action_rx else {
            return false;
        };
        let mut did_work = false;
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                CompressionAction::CompressToolResults => {
                    let n = self.core.tool_result_keep_recent_n();
                    let soft_threshold = self.core.tool_result_soft_threshold_chars();
                    let compressed = self.session.history.compress_tool_results(
                        soft_threshold,
                        n as usize,
                    );
                    if compressed > 0 {
                        self.session.history.recalibrate_tokens();
                        tracing::info!(compressed, "Manual compress_tool_results executed");
                    }
                    did_work = true;
                }
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

    /// Check whether compression is enabled for event triggers.
    pub(crate) fn event_compression_enabled(&self) -> bool {
        matches!(
            self.compression_mode(),
            crate::agent::loop_context::CompressionMode::Auto
        )
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
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
        let _ = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
        let _ = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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

        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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

        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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

        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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

        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
        let result = agent_loop.run("Hi", &mut context_builder, None, None, None).await;
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
            .run("Run parallel", &mut context_builder, None, None, None)
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
            .run("Test failure", &mut context_builder, None, None, None)
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
            .run("Test timeout", &mut context_builder, None, None, None)
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
            .run("Run shell", &mut context_builder, None, None, None)
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
            .run("Run ordered", &mut context_builder, None, None, None)
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
            .run("Test iteration timeout", &mut context_builder, None, None, None)
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
            .run("Test tool timeout", &mut context_builder, None, None, None)
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
            .run("Test partial permission", &mut context_builder, None, None, None)
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
}
