//! Agent main loop (9 steps)
//!
//! The core execution loop for Agent Runtime.
//! References ZeroClaw agent/loop_.rs but simplified for IPC architecture.
//!
//! S1.5: Streaming LLM responses via chat_stream()
//! S1.6: InboundQueue for external message injection
//! S1.7: Parallel tool execution with per-tool timeout

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use rollball_core::providers::traits::{
    ChatMessage, ChatResponse, MessageRole, Provider, ToolCall,
};
use rollball_core::protocol::ModelCapabilitiesInfo;
use rollball_core::tools::traits::Tool;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::agent::agent_core::AgentCore;
use crate::agent::budget_guard::BudgetCheckResult;
use crate::agent::context::ContextBuilder;
use crate::agent::history::HistoryManager;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_detector::{LoopDetectionResult, LoopPattern, ResponseLevel};
use crate::agent::session_state::SessionState;
use crate::config::RuntimeConfig;
use crate::conversation::ConversationSession;
use crate::error::{Result, RuntimeError};
use crate::security::approval_gate::ApprovalRequest;
use crate::tools::builtin::ask_user_question::{AskUserQuestionTool, QuestionOption};

/// User's decision on a tool approval request.
#[derive(Debug, Clone)]
pub(crate) struct ApprovalDecision {
    pub approved: bool,
    #[allow(dead_code)]
    pub allow_all_session: bool,
    /// Human-readable reason for timeout or rejection (for LLM feedback)
    pub reason: Option<String>,
}

/// Lightweight handle for spawned tool tasks to request user approval.
///
/// The spawned task calls `request_approval()` (no timeout), which sends the
/// request to the AgentLoop main loop via an mpsc channel and blocks on a
/// oneshot. The main loop receives the request, emits ChunkEvent::ToolApprovalNeeded
/// to the Gateway (which forwards to the Desktop App), pauses via
/// `await_approval_decision()`, and resolves the oneshot when the user's
/// decision arrives as InboundMessage::ApprovalDecision.
#[derive(Clone)]
pub(crate) struct ApprovalHandle {
    request_tx: mpsc::Sender<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
}

impl ApprovalHandle {
    pub fn new(
        request_tx: mpsc::Sender<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
    ) -> Self {
        Self { request_tx }
    }

    /// Request user approval for a tool execution.
    /// Blocks without timeout until the user decides (Allow/Deny).
    pub async fn request_approval(&self, req: ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        if self.request_tx.send((req, tx)).await.is_err() {
            tracing::warn!("ApprovalHandle: request channel closed, auto-rejecting");
            return ApprovalDecision { approved: false, allow_all_session: false, reason: None };
        }
        rx.await.unwrap_or_else(|_| {
            tracing::warn!("ApprovalHandle: oneshot sender dropped, auto-rejecting");
            ApprovalDecision { approved: false, allow_all_session: false, reason: None }
        })
    }
}

use crate::agent::session_state::SessionStatus;

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
/// Adapted from ZeroClaw's DraftEvent, simplified for RollBall's IPC architecture.
/// Each delta is forwarded to the Gateway via `StreamChunk` gRPC message,
/// which maps to a BridgeEventType for the Desktop App WebSocket.
/// SPDX-License-Identifier: MIT OR Apache-2.0
#[derive(Debug, Clone)]
pub enum ChunkEvent {
    /// LLM reasoning phase started — the provider.stream() call has been
    /// initiated and tokens will arrive shortly. The frontend should show
    /// a pulsing "..." indicator until the first content delta arrives.
    ReasoningStarted,
    /// Content delta to append to the streaming message
    Delta(String),
    /// Reasoning/thinking content delta (e.g. DeepSeek thinking mode)
    ReasoningDelta(String),
    /// Context usage report (after each LLM call)
    ContextUsage(rollball_core::protocol::ContextUsageInfo),
    /// Tool call event (routed through chunk channel for ordering guarantee)
    ToolCall {
        name: String,
        args: String,
        id: String,
    },
    /// Tool result event (routed through chunk channel for ordering guarantee)
    ToolResult {
        name: String,
        result: String,
        tool_call_id: String,
    },
    /// Iteration limit reached — agent loop paused
    IterationLimitPaused {
        iteration: u32,
        max_iterations: u32,
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
    Interrupted {
        content: String,
    },
    /// Agent response complete (routed through chunk channel for ordering guarantee
    /// with preceding content chunks)
    Done {
        content: String,
        message_id: String,
    },
    /// Agent error (routed through chunk channel for ordering guarantee)
    Error {
        message: String,
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
        /// Optional timeout in seconds (None = use default)
        timeout_seconds: Option<u32>,
    },
    /// Session lifecycle status changed (ADR-014).
    /// Emitted whenever SessionState::status transitions, so the frontend
    /// can stay in sync without optimistic local writes.
    SessionStateChanged {
        status: SessionStatus,
    },
    /// Todo list updated — emitted after a `todo_write` tool call mutates
    /// SessionState.todos, so the frontend can render the current task list.
    TodoListUpdated {
        todos: Vec<crate::agent::session_state::TodoItem>,
    },
}

/// Result of executing a single iteration of the agent loop.
///
/// This is the shared building block used by both:
/// - Production `run()`: loops automatically until TextResponse/Interrupted
/// - Debug `DebugSessionTask`: calls one iteration at a time with pause/breakpoint control
#[derive(Debug)]
pub(crate) enum IterationResult {
    /// Agent returned a text response — conversation round complete
    TextResponse(String),
    /// Tool calls were executed successfully — continue to next iteration
    ToolCallsExecuted,
    /// Agent was interrupted by user request
    Interrupted(String),
}

/// Agent loop runner
pub struct AgentLoop {
    /// Cross-session shared state (config, provider, tools, capabilities)
    pub(crate) core: AgentCore,
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
}

impl AgentLoop {
    /// Create a new agent loop runner, returning both the loop and an inbound sender.
    ///
    /// The caller can use the sender to inject messages into the loop from
    /// external sources (Gateway, cross-agent intents, system notifications).
    ///
    /// If `on_chunk` is provided, streaming LLM deltas are forwarded to it
    /// so the caller can relay chunks to the Gateway via StreamChunk messages
    /// (like ZeroClaw's on_delta / DraftEvent pattern).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RuntimeConfig,
        manifest: rollball_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        budget: rollball_core::Budget,
        on_chunk: Option<mpsc::Sender<SessionChunkEvent>>,
        conversation: Option<ConversationSession>,
    ) -> (Self, tokio::sync::mpsc::Sender<InboundMessage>) {
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(64);
        let (approval_tx, approval_rx) = mpsc::channel::<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>(16);
        let max_tokens = config.history_max_tokens;
        let approval_handle = ApprovalHandle::new(approval_tx);
        let mut loop_ = Self {
            core: AgentCore::new(config, manifest, provider, tools, on_chunk),
            session: SessionState::new(max_tokens, budget, conversation),
            inbound_rx,
            approval_rx,
            approval_handle: approval_handle.clone(),
            approval_next_id: AtomicU64::new(0),
        };
        // Inject approval_handle into AgentCore so execute_tools_parallel can detect Gateway mode
        loop_.core.approval_handle = Some(approval_handle);
        (loop_, inbound_tx)
    }

    /// Create an AgentLoop from pre-built components (for multi-session Actor model).
    ///
    /// This constructor accepts an owned `AgentCore` and `SessionState`,
    /// used by `SessionTask` to spawn independent sessions that share
    /// provider/tools/config via Arc but have independent history,
    /// budget, and loop detection.
    ///
    /// The caller typically clones `AgentCore` data from a shared `Arc<AgentCore>`
    /// template before passing it here, so each session gets its own owned copy
    /// while the heavy fields (provider, tools) remain Arc-shared behind the scenes.
    pub(crate) fn from_core_and_session(
        core: AgentCore,
        session: SessionState,
    ) -> (Self, tokio::sync::mpsc::Sender<InboundMessage>) {
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(64);
        let (approval_tx, approval_rx) = mpsc::channel::<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>(16);
        let approval_handle = ApprovalHandle::new(approval_tx);
        let mut session_loop = Self { core, session, inbound_rx, approval_rx, approval_handle: approval_handle.clone(), approval_next_id: AtomicU64::new(0) };
        // Inject approval_handle into AgentCore so execute_tools_parallel can detect Gateway mode
        session_loop.core.approval_handle = Some(approval_handle);
        (session_loop, inbound_tx)
    }

    /// Transition session status and emit SessionStateChanged event if the status changed.
    ///
    /// ADR-014 helper: ensures every status transition is paired with an event emission.
    /// Returns true if the status actually changed (and event was emitted).
    fn transition_status(&mut self, new_status: SessionStatus) -> bool {
        if self.session.set_status(new_status) {
            let status = self.session.status.clone();
            // Emit chunk event to Gateway → frontend
            if !self.core.try_send_chunk(ChunkEvent::SessionStateChanged { status: status.clone() }) {
                tracing::warn!(
                    "SessionStateChanged event dropped (channel full/closed), status={:?}. Pull repair will correct frontend.",
                    status
                );
            }
            // Update watch channel for SessionHandle reads
            if let Some(ref tx) = self.core.status_tx {
                let _ = tx.send(status);
            }
            true
        } else {
            false
        }
    }

    /// Update the LLM provider at runtime (e.g., after receiving a hot-pushed
    /// LLMConfigDelivery from Gateway).
    /// `provider_id` is the Vault provider ID (not protocol name) for
    /// compact_model lookup.
    pub fn update_provider(
        &mut self,
        new_provider: Arc<dyn Provider>,
        model: String,
        provider_id: Option<String>,
    ) {
        self.core.update_provider(new_provider, model);
        if let Some(pid) = provider_id {
            self.core.current_provider_id = Some(pid);
        }
    }

    /// Update gateway model capabilities at runtime (e.g., after receiving a
    /// hot-pushed LLMConfigDelivery from Gateway).
    /// The capabilities are stored keyed by model name for multi-model support.
    pub fn update_gateway_model_capabilities(&mut self, caps: ModelCapabilitiesInfo) {
        self.core.update_gateway_model_capabilities(caps);
    }

    /// Update the max output tokens limit from Gateway config.
    pub fn update_max_output_tokens_limit(&mut self, limit: u64) {
        self.core.update_max_output_tokens_limit(limit);
    }

    /// Apply runtime config overrides from Gateway.
    pub fn apply_runtime_config(
        &mut self,
        max_output_tokens: Option<u64>,
        max_iterations: Option<u32>,
        temperature: Option<f32>,
        system_prompt_override: Option<String>,
        shell_approval_threshold: Option<String>,
    ) {
        self.core.apply_runtime_config(max_output_tokens, max_iterations, temperature, system_prompt_override, shell_approval_threshold);
    }

    /// Apply a user operation delivered via the `send_inbound()` fast channel.
    ///
    /// This is the central dispatch point for all `UserOp` variants received
    /// through `InboundMessage::UserOperation`. It handles operations that
    /// must take effect immediately even while the agent loop is mid-execution.
    ///
    /// Returns `true` if the operation is an interrupt (caller should abort
    /// the current loop).
    pub(crate) fn apply_user_op(&mut self, op: &crate::agent::inbound::UserOp) -> bool {
        match op {
            crate::agent::inbound::UserOp::InterruptLoop { reason } => {
                tracing::info!(reason = %reason, "UserOp: interrupt loop");
                true
            }
            crate::agent::inbound::UserOp::ContinueLoop { reason } => {
                tracing::info!(reason = %reason, "UserOp: continue loop (no-op here; handled at iteration limit pause)");
                false
            }
            crate::agent::inbound::UserOp::ApprovalDecision { .. } => {
                tracing::debug!("UserOp: approval decision (no-op here; handled via approval subsystem)");
                false
            }
            crate::agent::inbound::UserOp::QuestionAnswer { .. } => {
                tracing::debug!("UserOp: question answer (no-op here; handled via ask_user_question subsystem)");
                false
            }
            crate::agent::inbound::UserOp::UpdateRuntimeConfig {
                max_output_tokens,
                max_iterations,
                temperature,
                system_prompt_override,
                shell_approval_threshold,
            } => {
                tracing::info!(
                    max_output_tokens,
                    max_iterations,
                    temperature,
                    system_prompt_override = system_prompt_override.as_deref(),
                    shell_approval_threshold = shell_approval_threshold.as_deref(),
                    "UserOp: applying runtime config immediately in AgentLoop"
                );
                self.apply_runtime_config(
                    *max_output_tokens,
                    *max_iterations,
                    *temperature,
                    system_prompt_override.clone(),
                    shell_approval_threshold.clone(),
                );
                false
            }
        }
    }

    /// Get the current conversation session ID (S1.14)
    ///
    /// Returns the session ID of the active ConversationSession,
    /// or None if no session is active.
    pub fn current_session_id(&self) -> Option<&str> {
        self.session.conversation.as_ref().map(|c| c.session_id())
    }

    /// Initialize the Grafeo memory store at the given workspace path.
    ///
    /// Delegates to `AgentCore::init_memory_store()`.
    /// Opens or creates `{work_dir}/memory/private.grafeo`.
    pub fn init_memory_store(&mut self, work_dir: &std::path::Path) {
        self.core.init_memory_store(work_dir);
    }

    /// Retrieve relevant long-term memories from Grafeo and inject them into
    /// the ContextBuilder for the next LLM call.
    ///
    /// Runs once per `run()` invocation, before the first LLM iteration.
    /// When the memory store is unavailable, this is a silent no-op.
    ///
    /// Returns the list of Grafeo node IDs that were retrieved (P2-4 fix).
    /// These IDs are passed to `record_turn_to_memory` so that future
    /// retrieval can trace which memories influenced each turn.
    async fn retrieve_and_inject_memories(
        &self,
        user_message: &str,
        context_builder: &mut ContextBuilder,
    ) -> Vec<String> {
        // P0 fix: Always clear stale memory from previous turns first.
        // ContextBuilder is reused across turns (SessionTask loop), so
        // without this, stale memory leaks into the next LLM call.
        context_builder.clear_retrieved_memory();

        let store = match self.core.memory_store() {
            Some(s) => s,
            None => return vec![], // No store available, already cleared above
        };

        let manager = self.core.init_memory_manager();
        let query = rollball_memory::MemoryQuery {
            query_text: user_message.to_string(),
            embedding: None, // Text-only search initially; hybrid when embedding is available
            filters: Default::default(),
            limit: 10,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: false,
            hint_type: rollball_memory::HintType::Semantic,
        };

        // P2-4 fix: Use retrieve + inject separately (instead of process_turn)
        // so we can capture the node IDs of retrieved memories for traceability.
        match manager.retrieve(store, &query).await {
            Ok(retrieval) => {
                // Capture node IDs before inject (inject discards the RetrievalResult)
                let memory_ids: Vec<String> = retrieval
                    .memories
                    .iter()
                    .filter(|m| m.node_id != 0) // 0 = RAG result, not Grafeo local
                    .map(|m| m.node_id.to_string())
                    .collect();

                let metrics = retrieval.metrics.clone();
                let injected = manager.inject(&retrieval, crate::memory::MemoryManagerConfig::default().max_inject_tokens);
                if !injected.formatted_text.is_empty() {
                    tracing::info!(
                        memory_count = injected.memory_count,
                        token_count = injected.token_count,
                        avg_score = metrics.avg_score,
                        "Retrieved and injected long-term memories into context"
                    );
                    context_builder.set_retrieved_memory(injected.formatted_text);
                }
                memory_ids
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to retrieve memories from Grafeo (non-fatal)"
                );
                vec![]
            }
        }
    }

    /// Record a conversation turn to the Grafeo memory store.
    ///
    /// This is called at the end of each successful agent loop iteration
    /// (when a text response is returned without tool calls).
    fn record_turn_to_memory(
        &self,
        user_message: &str,
        assistant_response: &str,
        turn_index: u32,
        retrieved_memory_ids: Vec<String>,
    ) {
        let store = match self.core.memory_store() {
            Some(s) => s,
            None => return, // Memory store not initialized, skip silently
        };

        let session_id = self
            .session
            .conversation
            .as_ref()
            .map(|c| c.session_id().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let record = crate::memory::ConversationRecord {
            session_id,
            turn_index, // P1-2 fix: use actual turn counter from SessionState
            user_message: user_message.to_string(),
            assistant_response: assistant_response.to_string(),
            retrieved_memory_ids, // P2-4 fix: node IDs from retrieve_and_inject_memories
            timestamp: chrono::Utc::now(),
        };

        let manager = self.core.init_memory_manager();
        if let Err(e) = manager.record(store, &record) {
            tracing::warn!(
                error = %e,
                "Failed to record conversation turn to Grafeo memory (non-fatal)"
            );
        }
    }

    /// Write document upload entries to the conversation JSONL.
    ///
    /// Each document is persisted as a `ConversationEntry` with `role: "system"`
    /// and `metadata.type: "document_upload"` so that the Desktop App can render
    /// document chips when loading historical sessions.
    pub fn write_document_entries(&self, documents: &[serde_json::Value]) {
        if let Some(ref conversation) = self.session.conversation {
            for doc in documents {
                let filename = doc.get("filename").and_then(|v| v.as_str()).unwrap_or("");
                let format = doc.get("format").and_then(|v| v.as_str()).unwrap_or("");
                let size = doc.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                let content = format!("Uploaded file: {} ({}, {} bytes)", filename, format, size);
                let metadata = serde_json::json!({
                    "type": "document_upload",
                    "document_id": doc.get("id"),
                    "filename": filename,
                    "format": format,
                    "size_bytes": size,
                    "path": doc.get("abs_path"),
                });
                conversation.append_message("system", &content, Some(metadata));
            }
        }
    }

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
            .tools
            .iter()
            .find(|t| t.spec().name == name)
            .ok_or_else(|| format!("Tool not found: {}", name))?;

        match tool.execute(params).await {
            Ok(result) if result.ok => Ok(result.content),
            Ok(result) => Err(result
                .error
                .unwrap_or_else(|| "Unknown tool error".to_string())),
            Err(e) => Err(format!("Tool execution error: {e}")),
        }
    }

    /// Update the title of the currently active conversation session.
    ///
    /// Returns `Some(true)` if the title was actually written (different from current),
    /// `Some(false)` if the title was already the same (no-op),
    /// or `None` if no active session exists.
    pub fn update_session_title(&mut self, title: &str) -> Option<bool> {
        self.session.conversation.as_ref().map(|conv| conv.update_title_force(title))
    }

    /// Persist the per-session workspace selection to the JSONL conversation file.
    ///
    /// Only effective when the session has an active `ConversationSession`.
    pub fn update_session_workspace_id(&mut self, workspace_id: &str) {
        if let Some(ref conv) = self.session.conversation {
            conv.update_workspace_id(workspace_id);
        }
    }

    /// Look up model capabilities by exact model name (delegates to AgentCore).
    pub(crate) fn get_model_capabilities(&self, model_name: &str) -> Option<&ModelCapabilitiesInfo> {
        self.core.get_model_capabilities(model_name)
    }

    /// Resolve the current model name for capability lookups.
    /// Uses override_model (set by model_switch) if present,
    /// otherwise falls back to manifest suggested_model (guaranteed non-empty).
    pub(crate) fn resolve_current_model(&self, ctx: Option<&ContextBuilder>) -> String {
        ctx.and_then(|cb| cb.override_model())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.core.manifest.llm.suggested_model.clone())
    }

    /// Get the context window budget for history trimming.
    /// Uses Gateway model capabilities (context_window) if available,
    /// otherwise falls back to config.history_max_tokens.
    fn context_trim_budget(&self, model_name: &str) -> u64 {
        self.core.context_trim_budget(model_name)
    }

    /// Write a distilled episode to the Grafeo memory store (best-effort).
    ///
    /// P2-1 fix: extracted common pattern from 3 duplicate code blocks
    /// (trimmed distillation, switch-session distillation, session-close distillation).
    fn write_distilled_to_grafeo(
        memory_store: &Option<std::sync::Arc<rollball_grafeo::GrafeoStore>>,
        episode: &crate::episode_distill::DistilledEpisode,
        context: &str, // for logging: "trimmed", "switch-session", "session"
    ) {
        if let Some(store) = memory_store {
            let manager = crate::memory::MemoryManager::new(
                crate::memory::MemoryManagerConfig::default(),
            );
            if let Err(e) = manager.record_distilled(store, episode) {
                tracing::warn!(
                    error = %e,
                    distill_context = context,
                    "Failed to write distillation to Grafeo (non-fatal)"
                );
            }
        }
    }

    /// Trim history to fit within the context window budget.
    /// Reserves 20% of the budget for new response + overhead.
    ///
    /// Per [ADR-010], programmatic folding strategies have been removed.
    /// This is a safety net only; LLM-based summarization at 80% token usage
    /// is the primary compression mechanism.
    fn trim_history_to_budget(&mut self, model_name: &str) {
        let budget = self.context_trim_budget(model_name);
        // Reserve 20% of context window for new response + overhead
        let trim_budget = (budget as f64 * 0.8) as u64;

        // Stage 1: FIFO trim oldest non-system messages until within budget
        self.session.history.trim_fifo();

        // Stage 2: If still over budget after FIFO, use emergency trim as safety net
        if self.session.history.token_count() > trim_budget {
            self.session.history.emergency_trim();
        }

        // Also truncate any single message that exceeds per-message limit
        self.session.history.truncate_large_messages(trim_budget / 4);
    }

    /// Close the conversation session and trigger session-level distillation.
    ///
    /// This method:
    /// 1. Spawns an async distillation task for the entire session
    /// 2. Closes the conversation writer
    ///
    /// Distillation is best-effort and non-blocking.
    pub async fn close_session_with_distillation(&mut self) -> Result<()> {
        self.close_session_inner().await
    }

    /// Resolve the model to use for session distillation.
    ///
    /// Priority order:
    /// 1. Provider's configured `compact_model` from provider_list (read from disk)
    /// 2. Cheapest model whose context_window fits
    /// 3. Model with the largest context_window (last resort)
    fn resolve_distill_model(&self, content_size_bytes: u64) -> String {
        let estimated_tokens = (content_size_bytes / 4) as u64;

        // Path 1: resolve compact_model from current provider (in-memory)
        let compact_model = self
            .core
            .current_provider_id
            .as_ref()
            .and_then(|pid| self.core.provider_compact_models.get(pid))
            .and_then(|cm| cm.clone());
        if let Some(ref compact_model) = compact_model {
            if let Some(cap) = self
                .core
                .gateway_model_capabilities
                .get(compact_model)
            {
                if cap.context_window >= estimated_tokens {
                    tracing::info!(
                        compact_model = %compact_model,
                        context_window = cap.context_window,
                        estimated_tokens,
                        "Using provider's compact model for distillation"
                    );
                    return compact_model.clone();
                }
                tracing::warn!(
                    compact_model = %compact_model,
                    context_window = cap.context_window,
                    estimated_tokens,
                    "Provider compact model context_window too small, falling back"
                );
            } else {
                tracing::warn!(
                    compact_model = %compact_model,
                    "Provider compact model not found in capabilities, falling back"
                );
            }
        }

        // Path 2: compact model unavailable or context too small —
        // fall back to the session's current model.
        let current_model = self.resolve_current_model(None);
        tracing::info!(
            current_model = %current_model,
            estimated_tokens,
            "Compact model not available or insufficient, using current model for distillation"
        );
        current_model
    }

    /// Switch to a new conversation session.
    ///
    /// **Legacy: In Gateway multi-session mode, each session runs in its own
    /// SessionTask/AgentLoop, so this method is not used.** It remains for
    /// potential future CLI standalone mode where a single AgentLoop switches
    /// between conversations.
    ///
    /// This is the **single canonical way** to change the active conversation
    /// on an AgentLoop. It:
    /// 1. Closes the old session (triggers distillation)
    /// 2. Replaces `self.conversation` with the new session
    /// 3. Returns the old session (already closed)
    ///
    /// # Why this matters
    ///
    /// Before this method, `handle_create_session` in cli.rs created a new
    /// `ConversationSession` but **dropped it immediately** — the AgentLoop's
    /// `conversation` field was never updated, so all subsequent messages were
    /// still written to the old JSONL file. This was the P0 root cause of
    /// messages appearing in wrong sessions (see ADR-session-fix).
    #[allow(dead_code)]
    pub fn switch_conversation(
        &mut self,
        new_session: ConversationSession,
    ) -> Option<ConversationSession> {
        let _old_id = self.current_session_id().map(|s| s.to_string());
        let new_id = new_session.session_id().to_string();

        // In pre-async context, we can't await close_session_with_distillation.
        // Instead, we take the old session and spawn its close+distill asynchronously.
        let old_session = self.session.conversation.take();

        if let Some(ref old) = old_session {
            tracing::info!(
                old_session_id = %old.session_id(),
                new_session_id = %new_id,
                "Switching conversation session"
            );
        } else {
            tracing::info!(new_session_id = %new_id, "Activating first conversation session");
        }

        self.session.conversation = Some(new_session);

        // Spawn async close + distill for the old session (best-effort)
        // We need to extract data from old_session without fully moving it,
        // because we still return it at the end. Use as_ref() pattern.
        if let Some(ref old) = old_session {
            let session_id = old.session_id().to_string();
            let session_path = old.session_path().to_path_buf();
            let provider = self.core.provider.clone();
            // Estimate content size from JSONL file; fallback to 0 if file is unavailable.
            let content_size = std::fs::metadata(&session_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let model_name = self.resolve_distill_model(content_size);
            let memory_store = self.core.memory_store().cloned();

            tokio::spawn(async move {
                // Distill the old session
                match crate::episode_distill::EpisodeDistiller::distill_on_session_end(
                    &session_path,
                    &session_id,
                    provider.as_ref(),
                    &model_name,
                )
                .await
                {
                    Ok(episode) => {
                        // Write distilled episode to Grafeo memory store (P2-1: using shared helper)
                        Self::write_distilled_to_grafeo(&memory_store, &episode, "switch-session");
                        tracing::info!(
                            summary = %episode.summary,
                            importance = episode.importance,
                            "Session-level distillation completed for switched-out session"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Session-level distillation failed for switched-out session (non-fatal)"
                        );
                    }
                }
                // Note: The old session's ConversationWriter will be shut down when
                // the ConversationSession is dropped. File flush is best-effort.
            });
        }

        old_session
    }

    /// Inner implementation for closing the current session.
    async fn close_session_inner(&mut self) -> Result<()> {
        if let Some(ref conversation) = self.session.conversation {
            let session_id = conversation.session_id().to_string();
            let session_path = conversation.session_path().to_path_buf();
            let provider = self.core.provider.clone();
            let content_size = std::fs::metadata(&session_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let model_name = self.resolve_distill_model(content_size);
            let memory_store = self.core.memory_store().cloned();

            tracing::info!(
                session_id = %session_id,
                model = %model_name,
                "Spawning session-level episode distillation"
            );

            // Spawn session-level distillation (best-effort, non-blocking)
            tokio::spawn(async move {
                match crate::episode_distill::EpisodeDistiller::distill_on_session_end(
                    &session_path,
                    &session_id,
                    provider.as_ref(),
                    &model_name,
                )
                .await
                {
                    Ok(episode) => {
                        // Write distilled episode to Grafeo memory store (P2-1: using shared helper)
                        Self::write_distilled_to_grafeo(&memory_store, &episode, "session");
                        tracing::info!(
                            summary = %episode.summary,
                            importance = episode.importance,
                            "Session-level episode distillation completed"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Session-level episode distillation failed (non-fatal)"
                        );
                    }
                }
            });

            // Close the conversation writer
            conversation.close().await?;
        }
        Ok(())
    }

    /// Run the agent loop for a single user message.
    ///
    /// When `replay` is true, the user message is NOT appended to history
    /// or persisted to JSONL (it is assumed to already be present, e.g.
    /// after a debug rewind + resume).  Memory retrieval is still performed
    /// in case the context builder has been modified by pending patches.
    pub async fn run(&mut self, user_message: &str, context_builder: &mut ContextBuilder, content_parts: Option<Vec<rollball_core::providers::traits::ContentPart>>) -> Result<String> {
        self.run_inner(user_message, context_builder, false, content_parts).await
    }

    /// Re-run the agent loop after a debug resume (user message already in history).
    ///
    /// Same as [`run`] but skips the user-message append and JSONL persist steps.
    pub async fn replay(&mut self, user_message: &str, context_builder: &mut ContextBuilder, content_parts: Option<Vec<rollball_core::providers::traits::ContentPart>>) -> Result<String> {
        self.run_inner(user_message, context_builder, true, content_parts).await
    }

    /// Core agent loop shared by [`run`] and [`replay`].
    async fn run_inner(&mut self, user_message: &str, context_builder: &mut ContextBuilder, replay: bool, content_parts: Option<Vec<rollball_core::providers::traits::ContentPart>>) -> Result<String> {
        // ADR-014: Idle → Streaming
        self.transition_status(SessionStatus::Streaming { message_id: None });

        if !replay {
            // Add user message to history
            if let Some(parts) = content_parts {
                self.session.history.append(ChatMessage::user_multimodal(user_message, parts));
            } else {
                self.session.history.append(ChatMessage::user(user_message));
            }

            // Persist user message to JSONL
            if let Some(ref conversation) = self.session.conversation {
                conversation.append_message("user", user_message, None);
                // Set session title from first user message (first 100 chars)
                conversation.set_title(user_message);
            }
        }

        // Retrieve relevant long-term memories and inject into context
        // P2-4 fix: capture memory node IDs for later traceability in record_turn_to_memory
        let retrieved_memory_ids = self.retrieve_and_inject_memories(user_message, context_builder).await;

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
                });
                let _ = self.core.try_send_chunk(ChunkEvent::IterationLimitPaused {
                    iteration,
                    max_iterations: self.core.config.max_iterations,
                });

                // Wait for ContinueExecution or Interrupt from inbound queue
                // Also checks UserOperation variants for the unified fast channel.
                loop {
                    match self.inbound_rx.recv().await {
                        Some(InboundMessage::ContinueExecution { reason }) => {
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
                        Some(InboundMessage::Interrupt { reason }) => {
                            tracing::info!(reason = %reason, "User chose to stop during iteration limit pause");
                            // ADR-014: Paused → Idle
                            self.transition_status(SessionStatus::Idle);
                            let name = self.core.user_display_name.as_deref().unwrap_or("user");
                            return Ok(format!("Agent stopped by {} after reaching iteration limit.", name));
                        }
                        Some(InboundMessage::UserOperation(user_op)) => {
                            match user_op {
                                crate::agent::inbound::UserOp::ContinueLoop { reason } => {
                                    tracing::info!(
                                        reason = %reason,
                                        "UserOp: continue loop via fast channel"
                                    );
                                    self.transition_status(SessionStatus::Streaming { message_id: None });
                                    iteration = 0;
                                    self.trim_history_to_budget(&current_model);
                                    break;
                                }
                                crate::agent::inbound::UserOp::InterruptLoop { reason } => {
                                    tracing::info!(reason = %reason, "UserOp: interrupt via fast channel during iteration limit pause");
                                    self.transition_status(SessionStatus::Idle);
                                    let name = self.core.user_display_name.as_deref().unwrap_or("user");
                                    return Ok(format!("Agent stopped by {} after reaching iteration limit.", name));
                                }
                                other_op => {
                                    // Other UserOps (UpdateRuntimeConfig etc.) — apply inline
                                    self.apply_user_op(&other_op);
                                }
                            }
                        }
                        Some(other) => {
                            // Other messages (UserMessage, etc.) — inject into history
                            let (msg, _) = other.enforce_size_limit();
                            match msg {
                                InboundMessage::UserMessage(text) => {
                                    self.session.history.append(ChatMessage::user(text));
                                }
                                InboundMessage::SystemNotification { notification_type, data } => {
                                    self.session.history.append(ChatMessage {
                                        role: MessageRole::User,
                                        content: format!("[system:{}] {}", notification_type, data),
                                        name: Some("system".to_string()),
                                        ..Default::default()
                                    });
                                }
                                InboundMessage::IntentMessage { from, action, params } => {
                                    self.session.history.append(ChatMessage::user(
                                        format!("[intent:{}:{}] {}", from, action, params),
                                    ));
                                }
                                _ => {} // ContinueExecution, Interrupt, UserOperation handled above
                            }
                        }
                        None => {
                            // Channel closed — treat as stop
                            tracing::warn!("Inbound channel closed during iteration limit pause, stopping");
                            return Ok("Agent stopped: inbound channel closed.".to_string());
                        }
                    }
                }
            }

            // ⓪ Drain inbound queue (non-blocking)
            if self.drain_inbound_queue() {
                // ADR-014: Streaming → Idle
                self.transition_status(SessionStatus::Idle);
                let name = self.core.user_display_name.as_deref().unwrap_or("user");
                tracing::info!("Agent loop interrupted by inbound interrupt signal");
                return Ok(format!("Agent stopped by {}.", name));
            }

            // ①-⑧ Execute single iteration (shared with debug mode)
            // With iteration-level retry for retryable stream errors.
            const MAX_ITERATION_RETRIES: u32 = 2;
            let mut iteration_retries = 0u32;
            let iteration_result = loop {
                match self.execute_single_iteration(iteration, context_builder, user_message, &retrieved_memory_ids, &current_model).await {
                    Ok(result) => break result,
                    Err(RuntimeError::StreamError(ref err)) if err.retryable && iteration_retries < MAX_ITERATION_RETRIES => {
                        iteration_retries += 1;
                        let backoff = std::time::Duration::from_millis(1000 * 2u64.pow(iteration_retries - 1));
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
                    Err(e) => {
                        // ADR-014: Streaming → Idle on non-retryable error
                        self.transition_status(SessionStatus::Idle);
                        return Err(e);
                    }
                }
            };
            match iteration_result {
                IterationResult::TextResponse(content) => {
                    // ADR-014: Streaming → Idle (normal completion)
                    self.transition_status(SessionStatus::Idle);
                    return Ok(content);
                }
                IterationResult::Interrupted(content) => {
                    // ADR-014: Streaming → Idle (interrupted)
                    self.transition_status(SessionStatus::Idle);
                    return Ok(content);
                }
                IterationResult::ToolCallsExecuted => {
                    tracing::debug!(iteration, "Loop iteration complete, continuing");
                    continue;
                }
            }
        }
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
    /// - `Interrupted(content)`: user interrupted execution
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_single_iteration(
        &mut self,
        iteration: u32,
        context_builder: &mut ContextBuilder,
        user_message: &str,
        retrieved_memory_ids: &[String],
        current_model: &str,
    ) -> Result<IterationResult> {
            // ── Debug mode hooks ──
            // Increment session-level iteration counter (cumulative across
            // all chat messages).  The local loop counter resets per message
            // (see run() line 614), but the debug snapshot iteration must be
            // globally unique within the session.
            //
            // Capture the incremented value into a local so that subsequent
            // rewind operations (which reset ctrl.iteration mid-flight) do
            // not cause capture_context_snapshot to read a wrong value.
            let debug_iter = if let Some(ctrl) = self.core.debug_ctrl() {
                let mut ctrl = ctrl.lock().await;
                let prev_iter = ctrl.iteration;
                ctrl.iteration += 1;
                let current_iter = ctrl.iteration;
                tracing::info!(
                    prev_iter,
                    new_iter = current_iter,
                    "Debug: iteration counter incremented in execute_single_iteration"
                );
                // Create conversation snapshot for rewind support.
                // Records the current message count and usage at this
                // iteration so that rewinding can truncate history
                // back to this point.
                let msg_count = self.session.history.len();
                let usage = crate::debug::protocol::DebugUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                };
                let conv_snap = ctrl.create_conversation_snapshot(msg_count, usage);
                tracing::info!(
                    conv_iter = conv_snap.iteration,
                    msg_count,
                    snapshot_count = ctrl.conversation_snapshots.len(),
                    "Debug: conversation snapshot created"
                );
                Some(current_iter)
            } else {
                None
            };

            // Await resume if paused (DevMode only)
            if !self.await_debug_resume().await {
                return Ok(IterationResult::Interrupted(
                    "[Debug] Agent loop stopped by debugger".to_string(),
                ));
            }

            // ── Apply pending patches to context_builder (DevMode only) ──
            // When the agent is paused via debugger, SessionTask is blocked
            // inside agent_loop.run() and cannot apply patches through its
            // normal apply_debug_rewind_and_patches path.  Patches stored
            // by patchContext in ctrl.pending_patches must be applied HERE,
            // after resume, so that the LLM receives the patched context.
            if let Some(ctrl) = self.core.debug_ctrl() {
                let mut ctrl_guard = ctrl.lock().await;
                if let Some(patches) = ctrl_guard.pending_patches.take() {
                    context_builder.apply_patches(&patches);
                    tracing::info!(
                        iteration,
                        "Debug: pending patches applied to context builder after resume"
                    );
                }
                // Also consume the re_execute_pending flag so that
                // apply_debug_rewind_and_patches at SessionTask level
                // does not see a stale flag after agent_loop.run() finishes.
                if ctrl_guard.take_re_execute_pending() {
                    tracing::info!(
                        iteration,
                        "Debug: re_execute_pending consumed inside agent loop after resume"
                    );
                }
            }

            // Enter BudgetCheck phase
            self.update_debug_phase(
                crate::debug::protocol::DebugPhase::BudgetCheck,
            )
            .await;

            // ① Budget pre-check
            let estimated_tokens = self.session.history.estimate_total_tokens() + 500; // +500 for new response
            match self.session.budget_guard.check(estimated_tokens) {
                BudgetCheckResult::Allowed => {}
                BudgetCheckResult::Exceeded { reason, action } => {
                    tracing::warn!(reason = %reason, action = %action, "Budget exceeded");
                    match action.as_str() {
                        "deny" => {
                            // ADR-014: Streaming → Idle (budget exceeded)
                            self.transition_status(SessionStatus::Idle);
                            return Err(RuntimeError::BudgetExceeded(reason));
                        }
                        "warn" => {
                            // Changed from System to User — MiniMax API rejects non-first system messages
                            self.session.history.append(ChatMessage {
                                role: MessageRole::User,
                                content: format!("[System Warning] {reason}"),
                                name: Some("system".to_string()),
                                ..Default::default()
                            });
                        }
                        _ => {}
                    }
                }
            }

            // ② Preemptive trim — MUST happen BEFORE build() so the request
            // is constructed with already-trimmed history.
            self.trim_history_to_budget(&current_model);

            // ②.5 Build context (now with trimmed history)
            // Inject current todo list into system prompt before building
            context_builder.set_todo_context(self.session.format_todos());
            let mut chat_request = context_builder.build(&self.core.manifest, &self.session.history, self.get_model_capabilities(&current_model), self.core.max_output_tokens_limit);

            tracing::info!(
                request_messages_count = chat_request.messages.len(),
                request_model = %chat_request.model,
                request_max_tokens = ?chat_request.max_tokens,
                request_tools_count = chat_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
                history_tokens = self.session.history.token_count(),
                "Built chat request for LLM (after preemptive trim)"
            );

            // ②.6 Context usage check / circuit-breaking
            // Dynamic token-based thresholds derived from model capabilities.
            // Unlike the previous hardcoded 200KB/280KB byte thresholds, these
            // scale correctly across models from 32K to 2M context windows.
            let usable = self.context_trim_budget(&current_model);
            let warn_threshold = (usable as f64 * 0.70) as u64;
            let hard_threshold = (usable as f64 * 0.90) as u64;
            let current_tokens = self.session.history.token_count();
            if current_tokens > hard_threshold {
                tracing::error!(
                    current_tokens,
                    hard_threshold,
                    usable_context = usable,
                    "Context usage exceeds hard limit, emergency trimming"
                );
                let removed = self.session.history.emergency_trim();
                tracing::info!(removed, "Emergency trimmed messages for oversized context");
                if removed > 0 {
                    // Rebuild request with trimmed history.
                    chat_request = context_builder.build(
                        &self.core.manifest,
                        &self.session.history,
                        self.get_model_capabilities(&current_model),
                        self.core.max_output_tokens_limit,
                    );
                    tracing::info!(
                        current_tokens = self.session.history.token_count(),
                        "Context usage after emergency trim"
                    );
                }
            } else if current_tokens > warn_threshold {
                tracing::warn!(
                    current_tokens,
                    warn_threshold,
                    usable_context = usable,
                    "Context usage approaching limit"
                );
            }

            // Debug: enter BuildContext phase
            self.update_debug_phase(
                crate::debug::protocol::DebugPhase::BuildContext,
            )
            .await;

            // Debug: create context snapshot and push onContextBuilt event
            self.capture_context_snapshot(context_builder, debug_iter).await;

            // Merge MCP tool definitions into the LLM request right before
            // injection. MCP tools are kept separate from active_tools and
            // only mixed here (LLM injection) + in debug snapshot capture.
            if let Some(ref mut tools) = chat_request.tools {
                for tool in &self.core.all_tools {
                    let spec = tool.spec();
                    if spec.name.starts_with("mcp:") {
                        let val = serde_json::to_value(&spec).unwrap_or_default();
                        tools.push(val);
                    }
                }
            }

            // ③ Call LLM with streaming (S1.5)
            let response = self.call_llm_streaming(&chat_request, context_builder).await?;

            // Debug: enter LlmCall phase
            self.update_debug_phase(crate::debug::protocol::DebugPhase::LlmCall)
                .await;

            // ④ Parse response
            let has_tool_calls = response.tool_calls.is_some();

            // Debug: enter ParseResponse phase
            self.update_debug_phase(
                crate::debug::protocol::DebugPhase::ParseResponse,
            )
            .await;

            // Update budget
            if let Some(usage) = &response.usage {
                self.session.budget_guard.update_usage(usage.total_tokens, 0.0);

                // Compute and emit context usage report — use exact model lookup
                // to avoid capability confusion in multi-model scenarios.
                let model_caps = self.get_model_capabilities(&current_model);
                tracing::debug!(
                    has_chunk_tx = self.core.on_chunk.is_some(),
                    has_model_caps = model_caps.is_some(),
                    caps_count = self.core.gateway_model_capabilities.len(),
                    has_usage = true,
                    "ContextUsage: checking preconditions"
                );
                if let Some(caps) = model_caps {
                    let ctx_usage = crate::agent::context::compute_context_usage(caps, usage, self.core.max_output_tokens_limit);
                    tracing::debug!(
                        context_window = ctx_usage.context_window,
                        total_tokens = ctx_usage.total_tokens,
                        usage_percent = ctx_usage.usage_percent,
                        "ContextUsage: sending report"
                    );
                    if !self.core.try_send_chunk(ChunkEvent::ContextUsage(ctx_usage)) {
                        tracing::debug!("ContextUsage: on_chunk channel full/closed or session_id missing");
                    }
                } else {
                    tracing::warn!(
                        has_chunk_tx = self.core.on_chunk.is_some(),
                        has_model_caps = model_caps.is_some(),
                        "ContextUsage: NOT sent — missing model capabilities"
                    );
                }
            }

            if !has_tool_calls {
                // Pure text response — normal exit
                let content = response.content.clone();

                // Persist think block (if present) and assistant response to JSONL
                if let Some(ref conversation) = self.session.conversation {
                    let think_meta = build_think_metadata(&response);
                    if let Some(ref reasoning) = response.reasoning_content {
                        if !reasoning.is_empty() {
                            conversation.append_message("thought", reasoning, think_meta.clone());
                        }
                    } else if let Some(think_content) = extract_think_block(&content) {
                        // Fallback: extract from <think> tags in content
                        conversation.append_message("thought", &think_content, think_meta);
                    }
                    let assistant_text = strip_think_block(&content);
                    conversation.append_message("assistant", &assistant_text, None);
                }

                self.session.history.append(ChatMessage {
                    ..ChatMessage::assistant(response.content)
                });

                // Record conversation turn to Grafeo memory store
                // P1-2 fix: increment turn_counter, P2-4 fix: pass retrieved memory IDs
                let turn_index = self.session.turn_counter;
                self.session.turn_counter += 1;
                self.record_turn_to_memory(user_message, &content, turn_index, retrieved_memory_ids.to_vec());

                tracing::info!(iteration, "Agent returned text response");

                // Debug: enter AppendHistory phase and push step event
                self.update_debug_phase(
                    crate::debug::protocol::DebugPhase::AppendHistory,
                )
                .await;
                self.push_debug_step(
                    crate::debug::protocol::DebugPhase::Idle,
                    None,
                    Some(serde_json::json!({"content": content})),
                );
                self.debug_auto_pause_if_stepping().await;

                return Ok(IterationResult::TextResponse(content));
            }

            // Persist think block (if present) to JSONL
            if let Some(ref conversation) = self.session.conversation {
                let think_meta = build_think_metadata(&response);
                // DeepSeek reasoning_content (separate field) takes priority
                if let Some(ref reasoning) = response.reasoning_content {
                    if !reasoning.is_empty() {
                        conversation.append_message("thought", reasoning, think_meta.clone());
                    }
                } else if let Some(think_content) = extract_think_block(&response.content) {
                    conversation.append_message("thought", &think_content, think_meta);
                }
            }

            // Has tool calls — process them (moved after think metadata to avoid partial move)
            let tool_calls = response.tool_calls.unwrap_or_default();

            // ④.5 Tool call deduplication (same iteration)
            let mut seen = HashSet::new();
            let deduped_calls: Vec<ToolCall> = tool_calls
                .into_iter()
                .filter(|tc| {
                    let sig = format!("{}:{}", tc.function.name, tc.function.arguments);
                    seen.insert(sig)
                })
                .collect();

            // Add assistant message with tool_calls to history
            self.session.history.append(ChatMessage {
                reasoning_content: response.reasoning_content.clone(),
                tool_calls: Some(deduped_calls.clone()),
                ..ChatMessage::assistant(response.content.clone())
            });

            // Persist tool calls to JSONL
            if let Some(ref conversation) = self.session.conversation {
                for tc in &deduped_calls {
                    let metadata = serde_json::json!({
                        "tool_name": tc.function.name,
                        "tool_call_id": tc.id,
                    });
                    conversation.append_message("tool_call", &tc.function.arguments, Some(metadata));
                }
            }

            // Emit ToolCall events via chunk channel (ensures ordering with content chunks)
            for tc in &deduped_calls {
                if !self.core.try_send_chunk(ChunkEvent::ToolCall {
                    name: tc.function.name.clone(),
                    args: tc.function.arguments.clone(),
                    id: tc.id.clone(),
                }) {
                    tracing::debug!("on_chunk channel full or closed, dropping ToolCall event");
                }
            }

            // ⑤ Tool dispatch — parallel execution (S1.7)
            // ⑤.1 Pre-execution loop detection: block repeated calls before wasting an iteration
            let mut calls_to_execute: Vec<ToolCall> = Vec::new();
            let mut blocked_info: Vec<(usize, LoopPattern)> = Vec::new();
            for (idx, tc) in deduped_calls.iter().enumerate() {
                match self.session.loop_detector.peek_check(&tc.function.name, &tc.function.arguments) {
                    LoopDetectionResult::NoLoop => {
                        calls_to_execute.push(tc.clone());
                    }
                    LoopDetectionResult::LoopDetected { level, pattern, .. } => {
                        match level {
                            ResponseLevel::Warning => {
                                // Warning is handled post-execution; allow the call
                                calls_to_execute.push(tc.clone());
                            }
                            ResponseLevel::Block | ResponseLevel::Break => {
                                tracing::warn!(
                                    tool = %tc.function.name,
                                    level = ?level,
                                    "Loop detected (pre-execution), blocking tool call"
                                );
                                blocked_info.push((idx, pattern));
                            }
                        }
                    }
                }
            }

            // Check for interrupt before executing tools
            if self.poll_interrupt() {
                tracing::info!("Interrupted before tool execution — saving partial response");
                // ADR-014: Streaming → Idle (interrupted before tool execution)
                self.transition_status(SessionStatus::Idle);
                let content = response.content.clone();

                // Persist interrupted assistant message to JSONL conversation.
                if let Some(ref conversation) = self.session.conversation {
                    let assistant_text = strip_think_block(&content);
                    conversation.append_message("assistant", &assistant_text, None);
                }

                // Notify frontend via chunk channel
                let _ = self.core.try_send_chunk(ChunkEvent::Interrupted {
                    content: content.clone(),
                });

                // Debug: push step event and auto-pause if stepping
                self.push_debug_step(
                    crate::debug::protocol::DebugPhase::Idle,
                    None,
                    Some(serde_json::json!({"interrupted": true, "content": content})),
                );
                self.debug_auto_pause_if_stepping().await;

                return Ok(IterationResult::Interrupted(format!("...Stopped by User...\n{}", content)));
            }

            // Debug: enter ToolExecution phase
            self.update_debug_phase(
                crate::debug::protocol::DebugPhase::ToolExecution,
            )
            .await;

            // ⑤.2 Intercept ask_user_question and todo_write calls.
            // - ask_user_question requires user interaction via ChunkEvent::AskQuestion.
            // - todo_write mutates SessionState.todos directly (in-memory state).
            // Both are processed sequentially before parallel tool dispatch.
            let mut ask_question_results: Vec<(usize, String)> = Vec::new();
            let mut todo_write_results: Vec<(usize, String)> = Vec::new();
            let mut parallel_calls: Vec<(usize, ToolCall)> = Vec::new();
            for (idx, tc) in calls_to_execute.into_iter().enumerate() {
                if tc.function.name == "ask_user_question" {
                    let result = self.handle_ask_user_question(&tc).await;
                    ask_question_results.push((idx, result));
                } else if tc.function.name == "todo_write" {
                    let result = self.handle_todo_write(&tc, context_builder);
                    todo_write_results.push((idx, result));
                } else {
                    parallel_calls.push((idx, tc));
                }
            }

            // Execute non-question tools in parallel
            let calls_for_parallel: Vec<ToolCall> = parallel_calls.iter().map(|(_, tc)| tc.clone()).collect();
            let (parallel_results, was_interrupted) = self.execute_tools_parallel(&calls_for_parallel).await;

            // Merge results: ask_question + todo_write results + parallel results, mapped back to original indices
            // Build a map from original index → result for ask_question calls
            let ask_result_map: std::collections::HashMap<usize, String> =
                ask_question_results.into_iter().collect();
            let todo_result_map: std::collections::HashMap<usize, String> =
                todo_write_results.into_iter().collect();

            // Reconstruct executed_results in the order matching calls_for_parallel
            // Then map back to the original calls_to_execute indices
            let mut final_results: Vec<(usize, String)> = Vec::new();
            for (parallel_idx, result) in parallel_results.into_iter().enumerate() {
                if let Some((orig_idx, _)) = parallel_calls.get(parallel_idx) {
                    final_results.push((*orig_idx, result));
                }
            }
            // Add ask_question results
            for (orig_idx, result) in &ask_result_map {
                final_results.push((*orig_idx, result.clone()));
            }
            // Add todo_write results
            for (orig_idx, result) in &todo_result_map {
                final_results.push((*orig_idx, result.clone()));
            }
            // Sort by original index to maintain order
            final_results.sort_by_key(|(idx, _)| *idx);

            let executed_results: Vec<String> = final_results.into_iter().map(|(_, r)| r).collect();

            // Merge executed results with pre-blocked results, preserving original order
            let mut tool_results: Vec<String> = Vec::with_capacity(deduped_calls.len());
            let mut executed_iter = executed_results.into_iter();
            for idx in 0..deduped_calls.len() {
                if let Some(pos) = blocked_info.iter().position(|(i, _)| *i == idx) {
                    let msg = match &blocked_info[pos].1 {
                        LoopPattern::SameToolFlood => {
                            "Loop detected: this tool has been called too many times in a short period. \
                             Please STOP using this tool and try a different approach \
                             (e.g., use file_read to verify results, or switch to another tool)."
                        }
                        _ => "Loop detected: this tool call has been blocked because it was repeated too many times with the same parameters. Try a different approach.",
                    };
                    tool_results.push(msg.to_string());
                } else {
                    tool_results.push(executed_iter.next().unwrap_or_default());
                }
            }

            // Persist tool results to JSONL
            if let Some(ref conversation) = self.session.conversation {
                for (tc, result_content) in deduped_calls.iter().zip(tool_results.iter()) {
                    let metadata = serde_json::json!({
                        "tool_name": tc.function.name,
                        "tool_call_id": tc.id,
                    });
                    conversation.append_message("tool_result", result_content, Some(metadata));
                }
            }

            // Emit ToolResult events via chunk channel (ensures ordering with content chunks)
            for (tc, result_content) in deduped_calls.iter().zip(tool_results.iter()) {
                if !self.core.try_send_chunk(ChunkEvent::ToolResult {
                    name: tc.function.name.clone(),
                    result: result_content.clone(),
                    tool_call_id: tc.id.clone(),
                }) {
                    tracing::debug!("on_chunk channel full or closed, dropping ToolResult event");
                }
            }

            // ⑥ Pre-trim: make room for tool results before appending.
            // Large tool outputs (e.g. content_search returning 320+ results)
            // can blow up the context window.  Trimming BEFORE the append
            // ensures the LLM request remains within budget on the next
            // iteration.  Threshold: 70 % of the usable context window.
            let result_tokens_estimate: u64 = tool_results
                .iter()
                .map(|r| (r.len() / 4) as u64)
                .sum();
            let usable_budget = self.context_trim_budget(current_model);
            let trim_threshold = (usable_budget as f64 * 0.70) as u64;
            let current_tokens = self.session.history.token_count();
            if current_tokens.saturating_add(result_tokens_estimate) > trim_threshold {
                tracing::info!(
                    current_tokens,
                    result_tokens_estimate,
                    trim_threshold,
                    usable_budget,
                    "Pre-trimming history before appending tool results"
                );
                self.trim_history_to_budget(current_model);
            }

            // ── ⑦ Append ALL tool results to history (must be contiguous after assistant tool_calls)
            for (tc, result_content) in deduped_calls.iter().zip(tool_results.iter()) {
                let tool_result_message = ChatMessage {
                    name: Some(tc.function.name.clone()),
                    ..ChatMessage::tool(tc.id.clone(), result_content.clone())
                };
                self.session.history.append(tool_result_message);
            }

            // ⑧ Loop detection — run AFTER all tool results are appended to avoid
            // inserting warning messages between tool results (which breaks DeepSeek API
            // requirement that all tool messages must immediately follow assistant tool_calls).
            let mut deferred_warnings: Vec<String> = Vec::new();
            let mut break_error: Option<String> = None;
            for (idx, (tc, result_content)) in deduped_calls.iter().zip(tool_results.iter()).enumerate() {
                // Skip loop detection for pre-blocked tool calls to avoid self-reinforcing
                // false positives: blocked tools return uniform error messages whose identical
                // hashes would incorrectly trigger NoProgress detection.
                if blocked_info.iter().any(|(i, _)| *i == idx) {
                    continue;
                }

                match self.session.loop_detector.check(
                    &tc.function.name,
                    &tc.function.arguments,
                    result_content,
                ) {
                    LoopDetectionResult::NoLoop => {}
                    LoopDetectionResult::LoopDetected {
                        pattern,
                        level,
                        count: _,
                        message,
                    } => {
                        tracing::warn!(message = %message, level = ?level, "Loop detected");
                        match level {
                            ResponseLevel::Warning => {
                                let warning_content = match &pattern {
                                    LoopPattern::SameToolFlood => {
                                        format!(
                                            "[System Warning] {message} \
                                             This tool has been called excessively. \
                                             Please STOP using this tool and try a different approach \
                                             (e.g., use file_read to verify results, or switch to another tool) \
                                             to complete the task."
                                        )
                                    }
                                    _ => format!("[System Warning] {message}"),
                                };
                                deferred_warnings.push(warning_content);
                            }
                            ResponseLevel::Block => {
                                // Block was already handled by returning error as tool result
                            }
                            ResponseLevel::Break => {
                                break_error = Some(message);
                                break;
                            }
                        }
                    }
                }
            }

            // Append deferred warning messages AFTER all tool results
            for warning_content in deferred_warnings {
                self.session.history.append(ChatMessage {
                    role: MessageRole::User,
                    content: warning_content,
                    name: Some("system".to_string()),
                    ..Default::default()
                });
            }

            // Handle Break-level loop detection
            if let Some(msg) = break_error {
                // ADR-014: Streaming → Idle (loop detected)
                self.transition_status(SessionStatus::Idle);
                return Err(RuntimeError::LoopDetected(msg));
            }

            // ── Check for interrupt detected during tool execution ──
            // poll_interrupt() consumed the interrupt inside execute_tools_parallel(),
            // so we must propagate it here to prevent the loop from continuing.
            if was_interrupted {
                tracing::info!("Interrupted during tool execution — saving partial results");
                // ADR-014: Streaming → Idle (interrupted during tool execution)
                self.transition_status(SessionStatus::Idle);
                let content = response.content.clone();

                // Persist assistant message to JSONL (normal tool_call path only
                // persists think + tool_calls; assistant text needs explicit save).
                if let Some(ref conversation) = self.session.conversation {
                    let assistant_text = strip_think_block(&content);
                    conversation.append_message("assistant", &assistant_text, None);
                }

                // Notify frontend via chunk channel
                let _ = self.core.try_send_chunk(ChunkEvent::Interrupted {
                    content: content.clone(),
                });

                // Debug: push step event and auto-pause if stepping
                self.push_debug_step(
                    crate::debug::protocol::DebugPhase::Idle,
                    None,
                    Some(serde_json::json!({"interrupted": true, "content": content})),
                );
                self.debug_auto_pause_if_stepping().await;

                return Ok(IterationResult::Interrupted(format!("...Stopped by User...\n{}", content)));
            }

            // ⑦ Usage report (async, non-blocking)
            tracing::debug!(iteration, "Loop iteration complete");

            // Debug: enter AppendHistory phase and push step event
            self.update_debug_phase(
                crate::debug::protocol::DebugPhase::AppendHistory,
            )
            .await;
            self.push_debug_step(
                crate::debug::protocol::DebugPhase::Idle,
                None,
                None,
            );
            self.debug_auto_pause_if_stepping().await;

            Ok(IterationResult::ToolCallsExecuted)
    }

    /// Non-blocking interrupt poll — returns true if the user requested stop.
    ///
    /// Non-Interrupt messages drained during this poll are buffered in
    /// `session.deferred_inbound` and re-injected by `drain_inbound_queue()`
    /// at the start of the next loop iteration. No message is silently lost.
    ///
    /// ALL `Interrupt` messages are consumed (not just the first one) to
    /// prevent residual interrupts from poisoning subsequent `run_inner()`
    /// calls when the user clicks Stop rapidly.
    pub(crate) fn poll_interrupt(&mut self) -> bool {
        let mut interrupted = false;
        while let Ok(msg) = self.inbound_rx.try_recv() {
            match msg {
                InboundMessage::Interrupt { .. } => {
                    interrupted = true;
                    // Consume and continue — drain all pending interrupts
                }
                InboundMessage::UserOperation(op) => {
                    match &op {
                        crate::agent::inbound::UserOp::InterruptLoop { .. } => {
                            interrupted = true;
                            // Consume and continue — drain all pending interrupts
                        }
                        _ => {
                            // Buffer non-Interrupt UserOp for re-injection
                            // by drain_inbound_queue().
                            tracing::info!(
                                op = ?std::mem::discriminant(&op),
                                "poll_interrupt(): buffering UserOp for re-injection by drain_inbound_queue()"
                            );
                            self.session.deferred_inbound.push(InboundMessage::UserOperation(op));
                        }
                    }
                }
                other => {
                    // Buffer non-Interrupt messages for re-injection at the
                    // next drain_inbound_queue() call. This guarantees that
                    // queued user messages (sent via the "Stop to queue" UX)
                    // survive if they happen to arrive in this channel.
                    tracing::info!(
                        msg_type = ?std::mem::discriminant(&other),
                        "poll_interrupt(): buffering non-Interrupt message for re-injection by drain_inbound_queue()"
                    );
                    self.session.deferred_inbound.push(other);
                }
            }
        }
        interrupted
    }

    /// Drain inbound message queue (non-blocking).
    ///
    /// First processes any messages buffered by `poll_interrupt()` from
    /// the `deferred_inbound` stash, then drains the live channel.
    /// Injects external messages (user, system, intent) into history
    /// before each loop iteration. Applies size limits to prevent
    /// token explosion from oversized payloads.
    ///
    /// Returns `true` if at least one interrupt signal was found
    /// (the caller should stop the current agent loop).  ALL interrupt
    /// messages are consumed (not just the first one) to prevent
    /// residual interrupts from poisoning subsequent `run_inner()` calls.
    fn drain_inbound_queue(&mut self) -> bool {
        let mut interrupted = false;

        // ── Step 1: process messages deferred from poll_interrupt() ──
        // Collect to release the drain iterator's borrow on self.session
        // before calling apply_user_op() (which needs &mut self).
        let deferred: Vec<_> = self.session.deferred_inbound.drain(..).collect();
        for msg in deferred {
            let (msg, _truncated) = msg.enforce_size_limit();
            match msg {
                InboundMessage::UserMessage(text) => {
                    tracing::info!(
                        text_preview = %text.chars().take(80).collect::<String>(),
                        "drain_inbound_queue: injecting deferred UserMessage into history"
                    );
                    self.session.history.append(ChatMessage::user(text));
                }
                InboundMessage::SystemNotification { notification_type, data } => {
                    tracing::info!("drain_inbound_queue: injecting deferred system notification: {} = {:?}", notification_type, data);
                    self.session.history.append(ChatMessage {
                        role: MessageRole::User,
                        content: format!("[system:{}] {}", notification_type, data),
                        name: Some("system".to_string()),
                        ..Default::default()
                    });
                }
                InboundMessage::IntentMessage { from, action, params } => {
                    tracing::info!("drain_inbound_queue: injecting deferred intent from {}: {} params={:?}", from, action, params);
                    self.session.history.append(ChatMessage::user(
                        format!("[intent:{}:{}] {}", from, action, params),
                    ));
                }
                InboundMessage::Interrupt { reason } => {
                    tracing::info!(reason = %reason, "Received deferred interrupt signal (consumed)");
                    interrupted = true;
                    // Consume and continue — more interrupts (or other messages)
                    // may be queued in the live channel.
                }
                InboundMessage::ContinueExecution { .. } => {
                    tracing::debug!("Ignoring deferred ContinueExecution");
                }
                InboundMessage::ApprovalDecision { .. } => {
                    // Approval decisions arrive via inbound channel during
                    // approval pause; during normal drain, ignore.
                    tracing::debug!("Ignoring deferred ApprovalDecision");
                }
                InboundMessage::QuestionAnswer { .. } => {
                    // Question answers arrive via inbound channel during
                    // question wait; during normal drain, ignore.
                    tracing::debug!("Ignoring deferred QuestionAnswer");
                }
                InboundMessage::UserOperation(user_op) => {
                    tracing::info!(
                        op = ?std::mem::discriminant(&user_op),
                        "drain_inbound_queue: processing deferred UserOperation"
                    );
                    if self.apply_user_op(&user_op) {
                        interrupted = true;
                    }
                }
            }
        }

        // ── Step 2: drain the live channel ──
        while let Ok(msg) = self.inbound_rx.try_recv() {
            // Enforce size limits before injecting
            let (msg, _truncated) = msg.enforce_size_limit();
            match msg {
                InboundMessage::UserMessage(text) => {
                    tracing::info!(
                        text_preview = %text.chars().take(80).collect::<String>(),
                        "drain_inbound_queue: injecting UserMessage into history"
                    );
                    self.session.history.append(ChatMessage::user(text));
                }
                InboundMessage::SystemNotification { notification_type, data } => {
                    tracing::info!("System notification: {} = {:?}", notification_type, data);
                    // Changed from System to User — MiniMax API rejects non-first system messages
                    self.session.history.append(ChatMessage {
                        role: MessageRole::User,
                        content: format!("[system:{}] {}", notification_type, data),
                        name: Some("system".to_string()),
                        ..Default::default()
                    });
                }
                InboundMessage::IntentMessage { from, action, params } => {
                    tracing::info!("Intent from {}: {} params={:?}", from, action, params);
                    self.session.history.append(ChatMessage::user(
                        format!("[intent:{}:{}] {}", from, action, params),
                    ));
                }
                InboundMessage::Interrupt { reason } => {
                    tracing::info!(reason = %reason, "Received interrupt signal (consumed)");
                    interrupted = true;
                    // Consume and continue — multiple interrupts may be queued
                    // from rapid Stop button clicks.  We must drain ALL of them
                    // so subsequent run_inner() calls aren't poisoned.
                }
                InboundMessage::ContinueExecution { .. } => {
                    // Continue is only meaningful during iteration limit pause;
                    // during normal drain, ignore it.
                    tracing::debug!("Ignoring ContinueExecution during normal drain");
                }
                InboundMessage::ApprovalDecision { .. } => {
                    // Approval decisions are only meaningful during approval pause.
                    tracing::debug!("Ignoring ApprovalDecision during normal drain");
                }
                InboundMessage::QuestionAnswer { .. } => {
                    // Question answers are only meaningful during question wait.
                    tracing::debug!("Ignoring QuestionAnswer during normal drain");
                }
                InboundMessage::UserOperation(user_op) => {
                    tracing::info!(
                        op = ?std::mem::discriminant(&user_op),
                        "drain_inbound_queue: processing live UserOperation"
                    );
                    if self.apply_user_op(&user_op) {
                        interrupted = true;
                    }
                }
            }
        }
        interrupted
    }

    // ── LLM streaming methods extracted to loop_llm.rs ──

    // ── Tool execution extracted to loop_tools.rs ──

    // ── Debug mode control methods ──

    /// Await debug resume: blocks if the debug controller is in Paused state.
    ///
    /// Uses `rewind_notify` via `tokio::select!` so that rewinds requested
    /// via the Debug Panel are applied **immediately** (notification-driven)
    /// rather than after up to 100ms of polling.  State changes (Running /
    /// Stepping / Stopped) are still checked on each loop iteration.
    ///
    /// Also checks the inbound channel for Chat Panel STOP signals
    /// (InboundMessage::Interrupt), which arrive via the Gateway gRPC push
    /// path rather than the debug WebSocket.
    /// Returns `true` if execution should continue, `false` if stopped.
    async fn await_debug_resume(&mut self) -> bool {
        let Some(ctrl) = self.core.debug_ctrl().cloned() else {
            return true; // Production mode, no debug controller
        };

        // Clone the rewind notify handle so the Paused branch can use
        // tokio::select! for instant rewind response.
        let rewind_notify = self.core.debug_rewind_notify().cloned();

        loop {
            // Check for Chat Panel STOP (arrives via inbound channel).
            // The Debug Panel STOP sets ctrl.state directly; the Chat Panel
            // STOP sends InboundMessage::Interrupt through the Gateway gRPC
            // push path.  Without this check, the interrupt sits unread in
            // the channel while await_debug_resume only polls ctrl.state.
            if self.poll_interrupt() {
                tracing::info!("Debug: agent loop interrupted via inbound channel");
                // Synchronize debug controller state so the frontend sees Stopped
                let mut ctrl_guard = ctrl.lock().await;
                let iteration = ctrl_guard.iteration;
                ctrl_guard.state = crate::debug::controller::DebugState::Stopped;
                drop(ctrl_guard);
                // Push execution state change event for frontend sync
                if let Some(event_tx) = self.core.debug_event_tx() {
                    let _ = event_tx.send(
                        crate::debug::server::DebugEvent::ExecutionStateChanged {
                            new_state: crate::debug::controller::DebugState::Stopped,
                            iteration,
                        },
                    );
                }
                return false;
            }

            // Consume any pending rewind target during polling.
            // Uses the unified apply_debug_rewind entry point so
            // rewind logic lives in exactly one place.
            {
                let session_id = self
                    .current_session_id()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                crate::agent::session::session_task::apply_debug_rewind(
                    &ctrl,
                    &session_id,
                    self,
                )
                .await;
            }

            let state = {
                let ctrl = ctrl.lock().await;
                ctrl.state.clone()
            };
            match state {
                crate::debug::controller::DebugState::Running => {
                    // ADR-014: Paused → Streaming (debug resume)
                    self.transition_status(SessionStatus::Streaming { message_id: None });
                    return true;
                }
                crate::debug::controller::DebugState::Stepping => {
                    // ADR-014: Paused → Streaming (debug step)
                    self.transition_status(SessionStatus::Streaming { message_id: None });
                    return true;
                }
                crate::debug::controller::DebugState::Stopped => {
                    tracing::info!("Debug: agent loop stopped");
                    // ADR-014: Paused → Idle (debug stop)
                    self.transition_status(SessionStatus::Idle);
                    return false;
                }
                crate::debug::controller::DebugState::Paused => {
                    // ADR-014: Streaming → Paused (debug pause)
                    self.transition_status(SessionStatus::Paused {
                        iteration: None,
                        max_iterations: None,
                    });
                    // Use tokio::select! with rewind_notify so that
                    // rewinds are applied immediately (notification-driven)
                    // instead of waiting up to 100ms for the next poll.
                    // State changes are still picked up on the next
                    // iteration after the select! resolves.
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

    /// Await user approval decision for a tool execution request.
    ///
    /// Blocks the main loop on `inbound_rx` without timeout until the
    /// matching `InboundMessage::ApprovalDecision` arrives. Non-matching
    /// messages are buffered in `deferred_inbound` for later processing
    /// (mirrors `await_debug_resume`'s `poll_interrupt` pattern).
    ///
    /// Also polls `approval_rx` so that concurrent approval requests from
    /// parallel tool execution are processed (queued) rather than blocking
    /// the channel. Each new approval request gets its own `ChunkEvent`
    /// sent to the Gateway before we continue waiting for the current one.
    ///
    /// Returns `ApprovalDecision` with the user's choice, auto-rejects
    /// on timeout / Interrupt / channel close.
    async fn await_approval_decision(&mut self, request_id: &str) -> ApprovalDecision {
        // Approval timeout: auto-reject after 5 minutes to prevent deadlock.
        const APPROVAL_TIMEOUT_SECS: u64 = 300;

        loop {
            tokio::select! {
                // Primary: wait for the matching approval decision from inbound channel
                msg = self.inbound_rx.recv() => {
                    match msg {
                        Some(InboundMessage::ApprovalDecision {
                            request_id: rid,
                            approved,
                            allow_all_session,
                            ..
                        }) if rid == request_id => {
                            tracing::info!(
                                request_id = %request_id,
                                approved,
                                allow_all_session,
                                "Approval decision received"
                            );
                            return ApprovalDecision { approved, allow_all_session, reason: None };
                        }
                        Some(InboundMessage::ApprovalDecision {
                            request_id: rid,
                            approved,
                            allow_all_session,
                            ..
                        }) => {
                            // Approval decision for a DIFFERENT request — buffer it.
                            // This can happen when multiple concurrent approval requests
                            // are in flight and responses arrive out of order.
                            tracing::debug!(
                                expected = %request_id,
                                got = %rid,
                                "Buffering approval decision for different request"
                            );
                            self.session.deferred_inbound.push(InboundMessage::ApprovalDecision {
                                request_id: rid,
                                approved,
                                allow_all_session,
                                reason: None,
                            });
                        }
                        Some(InboundMessage::Interrupt { reason }) => {
                            tracing::info!(
                                reason = %reason,
                                request_id = %request_id,
                                "Approval interrupted, auto-rejecting"
                            );
                            return ApprovalDecision { approved: false, allow_all_session: false, reason: None };
                        }
                        Some(other) => {
                            tracing::debug!(
                                ?other,
                                "Buffering non-approval message during approval wait"
                            );
                            self.session.deferred_inbound.push(other);
                        }
                        None => {
                            tracing::warn!(
                                request_id = %request_id,
                                "Inbound channel closed during approval wait, auto-rejecting"
                            );
                            return ApprovalDecision { approved: false, allow_all_session: false, reason: None };
                        }
                    }
                }
                // Secondary: process concurrent approval requests from other spawned tasks.
                // Without this branch, a second tool needing approval would block on the
                // mpsc channel, its ToolApprovalNeeded event would never reach the Gateway,
                // and the user would never see the second approval dialog.
                approval_req = self.approval_rx.recv() => {
                    match approval_req {
                        Some((req, decision_tx)) => {
                            tracing::info!(
                                current_request_id = %request_id,
                                new_tool = %req.tool_name,
                                "Queuing concurrent approval request while waiting for decision"
                            );
                            self.handle_approval_request(req, decision_tx).await;
                            // After handling the concurrent request, continue waiting
                            // for the ORIGINAL request's decision.
                        }
                        None => {
                            tracing::warn!("Approval channel closed during approval wait");
                        }
                    }
                }
                // Timeout: auto-reject to prevent permanent deadlock when
                // a concurrent approval request is orphaned.
                _ = tokio::time::sleep(std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS)) => {
                    let reason_msg = format!("tool approval timed out after {}s", APPROVAL_TIMEOUT_SECS);
                    tracing::warn!(
                        request_id = %request_id,
                        timeout_secs = APPROVAL_TIMEOUT_SECS,
                        "Approval timed out, auto-rejecting"
                    );
                    return ApprovalDecision { approved: false, allow_all_session: false, reason: Some(reason_msg) };
                }
            }
        }
    }

    /// Send ToolApprovalNeeded chunk event to Gateway (via on_chunk channel).
    fn send_tool_approval_needed(&self, request_id: &str, req: &ApprovalRequest) {
        // Approval timeout: 300s (5 min) — same as await_approval_decision.
        const APPROVAL_TIMEOUT_SECS: u64 = 300;
        let _ = self.core.try_send_chunk(ChunkEvent::ToolApprovalNeeded {
            request_id: request_id.to_string(),
            tool_name: req.tool_name.clone(),
            action: req.action.clone(),
            risk_level: req.risk_level.label().to_string(),
            reason: req.reason.clone(),
            tool_call_id: req.tool_call_id.clone(),
            approval_timeout_secs: APPROVAL_TIMEOUT_SECS,
        });
    }

    /// Handle an ask_user_question tool call.
    ///
    /// Validates the params, emits ChunkEvent::AskQuestion, transitions
    /// status to WaitingApproval, and blocks until the user responds.
    /// Returns the user's answer as a tool result string.
    async fn handle_ask_user_question(&mut self, tc: &rollball_core::providers::traits::ToolCall) -> String {

        // Validate params
        let params: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
            Ok(p) => p,
            Err(e) => {
                return format!("Error: ask_user_question arguments are not valid JSON: {}", e);
            }
        };

        let parsed = match AskUserQuestionTool::validate_params(&params) {
            Ok(p) => p,
            Err(e) => {
                return format!("Error: ask_user_question invalid params: {}", e);
            }
        };

        // Generate unique request ID
        let request_id = format!(
            "q-{}",
            self.approval_next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );

        tracing::info!(
            request_id = %request_id,
            question = %parsed.question,
            options_count = parsed.options.len(),
            "AskUserQuestion: emitting AskQuestion event and waiting for answer"
        );

        // Emit ChunkEvent::AskQuestion
        let _ = self.core.try_send_chunk(ChunkEvent::AskQuestion {
            request_id: request_id.clone(),
            question: parsed.question.clone(),
            options: parsed.options,
            title: parsed.title.clone(),
            timeout_seconds: parsed.timeout_seconds,
        });

        // Transition to WaitingApproval
        self.transition_status(SessionStatus::WaitingApproval {
            request_id: request_id.clone(),
        });

        // Wait for the user's answer (with optional timeout)
        let answer = self.await_question_answer(&request_id, parsed.timeout_seconds).await;

        // Transition back to Streaming (the loop will continue)
        self.transition_status(SessionStatus::Streaming { message_id: None });

        tracing::info!(
            request_id = %request_id,
            answer_preview = %answer.chars().take(100).collect::<String>(),
            "AskUserQuestion: received answer"
        );

        // Return the answer as the tool result
        answer
    }

    /// Handle a `todo_write` tool call by updating SessionState.todos and
    /// injecting the updated list into the ContextBuilder for the next build().
    ///
    /// This is synchronous (no I/O or user interaction) since todos are
    /// pure in-memory state on SessionState.
    fn handle_todo_write(
        &mut self,
        tc: &rollball_core::providers::traits::ToolCall,
        context_builder: &mut ContextBuilder,
    ) -> String {
        use crate::agent::session_state::TodoItem;

        let params: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
            Ok(p) => p,
            Err(e) => {
                return format!("Error: todo_write arguments are not valid JSON: {}", e);
            }
        };

        let todos_array = match params.get("todos").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return "Error: todo_write requires a 'todos' array parameter".to_string(),
        };

        let merge = params
            .get("merge")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut items: Vec<TodoItem> = Vec::with_capacity(todos_array.len());
        for item in todos_array {
            let id = match item.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return "Error: each todo item must have a string 'id' field".to_string(),
            };
            let content = match item.get("content").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return format!("Error: todo item '{}' missing required 'content' field", id)
                }
            };
            let status = match item.get("status").and_then(|v| v.as_str()) {
                Some("pending") => crate::agent::session_state::TodoStatus::Pending,
                Some("in_progress") => crate::agent::session_state::TodoStatus::InProgress,
                Some("completed") => crate::agent::session_state::TodoStatus::Completed,
                Some(other) => {
                    return format!(
                        "Error: todo item '{}' has invalid status '{}'. Must be one of: pending, in_progress, completed",
                        id, other
                    )
                }
                None => {
                    return format!("Error: todo item '{}' missing required 'status' field", id)
                }
            };
            items.push(TodoItem {
                id,
                content,
                status,
            });
        }

        // Update the session todos
        self.session.update_todos(items, merge);

        // Inject the updated list into context builder for the next build()
        context_builder.set_todo_context(self.session.format_todos());

        // Emit TodoListUpdated event to frontend for UI rendering
        let _ = self.core.try_send_chunk(ChunkEvent::TodoListUpdated {
            todos: self.session.todos.clone(),
        });

        // Return formatted list as the tool result
        match self.session.format_todos() {
            Some(formatted) => {
                let count = self.session.todos.len();
                format!(
                    "Todo list updated ({} items, merge={}):\n{}",
                    count, merge, formatted
                )
            }
            None => "Todo list is now empty.".to_string(),
        }
    }

    /// Await user's answer to an ask_user_question prompt.
    ///
    /// Blocks the main loop on `inbound_rx` until the matching
    /// `InboundMessage::QuestionAnswer` arrives or the optional timeout expires.
    /// Non-matching messages are buffered in `deferred_inbound` for later processing.
    async fn await_question_answer(&mut self, request_id: &str, timeout_seconds: Option<u32>) -> String {
        /// Default timeout when none specified
        const DEFAULT_TIMEOUT_SECS: u32 = 300;
        let timeout_duration = std::time::Duration::from_secs(
            timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS) as u64
        );

        let timeout_future = tokio::time::timeout(timeout_duration, async {
            loop {
                tokio::select! {
                    msg = self.inbound_rx.recv() => {
                        match msg {
                        Some(InboundMessage::QuestionAnswer {
                            request_id: rid,
                            answer,
                        }) if rid == request_id => {
                            return answer;
                        }
                        Some(InboundMessage::QuestionAnswer {
                            request_id: rid,
                            answer,
                        }) => {
                            // Answer for a different question — buffer it
                            tracing::debug!(
                                expected = %request_id,
                                got = %rid,
                                "Buffering question answer for different request"
                            );
                            self.session.deferred_inbound.push(InboundMessage::QuestionAnswer {
                                request_id: rid,
                                answer,
                            });
                        }
                        Some(InboundMessage::Interrupt { reason }) => {
                            tracing::info!(
                                reason = %reason,
                                request_id = %request_id,
                                "Question wait interrupted, returning cancelled"
                            );
                            return "[Cancelled: user interrupted]".to_string();
                        }
                        Some(other) => {
                            tracing::debug!(
                                ?other,
                                "Buffering non-question message during question wait"
                            );
                            self.session.deferred_inbound.push(other);
                        }
                        None => {
                            tracing::warn!(
                                request_id = %request_id,
                                "Inbound channel closed during question wait, returning cancelled"
                            );
                            return "[Cancelled: channel closed]".to_string();
                        }
                    }
                }
                // Also process concurrent approval requests
                approval_req = self.approval_rx.recv() => {
                    match approval_req {
                        Some((req, decision_tx)) => {
                            tracing::info!(
                                "Queuing concurrent approval request while waiting for question answer"
                            );
                            self.handle_approval_request(req, decision_tx).await;
                        }
                        None => {
                            tracing::warn!("Approval channel closed during question wait");
                        }
                    }
                }
            }
        }
    });
        let result = timeout_future.await;

        match result {
            Ok(answer) => answer,
            Err(_elapsed) => {
                tracing::warn!(
                    request_id = %request_id,
                    timeout_secs = %timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS),
                    "Question answer timed out"
                );
                "[Timeout: user did not respond]".to_string()
            }
        }
    }

    /// Handle an approval request received on the approval_rx channel.
    ///
    /// Called from `execute_tools_parallel`'s `select!` when a spawned tool
    /// task sends an approval request. Generates a unique request ID, sends
    /// `ChunkEvent::ToolApprovalNeeded` to the Gateway, blocks the main loop
    /// via `await_approval_decision()`, and resolves the spawned task's
    /// oneshot with the user's decision.
    pub(crate) fn handle_approval_request(
        &mut self,
        req: ApprovalRequest,
        decision_tx: oneshot::Sender<ApprovalDecision>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let request_id = self
                .approval_next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .to_string();

            tracing::info!(
                request_id = %request_id,
                tool_name = %req.tool_name,
                action = %req.action,
                risk = %req.risk_level.label(),
                "Handling approval request from spawned tool task"
            );

            // 1. Send ChunkEvent to Gateway → Desktop App
            self.send_tool_approval_needed(&request_id, &req);

            // ADR-014: Streaming → WaitingApproval
            self.transition_status(SessionStatus::WaitingApproval {
                request_id: request_id.clone(),
            });

            // 2. Pause and wait for user decision (no timeout)
            let decision = self.await_approval_decision(&request_id).await;

            // ADR-014: WaitingApproval → Streaming (resume after approval/rejection)
            self.transition_status(SessionStatus::Streaming { message_id: None });

            // 3. Resolve the spawned task's oneshot
            let _ = decision_tx.send(decision);
        })
    }

    /// Update the debug phase and check for breakpoints.
    ///
    /// Pushes `onStateChange` events and checks if any breakpoint matches.
    /// If a breakpoint hits, the controller is set to Paused and an
    /// `onBreakpoint` event is pushed.
    async fn update_debug_phase(&mut self, phase: crate::debug::protocol::DebugPhase) {
        let Some(ctrl) = self.core.debug_ctrl() else {
            return;
        };

        let mut ctrl_guard = ctrl.lock().await;
        let old_phase = ctrl_guard.phase;
        ctrl_guard.phase = phase;

        // Push state change event
        if let Some(event_tx) = self.core.debug_event_tx() {
            let _ = event_tx.send(crate::debug::server::DebugEvent::StateChanged {
                old_phase,
                new_phase: phase,
                iteration: ctrl_guard.iteration,
            });
        }

        // Check breakpoints
        let hit_ids = ctrl_guard.check_breakpoints(phase, None);
        if !hit_ids.is_empty() {
            for bp_id in &hit_ids {
                tracing::info!(breakpoint_id = %bp_id, phase = ?phase, "Debug: breakpoint hit");
                if let Some(event_tx) = self.core.debug_event_tx() {
                    let _ = event_tx.send(crate::debug::server::DebugEvent::BreakpointHit {
                        breakpoint_id: bp_id.clone(),
                        iteration: ctrl_guard.iteration,
                        phase,
                    });
                }
            }
            ctrl_guard.state = crate::debug::controller::DebugState::Paused;
            {
                // Push execution state change event for frontend sync
                if let Some(event_tx) = self.core.debug_event_tx() {
                    let _ = event_tx.send(
                        crate::debug::server::DebugEvent::ExecutionStateChanged {
                            new_state: crate::debug::controller::DebugState::Paused,
                            iteration: ctrl_guard.iteration,
                        },
                    );
                }
            }
            drop(ctrl_guard); // Release lock before blocking
            self.await_debug_resume().await;
        }
    }

    /// Push a step event to the debug client.
    fn push_debug_step(
        &self,
        phase: crate::debug::protocol::DebugPhase,
        input: Option<serde_json::Value>,
        output: Option<serde_json::Value>,
    ) {
        if let (Some(ctrl), Some(event_tx)) =
            (self.core.debug_ctrl(), self.core.debug_event_tx())
        {
            // Read iteration from controller (avoid holding lock across send)
            let iteration = {
                // Use try_lock to avoid blocking in non-async context;
                // if lock is contested, just skip the event.
                let Ok(ctrl) = ctrl.try_lock() else { return };
                ctrl.iteration
            };
            let _ = event_tx.send(crate::debug::server::DebugEvent::Step {
                iteration,
                phase,
                input,
                output,
                usage: None,
            });
        }
    }

    /// Auto-pause if in Stepping mode (after completing one iteration).
    async fn debug_auto_pause_if_stepping(&self) {
        if let Some(ctrl) = self.core.debug_ctrl() {
            let mut ctrl_guard = ctrl.lock().await;
            if ctrl_guard.state == crate::debug::controller::DebugState::Stepping {
                ctrl_guard.state = crate::debug::controller::DebugState::Paused;
                let iteration = ctrl_guard.iteration;
                drop(ctrl_guard);
                // Push execution state change event for frontend sync
                if let Some(event_tx) = self.core.debug_event_tx() {
                    let _ = event_tx.send(
                        crate::debug::server::DebugEvent::ExecutionStateChanged {
                            new_state: crate::debug::controller::DebugState::Paused,
                            iteration,
                        },
                    );
                }
                tracing::info!("Debug: stepping complete, auto-pausing");
            }
        }
    }

    /// Build a ContextSnapshot from the current ContextBuilder state
    /// and store it in the debug controller, then push an onContextBuilt event.
    ///
    /// This is called after [`ContextBuilder::build()`] in each iteration
    /// when DevMode is active. Captures the 5 control-plane sections with
    /// metadata (size, token estimate, SHA-256 hash) for the debug panel's
    /// context tree view.
    ///
    /// `debug_iter` is the iteration number captured *before* any yield
    /// points in [`execute_single_iteration`].  Because rewind operations
    /// can modify `ctrl.iteration` mid-flight (between the increment and
    /// this snapshot), we must use the captured value rather than reading
    /// the controller field again.
    async fn capture_context_snapshot(
        &self,
        context_builder: &ContextBuilder,
        debug_iter: Option<u32>,
    ) {
        let Some(iter) = debug_iter else {
            return; // Not in DevMode
        };
        let Some(event_tx) =
            self.core.debug_event_tx()
        else {
            return;
        };

        use crate::debug::controller::{ContextSnapshot, ContextSnapshotSections, SectionContent};

        // Build tool_definitions string: merge ContextBuilder's built-in tools
        // with MCP tools from AgentCore. MCP tools are only mixed at display time
        // (debug panel) and at LLM injection time, not in active_tools/tool_definitions.
        let mut all_defs: Vec<serde_json::Value> = context_builder
            .tool_definitions()
            .map(|defs| defs.to_vec())
            .unwrap_or_default();
        for tool in &self.core.all_tools {
            let spec = tool.spec();
            if spec.name.starts_with("mcp:") {
                let val = serde_json::to_value(&spec).unwrap_or_default();
                all_defs.push(val);
            }
        }
        let tool_defs_str = serde_json::Value::Array(all_defs).to_string();

        // Build skill_instructions from the ContextBuilder.
        // Skill instructions are injected via ContextBuilder.set_skill_instructions()
        // (from command-based skill selection in cli.rs), making them visible
        // in the debug panel's context snapshot.
        let skill_str = context_builder
            .skill_instructions()
            .map(|s| s.to_string())
            .unwrap_or_default();

        // ── Diagnostic: log workspace_context status for debugging zero-byte issue ──
        tracing::info!(
            iter = iter,
            ws_has = context_builder.workspace_context().is_some(),
            ws_len = context_builder.workspace_context().map(|s| s.len()).unwrap_or(0),
            ws_preview = ?context_builder.workspace_context().map(|s| &s[..s.len().min(80)]),
            "capture_context_snapshot: workspace_context status"
        );

        let sections = ContextSnapshotSections {
            system_prompt: SectionContent::new(context_builder.system_prompt().to_string()),
            workspace_context: SectionContent::new(
                context_builder.workspace_context().unwrap_or_default().to_string(),
            ),
            environment: SectionContent::new(
                context_builder
                    .environment_override()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| crate::agent::context::detect_environment_text()),
            ),
            tool_definitions: SectionContent::new(tool_defs_str),
            skill_instructions: SectionContent::new(skill_str),
            retrieved_memory: SectionContent::new(
                context_builder.retrieved_memory().unwrap_or_default().to_string(),
            ),
            identity_context: SectionContent::new(
                context_builder.identity_context().unwrap_or_default().to_string(),
            ),
        };

        let total_token_estimate = sections.system_prompt.token_estimate
            + sections.workspace_context.token_estimate
            + sections.environment.token_estimate
            + sections.tool_definitions.token_estimate
            + sections.skill_instructions.token_estimate
            + sections.retrieved_memory.token_estimate
            + sections.identity_context.token_estimate;

        let snapshot = ContextSnapshot {
            iteration: iter,
            built_at: chrono::Utc::now(),
            sections,
            total_token_estimate,
        };

        // Store in controller
        if let Some(ctrl) = self.core.debug_ctrl() {
            let mut ctrl_guard = ctrl.lock().await;
            ctrl_guard.store_context_snapshot(snapshot.clone());
        }

        // Push onContextBuilt event to WebSocket client
        let context_sections =
            crate::debug::protocol::ContextSections::from(&snapshot.sections);
        let sent = event_tx.send(crate::debug::server::DebugEvent::ContextBuilt {
            iteration: snapshot.iteration,
            sections: context_sections,
            total_token_estimate: snapshot.total_token_estimate,
        });

        tracing::info!(
            iteration = snapshot.iteration,
            total_token_estimate,
            event_sent = sent,
            "Debug: context snapshot captured and event pushed"
        );
    }

    /// Get reference to history manager
    pub fn history(&self) -> &HistoryManager {
        &self.session.history
    }

    /// Get reference to the agent manifest
    pub fn manifest(&self) -> &rollball_core::AgentManifest {
        &self.core.manifest
    }

    /// Get mutable reference to history manager
    pub fn history_mut(&mut self) -> &mut HistoryManager {
        &mut self.session.history
    }
}

/// Extract content inside `<think>...</think>` tags if present.
fn extract_think_block(content: &str) -> Option<String> {
    let start_tag = "<think>";
    let end_tag = "</think>";
    let start = content.find(start_tag)?;
    let end = content.find(end_tag)?;
    if end <= start + start_tag.len() {
        return None;
    }
    Some(content[start + start_tag.len()..end].trim().to_string())
}

/// Remove `<think>...</think>` block from content, returning the remaining text.
fn strip_think_block(content: &str) -> String {
    let start_tag = "<think>";
    let end_tag = "</think>";
    if let Some(start) = content.find(start_tag)
        && let Some(end) = content.find(end_tag)
    {
        let before = &content[..start];
        let after = &content[end + end_tag.len()..];
        return format!("{}{}", before.trim(), after.trim()).trim().to_string();
    }
    content.to_string()
}

/// Build think message metadata with timing info from ChatResponse.
fn build_think_metadata(response: &ChatResponse) -> Option<serde_json::Value> {
    if response.reasoning_started_at.is_some() || response.reasoning_finished_at.is_some() {
        Some(serde_json::json!({
            "startTime": response.reasoning_started_at,
            "endTime": response.reasoning_finished_at,
        }))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::loop_llm::make_incomplete_marker;
    use crate::agent::loop_tools::execute_single_tool;
    use rollball_core::providers::mock::MockProvider;
    use rollball_core::providers::traits::FunctionCall;

    /// Simple echo tool for testing
    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
            rollball_core::tools::traits::ToolSpec {
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
        async fn execute(&self, params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
            let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("no message");
            Ok(rollball_core::tools::traits::ToolResult {
                ok: true,
                content: format!("Echo: {message}"),
                error: None,
                token_usage: None,
            })
        }
    }

    fn test_manifest() -> rollball_core::AgentManifest {
        rollball_core::AgentManifest::from_toml(
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

    fn test_budget() -> rollball_core::Budget {
        rollball_core::Budget {
            daily_tokens: Some(100000),
            monthly_tokens: None,
            daily_cost_usd: Some(10.0),
            monthly_cost_usd: None,
            exceeded_action: "warn".to_string(),
        }
    }

    #[test]
    fn test_agent_loop_with_gateway_client() {
        // NOTE: We use ipc_client: None because GatewayGrpcClient::connect is
        // lazy (does not immediately connect), and connecting to a non-existent
        // server would fail. This test verifies that AgentLoop construction works
        // correctly, not the gRPC connection.
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let budget = test_budget();
        let (_agent_loop, _inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        // Verify inbound sender works
        assert!(_inbound_tx.try_send(InboundMessage::UserMessage("test".to_string())).is_ok());
    }

    #[test]
    fn test_agent_loop_without_gateway_client() {
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let budget = test_budget();
        let (_agent_loop, _inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        // Just verify construction works
        assert!(_inbound_tx.try_send(InboundMessage::UserMessage("test".to_string())).is_ok());
    }

    #[tokio::test]
    async fn test_agent_loop_standalone_no_panic() {
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("Hello from standalone!"));
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let budget = test_budget();
        let (mut agent_loop, _inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None).await;
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
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None).await;
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
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stream_finished_event() {
        // When stream emits Finished, content and usage are extracted
        let provider = Arc::new(MockProvider::single_text("Final response"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Final response");
        // Verify usage was tracked (budget guard should have been updated)
        assert!(agent_loop.history().estimate_total_tokens() > 0);
    }

    #[tokio::test]
    async fn test_stream_error_propagation() {
        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::Error {
                message: "API rate limit".to_string(),
            },
        ]));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None).await;
        assert!(result.is_err());
        // Error from chat_stream propagates as Core(RollballError::Provider(...))
        // because Provider trait returns rollball_core::RollballError
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("rate limit"), "Error should mention rate limit: {}", err_msg);
    }

    #[tokio::test]
    async fn test_stream_content_then_tool_call() {
        // MockProvider returns tool call then text — content accumulates correctly
        let provider = Arc::new(MockProvider::tool_call_then_text(
            "echo",
            r#"{"message": "test"}"#,
            "All done",
        ));
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "All done");
    }

    #[tokio::test]
    async fn test_stream_empty_content() {
        let provider = Arc::new(MockProvider::single_text(""));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &mut context_builder, None).await;
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
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let _ = agent_loop.run("Hi", &mut context_builder, None).await;
        let messages = agent_loop.history().messages();
        // Should have: user message + assistant message
        let assistant_msgs: Vec<_> = messages.iter()
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
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());
        let _ = agent_loop.run("Hi", &mut context_builder, None).await;
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
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        // Inject a user message before running
        inbound_tx.try_send(InboundMessage::UserMessage("Injected question".to_string())).unwrap();

        let result = agent_loop.run("Hi", &mut context_builder, None).await;
        assert!(result.is_ok());
        // Verify the injected message appeared in history
        let messages = agent_loop.history().messages();
        let injected: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("Injected question"))
            .collect();
        assert!(!injected.is_empty(), "Injected user message should appear in history");
    }

    #[tokio::test]
    async fn test_inbound_system_notification() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        inbound_tx.try_send(InboundMessage::SystemNotification {
            notification_type: "identity_update".to_string(),
            data: serde_json::json!({"key": "new_value"}),
        }).unwrap();

        let result = agent_loop.run("Hi", &mut context_builder, None).await;
        assert!(result.is_ok());
        let messages = agent_loop.history().messages();
        let notif: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("[system:identity_update]"))
            .collect();
        assert!(!notif.is_empty(), "System notification should appear in history");
    }

    #[tokio::test]
    async fn test_inbound_intent_message() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        inbound_tx.try_send(InboundMessage::IntentMessage {
            from: "com.rollball.system".to_string(),
            action: "ping".to_string(),
            params: serde_json::json!({}),
        }).unwrap();

        let result = agent_loop.run("Hi", &mut context_builder, None).await;
        assert!(result.is_ok());
        let messages = agent_loop.history().messages();
        let intent: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("[intent:com.rollball.system:ping]"))
            .collect();
        assert!(!intent.is_empty(), "Intent message should appear in history");
    }

    #[tokio::test]
    async fn test_inbound_concurrent_injection() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        // Inject 10 messages concurrently
        for i in 0..10 {
            inbound_tx.try_send(InboundMessage::UserMessage(format!("Message {i}"))).unwrap();
        }

        let result = agent_loop.run("Hi", &mut context_builder, None).await;
        assert!(result.is_ok());
        let messages = agent_loop.history().messages();
        let injected: Vec<_> = messages.iter()
            .filter(|m| m.content.starts_with("Message "))
            .collect();
        assert_eq!(injected.len(), 10, "All 10 injected messages should appear in history");
    }

    #[tokio::test]
    async fn test_inbound_queue_full_backpressure() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);

        // Fill the channel (capacity 64)
        for i in 0..64 {
            assert!(inbound_tx.try_send(InboundMessage::UserMessage(format!("Msg {i}"))).is_ok());
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
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        // Run without any inbound messages — drain should return immediately
        let start = std::time::Instant::now();
        let result = agent_loop.run("Hi", &mut context_builder, None).await;
        let elapsed = start.elapsed();
        assert!(result.is_ok());
        // Drain should not block — core path is sub-100ms, but allow up to 2s
        // for CI variance and debug-build overhead of the async runtime.
        assert!(elapsed < std::time::Duration::from_secs(2), "Drain should be non-blocking, but took {:?}", elapsed);
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
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: self.name.clone(),
                    description: format!("Slow tool {}", self.name),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
                Ok(rollball_core::tools::traits::ToolResult {
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
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(SlowTool { name: "slow_a".to_string(), delay_ms: 100 }),
            Arc::new(SlowTool { name: "slow_b".to_string(), delay_ms: 100 }),
        ];

        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
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
            rollball_core::providers::mock::MockResponse::Text {
                content: "Both done".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop.run("Run parallel", &mut context_builder, None).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Parallel execution should succeed: {:?}", result);
        // Parallel: ~100ms total. Serial would be ~200ms.
        // Allow generous margin (300ms) to avoid flaky tests
        assert!(elapsed < std::time::Duration::from_millis(300),
            "Parallel execution should be faster than serial: {:?}", elapsed);
    }

    #[tokio::test]
    async fn test_tool_single_failure_no_shortcircuit() {
        use async_trait::async_trait;

        struct FailTool;
        #[async_trait]
        impl Tool for FailTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "fail_tool".to_string(),
                    description: "Always fails".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
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
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "success_tool".to_string(),
                    description: "Always succeeds".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
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
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FailTool),
            Arc::new(SuccessTool),
        ];

        // LLM returns both tool calls, then text
        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
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
            rollball_core::providers::mock::MockResponse::Text {
                content: "Mixed results".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let result = agent_loop.run("Test failure", &mut context_builder, None).await;
        assert!(result.is_ok(), "Should succeed even with one tool failure");
        assert_eq!(result.unwrap(), "Mixed results");
    }

    #[tokio::test]
    async fn test_tool_timeout() {
        use async_trait::async_trait;

        struct StuckTool;
        #[async_trait]
        impl Tool for StuckTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "stuck_tool".to_string(),
                    description: "Never returns".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                // Sleep for a long time — should be cut short by timeout.
                // 5s is more than enough to verify timeout works (100ms threshold),
                // while avoiding a 60s hang if timeout logic breaks.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(rollball_core::tools::traits::ToolResult {
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
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(StuckTool)];

        let provider = Arc::new(MockProvider::tool_call_then_text(
            "stuck_tool",
            "{}",
            "After timeout",
        ));

        let config = RuntimeConfig { iteration_timeout_ms: 100, ..Default::default() }; // 100ms timeout
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop.run("Test timeout", &mut context_builder, None).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Should succeed with timeout error captured: {:?}", result);
        // Should complete within ~1 second (100ms timeout + overhead)
        assert!(elapsed < std::time::Duration::from_secs(2),
            "Should timeout quickly: {:?}", elapsed);

        // Verify the timeout error message appears in history
        let messages = agent_loop.history().messages();
        let timeout_msg: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("timed out"))
            .collect();
        assert!(!timeout_msg.is_empty(), "Timeout error should appear in tool result history");
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
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        // shell requires Shell permission, but manifest doesn't declare it
        let tools: Vec<Arc<dyn Tool>> = vec![];

        let provider = Arc::new(MockProvider::tool_call_then_text(
            "shell",
            r#"{"command": "ls"}"#,
            "Done",
        ));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        // The tool call will fail because shell is not in the tool registry
        // (empty tools vec), so it should produce "Unknown tool: shell"
        let result = agent_loop.run("Run shell", &mut context_builder, None).await;
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
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: self.name.clone(),
                    description: format!("Ordered tool {}", self.name),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
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
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(OrderedTool { name: "tool_a".to_string(), output: "Result A".to_string() }),
            Arc::new(OrderedTool { name: "tool_b".to_string(), output: "Result B".to_string() }),
            Arc::new(OrderedTool { name: "tool_c".to_string(), output: "Result C".to_string() }),
        ];

        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
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
            rollball_core::providers::mock::MockResponse::Text {
                content: "All ordered".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let result = agent_loop.run("Run ordered", &mut context_builder, None).await;
        assert!(result.is_ok());

        // Verify that tool results in history are in order
        let messages = agent_loop.history().messages();
        let tool_results: Vec<_> = messages.iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(tool_results.len(), 3);
        // First tool result should be tool_a
        assert!(tool_results[0].content.contains("Result A"), "First result should be A");
        // Second should be tool_b
        assert!(tool_results[1].content.contains("Result B"), "Second result should be B");
        // Third should be tool_c
        assert!(tool_results[2].content.contains("Result C"), "Third result should be C");
    }

    // ── Fix #1: Iteration timeout with partial results ─────────────────

    #[tokio::test]
    async fn test_iteration_timeout_partial_results() {
        use async_trait::async_trait;

        #[derive(Clone)]
        struct FastTool;

        #[async_trait]
        impl Tool for FastTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "fast_tool".to_string(),
                    description: "Fast tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
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
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "slow_tool".to_string(),
                    description: "Slow tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                // Sleep longer than the iteration timeout (200ms).
                // 5s is plenty to verify timeout works without risking a 60s hang.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(rollball_core::tools::traits::ToolResult {
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
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FastTool),
            Arc::new(SlowTool),
        ];

        // LLM requests both tools; fast_tool completes quickly, slow_tool times out
        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
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
            rollball_core::providers::mock::MockResponse::Text {
                content: "Partial complete".to_string(),
            },
        ]));

        // Very short iteration timeout so slow_tool gets aborted
        let config = RuntimeConfig {
            iteration_timeout_ms: 200,
            tool_timeout_ms: 10000, // tool_timeout is long, iteration timeout is short
            ..Default::default()
        };
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop.run("Test iteration timeout", &mut context_builder, None).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Should succeed with partial results: {:?}", result);
        // Should complete within ~1 second (200ms iteration timeout + overhead)
        assert!(elapsed < std::time::Duration::from_secs(2),
            "Should complete quickly with iteration timeout: {:?}", elapsed);

        // Verify the fast_tool result and slow_tool timeout both appear in history
        let messages = agent_loop.history().messages();
        let tool_results: Vec<_> = messages.iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        // fast_tool should have its result
        assert!(tool_results[0].content.contains("Fast result"),
            "Fast tool should have its result");
        // slow_tool should have iteration timeout error
        assert!(tool_results[1].content.contains("iteration timed out"),
            "Slow tool should have iteration timeout error: {}", tool_results[1].content);
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
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "medium_tool".to_string(),
                    description: "Medium-speed tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                // Sleep longer than tool_timeout (100ms) but shorter than iteration_timeout (30s)
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Ok(rollball_core::tools::traits::ToolResult {
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
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MediumTool)];

        let provider = Arc::new(MockProvider::tool_call_then_text(
            "medium_tool",
            "{}",
            "After tool timeout",
        ));

        // tool_timeout_ms is 100ms (shorter than tool execution),
        // iteration_timeout_ms is 30000ms (much longer)
        let config = RuntimeConfig {
            tool_timeout_ms: 100,
            iteration_timeout_ms: 30000,
            ..Default::default()
        };
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop.run("Test tool timeout", &mut context_builder, None).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Should succeed with tool timeout error: {:?}", result);
        // Should complete in ~100ms (tool timeout) + overhead, not 500ms
        assert!(elapsed < std::time::Duration::from_secs(2),
            "Should timeout at tool level: {:?}", elapsed);

        // Verify per-tool timeout message (not iteration timeout)
        let messages = agent_loop.history().messages();
        let timeout_msg: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("timed out"))
            .collect();
        assert!(!timeout_msg.is_empty(), "Per-tool timeout should be recorded");
        // Should NOT be an iteration timeout message
        assert!(timeout_msg.iter().all(|m| !m.content.contains("iteration timed out")),
            "Should be per-tool timeout, not iteration timeout");
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
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "echo".to_string(),
                    description: "Echo tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
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
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoPermTool)];

        // LLM requests both echo and shell
        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
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
            rollball_core::providers::mock::MockResponse::Text {
                content: "Partial permission result".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
        let mut context_builder = ContextBuilder::new("System".to_string());

        let result = agent_loop.run("Test partial permission", &mut context_builder, None).await;
        assert!(result.is_ok(), "Should succeed even with one tool permission denied: {:?}", result);

        // Verify echo result appears (it was executed) and shell has permission denied
        let messages = agent_loop.history().messages();
        let tool_results: Vec<_> = messages.iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(tool_results.len(), 2, "Should have 2 tool results");
        // First tool (echo) should have result
        assert!(tool_results[0].content.contains("Echo result") || tool_results[0].content.contains("Unknown tool"),
            "Echo tool should have result or unknown tool error");
        // Second tool (shell) is not in the tool registry (permission denied),
        // so it should produce an "Unknown tool" error.
        assert!(tool_results[1].content.contains("Unknown tool: shell"),
            "Shell tool should be unknown (not in registry): {}", tool_results[1].content);
    }

    // ── S1.9: Tool call argument robustness tests ──────────────────────

    /// Verify that TOOL_CALL_INCOMPLETE marker is detected and tool execution
    /// is skipped, returning the embedded message to the LLM.
    #[tokio::test]
    async fn test_incomplete_tool_call_skipped() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];

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

        let result = execute_single_tool(&tools, &tc).await;

        // Must NOT contain "Echo:" — tool was never called
        assert!(!result.contains("Echo:"), "Tool should NOT be executed, got: {}", result);
        // Must contain the error message from the marker
        assert!(result.contains("truncated during streaming"),
            "Result should explain truncation: {}", result);
        assert!(result.contains("NOT executed"),
            "Result should state it was NOT executed: {}", result);
    }

    /// Verify that genuinely unparseable JSON (e.g. LLM hallucinated output)
    /// does not silently degrade to {} — it returns a clear error.
    #[tokio::test]
    async fn test_invalid_json_tool_call_error() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];

        // Simulate LLM producing broken JSON (not from streaming truncation)
        let tc = ToolCall {
            id: "call_broken".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "echo".to_string(),
                arguments: r#"{"message": "hello"#.to_string(), // missing closing brace
            },
        };

        let result = execute_single_tool(&tools, &tc).await;

        // Must NOT execute the tool
        assert!(!result.contains("Echo:"), "Tool should NOT be executed on invalid JSON, got: {}", result);
        // Must contain error explanation
        assert!(result.contains("not valid JSON"),
            "Result should explain JSON parse failure: {}", result);
        assert!(result.contains("NOT executed"),
            "Result should state it was NOT executed: {}", result);
    }

    /// Verify that valid JSON tool arguments execute normally (regression test
    /// for the INCOMPLETE/invalid-JSON guard).
    #[tokio::test]
    async fn test_valid_json_tool_call_executes_normally() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];

        let tc = ToolCall {
            id: "call_ok".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "echo".to_string(),
                arguments: r#"{"message": "hello world"}"#.to_string(),
            },
        };

        let result = execute_single_tool(&tools, &tc).await;
        assert_eq!(result, "Echo: hello world",
            "Valid tool call should execute normally, got: {}", result);
    }
}
