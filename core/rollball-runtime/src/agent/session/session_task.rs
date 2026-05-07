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
use crate::agent::loop_::{AgentLoop, ChunkEvent};
use crate::agent::session_state::SessionState;

/// Messages that can be sent to a SessionTask.
#[derive(Debug, Clone)]
pub enum SessionMessage {
    /// User chat message to process
    ChatMessage {
        content: String,
        message_id: String,
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
/// `SessionMessage`s from its inbound channel. It creates an `AgentLoop`
/// from a cloned `AgentCore` and its own `SessionState`, ensuring
/// full per-session isolation.
pub(crate) struct SessionTask {
    /// Shared agent core template (Arc-cloned for cheap reference counting)
    core: Arc<AgentCore>,
    /// Per-session state (history, budget, loop detector)
    session: SessionState,
    /// Inbound message receiver
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
}

impl SessionTask {
    /// Create a new SessionTask with the given shared core, session state,
    /// message receiver, system prompt, and optional chunk channel.
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
    ) -> Self {
        Self {
            core,
            session,
            inbound_rx,
            system_prompt,
            chunk_tx,
            session_id,
            tool_definitions,
            identity_context,
        }
    }

    /// Run the session task, processing messages until Stop or channel close.
    ///
    /// Creates an `AgentLoop` from the cloned core data and session state,
    /// then processes incoming messages one at a time.
    pub async fn run(mut self) {
        // Clone core data for this session's AgentLoop.
        // Heavy fields (provider, tools) are Arc-cloned (refcount only).
        let core = self.core.clone_for_session(self.chunk_tx.clone());
        let (mut agent_loop, inbound_tx) =
            AgentLoop::from_core_and_session(core, self.session);

        // Build ContextBuilder with complete tool definitions and identity
        // from SessionManagerConfig, instead of building simplified ones from manifest.
        let mut context_builder = ContextBuilder::new(self.system_prompt.clone())
            .with_identity(self.identity_context.clone())
            .with_tools(self.tool_definitions.clone());

        loop {
            let msg = self.inbound_rx.recv().await;
            match msg {
                Some(SessionMessage::ChatMessage { content, message_id }) => {
                    if content.trim().is_empty() {
                        tracing::warn!(
                            session_id = %self.session_id,
                            "SessionTask received empty chat message, ignoring"
                        );
                        continue;
                    }
                    match agent_loop.run(&content, &context_builder).await {
                        Ok(response) => {
                            tracing::info!(
                                session_id = %self.session_id,
                                response_len = response.len(),
                                "SessionTask processed chat message"
                            );
                            if let Some(ref tx) = self.chunk_tx {
                                let event = ChunkEvent::Done {
                                    content: response,
                                    message_id,
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %self.session_id,
                                        "Failed to send Done chunk event"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                session_id = %self.session_id,
                                error = %e,
                                "SessionTask agent loop error"
                            );
                            if let Some(ref tx) = self.chunk_tx {
                                let event = ChunkEvent::Error {
                                    message: format!("Error: {}", e),
                                    message_id,
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %self.session_id,
                                        "Failed to send Error chunk event"
                                    );
                                }
                            }
                        }
                    }
                }
                Some(SessionMessage::ContinueExecution) => {
                    tracing::debug!(
                        session_id = %self.session_id,
                        "SessionTask: ContinueExecution received"
                    );
                    let _ = inbound_tx.send(crate::agent::inbound::InboundMessage::ContinueExecution {
                        reason: "user_requested".to_string(),
                    }).await;
                }
                Some(SessionMessage::ModelSwitch { model }) => {
                    tracing::info!(
                        session_id = %self.session_id,
                        model = %model,
                        "SessionTask: model switch requested"
                    );
                    context_builder.set_override_model(model);
                }
                Some(SessionMessage::UpdateProvider { provider_name, protocol_type, api_key, base_url, model }) => {
                    tracing::info!(
                        session_id = %self.session_id,
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
                Some(SessionMessage::UpdateCapabilities { caps }) => {
                    tracing::info!(
                        session_id = %self.session_id,
                        model = ?caps.name,
                        "SessionTask: updating model capabilities"
                    );
                    agent_loop.update_gateway_model_capabilities(caps);
                }
                Some(SessionMessage::UpdateMaxOutputTokens { limit }) => {
                    tracing::info!(
                        session_id = %self.session_id,
                        limit = %limit,
                        "SessionTask: updating max output tokens limit"
                    );
                    agent_loop.update_max_output_tokens_limit(limit);
                }
                Some(SessionMessage::UpdateWorkspaceContext { context_text }) => {
                    tracing::info!(
                        session_id = %self.session_id,
                        "SessionTask: updating workspace context"
                    );
                    context_builder.set_workspace_context(context_text);
                }
                Some(SessionMessage::UpdateSessionTitle { title }) => {
                    tracing::info!(
                        session_id = %self.session_id,
                        title = %title,
                        "SessionTask: updating session title"
                    );
                    let _ = agent_loop.update_session_title(&title);
                }
                Some(SessionMessage::Interrupt { reason }) => {
                    tracing::info!(
                        session_id = %self.session_id,
                        reason = %reason,
                        "SessionTask: forwarding interrupt signal"
                    );
                    let _ = inbound_tx.send(crate::agent::inbound::InboundMessage::Interrupt { reason }).await;
                }
                Some(SessionMessage::Stop) => {
                    tracing::info!(
                        session_id = %self.session_id,
                        "SessionTask: Stop received, shutting down"
                    );
                    break;
                }
                None => {
                    tracing::info!(
                        session_id = %self.session_id,
                        "SessionTask: inbound channel closed, shutting down"
                    );
                    break;
                }
            }
        }

        // Graceful shutdown: attempt to close session with distillation
        if let Err(e) = agent_loop.close_session_with_distillation().await {
            tracing::warn!(
                session_id = %self.session_id,
                error = %e,
                "SessionTask: failed to close session with distillation (non-fatal)"
            );
        }
    }
}
