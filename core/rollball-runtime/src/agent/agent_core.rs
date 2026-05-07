//! Cross-session shared state for Agent Runtime.
//!
//! `AgentCore` holds all resources that are shared across sessions:
//! runtime config, manifest, LLM provider, tool registry, streaming channel,
//! and Gateway model capabilities. These resources persist for the lifetime
//! of the agent process and are independent of any individual session.
//!
//! Phase 1: direct ownership inside AgentLoop.
//! Phase 2: wrapped in Arc for multi-session Actor sharing.

use std::collections::HashMap;
use std::sync::Arc;

use rollball_core::protocol::ModelCapabilitiesInfo;
use rollball_core::providers::traits::Provider;
use rollball_core::tools::traits::Tool;
use tokio::sync::mpsc;

use crate::agent::loop_::ChunkEvent;
use crate::config::RuntimeConfig;

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
    /// Optional streaming chunk sender (like ZeroClaw's on_delta).
    /// When set, each StreamEvent::Content delta is forwarded here
    /// so the caller can relay chunks to Gateway via StreamChunk.
    pub(crate) on_chunk: Option<mpsc::Sender<ChunkEvent>>,
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
            on_chunk,
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

    /// Create a cheap clone of this AgentCore for a new session.
    ///
    /// Heavy fields (provider, tools) are Arc-cloned (refcount increment),
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
            on_chunk,
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
