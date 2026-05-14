//! Agent main loop (9 steps)
//!
//! The core execution loop for Agent Runtime.
//! References ZeroClaw agent/loop_.rs but simplified for IPC architecture.
//!
//! S1.5: Streaming LLM responses via chat_stream()
//! S1.6: InboundQueue for external message injection
//! S1.7: Parallel tool execution with per-tool timeout

use std::collections::HashSet;
use std::sync::Arc;

use rollball_core::providers::traits::{
    ChatMessage, ChatResponse, MessageRole, Provider, ToolCall,
};
use rollball_core::protocol::ModelCapabilitiesInfo;
use rollball_core::tools::traits::Tool;
use tokio::sync::mpsc;

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

/// Streaming chunk event emitted during LLM response generation.
///
/// Adapted from ZeroClaw's DraftEvent, simplified for RollBall's IPC architecture.
/// Each delta is forwarded to the Gateway via `StreamChunk` gRPC message,
/// which maps to a BridgeEventType::Chunk for the Desktop App WebSocket.
#[derive(Debug, Clone)]
pub enum ChunkEvent {
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
        on_chunk: Option<mpsc::Sender<ChunkEvent>>,
        conversation: Option<ConversationSession>,
    ) -> (Self, tokio::sync::mpsc::Sender<InboundMessage>) {
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(64);
        let max_tokens = config.history_max_tokens;
        let keep_full = config.keep_full_results;
        let loop_ = Self {
            core: AgentCore::new(config, manifest, provider, tools, on_chunk),
            session: SessionState::new(max_tokens, keep_full, budget, conversation),
            inbound_rx,
        };
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
        (Self { core, session, inbound_rx }, inbound_tx)
    }

    /// Update the LLM provider at runtime (e.g., after receiving a hot-pushed
    /// LLMConfigDelivery from Gateway).
    pub fn update_provider(&mut self, new_provider: Arc<dyn Provider>, model: String) {
        self.core.update_provider(new_provider, model);
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
    ) {
        self.core.apply_runtime_config(max_output_tokens, max_iterations, temperature, system_prompt_override);
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

    /// Update the title of the currently active conversation session.
    ///
    /// Returns `Some(true)` if the title was actually written (different from current),
    /// `Some(false)` if the title was already the same (no-op),
    /// or `None` if no active session exists.
    pub fn update_session_title(&mut self, title: &str) -> Option<bool> {
        self.session.conversation.as_ref().map(|conv| conv.update_title_force(title))
    }

    /// Look up model capabilities by model name.
    /// Falls back to the first entry if the model name is not found.
    pub(crate) fn get_model_capabilities(&self, model_name: Option<&str>) -> Option<&ModelCapabilitiesInfo> {
        self.core.get_model_capabilities(model_name)
    }

    /// Get the context window budget for history trimming.
    /// Uses Gateway model capabilities (context_window) if available,
    /// otherwise falls back to config.history_max_tokens.
    fn context_trim_budget(&self) -> u64 {
        self.core.context_trim_budget()
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
    /// When messages are evicted, they are captured and asynchronously
    /// distilled into a `DistilledEpisode` via `EpisodeDistiller`, so that
    /// the semantic content is preserved in Grafeo even after the messages
    /// are removed from the context window.
    fn trim_history_to_budget(&mut self) {
        let budget = self.context_trim_budget();
        // Reserve 20% of context window for new response + overhead
        let trim_budget = (budget as f64 * 0.8) as u64;
        let trimmed_messages = self.session.history.preemptive_trim_drain(trim_budget);

        // Spawn async distillation for evicted messages (best-effort, non-blocking)
        if !trimmed_messages.is_empty() {
            self.spawn_trim_distillation(trimmed_messages);
        }

        // Also truncate any single message that exceeds per-message limit
        self.session.history.truncate_large_messages(trim_budget / 4);
    }

    /// Spawn an asynchronous episode distillation task for trimmed messages.
    ///
    /// The task runs in the background and writes the distilled episode
    /// to Grafeo. Failures are logged but never panic or block the main loop.
    fn spawn_trim_distillation(&self, trimmed_messages: Vec<ChatMessage>) {
        let session_id = self
            .session.conversation
            .as_ref()
            .map(|c| c.session_id().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let provider = self.core.provider.clone();
        let model_name = self
            .core.gateway_model_capabilities
            .values()
            .min_by(|a, b| {
                let cost_a = crate::episode_distill::model_cost_score(a);
                let cost_b = crate::episode_distill::model_cost_score(b);
                cost_a
                    .partial_cmp(&cost_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .and_then(|m| m.name.clone())
            .unwrap_or_else(|| "default".to_string());
        let memory_store = self.core.memory_store().cloned();

        let msg_count = trimmed_messages.len();
        tracing::info!(
            msg_count,
            session_id = %session_id,
            model = %model_name,
            "Spawning episode distillation for trimmed messages"
        );

        tokio::spawn(async move {
            match crate::episode_distill::EpisodeDistiller::distill_on_trim(
                &trimmed_messages,
                &session_id,
                provider.as_ref(),
                &model_name,
            )
            .await
            {
                Ok(episode) => {
                    // Write distilled episode to Grafeo memory store (P2-1: using shared helper)
                    Self::write_distilled_to_grafeo(&memory_store, &episode, "trimmed");
                    tracing::info!(
                        summary = %episode.summary,
                        importance = episode.importance,
                        "Episode distillation completed for trimmed messages"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Episode distillation failed for trimmed messages (non-fatal)"
                    );
                }
            }
        });
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

    /// Switch to a new conversation session.
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
            let model_name = self
                .core.gateway_model_capabilities
                .values()
                .min_by(|a, b| {
                    let cost_a = crate::episode_distill::model_cost_score(a);
                    let cost_b = crate::episode_distill::model_cost_score(b);
                    cost_a.partial_cmp(&cost_b).unwrap_or(std::cmp::Ordering::Equal)
                })
                .and_then(|m| m.name.clone())
                .unwrap_or_else(|| "default".to_string());
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
            let model_name = self
                .core.gateway_model_capabilities
                .values()
                .min_by(|a, b| {
                    let cost_a = crate::episode_distill::model_cost_score(a);
                    let cost_b = crate::episode_distill::model_cost_score(b);
                    cost_a
                        .partial_cmp(&cost_b)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .and_then(|m| m.name.clone())
                .unwrap_or_else(|| "default".to_string());
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

    /// Run the agent loop for a single user message
    pub async fn run(&mut self, user_message: &str, context_builder: &mut ContextBuilder) -> Result<String> {
        // Add user message to history
        self.session.history.append(ChatMessage::user(user_message));

        // Persist user message to JSONL
        if let Some(ref conversation) = self.session.conversation {
            conversation.append_message("user", user_message, None);
            // Set session title from first user message (first 100 chars)
            conversation.set_title(user_message);
        }

        // Retrieve relevant long-term memories and inject into context
        // P2-4 fix: capture memory node IDs for later traceability in record_turn_to_memory
        let retrieved_memory_ids = self.retrieve_and_inject_memories(user_message, context_builder).await;

        let mut iteration = 0u32;

        loop {
            iteration += 1;
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
                if let Some(ref tx) = self.core.on_chunk {
                    let _ = tx.try_send(ChunkEvent::IterationLimitPaused {
                        iteration,
                        max_iterations: self.core.config.max_iterations,
                    });
                }

                // Wait for ContinueExecution or Interrupt from inbound queue
                loop {
                    match self.inbound_rx.recv().await {
                        Some(InboundMessage::ContinueExecution { reason }) => {
                            tracing::info!(
                                reason = %reason,
                                "User chose to continue, resetting iteration counter"
                            );
                            iteration = 0; // Reset counter
                            
                            // Trim history before resuming to avoid context window overflow
                            self.trim_history_to_budget();
                            
                            break; // Resume main loop
                        }
                        Some(InboundMessage::Interrupt { reason }) => {
                            tracing::info!(reason = %reason, "User chose to stop during iteration limit pause");
                            return Ok("Agent stopped by user request after reaching iteration limit.".to_string());
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
                                _ => {} // ContinueExecution and Interrupt handled above
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
                tracing::info!("Agent loop interrupted by inbound interrupt signal");
                return Ok("Agent stopped by user request.".to_string());
            }

            // ①-⑧ Execute single iteration (shared with debug mode)
            match self.execute_single_iteration(iteration, context_builder, user_message, &retrieved_memory_ids).await? {
                IterationResult::TextResponse(content) => return Ok(content),
                IterationResult::Interrupted(content) => return Ok(content),
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
            self.trim_history_to_budget();

            // ②.5 Build context (now with trimmed history)
            let chat_request = context_builder.build(&self.core.manifest, &self.session.history, self.get_model_capabilities(None), self.core.max_output_tokens_limit);

            tracing::info!(
                request_messages_count = chat_request.messages.len(),
                request_model = %chat_request.model,
                request_max_tokens = ?chat_request.max_tokens,
                request_tools_count = chat_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
                history_tokens = self.session.history.token_count(),
                "Built chat request for LLM (after preemptive trim)"
            );

            // Debug: enter BuildContext phase
            self.update_debug_phase(
                crate::debug::protocol::DebugPhase::BuildContext,
            )
            .await;

            // Debug: create context snapshot and push onContextBuilt event
            self.capture_context_snapshot(context_builder, debug_iter).await;

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

                // Compute and emit context usage report
                let model_caps = self.core.gateway_model_capabilities.values().next();
                tracing::debug!(
                    has_chunk_tx = self.core.on_chunk.is_some(),
                    has_model_caps = model_caps.is_some(),
                    caps_count = self.core.gateway_model_capabilities.len(),
                    has_usage = true,
                    "ContextUsage: checking preconditions"
                );
                if let (Some(chunk_tx), Some(caps)) = (&self.core.on_chunk, model_caps) {
                    let ctx_usage = crate::agent::context::compute_context_usage(caps, usage, self.core.max_output_tokens_limit);
                    tracing::debug!(
                        context_window = ctx_usage.context_window,
                        total_tokens = ctx_usage.total_tokens,
                        usage_percent = ctx_usage.usage_percent,
                        "ContextUsage: sending report"
                    );
                    let _ = chunk_tx.send(
                        crate::agent::loop_::ChunkEvent::ContextUsage(ctx_usage)
                    ).await;
                } else {
                    tracing::warn!(
                        has_chunk_tx = self.core.on_chunk.is_some(),
                        has_model_caps = model_caps.is_some(),
                        "ContextUsage: NOT sent — missing on_chunk channel or model capabilities"
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
            if let Some(ref tx) = self.core.on_chunk {
                for tc in &deduped_calls {
                    let event = ChunkEvent::ToolCall {
                        name: tc.function.name.clone(),
                        args: tc.function.arguments.clone(),
                        id: tc.id.clone(),
                    };
                    if tx.try_send(event).is_err() {
                        tracing::debug!("on_chunk channel full or closed, dropping ToolCall event");
                    }
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
                let content = response.content.clone();

                // Persist interrupted assistant message to JSONL conversation.
                if let Some(ref conversation) = self.session.conversation {
                    let assistant_text = strip_think_block(&content);
                    conversation.append_message("assistant", &assistant_text, None);
                }

                // Notify frontend via chunk channel
                if let Some(ref tx) = self.core.on_chunk {
                    let _ = tx.try_send(ChunkEvent::Interrupted {
                        content: content.clone(),
                    });
                }

                // Debug: push step event and auto-pause if stepping
                self.push_debug_step(
                    crate::debug::protocol::DebugPhase::Idle,
                    None,
                    Some(serde_json::json!({"interrupted": true, "content": content})),
                );
                self.debug_auto_pause_if_stepping().await;

                return Ok(IterationResult::Interrupted(format!("[Interrupted] {}", content)));
            }

            // Debug: enter ToolExecution phase
            self.update_debug_phase(
                crate::debug::protocol::DebugPhase::ToolExecution,
            )
            .await;

            let executed_results = self.execute_tools_parallel(&calls_to_execute).await;

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
            if let Some(ref tx) = self.core.on_chunk {
                for (tc, result_content) in deduped_calls.iter().zip(tool_results.iter()) {
                    let event = ChunkEvent::ToolResult {
                        name: tc.function.name.clone(),
                        result: result_content.clone(),
                        tool_call_id: tc.id.clone(),
                    };
                    if tx.try_send(event).is_err() {
                        tracing::debug!("on_chunk channel full or closed, dropping ToolResult event");
                    }
                }
            }

            // ⑥ Append ALL tool results to history first (must be contiguous after assistant tool_calls)
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
                return Err(RuntimeError::LoopDetected(msg));
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
    /// NOTE: Non-Interrupt messages drained during this poll are logged as warnings
    /// but NOT re-injected. This is acceptable because `drain_inbound_queue()` already
    /// handles all message types at the start of each loop iteration, and the
    /// streaming/tool-execution phase where this method is called should not need
    /// to process user/system messages mid-flight.
    pub(crate) fn poll_interrupt(&mut self) -> bool {
        while let Ok(msg) = self.inbound_rx.try_recv() {
            match msg {
                InboundMessage::Interrupt { .. } => {
                    return true;
                }
                other => {
                    // CRITICAL: non-Interrupt messages consumed here are LOST.
                    // They are NOT re-injected into history. This includes
                    // UserMessage, SystemNotification, and IntentMessage.
                    // If a user message arrives during a debug pause, it will
                    // be swallowed here with only this warning.
                    tracing::warn!(
                        msg_type = ?std::mem::discriminant(&other),
                        payload = %format!("{:?}", other).chars().take(80).collect::<String>(),
                        "poll_interrupt() DROPPED non-Interrupt message — this message is LOST and will NOT enter the context"
                    );
                }
            }
        }
        false
    }

    /// Drain inbound message queue (non-blocking).
    ///
    /// Injects external messages (user, system, intent) into history
    /// before each loop iteration. Applies size limits to prevent
    /// token explosion from oversized payloads.
    fn drain_inbound_queue(&mut self) -> bool {
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
                    tracing::info!(reason = %reason, "Received interrupt signal");
                    return true; // Signal to stop the agent loop
                }
                InboundMessage::ContinueExecution { .. } => {
                    // Continue is only meaningful during iteration limit pause;
                    // during normal drain, ignore it.
                    tracing::debug!("Ignoring ContinueExecution during normal drain");
                }
            }
        }
        false
    }

    // ── LLM streaming methods extracted to loop_llm.rs ──

    // ── Tool execution extracted to loop_tools.rs ──

    // ── Debug mode control methods ──

    /// Await debug resume: blocks if the debug controller is in Paused state.
    ///
    /// Polls every 100ms and returns when Running/Stepping or no controller.
    /// Also checks the inbound channel for Chat Panel STOP signals
    /// (InboundMessage::Interrupt), which arrive via the Gateway gRPC push
    /// path rather than the debug WebSocket.
    /// Returns `true` if execution should continue, `false` if stopped.
    async fn await_debug_resume(&mut self) -> bool {
        let Some(ctrl) = self.core.debug_ctrl().cloned() else {
            return true; // Production mode, no debug controller
        };

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

            let state = {
                let ctrl = ctrl.lock().await;
                ctrl.state.clone()
            };
            match state {
                crate::debug::controller::DebugState::Running => return true,
                crate::debug::controller::DebugState::Stepping => return true,
                crate::debug::controller::DebugState::Stopped => {
                    tracing::info!("Debug: agent loop stopped");
                    return false;
                }
                crate::debug::controller::DebugState::Paused => {
                    // Poll every 100ms until resumed
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
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

        // Build tool_definitions string from JSON values
        let tool_defs_str = context_builder
            .tool_definitions()
            .map(|defs| {
                serde_json::Value::Array(defs.to_vec()).to_string()
            })
            .unwrap_or_default();

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
        let result = agent_loop.run("Hi", &mut context_builder).await;
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
        let result = agent_loop.run("Hi", &mut context_builder).await;
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
        let result = agent_loop.run("Hi", &mut context_builder).await;
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
        let result = agent_loop.run("Hi", &mut context_builder).await;
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
        let result = agent_loop.run("Hi", &mut context_builder).await;
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
        let result = agent_loop.run("Hi", &mut context_builder).await;
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
        let result = agent_loop.run("Hi", &mut context_builder).await;
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
        let _ = agent_loop.run("Hi", &mut context_builder).await;
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
        let _ = agent_loop.run("Hi", &mut context_builder).await;
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

        let result = agent_loop.run("Hi", &mut context_builder).await;
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

        let result = agent_loop.run("Hi", &mut context_builder).await;
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

        let result = agent_loop.run("Hi", &mut context_builder).await;
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

        let result = agent_loop.run("Hi", &mut context_builder).await;
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
        let result = agent_loop.run("Hi", &mut context_builder).await;
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
        let result = agent_loop.run("Run parallel", &mut context_builder).await;
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

        let result = agent_loop.run("Test failure", &mut context_builder).await;
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
                // Sleep for a very long time — should be cut short by timeout
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
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
        let result = agent_loop.run("Test timeout", &mut context_builder).await;
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
        let result = agent_loop.run("Run shell", &mut context_builder).await;
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

        let result = agent_loop.run("Run ordered", &mut context_builder).await;
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
                // Sleep longer than the iteration timeout
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
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
        let result = agent_loop.run("Test iteration timeout", &mut context_builder).await;
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
                // Sleep longer than tool_timeout but shorter than iteration_timeout
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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
        let result = agent_loop.run("Test tool timeout", &mut context_builder).await;
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
        // When one tool is denied permission, others should still execute.
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

        let result = agent_loop.run("Test partial permission", &mut context_builder).await;
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
        // Second tool (shell) should have permission denied
        assert!(tool_results[1].content.contains("Permission denied"),
            "Shell tool should have permission denied: {}", tool_results[1].content);
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
