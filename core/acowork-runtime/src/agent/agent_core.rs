//! Cross-session shared state for Agent Runtime.
//!
//! `AgentCore` holds all resources that are shared across sessions:
//! runtime config, manifest, LLM provider, tool registry,
//! Gateway model capabilities, Grafeo memory store, and the shared
//! streaming-lines map. These resources persist for the lifetime of
//! the agent process and are independent of any individual session.
//!
//! Per-session state (session_id, chunk channel, notification control,
//! JSONL counters, workspace, retry UX, approval) lives in
//! [`SessionCore`](super::session_core::SessionCore).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use acowork_core::protocol::{ModelCapabilitiesInfo, ProviderListItem};
use acowork_core::providers::traits::Provider;
use acowork_core::tools::traits::Tool;
use acowork_grafeo::consolidation::ConsolidationScheduler;
use acowork_grafeo::grafeo::GrafeoStore;
use acowork_grafeo::retrieval_metrics::MetricsAggregator;
use acowork_grafeo::types::GrafeoConfig;
use acowork_grafeo::types::{AutobioCategory, AutobiographicalNode, NodeStatus};
use chrono::Utc;

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
/// Per-session state lives in [`super::session_core::SessionCore`].
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
    pub(crate) all_tools: Vec<Arc<dyn Tool>>,
    /// Global provider list — full metadata including models, capabilities,
    /// base_url, protocol_type, compact_model for all configured providers.
    pub(crate) global_provider_list: Arc<RwLock<Vec<ProviderListItem>>>,
    /// Provider list version for diff sync with Gateway.
    pub(crate) provider_list_version: u64,
    /// Provider key vault (in-memory only, never persisted).
    pub(crate) provider_key_vault: Arc<RwLock<HashMap<String, String>>>,
    /// Provider→compact_model mapping from provider_list at AgentHello.
    pub(crate) provider_compact_models: HashMap<String, Option<String>>,
    /// LLM temperature override (from Gateway config via agent_config.json).
    /// Layer 1 in the resolution chain.
    pub(crate) temperature_override: Option<f32>,
    /// LLM temperature from manifest.toml [llm].temperature (Layer 2).
    /// Seeded at agent startup in cli.rs; independent of temperature_override
    /// so the resolution chain is self-contained in AgentCore.
    pub(crate) manifest_temperature: Option<f32>,
    /// System prompt override (from Gateway config).
    pub(crate) system_prompt_override: Option<String>,
    /// Grafeo memory store (shared across all sessions of this agent).
    pub(crate) memory_store: Option<Arc<GrafeoStore>>,
    /// Debug observer slot — Production (no-op) or Dev (real observer).
    pub(crate) debug_observer: DebugObserverSlot,
    /// Approval gate for shell command risk confirmation.
    pub(crate) approval_gate: Option<Arc<dyn ApprovalGate>>,
    /// Shell approval threshold: Low / Medium / High / Never.
    pub(crate) shell_approval_threshold: ShellApprovalThreshold,
    /// Memory session handle — shared between agent loop and memory tools.
    pub(crate) memory_session: Option<Arc<crate::memory::MemorySessionHandle>>,
    /// Embedding provider for vector-based memory retrieval.
    pub(crate) embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// P3-1: Retrieval quality metrics aggregator (shared across sessions).
    pub(crate) metrics_aggregator: Arc<std::sync::Mutex<MetricsAggregator>>,
    /// P3: Consolidation scheduler — decides when to run offline consolidation.
    pub(crate) consolidation_scheduler: Option<Arc<ConsolidationScheduler>>,
    /// P3: Background consolidation task handle.
    pub(crate) consolidation_bg_task: Option<ConsolidationBgTask>,
}

impl AgentCore {
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_observer(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        observer: DebugObserverSlot,
    ) -> Self {
        let shell_approval_threshold =
            ShellApprovalThreshold::from_str_loose(&config.shell_approval_threshold)
                .unwrap_or_default();
        let manifest_temperature = manifest.llm.temperature;
        tracing::debug!(
            ?manifest_temperature,
            "AgentCore: seeding manifest_temperature from manifest [llm].temperature"
        );
        Self {
            config,
            manifest,
            provider,
            tools: tools.clone(),
            mcp_tools: None,
            all_tools: tools,
            global_provider_list: Arc::new(RwLock::new(Vec::new())),
            provider_list_version: 0,
            provider_key_vault: Arc::new(RwLock::new(HashMap::new())),
            provider_compact_models: HashMap::new(),
            temperature_override: None,
            manifest_temperature,
            system_prompt_override: None,
            memory_store: None,
            memory_session: None,
            debug_observer: observer,
            approval_gate: None,
            shell_approval_threshold,
            embedding_provider: None,
            metrics_aggregator: Arc::new(std::sync::Mutex::new(MetricsAggregator::with_defaults(
                1.0,
            ))),
            consolidation_scheduler: None,
            consolidation_bg_task: None,
        }
    }

    pub fn new(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        Self::new_with_observer(config, manifest, provider, tools, DebugObserverSlot::production())
    }

    pub(crate) fn rebuild_all_tools(&mut self) {
        let mut merged = self.tools.clone();
        if let Some(ref mcp) = self.mcp_tools {
            merged.extend(mcp.clone());
        }
        self.all_tools = merged;
    }

    pub fn config(&self) -> &RuntimeConfig { &self.config }
    pub fn manifest(&self) -> &acowork_core::AgentManifest { &self.manifest }
    pub fn provider(&self) -> &Arc<dyn Provider> { &self.provider }
    pub fn tools(&self) -> &[Arc<dyn Tool>] { &self.tools }

    pub fn gateway_model_capabilities(&self) -> HashMap<String, ModelCapabilitiesInfo> {
        let list = self.global_provider_list.read().unwrap();
        let mut map = HashMap::new();
        for provider in list.iter() {
            for model in &provider.models {
                map.insert(model.id.clone(), model.capabilities.clone());
            }
        }
        map
    }

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

    pub fn update_embedding_provider(
        &mut self,
        new_provider: Arc<dyn EmbeddingProvider>,
    ) {
        let old_name = self
            .embedding_provider
            .as_ref()
            .map(|p| p.name())
            .unwrap_or("none")
            .to_string();
        let new_name = new_provider.name().to_string();
        self.embedding_provider = Some(new_provider);
        tracing::info!(
            old_provider = %old_name,
            new_provider = %new_name,
            "Embedding provider updated at runtime via EmbeddingConfigUpdate"
        );
    }

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

    pub fn update_max_output_tokens_limit(&mut self, limit: u64) {
        tracing::info!(new_limit = limit, "AgentCore max_output_tokens_limit updated from Gateway (all models)");
        let mut list = self.global_provider_list.write().unwrap();
        for provider in list.iter_mut() {
            for model in provider.models.iter_mut() {
                model.max_output_tokens_limit = limit;
            }
        }
    }

    pub fn apply_runtime_config(
        &mut self,
        max_output_tokens: Option<u64>,
        max_iterations: Option<u32>,
        temperature: Option<f32>,
        system_prompt_override: Option<String>,
        shell_approval_threshold: Option<String>,
    ) {
        if let Some(limit) = max_output_tokens {
            tracing::info!(new = limit, "runtime config: max_output_tokens updated (all models)");
            self.update_max_output_tokens_limit(limit);
        }
        if let Some(n) = max_iterations {
            tracing::info!(old = self.config.max_iterations, new = n, "runtime config: max_iterations updated");
            self.config.max_iterations = n;
        }
        if let Some(temp) = temperature {
            tracing::info!(old = ?self.temperature_override, new = temp, "runtime config: temperature updated");
            self.temperature_override = Some(temp);
        }
        if system_prompt_override.is_some() {
            tracing::info!(has_override = system_prompt_override.as_ref().map(|s| !s.is_empty()).unwrap_or(false), "runtime config: system_prompt_override updated");
            self.system_prompt_override = system_prompt_override;
        }
        if let Some(ref threshold) = shell_approval_threshold {
            let new_threshold = ShellApprovalThreshold::from_str_loose(threshold).unwrap_or_default();
            tracing::info!(old = ?self.shell_approval_threshold, new = ?new_threshold, "runtime config: shell_approval_threshold updated");
            self.shell_approval_threshold = new_threshold;
        }
    }

    pub fn init_memory_store(&mut self, work_dir: &std::path::Path) {
        if self.memory_store.is_some() {
            tracing::debug!("init_memory_store: already initialized, skipping");
            return;
        }
        let memory_dir = work_dir.join("memory");
        if let Err(e) = std::fs::create_dir_all(&memory_dir) {
            tracing::warn!(error = %e, dir = %memory_dir.display(), "Failed to create memory directory, memory features disabled");
            return;
        }
        let db_path = memory_dir.join("private.grafeo");
        let embedding_dim = self.embedding_provider.as_ref().map(|p| p.dimension()).unwrap_or(acowork_grafeo::types::DEFAULT_EMBEDDING_DIM);
        let config = GrafeoConfig { db_path: db_path.clone(), embedding_dim };
        match GrafeoStore::open(&config) {
            Ok(store) => {
                let graph = store.db().graph_store();
                let existing: usize = ["Episodic", "Knowledge", "Procedural", "Autobiographical"]
                    .iter().map(|l| graph.nodes_by_label(l).len()).sum();
                tracing::info!(path = %db_path.display(), existing_nodes = existing, "Grafeo memory store opened");
                let store_arc = Arc::new(store);
                self.bootstrap_autobiographical_from_manifest(&store_arc);
                if let Some(ref session) = self.memory_session {
                    session.set_store(store_arc.clone());
                }
                self.memory_store = Some(store_arc);
                self.start_consolidation_pipeline();
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %db_path.display(), "Failed to open Grafeo memory store, memory features disabled");
            }
        }
    }

    pub fn memory_store(&self) -> Option<&Arc<GrafeoStore>> {
        self.memory_store.as_ref()
    }

    fn bootstrap_autobiographical_from_manifest(&self, store: &GrafeoStore) {
        match store.find_autobiographical_by_category(AutobioCategory::Identity) {
            Ok(existing) if !existing.is_empty() => {
                tracing::debug!(count = existing.len(), "Autobiographical nodes already exist, skipping manifest bootstrap");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to probe existing Autobiographical nodes, attempting bootstrap anyway");
            }
            _ => {}
        }
        let manifest = &self.manifest;
        let now = Utc::now();
        let identity_entries: Vec<(&str, String)> = {
            let mut v = vec![
                ("agent_id", manifest.agent_id.clone()),
                ("name", manifest.name.clone()),
                ("description", manifest.description.clone()),
            ];
            if let Some(ref dn) = manifest.display_name { v.push(("display_name", dn.clone())); }
            if let Some(ref role) = manifest.role { v.push(("role", role.clone())); }
            v
        };
        for (key, value) in &identity_entries {
            let node = AutobiographicalNode {
                id: None, category: AutobioCategory::Identity, key: key.to_string(),
                value: value.clone(), confidence: 1.0, source_episode_id: None,
                embedding: None, status: NodeStatus::Active,
                created_at: now, updated_at: now, metadata: HashMap::new(),
            };
            if let Err(e) = store.store_autobiographical(&node) {
                tracing::warn!(key = %key, error = %e, "Failed to bootstrap Autobiographical/Identity node");
            }
        }
        for (cap_key, cap_def) in &manifest.capabilities {
            let node = AutobiographicalNode {
                id: None, category: AutobioCategory::Capability, key: cap_key.clone(),
                value: cap_def.description.clone(), confidence: 1.0, source_episode_id: None,
                embedding: None, status: NodeStatus::Active,
                created_at: now, updated_at: now, metadata: HashMap::new(),
            };
            if let Err(e) = store.store_autobiographical(&node) {
                tracing::warn!(capability = %cap_key, error = %e, "Failed to bootstrap Autobiographical/Capability node");
            }
        }
        tracing::info!(identity_count = identity_entries.len(), capability_count = manifest.capabilities.len(), "Bootstrapped Autobiographical nodes from manifest");
    }

    pub fn init_memory_manager(&self) -> MemoryManager {
        MemoryManager::new(MemoryManagerConfig::default())
    }

    pub fn start_consolidation_pipeline(&mut self) {
        let Some(ref store) = self.memory_store else {
            tracing::debug!("Cannot start consolidation: memory store not initialized");
            return;
        };
        let Some(ref embedding) = self.embedding_provider else {
            tracing::debug!("Cannot start consolidation: embedding provider not available");
            return;
        };
        if self.consolidation_scheduler.is_some() {
            tracing::debug!("Consolidation pipeline already running");
            return;
        }
        use crate::memory::consolidation_bg::{ConsolidationParams, start_consolidation_pipeline};
        use acowork_grafeo::consolidation::SchedulerConfig;
        use std::time::Duration;
        let model = {
            let list = self.global_provider_list.read().unwrap();
            list.iter().flat_map(|p| p.models.iter()).next().map(|m| m.id.clone()).unwrap_or_else(|| "default".to_string())
        };
        let params = ConsolidationParams {
            store: store.clone(), provider: self.provider.clone(), model,
            embedding_provider: embedding.clone(), scheduler_config: SchedulerConfig::default(),
            poll_interval: Duration::from_secs(60),
            work_dir: Some(std::path::PathBuf::from(&self.config.work_dir)),
        };
        let (scheduler, bg_task) = start_consolidation_pipeline(params);
        self.consolidation_scheduler = Some(scheduler);
        self.consolidation_bg_task = Some(bg_task);
        tracing::info!("Consolidation background pipeline started");
    }

    pub async fn notify_consolidation_active(&self) {
        if let Some(ref scheduler) = self.consolidation_scheduler {
            scheduler.notify_active().await;
        }
    }

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
            let available: Vec<&str> = list.iter().flat_map(|p| p.models.iter().map(|m| m.id.as_str())).collect();
            tracing::warn!(model = %model_name, available = ?available, "Model capabilities not found for '{}'", model_name);
        }
        None
    }

    pub fn get_provider(&self, provider_id: &str) -> Option<ProviderListItem> {
        let list = self.global_provider_list.read().unwrap();
        list.iter().find(|p| p.id == provider_id).cloned()
    }

    pub fn get_provider_api_key(&self, provider_id: &str) -> Option<String> {
        let vault = self.provider_key_vault.read().unwrap();
        vault.get(provider_id).cloned()
    }

    pub fn set_debug_mode(&mut self, observer: crate::debug::DebugObserverImpl) {
        tracing::info!(is_dev = crate::debug::observer::DebugObserver::is_dev_mode(&observer), "AgentCore::set_debug_mode called (observer pipeline)");
        self.debug_observer = DebugObserverSlot::dev(observer);
    }

    pub fn set_debug_pending_injection(
        &mut self,
        ch: Arc<tokio::sync::Mutex<Option<crate::debug::DebugHandles>>>,
    ) {
        self.debug_observer.set_pending_injection(ch);
    }

    pub fn debug_observer(&self) -> &DebugObserverSlot { &self.debug_observer }
    pub fn debug_observer_mut(&mut self) -> &mut DebugObserverSlot { &mut self.debug_observer }
    pub fn is_dev_mode(&self) -> bool { self.debug_observer.is_dev_mode() }
    pub fn approval_gate(&self) -> Option<&Arc<dyn ApprovalGate>> { self.approval_gate.as_ref() }
    pub fn set_approval_gate(&mut self, gate: Arc<dyn ApprovalGate>) { self.approval_gate = Some(gate); }
    pub fn shell_approval_threshold(&self) -> &ShellApprovalThreshold { &self.shell_approval_threshold }

    pub fn context_trim_budget(&self, model_name: &str) -> u64 {
        let max_output_limit = self.max_output_tokens_limit_for_model(model_name);
        self.get_model_capabilities(model_name)
            .map(|caps| {
                let usable = caps.effective_input_budget(max_output_limit);
                tracing::debug!(model = %model_name, context_window = caps.context_window, max_input_tokens = ?caps.max_input_tokens, max_output_tokens_limit = max_output_limit, effective_input_budget = usable, "Computed usable context budget from model capabilities");
                usable
            })
            .unwrap_or_else(|| {
                tracing::debug!(model = %model_name, "No model capabilities for '{}', using config.history_max_tokens as fallback.", model_name);
                self.config.history_max_tokens
            })
    }
}

impl Clone for AgentCore {
    fn clone(&self) -> Self {
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
            manifest_temperature: self.manifest_temperature,
            system_prompt_override: self.system_prompt_override.clone(),
            memory_store: self.memory_store.clone(),
            memory_session: self.memory_session.clone(),
            debug_observer: self.debug_observer.clone_production(),
            approval_gate: self.approval_gate.clone(),
            shell_approval_threshold: self.shell_approval_threshold,
            embedding_provider: self.embedding_provider.clone(),
            metrics_aggregator: self.metrics_aggregator.clone(),
            consolidation_scheduler: self.consolidation_scheduler.clone(),
            consolidation_bg_task: None, // sessions don't own bg task
        }
    }
}
