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
use crate::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};

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

    // ADR-033: MQTT client (None when MQTT not available).
    // These are populated during startup and will be consumed when
    // session routing via control_handler is completed (Phase 4).
    #[allow(dead_code)]
    pub mqtt_client: Option<crate::mqtt::RuntimeMqttClient>,
    #[allow(dead_code)]
    pub available_cache: Option<crate::mqtt::SharedAvailableCache>,
    /// Control command receiver (from MQTT control topics)
    pub control_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, Vec<u8>)>>,
    #[allow(dead_code)]
    pub runtime_http_port: Option<u16>,

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

    // Embedding provider (None when Tier-1 ONNX provider construction failed —
    // memory then degrades to text-only search via memory::manager fallback)
    pub emb_provider: Option<Arc<dyn crate::embedding::EmbeddingProvider>>,

    // Tools
    /// ADR-029: builtin tools as `BuiltinToolEntry` (each carries its
    /// `enabled` flag). Phase B reads this directly to seed
    /// [`crate::agent::AgentCore::builtin_tools`] via
    /// [`crate::agent::AgentCore::new`].
    pub active_tools: Vec<crate::agent::agent_core::BuiltinToolEntry>,
    /// Flattened tool spec JSON objects for the LLM (enabled builtin only).
    pub tool_definitions: Vec<serde_json::Value>,
    /// Full builtin specs including disabled ones (keyed by name),
    /// used by `/api/agents/{id}/builtin-tools` GET responses.
    pub full_tool_specs: Vec<(String, serde_json::Value)>,

    // ADR-034 §8 Phase 6 cleanup: legacy standalone-mode gRPC path removed.
    pub system_prompt: String,

    // Shared handles
    pub memory_session: Arc<crate::memory::MemorySessionHandle>,
    pub mcp_notifier: Arc<crate::mcp_notify::McpConfigNotifier>,
    pub workspace_resolver: crate::tools::workspace_resolver::SharedResolver,

    // Context builder (standalone mode only)
    pub context_builder: Option<crate::agent::context::ContextBuilder>,

    // Session manager config fields
    pub identity_context: Option<String>,
    /// ADR-021: Single chunk channel for control events.
    pub chunk_tx: Option<tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>>,
    pub chunk_rx: Option<tokio::sync::mpsc::Receiver<crate::agent::loop_::SessionChunkEvent>>,

    // Budget
    pub budget: acowork_core::Budget,

    // Resource cache (for session validation)
    pub resource_cache: RuntimeResourceCache,

    /// Shared session snapshot map, populated by Phase A and consumed by
    /// Phase B (via SessionManagerConfig). The same `Arc` is also held by
    /// the Runtime HTTP server so session state writes from AgentLoop are
    /// immediately visible to HTTP GET /sessions/{sid}/state.
    pub session_snapshots: SharedSessionSnapshots,

    /// Shared latest session Arc, populated by Phase A and consumed by
    /// Phase B (via SessionManagerConfig). SessionManager writes to it on
    /// every session creation; the HTTP server reads from it for
    /// GET /sessions/latest.
    pub latest_session: SharedLatestSession,

    // Reconnect params (Gateway mode)
    pub agent_id: String,
    // ADR-034 §8 Phase 6 cleanup: gRPC reconnect param removed.
    /// ADR-033: Dispatch receiver for Runtime HTTP → agent loop.
    /// HTTP handlers send (session_id, InboundMessage); gateway loop
    /// forwards to the right session's AgentLoop.
    pub http_dispatch_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, crate::agent::inbound::InboundMessage)>>,

    /// ADR-033: Shared handle to the Grafeo memory store. Cloned into
    /// the Runtime HTTP server at Phase A and populated by Phase B once
    /// [`crate::agent::AgentCore::init_memory_store`] succeeds. Memory
    /// endpoints gracefully report "no store" when this is still `None`.
    pub memory_store_shared: crate::http::SharedMemoryStore,

    /// ADR-033: Shared embedding-provider dimension. Initialised to 0
    /// in Phase A and updated once the embed provider is built. Read
    /// by the memory-stats endpoint as `model_dim` for HNSW
    /// dimension-mismatch detection.
    ///
    /// The HTTP server already holds a clone of the same `Arc` passed
    /// to [`crate::http::RuntimeHttpServer::start`]; this slot in
    /// `AgentBootContext` exists for symmetry with
    /// [`Self::memory_store_shared`] and is reserved for any future
    /// post-Phase-A consumer (e.g. memory diagnostics in the gateway
    /// loop). Phase B does not currently read it.
    #[allow(dead_code)]
    pub embed_dim_shared: crate::http::SharedEmbedDimension,

    /// Startup-phase degradation reasons — non-fatal errors that
    /// degrade runtime capabilities (e.g. session persistence
    /// unavailable due to filesystem sandbox). Read by `/health`.
    /// Populated during Phase B; passed to the HTTP server in
    /// Phase A via the same `Arc`.
    pub degraded_reasons: crate::http::SharedDegradation,
}

/// Context produced by Phase B (per-session initialization).
///
/// Contains session-specific resources needed by Phase C and D.
pub(crate) struct SessionBootContext {
    // ADR-034 §8 Phase 6 cleanup: legacy standalone-mode gRPC path removed.
    pub session_manager: crate::agent::session::SessionManager,
    /// ADR-022: Shared committed-lines counter for the initial session.
    /// The writer thread increments this after each disk write.
    /// `AgentCore` adopts this Arc so `read_messages_since` sees the
    /// true on-disk line count.
    pub committed_lines: Arc<std::sync::atomic::AtomicUsize>,
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
        tool_definitions: ctx.tool_definitions.clone(),
        full_tool_specs: ctx.full_tool_specs.clone(),
        identity_context: ctx.identity_context.clone(),
        protocol_type: ctx.protocol_type.clone(),
        session_snapshots: Some(ctx.session_snapshots.clone()),
        latest_session: Some(ctx.latest_session.clone()),
    }
}
