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

use rollball_core::protocol::ModelCapabilitiesInfo;
use rollball_core::providers::traits::Provider;
use rollball_core::tools::traits::Tool;
use rollball_grafeo::grafeo::GrafeoStore;
use tokio::sync::mpsc;

use crate::agent::loop_::ChunkEvent;
use crate::config::RuntimeConfig;
use crate::debug::controller::DebugController;
use crate::debug::server::DebugEventSender;
use crate::memory::{MemoryManager, MemoryManagerConfig};

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
    /// Tool registry
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    /// Model capabilities from Gateway, keyed by model name.
    /// When Gateway delivers capabilities for a model, they are stored here
    /// so that ContextBuilder can look them up at build() time.
    pub(crate) gateway_model_capabilities: HashMap<String, ModelCapabilitiesInfo>,
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
    /// Optional streaming chunk sender (like ZeroClaw's on_delta).
    /// When set, each StreamEvent::Content delta is forwarded here
    /// so the caller can relay chunks to Gateway via StreamChunk.
    pub(crate) on_chunk: Option<mpsc::Sender<ChunkEvent>>,
    /// Grafeo memory store (shared across all sessions of this agent).
    /// Opened at agent startup from `{work_dir}/memory/private.grafeo`.
    /// None if initialization failed (memory features degraded gracefully).
    pub(crate) memory_store: Option<Arc<GrafeoStore>>,
    /// Debug controller (shared across all sessions, only in DevMode).
    /// Provides execution control (pause/step/resume), breakpoints, and snapshots.
    /// None in production mode.
    pub(crate) debug_ctrl: Option<Arc<tokio::sync::Mutex<DebugController>>>,
    /// Debug event sender (clone for each session to push events to WebSocket).
    /// None in production mode.
    pub(crate) debug_event_tx: Option<DebugEventSender>,
    /// User display name delivered by Gateway via identity delivery.
    /// Used for user-facing messages like stop confirmation.
    pub(crate) user_display_name: Option<String>,
}

impl AgentCore {
    /// Create a new AgentCore with the given shared resources.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RuntimeConfig,
        manifest: rollball_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        on_chunk: Option<mpsc::Sender<ChunkEvent>>,
    ) -> Self {
        Self {
            config,
            manifest,
            provider,
            tools,
            gateway_model_capabilities: HashMap::new(),
            max_output_tokens_limit: 32_768,
            temperature_override: None,
            system_prompt_override: None,
            on_chunk,
            memory_store: None,
            debug_ctrl: None,
            debug_event_tx: None,
            user_display_name: None,
        }
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
    pub fn on_chunk(&self) -> Option<&mpsc::Sender<ChunkEvent>> {
        self.on_chunk.as_ref()
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
    /// The capabilities are stored keyed by model name for multi-model support.
    pub fn update_gateway_model_capabilities(&mut self, caps: ModelCapabilitiesInfo) {
        let model_name = caps.name.clone().unwrap_or_else(|| "default".to_string());
        tracing::info!(
            model = %model_name,
            context_window = caps.context_window,
            max_output_tokens = caps.max_output_tokens,
            supports_tool_calling = caps.supports_tool_calling,
            supports_reasoning = ?caps.supports_reasoning,
            cost = ?caps.cost.as_ref().map(|c| (c.input_per_million, c.output_per_million)),
            source = "gateway",
            "AgentCore received model capabilities from Gateway"
        );
        self.gateway_model_capabilities.insert(model_name, caps);
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
        match GrafeoStore::open(&db_path) {
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
                self.memory_store = Some(Arc::new(store));
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
    /// The `on_chunk` channel is replaced with the caller-provided one,
    /// since each session needs its own streaming channel.
    pub(crate) fn clone_for_session(&self, on_chunk: Option<mpsc::Sender<ChunkEvent>>) -> Self {
        Self {
            config: self.config.clone(),
            manifest: self.manifest.clone(),
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            gateway_model_capabilities: self.gateway_model_capabilities.clone(),
            max_output_tokens_limit: self.max_output_tokens_limit,
            temperature_override: self.temperature_override,
            system_prompt_override: self.system_prompt_override.clone(),
            on_chunk,
            memory_store: self.memory_store.clone(),
            debug_ctrl: self.debug_ctrl.clone(),
            debug_event_tx: self.debug_event_tx.clone(),
            user_display_name: self.user_display_name.clone(),
        }
    }

    /// Look up model capabilities by model name.
    /// Falls back to the first entry if the model name is not found.
    pub fn get_model_capabilities(&self, model_name: Option<&str>) -> Option<&ModelCapabilitiesInfo> {
        if let Some(name) = model_name
            && let Some(caps) = self.gateway_model_capabilities.get(name)
        {
            return Some(caps);
        }
        // Fallback: return any available capabilities
        self.gateway_model_capabilities.values().next()
    }

    /// Set the debug controller and event sender (DevMode only).
    pub fn set_debug_mode(
        &mut self,
        ctrl: Arc<tokio::sync::Mutex<DebugController>>,
        event_tx: DebugEventSender,
    ) {
        self.debug_ctrl = Some(ctrl);
        self.debug_event_tx = Some(event_tx);
    }

    /// Access the debug controller, if in DevMode.
    pub fn debug_ctrl(&self) -> Option<&Arc<tokio::sync::Mutex<DebugController>>> {
        self.debug_ctrl.as_ref()
    }

    /// Access the debug event sender, if in DevMode.
    pub fn debug_event_tx(&self) -> Option<&DebugEventSender> {
        self.debug_event_tx.as_ref()
    }

    /// Get the context window budget for history trimming.
    /// Uses Gateway model capabilities (context_window) if available,
    /// otherwise falls back to config.history_max_tokens.
    pub fn context_trim_budget(&self) -> u64 {
        self.get_model_capabilities(None)
            .map(|caps| caps.context_window)
            .unwrap_or_else(|| {
                tracing::debug!(
                    "No model capabilities received from Gateway, using config.history_max_tokens as fallback."
                );
                self.config.history_max_tokens
            })
    }
}
