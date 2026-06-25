//! Cross-session shared state for Agent Runtime.
//!
//! `AgentCore` holds all resources that are shared across sessions:
//! runtime config, manifest, LLM provider, tool registry, streaming channel,
//! Gateway model capabilities, and Grafeo memory store. These resources
//! persist for the lifetime of the agent process and are independent of
//! any individual session.
//!
//! Phase 1: direct ownership inside AgentLoop.
//! Phase 2: wrapped in Arc for multi-session Actor sharing.

use std::collections::HashMap;
use std::sync::Arc;

use acowork_core::protocol::{ModelCapabilitiesInfo, ProviderListItem};
use acowork_core::providers::traits::Provider;
use acowork_core::tools::traits::Tool;
use acowork_grafeo::consolidation::ConsolidationScheduler;
use acowork_grafeo::grafeo::GrafeoStore;
use acowork_grafeo::retrieval_metrics::MetricsAggregator;
use acowork_grafeo::types::GrafeoConfig;
use acowork_grafeo::types::{AutobioCategory, AutobiographicalNode, NodeStatus};
use chrono::Utc;
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};
use crate::agent::loop_approval::ApprovalHandle;
use crate::config::RuntimeConfig;
use crate::debug::DebugObserverSlot;
use crate::embedding::EmbeddingProvider;
use crate::memory::ConsolidationBgTask;
use crate::memory::{MemoryManager, MemoryManagerConfig};
use crate::security::approval_gate::ApprovalGate;
use acowork_core::ShellApprovalThreshold;

/// Cross-session shared state for the agent loop.
///
/// Fields here are immutable or rarely mutated at runtime (e.g. provider swap
/// via model_switch), and are shared across all sessions of the same agent.
pub struct AgentCore {
    /// Runtime configuration
    pub(crate) config: RuntimeConfig,
    /// Agent manifest (declarative .agent package metadata)
    pub(crate) manifest: acowork_core::AgentManifest,
    /// LLM Provider
    pub(crate) provider: Arc<dyn Provider>,
    /// Tool registry — built-in tools only (used as base for rebuilding).
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    /// MCP (Model Context Protocol) tool wrappers, populated when MCP servers
    /// have been connected. These are merged into [`all_tools`] at rebuild time.
    pub(crate) mcp_tools: Option<Vec<Arc<dyn Tool>>>,
    /// Merged tool list for dispatch — always contains built-in + MCP tools.
    /// Rebuilt once when MCP tools change, instead of per-dispatch merge.
    pub(crate) all_tools: Vec<Arc<dyn Tool>>,
    /// Global provider list — full metadata including models, capabilities,
    /// base_url, protocol_type, compact_model for all configured providers.
    /// Populated at AgentHello, updated by ProviderListUpdate pushes.
    /// Wrapped in Arc<RwLock> for cross-session shared read access.
    pub(crate) global_provider_list: Arc<std::sync::RwLock<Vec<ProviderListItem>>>,
    /// Provider list version for diff sync with Gateway.
    pub(crate) provider_list_version: u64,
    /// Provider key vault (in-memory only, never persisted).
    /// Keyed by provider_id → api_key for O(1) lookup.
    /// Wrapped in Arc<RwLock> for cross-session shared read access.
    pub(crate) provider_key_vault: Arc<std::sync::RwLock<HashMap<String, String>>>,
    /// Provider→compact_model mapping from provider_list at AgentHello.
    /// Keyed by Vault provider ID.  Static after init — provider changes
    /// (add/remove model, compact_model) require agent restart.
    pub(crate) provider_compact_models: HashMap<String, Option<String>>,
    /// LLM temperature override (from Gateway config).
    /// None = use model/provider default.
    pub(crate) temperature_override: Option<f32>,
    /// System prompt override (from Gateway config).
    /// None = use manifest-compiled system prompt.
    pub(crate) system_prompt_override: Option<String>,
    /// Session ID of the owning session (set by SessionTask at creation).
    /// Used to annotate all ChunkEvents with their origin session, eliminating
    /// the need for external relay-side injection (which had a race condition).
    pub(crate) session_id: Option<String>,
    /// Optional streaming chunk sender (like ZeroClaw's on_delta).
    /// When set, each StreamEvent::Content delta is forwarded here
    /// so the caller can relay chunks to Gateway via StreamChunk.
    pub(crate) on_chunk: Option<mpsc::Sender<crate::agent::loop_::SessionChunkEvent>>,
    /// Dedicated control-event channel for high-priority events that MUST
    /// reach the frontend (Stopped, SessionStateChanged, Done, Error,
    /// ToolApprovalNeeded, AskQuestion, IterationLimitPaused).
    ///
    /// This channel is consumed with priority over `on_chunk` by the chunk
    /// relay task, ensuring control events are never blocked by data-event
    /// backpressure (e.g. a burst of ReasoningDelta chunks filling the
    /// data channel). None in standalone mode.
    pub(crate) control_chunk: Option<mpsc::Sender<crate::agent::loop_::SessionChunkEvent>>,
    /// Grafeo memory store (shared across all sessions of this agent).
    /// Opened at agent startup from `{work_dir}/memory/private.grafeo`.
    /// None if initialization failed (memory features degraded gracefully).
    pub(crate) memory_store: Option<Arc<GrafeoStore>>,
    /// Debug observer slot — Production (no-op) or Dev (real observer).
    ///
    /// Consolidates the previous 6 `Option<T>` debug fields (debug_ctrl,
    /// pending_debug_handles, debug_rewind_notify, debug_resume_notify,
    /// debug_event_tx) into a single pluggable observer. See ADR-013.
    pub(crate) debug_observer: DebugObserverSlot,
    /// Urgent stop notify — fired by Gateway gRPC (Stop / Restart-in-Debug)
    /// to cancel tool execution immediately without waiting for 500ms poll.
    /// Each session gets its own independent Notify; fire_urgent_stop() only
    /// wakes the target session's tokio::select! branches.
    pub(crate) urgent_stop: Option<Arc<Notify>>,
    /// Approval gate for shell command risk confirmation.
    /// None in standalone/CLI mode (uses CliApprovalGate).
    /// Some(Arc<dyn ApprovalGate>) in CLI mode with non-default gate.
    /// Note: In Gateway mode, `approval_handle` is used instead
    /// (unified pause architecture via AgentLoop).
    pub(crate) approval_gate: Option<Arc<dyn ApprovalGate>>,
    /// Approval handle for shell command risk confirmation (Gateway mode).
    /// When set, spawned tool tasks use this handle to route approval
    /// requests through the AgentLoop main loop (unified pause architecture).
    /// None in CLI mode (uses `approval_gate` directly).
    pub(crate) approval_handle: Option<ApprovalHandle>,
    /// Shell approval threshold: Low / Medium / High / Never.
    /// Default: "medium" — Medium and High risk commands need approval.
    pub(crate) shell_approval_threshold: ShellApprovalThreshold,
    /// Watch sender for session status (ADR-014).
    /// The AgentLoop writes to this via `transition_status()`;
    /// the SessionHandle holds the Receiver for non-blocking reads.
    /// None for CLI-only sessions (no SessionHandle).
    pub(crate) status_tx:
        Option<tokio::sync::watch::Sender<crate::agent::session_state::SessionStatus>>,
    /// Shared session state snapshot for the Gateway pull API.
    ///
    /// Written by `AgentLoop::emit_session_state` on every status transition.
    /// Read by `SessionManager::snapshot_session_state` to serve
    /// `GET /api/agents/{id}/sessions/{session_id}/state` without
    /// sending a new gRPC message to the Runtime.
    /// None for CLI-only sessions.
    pub(crate) snapshot_slot:
        Option<Arc<std::sync::RwLock<Option<crate::agent::session_state::SessionStateSnapshot>>>>,
    /// Memory session handle — shared between agent loop and memory tools.
    /// Created at tool registry time, store initialized lazily.
    pub(crate) memory_session: Option<Arc<crate::memory::MemorySessionHandle>>,
    /// Embedding provider for vector-based memory retrieval.
    /// Built from LLM provider registry; shared across all sessions.
    /// Used by [`init_memory_store`] to determine Grafeo vector dimension.
    pub(crate) embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// P3-1: Retrieval quality metrics aggregator (shared across sessions).
    /// Tracks NRR, abstention rate, degradation, conflict accuracy, and
    /// LLM Judge scores. Wrapped in Arc<Mutex> so it can be shared with
    /// background tokio::spawn tasks (e.g., LLM Judge evaluation).
    pub(crate) metrics_aggregator: Arc<std::sync::Mutex<MetricsAggregator>>,
    /// P3: Consolidation scheduler — decides when to run offline consolidation.
    /// Created after memory store initialization, shared across all sessions.
    /// None if memory store is not initialized.
    pub(crate) consolidation_scheduler: Option<Arc<ConsolidationScheduler>>,
    /// P3: Background consolidation task handle.
    /// Dropping this cancels the background task.
    /// None if memory store is not initialized or embedding provider is unavailable.
    pub(crate) consolidation_bg_task: Option<ConsolidationBgTask>,
    /// Current workspace directory for tool execution.
    /// Set by SessionTask before run(), updated via SetWorkDir message.
    /// Filesystem tools use this as the base directory for relative path resolution.
    pub(crate) current_work_dir: Option<String>,
    /// Shared session status for 429 retry UX.
    ///
    /// Written by both [`AgentLoop::transition_status`] (normal flow) and
    /// [`crate::providers::reliable::ReliableProvider`] (retry pause/resume).
    /// A clone is passed to the ReliableProvider so it can emit
    /// `SessionStateChanged` events during long retry waits.
    pub(crate) retry_session_status:
        Option<Arc<std::sync::RwLock<crate::agent::session_state::SessionStatus>>>,
    /// Active retry-wait handle for 429 UX.
    ///
    /// Set when the ReliableProvider is wired up with UX support.
    /// [`crate::agent::session::SessionTask`] checks this when handling
    /// `ContinueExecution` to trigger `skip_notify` and wake the retry loop.
    pub(crate) retry_wait_handle:
        Option<crate::providers::reliable::RetryWaitHandle>,
}

impl AgentCore {
    /// Create a new AgentCore with the given shared resources and a
    /// pre-configured debug observer.
    ///
    /// This constructor supports integration testing and advanced embedding
    /// scenarios where the caller needs to control the observer lifecycle.
    /// For normal usage, prefer [`AgentCore::new()`] which defaults to
    /// Production mode (zero-cost no-ops). See ADR-013.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_observer(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        on_chunk: Option<mpsc::Sender<SessionChunkEvent>>,
        observer: crate::debug::DebugObserverSlot,
    ) -> Self {
        let initial_work_dir = config.work_dir.clone();
        let shell_approval_threshold =
            ShellApprovalThreshold::from_str_loose(&config.shell_approval_threshold)
                .unwrap_or_default();
        Self {
            config,
            manifest,
            provider,
            tools: tools.clone(),
            mcp_tools: None,
            all_tools: tools,
            global_provider_list: Arc::new(std::sync::RwLock::new(Vec::new())),
            provider_list_version: 0,
            provider_key_vault: Arc::new(std::sync::RwLock::new(HashMap::new())),
            provider_compact_models: HashMap::new(),
            temperature_override: None,
            system_prompt_override: None,
            session_id: None,
            on_chunk,
            control_chunk: None,
            memory_store: None,
            memory_session: None,
            debug_observer: observer,
            urgent_stop: Some(Arc::new(Notify::new())),
            approval_gate: None,
            approval_handle: None,
            shell_approval_threshold,
            status_tx: None,
            snapshot_slot: None,
            embedding_provider: None,
            metrics_aggregator: Arc::new(std::sync::Mutex::new(MetricsAggregator::with_defaults(
                1.0,
            ))),
            consolidation_scheduler: None,
            consolidation_bg_task: None,
            current_work_dir: Some(initial_work_dir),
            retry_session_status: None,
            retry_wait_handle: None,
        }
    }

    /// Create a new AgentCore with the given shared resources.
    ///
    /// Defaults to Production mode (zero-cost debug no-ops).
    /// Use [`AgentCore::new_with_observer()`] to inject a DevMode observer.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        on_chunk: Option<mpsc::Sender<SessionChunkEvent>>,
    ) -> Self {
        Self::new_with_observer(
            config,
            manifest,
            provider,
            tools,
            on_chunk,
            DebugObserverSlot::production(),
        )
    }

    /// Rebuild the merged `all_tools` list from built-in `tools` + `mcp_tools`.
    ///
    /// Call this after MCP tools change (connect/disconnect) so that
    /// `all_tools` is always up-to-date for dispatch without per-call merging.
    pub(crate) fn rebuild_all_tools(&mut self) {
        let mut merged = self.tools.clone();
        if let Some(ref mcp) = self.mcp_tools {
            merged.extend(mcp.clone());
        }
        self.all_tools = merged;
    }

    /// Access the runtime configuration.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Access the agent manifest.
    pub fn manifest(&self) -> &acowork_core::AgentManifest {
        &self.manifest
    }

    /// Access the LLM provider.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    /// Access the tool registry.
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Access Gateway model capabilities.
    /// Deprecated: use get_model_capabilities(model_id) instead.
    pub fn gateway_model_capabilities(&self) -> HashMap<String, ModelCapabilitiesInfo> {
        // Build a HashMap view from global_provider_list for backward compat.
        let list = self.global_provider_list.read().unwrap();
        let mut map = HashMap::new();
        for provider in list.iter() {
            for model in &provider.models {
                map.insert(model.id.clone(), model.capabilities.clone());
            }
        }
        map
    }

    /// Get the max output tokens limit for a given model from its provider entry.
    /// Falls back to the system default (32768) if not found.
    pub fn max_output_tokens_limit_for_model(&self, model_id: &str) -> u64 {
        let list = self.global_provider_list.read().unwrap();
        for provider in list.iter() {
            for model in &provider.models {
                if model.id == model_id {
                    return model.max_output_tokens_limit;
                }
            }
        }
        32_768
    }

    /// Access the streaming chunk sender.
    pub fn on_chunk(&self) -> Option<&mpsc::Sender<SessionChunkEvent>> {
        self.on_chunk.as_ref()
    }

    /// Wrap a ChunkEvent into a SessionChunkEvent using this core's session_id.
    ///
    /// Returns None if session_id is not set (should not happen in Gateway mode).
    /// This is the single point where session_id is attached to events, replacing
    /// the old watch-channel relay injection that had a race condition.
    pub fn make_chunk_event(&self, event: ChunkEvent) -> Option<SessionChunkEvent> {
        self.session_id.as_ref().map(|sid| SessionChunkEvent {
            session_id: sid.clone(),
            event,
        })
    }

    /// Try-send a ChunkEvent via the on_chunk channel, wrapped with session_id.
    ///
    /// Convenience method used by AgentLoop emit sites. Returns true if sent,
    /// false if channel full/closed or session_id missing.
    pub fn try_send_chunk(&self, event: ChunkEvent) -> bool {
        // Auto-route: control events go to the dedicated control channel
        // (never blocked by data backpressure); data events use the regular
        // on_chunk channel. Falls back to on_chunk when control_chunk is None
        // (standalone mode).
        let is_control = event.is_control();
        if let Some(wrapped) = self.make_chunk_event(event) {
            if is_control
                && let Some(tx) = self.control_chunk.as_ref()
            {
                return tx.try_send(wrapped).is_ok();
            }
            // Fallback: standalone mode has no control channel, or data event
            self.on_chunk
                .as_ref()
                .map(|tx| tx.try_send(wrapped).is_ok())
                .unwrap_or(false)
        } else {
            tracing::debug!("Cannot send chunk event: session_id not set on AgentCore");
            false
        }
    }

    /// Update the LLM provider at runtime (e.g., after receiving a
    /// model_switch from Gateway).
    pub fn update_provider(&mut self, new_provider: Arc<dyn Provider>, model: String) {
        let old_name = self.provider.name().to_string();
        self.provider = new_provider;
        tracing::info!(
            old_provider = %old_name,
            new_provider = %self.provider.name(),
            model = %model,
            "LLM provider updated at runtime (model_switch)"
        );
    }

    /// Update the embedding provider at runtime (hot-push from Gateway
    /// EmbeddingConfigUpdate). Replaces the current provider with a
    /// new ONNX provider as the first entry in the FallbackEmbeddingProvider chain.
    pub fn update_embedding_provider(
        &mut self,
        new_provider: Arc<dyn crate::embedding::EmbeddingProvider>,
    ) {
        let old_name = self
            .embedding_provider
            .as_ref()
            .map(|p| p.name())
            .unwrap_or("none")
            .to_string(); // Detach from borrow before assigning
        let new_name = new_provider.name().to_string(); // Read before move
        self.embedding_provider = Some(new_provider);
        tracing::info!(
            old_provider = %old_name,
            new_provider = %new_name,
            "Embedding provider updated at runtime via EmbeddingConfigUpdate"
        );
    }

    /// Update gateway model capabilities at runtime.
    /// Now inserts into the global_provider_list for the matching model.
    /// `model_id` is the model identifier string.
    pub fn update_gateway_model_capabilities(
        &mut self,
        model_id: &str,
        caps: ModelCapabilitiesInfo,
    ) {
        tracing::info!(
            model = %model_id,
            context_window = caps.context_window,
            max_output_tokens = caps.max_output_tokens,
            supports_tool_calling = caps.supports_tool_calling,
            supports_reasoning = ?caps.supports_reasoning,
            cost = ?caps.cost.as_ref().map(|c| (c.input_per_million, c.output_per_million)),
            caps_name = ?caps.name,
            source = "gateway",
            "AgentCore received model capabilities from Gateway"
        );
        // Update capabilities in global_provider_list for matching model.
        let mut list = self.global_provider_list.write().unwrap();
        for provider in list.iter_mut() {
            for model in provider.models.iter_mut() {
                if model.id == model_id {
                    model.capabilities = caps;
                    return;
                }
            }
        }
    }

    /// Update the max output tokens limit from Gateway config.
    /// Now updates per-model limits in the global_provider_list.
    pub fn update_max_output_tokens_limit(&mut self, limit: u64) {
        tracing::info!(
            new_limit = limit,
            "AgentCore max_output_tokens_limit updated from Gateway (all models)"
        );
        let mut list = self.global_provider_list.write().unwrap();
        for provider in list.iter_mut() {
            for model in provider.models.iter_mut() {
                model.max_output_tokens_limit = limit;
            }
        }
    }

    /// Apply runtime config overrides from Gateway.
    /// Only updates fields that are Some — None means "keep current value".
    pub fn apply_runtime_config(
        &mut self,
        max_output_tokens: Option<u64>,
        max_iterations: Option<u32>,
        temperature: Option<f32>,
        system_prompt_override: Option<String>,
        shell_approval_threshold: Option<String>,
    ) {
        if let Some(limit) = max_output_tokens {
            tracing::info!(
                new = limit,
                "runtime config: max_output_tokens updated (all models)"
            );
            self.update_max_output_tokens_limit(limit);
        }
        if let Some(n) = max_iterations {
            tracing::info!(
                old = self.config.max_iterations,
                new = n,
                "runtime config: max_iterations updated"
            );
            self.config.max_iterations = n;
        }
        if let Some(temp) = temperature {
            tracing::info!(old = ?self.temperature_override, new = temp, "runtime config: temperature updated");
            self.temperature_override = Some(temp);
        }
        if system_prompt_override.is_some() {
            tracing::info!(
                has_override = system_prompt_override
                    .as_ref()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false),
                "runtime config: system_prompt_override updated"
            );
            self.system_prompt_override = system_prompt_override;
        }
        if let Some(ref threshold) = shell_approval_threshold {
            let new_threshold =
                ShellApprovalThreshold::from_str_loose(threshold).unwrap_or_default();
            tracing::info!(
                old = ?self.shell_approval_threshold,
                new = ?new_threshold,
                "runtime config: shell_approval_threshold updated"
            );
            self.shell_approval_threshold = new_threshold;
        }
    }

    /// Initialize the Grafeo memory store at the given workspace path.
    ///
    /// Opens or creates `{work_dir}/memory/private.grafeo`.
    /// On failure, logs a warning and leaves `memory_store` as None —
    /// memory features degrade gracefully (no crash, no panic).
    pub fn init_memory_store(&mut self, work_dir: &std::path::Path) {
        // Guard against double-init (called from both gRPC and standalone paths).
        if self.memory_store.is_some() {
            tracing::debug!("init_memory_store: already initialized, skipping");
            return;
        }

        let memory_dir = work_dir.join("memory");
        if let Err(e) = std::fs::create_dir_all(&memory_dir) {
            tracing::warn!(
                error = %e,
                dir = %memory_dir.display(),
                "Failed to create memory directory, memory features disabled"
            );
            return;
        }

        let db_path = memory_dir.join("private.grafeo");
        let embedding_dim = self
            .embedding_provider
            .as_ref()
            .map(|p| p.dimension())
            .unwrap_or(acowork_grafeo::types::DEFAULT_EMBEDDING_DIM);
        let config = GrafeoConfig {
            db_path: db_path.clone(),
            embedding_dim,
        };
        match GrafeoStore::open(&config) {
            Ok(store) => {
                // Count existing nodes to confirm data loaded from disk.
                let graph = store.db().graph_store();
                let existing: usize = ["Episodic", "Knowledge", "Procedural", "Autobiographical"]
                    .iter()
                    .map(|l| graph.nodes_by_label(l).len())
                    .sum();
                tracing::info!(
                    path = %db_path.display(),
                    existing_nodes = existing,
                    "Grafeo memory store opened"
                );
                let store_arc = Arc::new(store);
                // Bootstrap Autobiographical nodes from manifest on cold start.
                self.bootstrap_autobiographical_from_manifest(&store_arc);
                // Propagate to memory session handle so tools can use it.
                if let Some(ref session) = self.memory_session {
                    session.set_store(store_arc.clone());
                }
                self.memory_store = Some(store_arc);

                // Start consolidation background pipeline if embedding provider is available.
                self.start_consolidation_pipeline();
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %db_path.display(),
                    "Failed to open Grafeo memory store, memory features disabled"
                );
            }
        }
    }

    /// Access the Grafeo memory store, if initialized.
    pub fn memory_store(&self) -> Option<&Arc<GrafeoStore>> {
        self.memory_store.as_ref()
    }

    /// Bootstrap Autobiographical nodes from the agent manifest.
    ///
    /// On first run (cold start), derives Identity/Capability nodes from
    /// [`AgentManifest`] fields and writes them to Grafeo. The bootstrap is
    /// **idempotent**: if any Autobiographical/Identity nodes already exist,
    /// the entire bootstrap is skipped.
    ///
    /// This ensures the agent has searchable self-knowledge from the moment
    /// Grafeo is initialized, without waiting for LLM-triggered
    /// `memory_store` calls.
    ///
    /// ## Mapping
    ///
    /// | Manifest field           | → Node                              |
    /// |--------------------------|-------------------------------------|
    /// | `agent_id`               | `Identity: agent_id: ...`           |
    /// | `name`                   | `Identity: name: ...`               |
    /// | `display_name`           | `Identity: display_name: ...`       |
    /// | `role`                   | `Identity: role: ...`               |
    /// | `description`            | `Identity: description: ...`        |
    /// | `capabilities.*.description` | `Capability: {key}: {desc}`     |
    fn bootstrap_autobiographical_from_manifest(&self, store: &GrafeoStore) {
        // Idempotency: skip if any Identity nodes already exist.
        match store.find_autobiographical_by_category(AutobioCategory::Identity) {
            Ok(existing) if !existing.is_empty() => {
                tracing::debug!(
                    count = existing.len(),
                    "Autobiographical nodes already exist, skipping manifest bootstrap"
                );
                return;
            }
            Err(e) => {
                // Non-fatal: graph may not have index yet on first access.
                tracing::warn!(error = %e, "Failed to probe existing Autobiographical nodes, attempting bootstrap anyway");
            }
            _ => {}
        }

        let manifest = &self.manifest;
        let now = Utc::now();
        let mut bootstrapped = 0u32;

        // ── Identity nodes ──
        let identity_entries: Vec<(&str, String)> = {
            let mut v = vec![
                ("agent_id", manifest.agent_id.clone()),
                ("name", manifest.name.clone()),
                ("description", manifest.description.clone()),
            ];
            if let Some(ref dn) = manifest.display_name {
                v.push(("display_name", dn.clone()));
            }
            if let Some(ref role) = manifest.role {
                v.push(("role", role.clone()));
            }
            v
        };

        for (key, value) in &identity_entries {
            let node = AutobiographicalNode {
                id: None,
                category: AutobioCategory::Identity,
                key: key.to_string(),
                value: value.clone(),
                confidence: 1.0,
                source_episode_id: None,
                embedding: None,
                status: NodeStatus::Active,
                created_at: now,
                updated_at: now,
                metadata: HashMap::new(),
            };
            match store.store_autobiographical(&node) {
                Ok(_) => bootstrapped += 1,
                Err(e) => {
                    tracing::warn!(key = %key, error = %e, "Failed to bootstrap Autobiographical/Identity node")
                }
            }
        }

        // ── Capability nodes ──
        for (cap_key, cap_def) in &manifest.capabilities {
            let node = AutobiographicalNode {
                id: None,
                category: AutobioCategory::Capability,
                key: cap_key.clone(),
                value: cap_def.description.clone(),
                confidence: 1.0,
                source_episode_id: None,
                embedding: None,
                status: NodeStatus::Active,
                created_at: now,
                updated_at: now,
                metadata: HashMap::new(),
            };
            match store.store_autobiographical(&node) {
                Ok(_) => bootstrapped += 1,
                Err(e) => {
                    tracing::warn!(capability = %cap_key, error = %e, "Failed to bootstrap Autobiographical/Capability node")
                }
            }
        }

        tracing::info!(
            identity_count = identity_entries.len(),
            capability_count = manifest.capabilities.len(),
            bootstrapped,
            "Bootstrapped Autobiographical nodes from manifest"
        );
    }

    /// Initialize and return a MemoryManager for this agent.
    ///
    /// The MemoryManager is a stateless orchestrator that operates on the
    /// shared GrafeoStore. It does not own any state — it's just the
    /// retrieve/inject/record pipeline configuration.
    pub fn init_memory_manager(&self) -> MemoryManager {
        MemoryManager::new(MemoryManagerConfig::default())
    }

    /// Start the consolidation background pipeline.
    ///
    /// Called automatically after `init_memory_store()` succeeds and
    /// an embedding provider is available. Creates the
    /// ConsolidationScheduler and spawns a background tokio task
    /// that polls for consolidation triggers.
    ///
    /// If the embedding provider is not set, consolidation is deferred
    /// until it becomes available (call this method again after setting it).
    pub fn start_consolidation_pipeline(&mut self) {
        let Some(ref store) = self.memory_store else {
            tracing::debug!("Cannot start consolidation: memory store not initialized");
            return;
        };
        let Some(ref embedding) = self.embedding_provider else {
            tracing::debug!("Cannot start consolidation: embedding provider not available");
            return;
        };

        // Don't restart if already running.
        if self.consolidation_scheduler.is_some() {
            tracing::debug!("Consolidation pipeline already running");
            return;
        }

        use crate::memory::consolidation_bg::{ConsolidationParams, start_consolidation_pipeline};
        use acowork_grafeo::consolidation::SchedulerConfig;
        use std::time::Duration;

        // Resolve the model name for the LLM adapter.
        // Try global_provider_list first model, then fall back to "default".
        let model = {
            let list = self.global_provider_list.read().unwrap();
            list.iter()
                .flat_map(|p| p.models.iter())
                .next()
                .map(|m| m.id.clone())
                .unwrap_or_else(|| "default".to_string())
        };
        let params = ConsolidationParams {
            store: store.clone(),
            provider: self.provider.clone(),
            model,
            embedding_provider: embedding.clone(),
            scheduler_config: SchedulerConfig::default(),
            poll_interval: Duration::from_secs(60),
            work_dir: Some(std::path::PathBuf::from(&self.config.work_dir)),
        };

        let (scheduler, bg_task) = start_consolidation_pipeline(params);
        self.consolidation_scheduler = Some(scheduler);
        self.consolidation_bg_task = Some(bg_task);

        tracing::info!("Consolidation background pipeline started");
    }

    /// Notify the consolidation scheduler that the agent is active.
    ///
    /// Should be called after each user message is processed, to reset
    /// the idle timer so consolidation doesn't run during active use.
    pub async fn notify_consolidation_active(&self) {
        if let Some(ref scheduler) = self.consolidation_scheduler {
            scheduler.notify_active().await;
        }
    }

    /// Create a cheap clone of this AgentCore for a new session.
    ///
    /// Heavy fields (provider, tools, memory_store) are Arc-cloned (refcount increment),
    /// while value fields (config, manifest, capabilities) are deep-cloned.
    /// The `on_chunk` channel and `session_id` are replaced with the caller-provided ones,
    /// since each session needs its own streaming channel and identity.
    pub(crate) fn clone_for_session(
        &self,
        on_chunk: Option<mpsc::Sender<SessionChunkEvent>>,
        control_chunk: Option<mpsc::Sender<SessionChunkEvent>>,
        session_id: String,
    ) -> Self {
        Self {
            config: self.config.clone(),
            manifest: self.manifest.clone(),
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            mcp_tools: self.mcp_tools.clone(),
            all_tools: self.all_tools.clone(),
            global_provider_list: self.global_provider_list.clone(),
            provider_list_version: self.provider_list_version,
            provider_key_vault: self.provider_key_vault.clone(),
            provider_compact_models: self.provider_compact_models.clone(),
            temperature_override: self.temperature_override,
            system_prompt_override: self.system_prompt_override.clone(),
            session_id: Some(session_id),
            on_chunk,
            control_chunk,
            memory_store: self.memory_store.clone(),
            memory_session: self.memory_session.clone(),
            // Debug observer is NOT cloned — each session gets a fresh
            // Production slot; DevMode is injected via SessionManager.
            debug_observer: DebugObserverSlot::production(),
            // Per-session Notify — each session gets its own independent
            // Notify so fire_urgent_stop() only wakes the target session.
            urgent_stop: Some(Arc::new(Notify::new())),
            approval_gate: self.approval_gate.clone(),
            approval_handle: self.approval_handle.clone(),
            shell_approval_threshold: self.shell_approval_threshold,
            status_tx: None, // set separately by SessionTask
            snapshot_slot: None, // set separately by SessionTask
            embedding_provider: self.embedding_provider.clone(),
            // P3-1: Metrics aggregator is shared across sessions via Arc clone.
            // This ensures LLM Judge evaluations from background tasks are
            // reflected across all session views.
            metrics_aggregator: self.metrics_aggregator.clone(),
            // Consolidation scheduler is shared across sessions (Arc clone).
            consolidation_scheduler: self.consolidation_scheduler.clone(),
            // Background task is NOT cloned — it's owned by the primary AgentCore.
            // Session clones don't need their own bg task.
            consolidation_bg_task: None,
            // work_dir is set separately by SessionTask after clone;
            // default to agent_home to avoid None window before SetWorkDir arrives.
            current_work_dir: Some(self.config.work_dir.clone()),
            retry_session_status: None, // set separately by SessionTask
            retry_wait_handle: None,    // set separately by SessionTask
        }
    }

    /// Look up model capabilities by exact model name.
    ///
    /// Searches across all providers in the global_provider_list.
    /// Returns `None` when the requested model is not found — callers must
    /// handle this case explicitly.
    pub(crate) fn get_model_capabilities(&self, model_name: &str) -> Option<ModelCapabilitiesInfo> {
        let list = self.global_provider_list.read().unwrap();
        for provider in list.iter() {
            for model in &provider.models {
                if model.id == model_name {
                    return Some(model.capabilities.clone());
                }
            }
        }
        if !list.is_empty() {
            let available: Vec<&str> = list
                .iter()
                .flat_map(|p| p.models.iter().map(|m| m.id.as_str()))
                .collect();
            tracing::warn!(
                model = %model_name,
                available = ?available,
                "Model capabilities not found for '{}' — \
                 context usage reporting and compaction will be skipped. \
                 This indicates a model name mismatch between Runtime and Gateway (e.g. case sensitivity).",
                model_name
            );
        }
        None
    }

    /// Look up a provider's full metadata from the global cache.
    pub fn get_provider(&self, provider_id: &str) -> Option<ProviderListItem> {
        let list = self.global_provider_list.read().unwrap();
        list.iter().find(|p| p.id == provider_id).cloned()
    }

    /// Look up a provider's API key from the in-memory vault.
    pub fn get_provider_api_key(&self, provider_id: &str) -> Option<String> {
        let vault = self.provider_key_vault.read().unwrap();
        vault.get(provider_id).cloned()
    }

    /// Rebuild Provider instance for a given provider_id from global cache.
    /// Returns None if provider not found in cache or no API key available.
    ///
    /// The returned provider is wrapped in [`ReliableProvider`] so that
    /// per-session model switches also benefit from retry logic (backoff,
    /// retry-after, jitter). When [`retry_session_status`] and
    /// [`retry_wait_handle`] are set on this `AgentCore`, the provider is
    /// also wired up with 429-retry UX (countdown timer + skip button).
    pub fn build_provider_for(&self, provider_id: &str) -> Option<Arc<dyn Provider>> {
        let provider_meta = self.get_provider(provider_id)?;
        let api_key = self.get_provider_api_key(provider_id);
        let timeouts = Some(crate::providers::router::ProviderTimeouts::from(
            &self.config,
        ));
        let raw = crate::providers::router::create_provider(
            &provider_meta.id,
            &provider_meta.protocol_type,
            api_key.as_deref(),
            if provider_meta.base_url.is_empty() {
                None
            } else {
                Some(&provider_meta.base_url)
            },
            timeouts,
        );
        let retry_config = crate::providers::reliable::RetryConfig::default();
        let mut reliable = crate::providers::reliable::ReliableProvider::new(raw, retry_config);

        // Wire up 429 retry UX if the session has set up the shared state
        if let Some(status) = &self.retry_session_status
            && let Some(handle) = &self.retry_wait_handle
            && let Some(tx) = &self.on_chunk
            && let Some(sid) = &self.session_id
        {
            reliable = reliable.with_retry_ux(
                crate::providers::reliable::RetryWaitHandle {
                    state: handle.state.clone(),
                    skip_notify: handle.skip_notify.clone(),
                },
                status.clone(),
                tx.clone(),
                sid.clone(),
            );
        }

        Some(Arc::new(reliable))
    }

    /// Set debug mode by replacing the observer slot with a DevMode observer.
    ///
    /// This is the primary injection point — called by SessionManager when
    /// Gateway pushes EnableDebugMode. The DebugObserverImpl bundles all
    /// debug state (controller, event sender, notify handles) into one
    /// cohesive unit. See ADR-013.
    pub fn set_debug_mode(&mut self, observer: crate::debug::DebugObserverImpl) {
        tracing::info!(
            is_dev = crate::debug::observer::DebugObserver::is_dev_mode(&observer),
            "AgentCore::set_debug_mode called (observer pipeline)"
        );
        self.debug_observer = DebugObserverSlot::dev(observer);
    }

    /// Set the pending injection channel on the debug observer (DevMode only).
    /// No-op for Production mode.
    pub fn set_debug_pending_injection(
        &mut self,
        ch: std::sync::Arc<tokio::sync::Mutex<Option<crate::debug::DebugHandles>>>,
    ) {
        self.debug_observer.set_pending_injection(ch);
    }

    /// Access the debug observer slot.
    pub fn debug_observer(&self) -> &DebugObserverSlot {
        &self.debug_observer
    }

    /// Access the debug observer slot mutably.
    pub fn debug_observer_mut(&mut self) -> &mut DebugObserverSlot {
        &mut self.debug_observer
    }

    /// Check if DevMode is active.
    pub fn is_dev_mode(&self) -> bool {
        self.debug_observer.is_dev_mode()
    }

    /// Access the approval gate, if configured.
    pub fn approval_gate(&self) -> Option<&Arc<dyn ApprovalGate>> {
        self.approval_gate.as_ref()
    }

    /// Set the approval gate (for Gateway mode initialization).
    pub fn set_approval_gate(&mut self, gate: Arc<dyn ApprovalGate>) {
        self.approval_gate = Some(gate);
    }

    /// Access the shell approval threshold.
    pub fn shell_approval_threshold(&self) -> &ShellApprovalThreshold {
        &self.shell_approval_threshold
    }

    /// Get the usable context budget for history trimming.
    /// Uses Gateway model capabilities if available: delegates to
    /// [`ModelCapabilitiesInfo::effective_input_budget`] with the per-model
    /// `max_output_tokens_limit` as the output cap.
    /// Falls back to config.history_max_tokens when no capabilities are present.
    pub fn context_trim_budget(&self, model_name: &str) -> u64 {
        let max_output_limit = self.max_output_tokens_limit_for_model(model_name);
        self.get_model_capabilities(model_name)
            .map(|caps| {
                let usable = caps.effective_input_budget(max_output_limit);
                tracing::debug!(
                    model = %model_name,
                    context_window = caps.context_window,
                    max_input_tokens = ?caps.max_input_tokens,
                    max_output_tokens_limit = max_output_limit,
                    effective_input_budget = usable,
                    "Computed usable context budget from model capabilities"
                );
                usable
            })
            .unwrap_or_else(|| {
                tracing::debug!(
                    model = %model_name,
                    "No model capabilities for '{}', using config.history_max_tokens as fallback.",
                    model_name
                );
                self.config.history_max_tokens
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use acowork_core::providers::mock::MockProvider;

    fn make_core_with_channel(
        session_id: Option<&str>,
    ) -> (
        AgentCore,
        mpsc::Receiver<crate::agent::loop_::SessionChunkEvent>,
    ) {
        let (tx, rx) = mpsc::channel(16);
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.core"
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
        .unwrap();
        let config = RuntimeConfig::default();
        let provider = Arc::new(MockProvider::single_text("OK"));
        let mut core = AgentCore::new(config, manifest, provider, vec![], Some(tx));
        core.session_id = session_id.map(|s| s.to_string());
        (core, rx)
    }

    #[test]
    fn test_try_send_chunk_normal() {
        let (core, mut rx) = make_core_with_channel(Some("s1"));
        assert!(core.try_send_chunk(crate::agent::loop_::ChunkEvent::ReasoningStarted));
        let evt = rx.try_recv().unwrap();
        assert_eq!(evt.session_id, "s1");
        assert!(matches!(
            evt.event,
            crate::agent::loop_::ChunkEvent::ReasoningStarted
        ));
    }

    #[test]
    fn test_try_send_chunk_no_session_id() {
        let (core, _rx) = make_core_with_channel(None);
        // session_id is None — make_chunk_event returns None, try_send_chunk returns false
        assert!(!core.try_send_chunk(crate::agent::loop_::ChunkEvent::ReasoningStarted));
    }

    #[test]
    fn test_try_send_chunk_channel_full() {
        // Channel capacity = 1 (small), fill it then try_send_chunk should fail
        let (tx, mut rx) = mpsc::channel(1);
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.full"
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
        .unwrap();
        let config = RuntimeConfig::default();
        let provider = Arc::new(MockProvider::single_text("OK"));
        let mut core = AgentCore::new(config, manifest, provider, vec![], Some(tx));
        core.session_id = Some("s1".to_string());

        // Fill the channel
        assert!(core.try_send_chunk(crate::agent::loop_::ChunkEvent::ReasoningStarted));
        // Second send should fail (channel full)
        assert!(!core.try_send_chunk(crate::agent::loop_::ChunkEvent::Delta("x".to_string())));

        // Drain and retry should work
        let _ = rx.try_recv().unwrap();
        assert!(core.try_send_chunk(crate::agent::loop_::ChunkEvent::Delta("y".to_string())));
    }

    #[test]
    fn test_make_chunk_event_with_session_id() {
        let (core, _rx) = make_core_with_channel(Some("abc"));
        let wrapped = core.make_chunk_event(crate::agent::loop_::ChunkEvent::ReasoningStarted);
        assert!(wrapped.is_some());
        assert_eq!(wrapped.unwrap().session_id, "abc");
    }

    #[test]
    fn test_make_chunk_event_without_session_id() {
        let (core, _rx) = make_core_with_channel(None);
        let wrapped = core.make_chunk_event(crate::agent::loop_::ChunkEvent::ReasoningStarted);
        assert!(wrapped.is_none());
    }
}
