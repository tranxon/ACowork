//! Agent boot context — intermediate state passed between startup phases.
//!
//! `AgentBootContext` carries all per-agent resources produced by Phase A
//! and consumed by Phase B, C, and D.  It avoids propagating dozens of
//! local variables through function signatures.

use std::sync::Arc;

use crate::cli::RuntimeResourceCache;
use crate::config::RuntimeConfig;
use acowork_core::protocol::ProtocolType;
use crate::agent::session::SessionManagerConfig;

/// Intermediate context produced by Phase A (per-agent initialization).
///
/// Contains all resources that are shared across sessions and needed by
/// subsequent phases.  Fields are `pub` so Phase B/C/D can consume them
/// without extra accessor indirection.
pub(crate) struct AgentBootContext {
    // Package & manifest
    pub loaded: crate::package::loader::LoadedPackage,

    // Gateway connection (None in standalone mode)
    pub grpc_client: Option<crate::grpc::client::GatewayGrpcClient>,
    pub hello_config: Option<crate::grpc::client::AgentHelloConfig>,

    // LLM provider
    pub provider: Arc<dyn acowork_core::providers::traits::Provider>,
    /// Startup-resolved model (kept for future Phase 1/2/3 use).
    #[allow(dead_code)]
    pub resolved_model: String,
    /// All available models from the startup provider (kept for future Phase 1/2/3 use).
    #[allow(dead_code)]
    pub available_models: Vec<String>,
    pub protocol_type: ProtocolType,
    /// Provider ID resolved at startup (used to detect session mismatch).
    pub gateway_current_provider_id: Option<String>,

    // Embedding provider
    pub emb_provider: Arc<dyn crate::embedding::EmbeddingProvider>,

    // Tools
    pub active_tools: Vec<Arc<dyn acowork_core::tools::traits::Tool>>,
    pub tool_definitions: Vec<serde_json::Value>,
    pub full_tool_specs: Vec<(String, serde_json::Value)>,

    // Skills
    pub skill_registry: crate::skills::parser::SkillRegistry,
    pub system_prompt: String,

    // Shared handles
    pub memory_session: Arc<crate::memory::MemorySessionHandle>,
    pub mcp_notifier: Arc<crate::mcp_notify::McpConfigNotifier>,
    pub workspace_resolver: crate::tools::workspace_resolver::SharedResolver,

    // Context builder (standalone mode only)
    pub context_builder: Option<crate::agent::context::ContextBuilder>,

    // Session manager config fields
    pub identity_context: Option<String>,
    pub chunk_tx: Option<tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>>,
    pub chunk_rx: Option<tokio::sync::mpsc::Receiver<crate::agent::loop_::SessionChunkEvent>>,
    /// Dedicated control-event channel (Stopped, SessionStateChanged, Done,
    /// Error, etc.). Separated from data channel to prevent backpressure
    /// from blocking control events.
    pub control_chunk_tx: Option<tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>>,
    pub control_chunk_rx: Option<tokio::sync::mpsc::Receiver<crate::agent::loop_::SessionChunkEvent>>,

    // Budget
    pub budget: acowork_core::Budget,

    // Resource cache (for session validation)
    pub resource_cache: RuntimeResourceCache,

    // Reconnect params (Gateway mode)
    pub agent_id: String,
    pub version: String,
    pub socket_path: String,
}

/// Context produced by Phase B (per-session initialization).
///
/// Contains session-specific resources needed by Phase C and D.
pub(crate) struct SessionBootContext {
    pub initial_session_id: String,
    pub session_manager: crate::agent::session::SessionManager,
}

/// Build a `SessionManagerConfig` from the boot context.
///
/// Called at the start of Phase B to avoid moving individual fields out of
/// `AgentBootContext` before the context might still be needed.
pub(crate) fn build_session_manager_config(
    ctx: &mut AgentBootContext,
    config: &RuntimeConfig,
) -> SessionManagerConfig {
    SessionManagerConfig {
        inbound_channel_capacity: 64,
        system_prompt: ctx.system_prompt.clone(),
        per_session_budget: ctx.budget.clone(),
        history_max_tokens: config.history_max_tokens,
        chunk_tx: ctx.chunk_tx.clone(),
        control_chunk_tx: ctx.control_chunk_tx.clone(),
        tool_definitions: ctx.tool_definitions.clone(),
        full_tool_specs: ctx.full_tool_specs.clone(),
        identity_context: ctx.identity_context.clone(),
        protocol_type: ctx.protocol_type.clone(),
    }
}
