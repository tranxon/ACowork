//! SessionTask: independent execution actor for a single session.
//!
//! Each `SessionTask` runs in its own tokio task, processing messages
//! from an inbound channel. It owns an `AgentLoop` instance for the
//! session's lifetime, ensuring per-session isolation of history,
//! budget, and loop detection while sharing provider/tools via Arc.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agent::agent_core::AgentCore;
use crate::agent::context::ContextBuilder;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_::{AgentLoop, ChunkEvent};
use crate::agent::session_state::SessionState;
use crate::debug::controller::DebugController;

/// Messages that can be sent to a SessionTask.
#[derive(Debug, Clone)]
pub enum SessionMessage {
    /// User chat message to process
    ChatMessage {
        content: String,
        message_id: String,
        /// Skill instructions to inject into the system prompt (from command-based skill selection).
        /// When set, the instructions are injected via ContextBuilder rather than prepended to user content.
        skill_instructions: Option<String>,
    },
    /// Continue execution after tool result or iteration pause
    ContinueExecution,
    /// Switch the LLM model at runtime
    ModelSwitch { model: String },
    /// Update the LLM provider at runtime (hot-push from Gateway)
    UpdateProvider {
        provider_name: String,
        protocol_type: rollball_core::protocol::ProtocolType,
        api_key: Option<String>,
        base_url: Option<String>,
        model: String,
    },
    /// Update gateway model capabilities at runtime
    UpdateCapabilities {
        caps: rollball_core::protocol::ModelCapabilitiesInfo,
    },
    /// Update max output tokens limit from Gateway config
    UpdateMaxOutputTokens { limit: u64 },
    /// Apply runtime config overrides from Gateway
    UpdateRuntimeConfig {
        max_output_tokens: Option<u64>,
        max_iterations: Option<u32>,
        temperature: Option<f32>,
        system_prompt_override: Option<String>,
    },
    /// Update workspace context text
    UpdateWorkspaceContext { context_text: String },
    /// Update the title of the session's conversation
    UpdateSessionTitle { title: String },
    /// Interrupt signal to stop the current agent loop iteration
    Interrupt { reason: String },
    /// Stop the session gracefully
    Stop,
}

/// Independent execution actor for a single session.
///
/// Each `SessionTask` runs as a separate tokio task, processing
/// `SessionMessage`s from its inbound channel. It owns an `AgentLoop`
/// built from a cloned `AgentCore` plus its own `SessionState`,
/// ensuring full per-session isolation.
pub(crate) struct SessionTask {
    /// The session's AgentLoop, pre-constructed so that external callers
    /// can obtain its `InboundMessage` sender at session-creation time.
    agent_loop: AgentLoop,
    /// Clone of the AgentLoop's inbound sender, kept here purely as a
    /// fallback so that legacy `SessionMessage::ContinueExecution` /
    /// `SessionMessage::Interrupt` messages (if anyone still sends them)
    /// can be forwarded. The primary, deadlock-safe path is via
    /// `SessionHandle::send_inbound`.
    agent_inbound_tx: mpsc::Sender<InboundMessage>,
    /// Inbound message receiver (SessionMessage-level, not InboundMessage)
    inbound_rx: mpsc::Receiver<SessionMessage>,
    /// System prompt for context building
    system_prompt: String,
    /// Optional streaming chunk sender for forwarding responses to Gateway
    chunk_tx: Option<mpsc::Sender<ChunkEvent>>,
    /// Unique session identifier (used for logging and chunk tagging)
    session_id: String,
    /// Complete tool definitions (with input_schema) for ContextBuilder
    tool_definitions: Vec<serde_json::Value>,
    /// Identity context string injected by Gateway
    identity_context: Option<String>,
    /// Model override from Gateway (takes precedence over manifest's suggested_model)
    override_model: Option<String>,
    /// Debug controller (shared across sessions, only in DevMode).
    /// Used to consume rewind_target / pending_patches / re_execute_pending
    /// before each agent_loop.run() invocation.
    debug_ctrl: Option<Arc<tokio::sync::Mutex<DebugController>>>,
}

impl SessionTask {
    /// Create a new SessionTask with the given shared core, session state,
    /// message receiver, system prompt, and optional chunk channel.
    ///
    /// Returns the task together with the `AgentLoop`'s `InboundMessage`
    /// sender. Callers (SessionManager) must stash that sender in
    /// `SessionHandle` so that out-of-band signals (Continue/Interrupt)
    /// can be delivered directly to the AgentLoop without going through
    /// the SessionTask's main loop — which would otherwise deadlock
    /// whenever the AgentLoop is awaiting a pause-resume signal.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        core: Arc<AgentCore>,
        session: SessionState,
        inbound_rx: mpsc::Receiver<SessionMessage>,
        system_prompt: String,
        chunk_tx: Option<mpsc::Sender<ChunkEvent>>,
        session_id: String,
        tool_definitions: Vec<serde_json::Value>,
        identity_context: Option<String>,
        override_model: Option<String>,
    ) -> (Self, mpsc::Sender<InboundMessage>) {
        // Build the AgentLoop eagerly so its inbound sender can be exposed.
        // Heavy fields (provider, tools) are Arc-cloned (refcount only).
        // Extract debug_ctrl from the original core BEFORE clone_for_session —
        // both the AgentLoop (via clone_for_session) and SessionTask hold an
        // Arc to the same DebugController.
        let debug_ctrl = core.debug_ctrl().cloned();
        let core_for_session = core.clone_for_session(chunk_tx.clone());
        let (agent_loop, agent_inbound_tx) =
            AgentLoop::from_core_and_session(core_for_session, session);

        let task = Self {
            agent_loop,
            agent_inbound_tx: agent_inbound_tx.clone(),
            inbound_rx,
            system_prompt,
            chunk_tx,
            session_id,
            tool_definitions,
            identity_context,
            override_model,
            debug_ctrl,
        };
        (task, agent_inbound_tx)
    }

    /// Run the session task, processing messages until Stop or channel close.
    pub async fn run(self) {
        let Self {
            mut agent_loop,
            agent_inbound_tx,
            debug_ctrl,
            session_id,
            chunk_tx,
            mut inbound_rx,
            system_prompt,
            tool_definitions,
            identity_context,
            override_model,
        } = self;

        // Build ContextBuilder with complete tool definitions and identity
        // from SessionManagerConfig, instead of building simplified ones from manifest.
        let mut context_builder = ContextBuilder::new(system_prompt.clone())
            .with_identity(identity_context.clone())
            .with_tools(tool_definitions.clone());

        // Apply Gateway-resolved model override so the first message uses
        // the correct model (not the manifest's suggested_model fallback).
        if let Some(ref model) = override_model {
            context_builder = context_builder.with_override_model(model.clone());
        }

        loop {
            // Use a 500ms timeout so that debug rewind operations can be
            // consumed even when the SessionTask is idle (no new ChatMessage).
            // Without this, rewind_target set via debugger.rewind would sit
            // in the controller forever after agent_loop.run() finishes.
            let msg = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                inbound_rx.recv(),
            )
            .await;

            match msg {
                Ok(Some(SessionMessage::ChatMessage { content, message_id, skill_instructions })) => {
                    if content.trim().is_empty() {
                        tracing::warn!(
                            session_id = %session_id,
                            "SessionTask received empty chat message, ignoring"
                        );
                        continue;
                    }

                    // Apply skill instructions to ContextBuilder (system prompt injection).
                    // This replaces the old behavior of prepending skill text to the user message,
                    // making skill instructions visible in the debug panel's system prompt section.
                    // When skill_instructions is None (no command specified), clear any
                    // previously set skill to prevent stale instructions leaking across turns.
                    if let Some(ref instructions) = skill_instructions {
                        tracing::info!(
                            session_id = %session_id,
                            skill_len = instructions.len(),
                            "Applying skill instructions to ContextBuilder"
                        );
                        context_builder.set_skill_instructions(instructions.clone());
                    } else {
                        context_builder.clear_skill_instructions();
                    }

                    // ── Debug mode: apply rewind/patches before running agent loop ──
                    apply_debug_rewind_and_patches(
                        &debug_ctrl,
                        &session_id,
                        &mut agent_loop,
                        &mut context_builder,
                    )
                    .await;

                    match agent_loop.run(&content, &mut context_builder).await {
                        Ok(response) => {
                            tracing::info!(
                                session_id = %session_id,
                                response_len = response.len(),
                                "SessionTask processed chat message"
                            );
                            if let Some(ref tx) = chunk_tx {
                                let event = ChunkEvent::Done {
                                    content: response,
                                    message_id,
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Failed to send Done chunk event"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                session_id = %session_id,
                                error = %e,
                                "SessionTask agent loop error"
                            );
                            if let Some(ref tx) = chunk_tx {
                                let event = ChunkEvent::Error {
                                    message: format!("Error: {}", e),
                                    message_id,
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Failed to send Error chunk event"
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(Some(SessionMessage::ContinueExecution)) => {
                    tracing::debug!(
                        session_id = %session_id,
                        "SessionTask: ContinueExecution received"
                    );
                    let _ = agent_inbound_tx.send(crate::agent::inbound::InboundMessage::ContinueExecution {
                        reason: "user_requested".to_string(),
                    }).await;
                }
                Ok(Some(SessionMessage::ModelSwitch { model })) => {
                    tracing::info!(
                        session_id = %session_id,
                        model = %model,
                        "SessionTask: model switch requested"
                    );
                    context_builder.set_override_model(model);
                }
                Ok(Some(SessionMessage::UpdateProvider { provider_name, protocol_type, api_key, base_url, model })) => {
                    tracing::info!(
                        session_id = %session_id,
                        provider = %provider_name,
                        model = %model,
                        "SessionTask: updating provider"
                    );
                    let new_provider = crate::providers::router::create_provider(
                        &provider_name,
                        &protocol_type,
                        api_key.as_deref(),
                        base_url.as_deref(),
                    );
                    agent_loop.update_provider(new_provider, model);
                }
                Ok(Some(SessionMessage::UpdateCapabilities { caps })) => {
                    tracing::info!(
                        session_id = %session_id,
                        model = ?caps.name,
                        "SessionTask: updating model capabilities"
                    );
                    agent_loop.update_gateway_model_capabilities(caps);
                }
                Ok(Some(SessionMessage::UpdateMaxOutputTokens { limit })) => {
                    tracing::info!(
                        session_id = %session_id,
                        limit = %limit,
                        "SessionTask: updating max output tokens limit"
                    );
                    agent_loop.update_max_output_tokens_limit(limit);
                }
                Ok(Some(SessionMessage::UpdateRuntimeConfig {
                    max_output_tokens,
                    max_iterations,
                    temperature,
                    system_prompt_override,
                })) => {
                    tracing::info!(
                        session_id = %session_id,
                        max_output_tokens = ?max_output_tokens,
                        max_iterations = ?max_iterations,
                        temperature = ?temperature,
                        "SessionTask: applying runtime config overrides"
                    );
                    agent_loop.apply_runtime_config(
                        max_output_tokens,
                        max_iterations,
                        temperature,
                        system_prompt_override,
                    );
                }
                Ok(Some(SessionMessage::UpdateWorkspaceContext { context_text })) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: updating workspace context"
                    );
                    context_builder.set_workspace_context(context_text);
                }
                Ok(Some(SessionMessage::UpdateSessionTitle { title })) => {
                    tracing::info!(
                        session_id = %session_id,
                        title = %title,
                        "SessionTask: updating session title"
                    );
                    let _ = agent_loop.update_session_title(&title);
                }
                Ok(Some(SessionMessage::Interrupt { reason })) => {
                    tracing::info!(
                        session_id = %session_id,
                        reason = %reason,
                        "SessionTask: forwarding interrupt signal"
                    );
                    let _ = agent_inbound_tx.send(crate::agent::inbound::InboundMessage::Interrupt { reason }).await;
                }
                Ok(Some(SessionMessage::Stop)) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: Stop received, shutting down"
                    );
                    break;
                }
                Ok(None) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: inbound channel closed, shutting down"
                    );
                    break;
                }
                Err(_elapsed) => {
                    // Timeout — check for pending debug rewind operations.
                    // This ensures rewinds are consumed even when the
                    // SessionTask is idle (no new ChatMessage).
                    check_and_apply_debug_rewind(
                        &debug_ctrl,
                        &session_id,
                        &mut agent_loop,
                    )
                    .await;
                }
            }
        }

        // Graceful shutdown: attempt to close session with distillation
        if let Err(e) = agent_loop.close_session_with_distillation().await {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "SessionTask: failed to close session with distillation (non-fatal)"
            );
        }
    }
}

/// Apply any pending debug controller operations (rewind, patches) before
/// the next agent loop invocation.
///
/// This is called before each `agent_loop.run()` when DevMode is active.
/// It consumes `rewind_target` (truncates history) and applies pending
/// `PatchSet` to the `ContextBuilder`. The `re_execute_pending` flag is
/// consumed for logging purposes only — execution proceeds naturally
/// when the agent loop sees `Running` state.
async fn apply_debug_rewind_and_patches(
    debug_ctrl: &Option<Arc<tokio::sync::Mutex<DebugController>>>,
    session_id: &str,
    agent_loop: &mut AgentLoop,
    context_builder: &mut ContextBuilder,
) {
    let Some(debug_ctrl) = debug_ctrl else {
        return; // Production mode, no debug controller
    };

    let mut ctrl = debug_ctrl.lock().await;

    // ── Handle rewind ──
    if let Some(target_iter) = ctrl.take_rewind_target() {
        // Find the message_count from the conversation snapshot at the target iteration
        let msg_count = ctrl
            .conversation_snapshots
            .iter()
            .find(|s| s.iteration == target_iter)
            .map(|s| s.message_count);

        if let Some(count) = msg_count {
            agent_loop.session.history.truncate_to(count);
            tracing::info!(
                session_id = %session_id,
                target_iteration = target_iter,
                messages_trimmed_to = count,
                "Debug rewind: history truncated"
            );
        } else {
            tracing::warn!(
                session_id = %session_id,
                target_iteration = target_iter,
                "Debug rewind: no snapshot found for target iteration, history unchanged"
            );
        }

        // Reset iteration counter to the target so the agent loop
        // resumes from the correct point
        ctrl.iteration = target_iter;
        tracing::info!(
            session_id = %session_id,
            target_iteration = target_iter,
            ctrl_iteration_now = ctrl.iteration,
            "Debug: apply_debug_rewind_and_patches reset ctrl.iteration"
        );
    }

    // ── Apply pending patches (consume them) ──
    if let Some(patches) = ctrl.pending_patches.take() {
        context_builder.apply_patches(&patches);
        tracing::info!(
            session_id = %session_id,
            "Debug: pending patches applied to context builder and consumed"
        );
    }

    // ── Consume re-execute pending flag ──
    if ctrl.take_re_execute_pending() {
        tracing::info!(
            session_id = %session_id,
            "Debug: re-execute flag consumed, agent will process next message with updated context"
        );
    }
}

/// Check for and apply pending debug rewind operations.
///
/// Called periodically (via timeout) from `SessionTask::run()` when the
/// session is idle, so that rewind_target set by `debugger.rewind` is
/// consumed even without a new ChatMessage arriving. This function only
/// handles history truncation — patches and re-execute are handled by
/// `apply_debug_rewind_and_patches` before the next `agent_loop.run()`.
async fn check_and_apply_debug_rewind(
    debug_ctrl: &Option<Arc<tokio::sync::Mutex<DebugController>>>,
    session_id: &str,
    agent_loop: &mut AgentLoop,
) {
    let Some(debug_ctrl) = debug_ctrl else {
        return;
    };

    let mut ctrl = debug_ctrl.lock().await;

    if let Some(target_iter) = ctrl.take_rewind_target() {
        let msg_count = ctrl
            .conversation_snapshots
            .iter()
            .find(|s| s.iteration == target_iter)
            .map(|s| s.message_count);

        if let Some(count) = msg_count {
            agent_loop.session.history.truncate_to(count);
            tracing::info!(
                session_id = %session_id,
                target_iteration = target_iter,
                messages_trimmed_to = count,
                "Debug rewind: history truncated (idle polling)"
            );
        } else {
            tracing::warn!(
                session_id = %session_id,
                target_iteration = target_iter,
                "Debug rewind: no snapshot found for target iteration, history unchanged"
            );
        }

        // Reset iteration counter to the target so the frontend
        // reflects the correct state after the rewind.
        ctrl.iteration = target_iter;
    }
}
