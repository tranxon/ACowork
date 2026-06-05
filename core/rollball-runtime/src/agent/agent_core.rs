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

use chrono::Utc;
use rollball_core::protocol::ModelCapabilitiesInfo;
use rollball_core::providers::traits::Provider;
use rollball_core::tools::traits::Tool;
use rollball_grafeo::grafeo::GrafeoStore;
use rollball_grafeo::types::GrafeoConfig;
use rollball_grafeo::types::{AutobioCategory, AutobiographicalNode, NodeStatus};
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};
use crate::agent::loop_approval::ApprovalHandle;
use crate::config::RuntimeConfig;
use crate::debug::DebugObserverSlot;
use crate::embedding::EmbeddingProvider;
use crate::memory::{MemoryManager, MemoryManagerConfig};
use crate::security::approval_gate::ApprovalGate;
use rollball_core::ShellApprovalThreshold;

/// Cross-session shared state for the agent loop.
///
/// Fields here are immutable or rarely mutated at runtime (e.g. provider swap
/// via LLMConfigDelivery), and are shared across all sessions of the same agent.
pub struct AgentCore {
    /// Runtime configuration
    pub(crate) config: RuntimeConfig,
    /// Agent manifest (declarative .agent package metadata)
    pub(crate) manifest: rollball_core::AgentManifest,
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
    /// Model capabilities from Gateway, keyed by model name.
    /// When Gateway delivers capabilities for a model, they are stored here
    /// so that ContextBuilder can look them up at build() time.
    pub(crate) gateway_model_capabilities: HashMap<String, ModelCapabilitiesInfo>,
    /// Provider→compact_model mapping from provider_list at AgentHello.
    /// Keyed by Vault provider ID.  Static after init — provider changes
    /// (add/remove model, compact_model) require agent restart.
    pub(crate) provider_compact_models: HashMap<String, Option<String>>,
    /// The Vault provider ID currently in use (updated on model_switch / AgentHello).
    /// Used with `provider_compact_models` to resolve compact_model in memory.
    pub(crate) current_provider_id: Option<String>,
    /// Global max output tokens limit from Gateway config.
    /// When a model's max_output_tokens exceeds this value, the value is capped.
    /// Default: 32768 (32K). Set to 0 to disable the limit.
    pub(crate) max_output_tokens_limit: u64,
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
    pub(crate) status_tx: Option<tokio::sync::watch::Sender<crate::agent::session_state::SessionStatus>>,
    /// Memory session handle — shared between agent loop and memory tools.
    /// Created at tool registry time, store initialized lazily.
    pub(crate) memory_session: Option<Arc<crate::memory::MemorySessionHandle>>,
    /// Embedding provider for vector-based memory retrieval.
    /// Built from LLM provider registry; shared across all sessions.
    /// Used by [`init_memory_store`] to determine Grafeo vector dimension.
    pub(crate) embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
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
        manifest: rollball_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        on_chunk: Option<mpsc::Sender<SessionChunkEvent>>,
        observer: crate::debug::DebugObserverSlot,
    ) -> Self {
        let shell_approval_threshold = ShellApprovalThreshold::from_str_loose(&config.shell_approval_threshold)
            .unwrap_or_default();
        Self {
            config,
            manifest,
            provider,
            tools: tools.clone(),
            mcp_tools: None,
            all_tools: tools,
            gateway_model_capabilities: HashMap::new(),
            provider_compact_models: HashMap::new(),
            current_provider_id: None,
            max_output_tokens_limit: 32_768,
            temperature_override: None,
            system_prompt_override: None,
            session_id: None,
            on_chunk,
            memory_store: None,
            memory_session: None,
            debug_observer: observer,
            urgent_stop: Some(Arc::new(Notify::new())),
            approval_gate: None,
            approval_handle: None,
            shell_approval_threshold,
            status_tx: None,
            embedding_provider: None,
        }
    }

    /// Create a new AgentCore with the given shared resources.
    ///
    /// Defaults to Production mode (zero-cost debug no-ops).
    /// Use [`AgentCore::new_with_observer()`] to inject a DevMode observer.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RuntimeConfig,
        manifest: rollball_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        on_chunk: Option<mpsc::Sender<SessionChunkEvent>>,
    ) -> Self {
        Self::new_with_observer(config, manifest, provider, tools, on_chunk, DebugObserverSlot::production())
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
    pub fn manifest(&self) -> &rollball_core::AgentManifest {
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
    pub fn gateway_model_capabilities(&self) -> &HashMap<String, ModelCapabilitiesInfo> {
        &self.gateway_model_capabilities
    }

    /// Access the max output tokens limit.
    pub fn max_output_tokens_limit(&self) -> u64 {
        self.max_output_tokens_limit
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
        if let Some(wrapped) = self.make_chunk_event(event) {
            self.on_chunk.as_ref()
                .map(|tx| tx.try_send(wrapped).is_ok())
                .unwrap_or(false)
        } else {
            tracing::debug!("Cannot send chunk event: session_id not set on AgentCore");
            false
        }
    }

    /// Update the LLM provider at runtime (e.g., after receiving a hot-pushed
    /// LLMConfigDelivery from Gateway).
    pub fn update_provider(&mut self, new_provider: Arc<dyn Provider>, model: String) {
        let old_name = self.provider.name().to_string();
        self.provider = new_provider;
        tracing::info!(
            old_provider = %old_name,
            new_provider = %self.provider.name(),
            model = %model,
            "LLM provider updated at runtime via LLMConfigDelivery"
        );
    }

    /// Update gateway model capabilities at runtime (e.g., after receiving a
    /// hot-pushed LLMConfigDelivery from Gateway).
    /// The capabilities are stored keyed by model ID for multi-model support.
    /// `model_id` is the model identifier string (e.g. "deepseek-v4-flash"),
    /// always provided by the caller — never derived from `caps.name` which
    /// may be None for models not in models.dev.
    pub fn update_gateway_model_capabilities(&mut self, model_id: &str, caps: ModelCapabilitiesInfo) {
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
        self.gateway_model_capabilities.insert(model_id.to_string(), caps);
    }

    /// Update the max output tokens limit from Gateway config.
    pub fn update_max_output_tokens_limit(&mut self, limit: u64) {
        tracing::info!(
            old_limit = self.max_output_tokens_limit,
            new_limit = limit,
            "AgentCore max_output_tokens_limit updated from Gateway"
        );
        self.max_output_tokens_limit = limit;
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
            tracing::info!(old = self.max_output_tokens_limit, new = limit, "runtime config: max_output_tokens updated");
            self.max_output_tokens_limit = limit;
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
                has_override = system_prompt_override.as_ref().map(|s| !s.is_empty()).unwrap_or(false),
                "runtime config: system_prompt_override updated"
            );
            self.system_prompt_override = system_prompt_override;
        }
        if let Some(ref threshold) = shell_approval_threshold {
            let new_threshold = ShellApprovalThreshold::from_str_loose(threshold)
                .unwrap_or_default();
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
            .unwrap_or(rollball_grafeo::types::DEFAULT_EMBEDDING_DIM);
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
                Err(e) => tracing::warn!(key = %key, error = %e, "Failed to bootstrap Autobiographical/Identity node"),
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
                Err(e) => tracing::warn!(capability = %cap_key, error = %e, "Failed to bootstrap Autobiographical/Capability node"),
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

    /// Create a cheap clone of this AgentCore for a new session.
    ///
    /// Heavy fields (provider, tools, memory_store) are Arc-cloned (refcount increment),
    /// while value fields (config, manifest, capabilities) are deep-cloned.
    /// The `on_chunk` channel and `session_id` are replaced with the caller-provided ones,
    /// since each session needs its own streaming channel and identity.
    pub(crate) fn clone_for_session(&self, on_chunk: Option<mpsc::Sender<SessionChunkEvent>>, session_id: String) -> Self {
        Self {
            config: self.config.clone(),
            manifest: self.manifest.clone(),
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            mcp_tools: self.mcp_tools.clone(),
            all_tools: self.all_tools.clone(),
            gateway_model_capabilities: self.gateway_model_capabilities.clone(),
            provider_compact_models: self.provider_compact_models.clone(),
            current_provider_id: self.current_provider_id.clone(),
            max_output_tokens_limit: self.max_output_tokens_limit,
            temperature_override: self.temperature_override,
            system_prompt_override: self.system_prompt_override.clone(),
            session_id: Some(session_id),
            on_chunk,
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
            shell_approval_threshold: self.shell_approval_threshold.clone(),
            status_tx: None, // set separately by SessionTask
            embedding_provider: self.embedding_provider.clone(),
        }
    }

    /// Look up model capabilities by exact model name.
    ///
    /// Returns `None` when the requested model is not found — callers must
    /// handle this case explicitly. The legacy `.values().next()` fallback
    /// has been removed because it silently picks wrong model capabilities
    /// when model names don't match (e.g. case sensitivity), causing incorrect
    /// context usage percentages and preventing compaction from triggering.
    pub(crate) fn get_model_capabilities(&self, model_name: &str) -> Option<&ModelCapabilitiesInfo> {
        let caps = self.gateway_model_capabilities.get(model_name);
        if caps.is_none() && !self.gateway_model_capabilities.is_empty() {
            let available: Vec<&str> = self.gateway_model_capabilities.keys().map(|s| s.as_str()).collect();
            tracing::warn!(
                model = %model_name,
                available = ?available,
                "Model capabilities not found for '{}' — \
                 context usage reporting and compaction will be skipped. \
                 This indicates a model name mismatch between Runtime and Gateway (e.g. case sensitivity).",
                model_name
            );
        }
        caps
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
            "[DBG-TRACE] AgentCore::set_debug_mode called (observer pipeline)"
        );
        self.debug_observer = DebugObserverSlot::dev(observer);
    }

    /// Set the pending injection channel on the debug observer (DevMode only).
    /// No-op for Production mode.
    pub fn set_debug_pending_injection(&mut self, ch: std::sync::Arc<tokio::sync::Mutex<Option<crate::debug::DebugHandles>>>) {
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
    /// [`ModelCapabilitiesInfo::effective_input_budget`] with the global
    /// `max_output_tokens_limit` as the output cap.
    /// Falls back to config.history_max_tokens when no capabilities are present.
    pub fn context_trim_budget(&self, model_name: &str) -> u64 {
        self.get_model_capabilities(model_name)
            .map(|caps| {
                let usable = caps.effective_input_budget(self.max_output_tokens_limit);
                tracing::debug!(
                    model = %model_name,
                    context_window = caps.context_window,
                    max_input_tokens = ?caps.max_input_tokens,
                    max_output_tokens_limit = self.max_output_tokens_limit,
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
    use rollball_core::providers::mock::MockProvider;

    fn make_core_with_channel(
        session_id: Option<&str>,
    ) -> (AgentCore, mpsc::Receiver<crate::agent::loop_::SessionChunkEvent>) {
        let (tx, rx) = mpsc::channel(16);
        let manifest = rollball_core::AgentManifest::from_toml(
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
            "#
        ).unwrap();
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
        assert!(matches!(evt.event, crate::agent::loop_::ChunkEvent::ReasoningStarted));
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
        let manifest = rollball_core::AgentManifest::from_toml(
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
            "#
        ).unwrap();
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
