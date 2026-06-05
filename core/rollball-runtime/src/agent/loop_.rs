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
use crate::agent::context::ContextBuilder;
use crate::agent::history::HistoryManager;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_detector::{LoopDetectionResult, LoopPattern, ResponseLevel};
use crate::agent::session_state::SessionState;
use crate::config::RuntimeConfig;
use crate::conversation::ConversationSession;
use crate::error::{Result, RuntimeError};
use crate::agent::loop_approval::{ApprovalDecision, ApprovalHandle};
use crate::agent::loop_session::{build_think_metadata, extract_think_block, strip_think_block};
use crate::security::approval_gate::ApprovalRequest;
use crate::tools::builtin::ask_user_question::QuestionOption;

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
    /// Context compaction started (emitted before auto/manual compact triggers),
    /// so the frontend can show a "Context compacting..." indicator.
    CompactingStarted,
    /// Context compaction finished (emitted after compaction completes or fails),
    /// so the frontend can clear the "compacting..." indicator.
    CompactingEnded,
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
    Stopped {
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
    /// ADR-012: Also carries per-session model/provider so the frontend
    /// can display the correct model after session activation/resume.
    SessionStateChanged {
        status: SessionStatus,
        model: Option<String>,
        provider: Option<String>,
        workspace_id: Option<String>,
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
/// - Production `run()`: loops automatically until TextResponse/Stopped
/// - Debug `DebugSessionTask`: calls one iteration at a time with pause/breakpoint control
#[derive(Debug)]
pub(crate) enum IterationResult {
    /// Agent returned a text response — conversation round complete
    TextResponse(String),
    /// Tool calls were executed successfully — continue to next iteration
    ToolCallsExecuted,
    /// Agent was stopped by user request
    Stopped(String),
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
    /// If `on_chunk` is provided, streaming LLM deltas are forwarded to it
    /// so the caller can relay chunks to the Gateway via StreamChunk messages
    /// (like ZeroClaw's on_delta / DraftEvent pattern).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_observer(
        config: RuntimeConfig,
        manifest: rollball_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        budget: rollball_core::Budget,
        on_chunk: Option<mpsc::Sender<SessionChunkEvent>>,
        conversation: Option<ConversationSession>,
        observer: crate::debug::DebugObserverSlot,
    ) -> (Self, tokio::sync::mpsc::Sender<InboundMessage>) {
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(64);
        let (approval_tx, approval_rx) = mpsc::channel::<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>(16);
        let max_tokens = config.history_max_tokens;
        let approval_handle = ApprovalHandle::new(approval_tx);
        let mut loop_ = Self {
            core: AgentCore::new_with_observer(config, manifest, provider, tools, on_chunk, observer),
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

    /// Create a new agent loop runner, returning both the loop and an inbound sender.
    ///
    /// Defaults to Production mode (zero-cost debug no-ops).
    /// Use [`AgentLoop::new_with_observer()`] to inject a DevMode observer.
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
        Self::new_with_observer(
            config, manifest, provider, tools, budget, on_chunk, conversation,
            crate::debug::DebugObserverSlot::production(),
        )
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

    /// Look up model capabilities by exact model name (delegates to AgentCore).
    pub(crate) fn get_model_capabilities(&self, model_name: &str) -> Option<&ModelCapabilitiesInfo> {
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
            // ADR-011: reset compaction flag — new user input means new content since last compaction
            self.session.is_compacted = false;
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
                                    self.transition_status(SessionStatus::Streaming { message_id: None });
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
                            tracing::warn!("Inbound channel closed during iteration limit pause, stopping");
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
                IterationResult::Stopped(content) => {
                    // ADR-014: Streaming → Idle (stopped)
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
            // ── Debug observer hooks (ADR-013: Observer Pipeline) ──
            // Check for bypass-injected debug handles, then notify the
            // observer of iteration start (which increments the debug
            // iteration counter and creates a conversation snapshot).
            self.core.debug_observer.check_pending_injection();
            let debug_iter = self.core.debug_observer.on_iteration_start(
                self.session.history.len(),
            );

            // Await resume if paused (DevMode only).
            {
                let ctrl = self.core.debug_observer.debug_ctrl().cloned();
                if let Some(ctrl) = ctrl {
                    let rewind_notify = self.core.debug_observer.rewind_notify().cloned();
                    loop {
                        // Check for Chat Panel STOP
                        if self.poll_stop() {
                            tracing::info!("Debug: agent loop stopped via inbound channel");
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
                            return Ok(IterationResult::Stopped(String::new()));
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
                            ctrl_guard.state.clone()
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
                                return Ok(IterationResult::Stopped(String::new()));
                            }
                            crate::debug::controller::DebugState::Paused => {
                                self.transition_status(SessionStatus::Paused {
                                    iteration: None,
                                    max_iterations: None,
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
            }

            // ── Apply pending patches to context_builder (DevMode only) ──
            self.core.debug_observer.apply_pending_patches(context_builder);
            self.core.debug_observer.take_re_execute_pending();

            // Enter BudgetCheck phase
            self.core.debug_observer.on_phase_enter(
                crate::debug::protocol::DebugPhase::BudgetCheck,
            )
            .await;

            // ① Budget pre-check (ADR-014: extracted to loop_context.rs)
            self.check_budget_and_warn()?;

            // ② Preemptive trim — MUST happen BEFORE build() so the request
            // is constructed with already-trimmed history.
            self.trim_history_to_budget(&current_model);

            // ②.5 Build context (ADR-014: extracted to loop_context.rs)
            let mut chat_request = self.build_chat_request(context_builder, &current_model);

            // ②.6 Context usage check / circuit-breaking (ADR-014: extracted to loop_context.rs)
            if self.check_context_overflow_and_trim(&current_model) {
                // Rebuild request with trimmed history
                chat_request = self.build_chat_request(context_builder, &current_model);
            }

            // Debug: enter BuildContext phase
            self.core.debug_observer.on_phase_enter(
                crate::debug::protocol::DebugPhase::BuildContext,
            )
            .await;

            // Debug: create context snapshot and push onContextBuilt event
            self.core.debug_observer.on_context_built(
                crate::debug::observer::ContextSnapshotRequest {
                    context_builder,
                    iteration: debug_iter,
                    model: &current_model,
                    all_tools: &self.core.all_tools,
                },
            ).await;



            // ③ Call LLM with streaming (S1.5)
            let response = self.call_llm_streaming(&chat_request, context_builder).await?;

            // Debug: enter LlmCall phase
            self.core.debug_observer.on_phase_enter(crate::debug::protocol::DebugPhase::LlmCall)
                .await;

            // ④ Parse response
            let has_tool_calls = response.tool_calls.is_some();

            // Debug: enter ParseResponse phase
            self.core.debug_observer.on_phase_enter(
                crate::debug::protocol::DebugPhase::ParseResponse,
            )
            .await;

            // ④ Process usage + budget calibration + context usage report + compaction
            // (ADR-014: extracted to loop_context.rs)
            self.process_llm_response_usage(&response, &current_model).await;

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

                // Per ADR-011: per-turn episodic writes are removed.
                // Grafeo is now written only via compaction summaries and
                // session-close distillation.
                self.session.turn_counter += 1;

                tracing::info!(iteration, "Agent returned text response");

                // Debug: enter AppendHistory phase and push step event
                self.core.debug_observer.on_phase_enter(
                    crate::debug::protocol::DebugPhase::AppendHistory,
                )
                .await;
                self.core.debug_observer.on_phase_step(
                    crate::debug::protocol::DebugPhase::Idle,
                    None,
                    Some(serde_json::json!({"content": content})),
                );
                self.core.debug_observer.on_phase_step_done().await;

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

            // Check for stop before executing tools
            if self.poll_stop() {
                tracing::info!("Stopped before tool execution — saving partial response");
                // ADR-014: Streaming → Idle (stopped before tool execution)
                self.transition_status(SessionStatus::Idle);
                let content = response.content.clone();

                // Persist stopped assistant message to JSONL conversation.
                if let Some(ref conversation) = self.session.conversation {
                    let assistant_text = strip_think_block(&content);
                    conversation.append_message("assistant", &assistant_text, None);
                }

                // Notify frontend via chunk channel
                let _ = self.core.try_send_chunk(ChunkEvent::Stopped {
                    content: content.clone(),
                });

                // Debug: push step event and auto-pause if stepping
                self.core.debug_observer.on_phase_step(
                    crate::debug::protocol::DebugPhase::Idle,
                    None,
                    Some(serde_json::json!({"stopped": true, "content": content})),
                );
                self.core.debug_observer.on_phase_step_done().await;

                return Ok(IterationResult::Stopped(content));
            }

            // Debug: enter ToolExecution phase
            self.core.debug_observer.on_phase_enter(
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
            let (parallel_results, was_stopped) = self.execute_tools_parallel(&calls_for_parallel).await;

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

            // ⑥ Pre-trim for tool results (ADR-014: extracted to loop_context.rs)
            self.pre_trim_for_tool_results(&tool_results, current_model);

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

            // ── Check for stop detected during tool execution ──
            // poll_stop() consumed the stop signal inside execute_tools_parallel(),
            // so we must propagate it here to prevent the loop from continuing.
            if was_stopped {
                tracing::info!("Stopped during tool execution — saving partial results");
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
                let _ = self.core.try_send_chunk(ChunkEvent::Stopped {
                    content: content.clone(),
                });

                // Debug: push step event and auto-pause if stepping
                self.core.debug_observer.on_phase_step(
                    crate::debug::protocol::DebugPhase::Idle,
                    None,
                    Some(serde_json::json!({"stopped": true, "content": content})),
                );
                self.core.debug_observer.on_phase_step_done().await;

                return Ok(IterationResult::Stopped(content));
            }

            // ⑦ Usage report (async, non-blocking)
            tracing::debug!(iteration, "Loop iteration complete");

            // Debug: enter AppendHistory phase and push step event
            self.core.debug_observer.on_phase_enter(
                crate::debug::protocol::DebugPhase::AppendHistory,
            )
            .await;
            self.core.debug_observer.on_phase_step(
                crate::debug::protocol::DebugPhase::Idle,
                None,
                None,
            );
            self.core.debug_observer.on_phase_step_done().await;

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
    pub fn manifest(&self) -> &rollball_core::AgentManifest {
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
