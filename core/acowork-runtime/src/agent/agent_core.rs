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
use std::sync::atomic::{AtomicU64, Ordering};

use acowork_core::protocol::{ModelCapabilitiesInfo, ProviderListItem};
use acowork_core::providers::traits::{Provider, UsageInfo};
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
use crate::agent::session::session_manager::RuntimeConfigOverrides;
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
    /// Built-in tool registry — the static base set used as the seed for
    /// rebuilding `all_tools` whenever `mcp_tools` changes. These are the
    /// tools shipped with the runtime binary and registered in
    /// `crate::tools::builtin`; MCP tools are kept separate in [`Self::mcp_tools`].
    pub(crate) builtin_tools: Vec<Arc<dyn Tool>>,
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
    /// Per-agent context window cap (from agent_config.json, set via Agent Setup panel).
    /// Layer 1 in the resolution chain. 0 means "no limit".
    pub(crate) context_window_override: Option<u64>,
    /// Context window cap from manifest.toml [llm].context_window (Layer 2).
    /// Seeded at agent startup in cli.rs; independent of context_window_override
    /// so the resolution chain is self-contained in AgentCore.
    pub(crate) manifest_context_window: Option<u64>,
    /// Approval timeout in seconds for loop approval. None = use system default (300).
    pub(crate) approval_timeout_secs: Option<u64>,
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
    /// ADR-028: cumulative input tokens across every LLM call made by this
    /// agent process. Sourced from `accumulate_llm_usage` on every LLM call
    /// and from `merge_token_totals` on every `list_sessions` scan. Not
    /// persisted; on process restart the next session-list scan rebuilds the
    /// baseline via atomic-max merge with the on-disk `SessionTokens`.
    pub(crate) agent_total_input_tokens: AtomicU64,
    /// ADR-028: cumulative output tokens across every LLM call made by this
    /// agent process. See [`Self::agent_total_input_tokens`] for semantics.
    pub(crate) agent_total_output_tokens: AtomicU64,
}

impl AgentCore {
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_observer(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        builtin_tools: Vec<Arc<dyn Tool>>,
        observer: DebugObserverSlot,
    ) -> Self {
        let shell_approval_threshold =
            ShellApprovalThreshold::from_str_loose(&config.shell_approval_threshold)
                .unwrap_or_default();
        let manifest_temperature = manifest.llm.temperature;
        let manifest_context_window = manifest.llm.context_window;
        tracing::debug!(
            ?manifest_temperature,
            ?manifest_context_window,
            "AgentCore: seeding manifest_temperature and manifest_context_window from manifest [llm]"
        );
        Self {
            config,
            manifest,
            provider,
            builtin_tools: builtin_tools.clone(),
            mcp_tools: None,
            all_tools: builtin_tools,
            global_provider_list: Arc::new(RwLock::new(Vec::new())),
            provider_list_version: 0,
            provider_key_vault: Arc::new(RwLock::new(HashMap::new())),
            provider_compact_models: HashMap::new(),
            temperature_override: None,
            manifest_temperature,
            context_window_override: None,
            manifest_context_window,
            approval_timeout_secs: None,
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
            // ADR-028: counters start at 0; the next `list_sessions` scan
            // rebuilds the baseline via `merge_token_totals`.
            agent_total_input_tokens: AtomicU64::new(0),
            agent_total_output_tokens: AtomicU64::new(0),
        }
    }

    pub fn new(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        builtin_tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        Self::new_with_observer(
            config,
            manifest,
            provider,
            builtin_tools,
            DebugObserverSlot::production(),
        )
    }

    pub(crate) fn rebuild_all_tools(&mut self) {
        let mut merged = self.builtin_tools.clone();
        if let Some(ref mcp) = self.mcp_tools {
            merged.extend(mcp.clone());
        }
        self.all_tools = merged;
    }

    pub fn config(&self) -> &RuntimeConfig { &self.config }
    pub fn manifest(&self) -> &acowork_core::AgentManifest { &self.manifest }
    pub fn provider(&self) -> &Arc<dyn Provider> { &self.provider }
    pub fn builtin_tools(&self) -> &[Arc<dyn Tool>] { &self.builtin_tools }

    // ── ADR-028: Agent-scoped cumulative token usage ───────────────────
    //
    // Counter semantics:
    // - `accumulate_llm_usage` adds a fresh LLM call's token usage to the
    //   in-process totals.
    // - `merge_token_totals` performs an atomic-max merge with a scanned
    //   total (summed from every on-disk `SessionMeta.tokens`). This both
    //   (a) recovers the historical baseline on first session-list scan
    //   after startup, and (b) reconciles any on-disk writers that updated
    //   `SessionTokens` behind us (defence in depth).
    // - `agent_token_totals` snapshots both counters for the
    //   `ContextUsageInfo` push.
    //
    // Not persisted: a process restart zeroes the counters, and the
    // next `list_sessions` fetch rebuilds the baseline via scan + merge.
    // See ADR-028 §"Why no startup seed" for the rationale.

    /// Add an LLM call's token usage to the running agent-scoped totals.
    ///
    /// Mirrors [`crate::conversation::ConversationSession::accumulate_llm_usage`]:
    /// a zero-input call is still recorded (the LLM did happen), but its
    /// `prompt_tokens` contribution is skipped to avoid masking a reliable
    /// baseline with a Provider-fallback zero ("宁可 miss 也不估计").
    pub fn accumulate_llm_usage(&self, usage: &UsageInfo) {
        if usage.prompt_tokens > 0 {
            self.agent_total_input_tokens
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                    Some(cur.saturating_add(usage.prompt_tokens))
                })
                .ok();
        }
        self.agent_total_output_tokens
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.saturating_add(usage.completion_tokens))
            })
            .ok();
    }

    /// Atomically merge a freshly-scanned total into the in-process counter
    /// using `max(counter, scanned)`.
    ///
    /// Idempotent: calling with the same `(in, out)` value repeatedly is a
    /// no-op once the counter has been raised. Race-safe with concurrent
    /// `accumulate_llm_usage` calls — whichever side sees the larger value
    /// wins, so a slow scan followed by a fresh LLM call won't accidentally
    /// downgrade the counter to the (stale) scan result.
    ///
    /// Pass `None` for either side if the scan yielded no data
    /// (e.g. agents with no persisted sessions yet).
    pub fn merge_token_totals(&self, scanned: (Option<u64>, Option<u64>)) {
        if let Some(inp) = scanned.0 {
            self.agent_total_input_tokens
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                    Some(cur.max(inp))
                })
                .ok();
        }
        if let Some(out) = scanned.1 {
            self.agent_total_output_tokens
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                    Some(cur.max(out))
                })
                .ok();
        }
    }

    /// Snapshot the current agent-scoped cumulative token totals.
    ///
    /// Returns `(input_tokens, output_tokens)`. Always present (zero is a
    /// valid baseline before the first LLM call). Callers embed these in
    /// `ContextUsageInfo` so the frontend can show a "agent-total" line in
    /// the Results Panel before any per-session cumulative figure exists.
    pub fn agent_token_totals(&self) -> (u64, u64) {
        (
            self.agent_total_input_tokens.load(Ordering::Acquire),
            self.agent_total_output_tokens.load(Ordering::Acquire),
        )
    }

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

    pub fn apply_runtime_config(&mut self, overrides: &RuntimeConfigOverrides) {
        if let Some(limit) = overrides.max_output_tokens {
            tracing::info!(new = limit, "runtime config: max_output_tokens updated (all models)");
            self.update_max_output_tokens_limit(limit);
        }
        if let Some(n) = overrides.max_iterations {
            tracing::info!(
                old = self.config.max_iterations,
                new = n,
                "runtime config: max_iterations updated"
            );
            self.config.max_iterations = n;
        }
        if let Some(temp) = overrides.temperature {
            tracing::info!(
                old = ?self.temperature_override,
                new = temp,
                "runtime config: temperature updated"
            );
            self.temperature_override = Some(temp);
        }
        if let Some(cw) = overrides.context_window {
            tracing::info!(
                old = ?self.context_window_override,
                new = cw,
                "runtime config: context_window updated"
            );
            self.context_window_override = Some(cw);
        }
        if overrides.system_prompt_override.is_some() {
            tracing::info!(
                has_override = overrides
                    .system_prompt_override
                    .as_ref()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false),
                "runtime config: system_prompt_override updated"
            );
            self.system_prompt_override = overrides.system_prompt_override.clone();
        }
        if let Some(ref threshold) = overrides.shell_approval_threshold {
            let new_threshold = ShellApprovalThreshold::from_str_loose(threshold).unwrap_or_default();
            tracing::info!(
                old = ?self.shell_approval_threshold,
                new = ?new_threshold,
                "runtime config: shell_approval_threshold updated"
            );
            self.shell_approval_threshold = new_threshold;
        }
        if let Some(timeout) = overrides.approval_timeout_secs {
            tracing::info!(
                old = ?self.approval_timeout_secs,
                new = timeout,
                "runtime config: approval_timeout_secs updated"
            );
            self.approval_timeout_secs = Some(timeout);
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

    /// Resolve the effective context window budget for history trimming.
    ///
    /// Resolution chain for the user-configured cap:
    ///   1. agent_config.json.context_window (Layer 1)
    ///   2. manifest.llm.context_window (Layer 2)
    ///   3. DEFAULT_CONTEXT_WINDOW (Layer 3, 200K)
    ///
    /// The resolved cap is then clamped to the model's actual context window:
    ///   effective = min(resolved_cap, model.effective_input_budget)
    ///
    /// When resolved_cap == 0, no cap is applied (use model's full capacity).
    pub fn context_trim_budget(&self, model_name: &str) -> u64 {
        let max_output_limit = self.max_output_tokens_limit_for_model(model_name);
        let resolved_cap = self
            .context_window_override
            .or(self.manifest_context_window)
            .unwrap_or(crate::config::DEFAULT_CONTEXT_WINDOW);

        self.get_model_capabilities(model_name)
            .map(|caps| {
                let model_budget = caps.effective_input_budget(max_output_limit);
                let effective = if resolved_cap == 0 {
                    // No user-imposed cap — use model's full capacity
                    model_budget
                } else {
                    std::cmp::min(resolved_cap, model_budget)
                };
                tracing::debug!(
                    model = %model_name,
                    context_window = caps.context_window,
                    max_input_tokens = ?caps.max_input_tokens,
                    max_output_tokens_limit = max_output_limit,
                    resolved_cap,
                    model_budget,
                    effective,
                    "Computed usable context budget (capped by per-agent context_window)"
                );
                effective
            })
            .unwrap_or_else(|| {
                let fallback = if resolved_cap == 0 {
                    self.config.history_max_tokens
                } else {
                    std::cmp::min(resolved_cap, self.config.history_max_tokens)
                };
                tracing::debug!(
                    model = %model_name,
                    resolved_cap,
                    fallback,
                    "No model capabilities for '{}', using capped history_max_tokens as fallback",
                    model_name
                );
                fallback
            })
    }
}

impl Clone for AgentCore {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            manifest: self.manifest.clone(),
            provider: self.provider.clone(),
            builtin_tools: self.builtin_tools.clone(),
            mcp_tools: self.mcp_tools.clone(),
            all_tools: self.all_tools.clone(),
            global_provider_list: self.global_provider_list.clone(),
            provider_list_version: self.provider_list_version,
            provider_key_vault: self.provider_key_vault.clone(),
            provider_compact_models: self.provider_compact_models.clone(),
            temperature_override: self.temperature_override,
            manifest_temperature: self.manifest_temperature,
            context_window_override: self.context_window_override,
            manifest_context_window: self.manifest_context_window,
            approval_timeout_secs: self.approval_timeout_secs,
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
            // ADR-028: agent-scoped counters are intentionally SHARED across
            // clones. Cloning an `AtomicU64` snapshots the *current value*
            // (good — every session sees the latest), and updates from any
            // clone are visible to all other clones. This is exactly what we
            // want: a session-task or distillation spawn should keep
            // advancing the same agent-wide total.
            agent_total_input_tokens: AtomicU64::new(
                self.agent_total_input_tokens.load(Ordering::Acquire),
            ),
            agent_total_output_tokens: AtomicU64::new(
                self.agent_total_output_tokens.load(Ordering::Acquire),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::protocol::{ModelCapabilitiesInfo, ProviderListItem, ProviderModelEntry};
    use acowork_core::providers::mock::MockProvider;
    use crate::config::RuntimeConfig;

    /// Build a minimal AgentCore for testing context_trim_budget.
    fn make_core(
        context_window_override: Option<u64>,
        manifest_context_window: Option<u64>,
        model_caps: Option<ModelCapabilitiesInfo>,
        history_max_tokens: u64,
    ) -> AgentCore {
        let config = RuntimeConfig {
            history_max_tokens,
            ..RuntimeConfig::default()
        };
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.cw"
            version = "1.0.0"
            name = "Test CW"
            description = "Context window test agent"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(MockProvider::single_text("test"));

        let mut core = AgentCore::new(config, manifest, provider, vec![]);
        core.context_window_override = context_window_override;
        core.manifest_context_window = manifest_context_window;

        if let Some(caps) = model_caps {
            let model = ProviderListItem {
                id: "test-provider".into(),
                base_url: "http://localhost".into(),
                protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
                models: vec![ProviderModelEntry {
                    id: "test-model".into(),
                    capabilities: caps,
                    max_output_tokens_limit: 32_768,
                }],
                compact_model: None,
                custom: false,
            };
            let mut list = core.global_provider_list.write().unwrap();
            *list = vec![model];
        }
        core
    }

    fn test_model_caps(context_window: u64, max_output_tokens: u64) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
        }
    }

    // ── Resolution chain tests ──────────────────────────────────────

    #[test]
    fn test_resolution_chain_config_wins_over_manifest() {
        // Layer 1: config=100K, Layer 2: manifest=64K → should use 100K
        // Model: 200K, so min(100K, 200K-reserve) = 100K - 32768 = 67232
        let core = make_core(
            Some(100_000),
            Some(64_000),
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        // effective_input_budget: min(200K, 200K-16K=184K) given max_output_limit=32K
        // Actually effective_input_budget = context_window - min(max_output_tokens, max_output_tokens_limit)
        // = 200_000 - min(16_384, 32_768) = 200_000 - 16_384 = 183_616
        // min(100_000, 183_616) = 100_000
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 100_000);
    }

    #[test]
    fn test_resolution_chain_manifest_over_default() {
        // Layer 1: None, Layer 2: 64K → should use 64K
        let core = make_core(
            None,
            Some(64_000),
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 64_000);
    }

    #[test]
    fn test_resolution_chain_falls_back_to_default() {
        // Both None → should use DEFAULT_CONTEXT_WINDOW (200K)
        // Model is also 200K, so min(200K, 200K-16K) = 184K?
        // Actually: min(200_000, 183_616) = 183_616
        let core = make_core(
            None,
            None,
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        // DEFAULT_CONTEXT_WINDOW=200K > model_budget=183_616 → min = 183_616
        assert_eq!(budget, 183_616);
    }

    #[test]
    fn test_resolution_chain_default_smaller_than_model() {
        // Both None, model=1M → default 200K caps it
        let core = make_core(
            None,
            None,
            Some(test_model_caps(1_000_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        // DEFAULT_CONTEXT_WINDOW=200K < model_budget=983_616 → min = 200_000
        assert_eq!(budget, 200_000);
    }

    // ── Zero = no limit tests ───────────────────────────────────────

    #[test]
    fn test_zero_cap_means_no_user_limit() {
        // config=0 → no cap → use model's full budget
        let core = make_core(
            Some(0),
            None,
            Some(test_model_caps(500_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        // model_budget = 500_000 - 16_384 = 483_616
        assert_eq!(budget, 483_616);
    }

    #[test]
    fn test_zero_cap_with_small_model() {
        // config=0, small model → use model's budget (no user cap)
        let core = make_core(
            Some(0),
            None,
            Some(test_model_caps(32_000, 4_096)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        // model_budget = 32_000 - 4_096 = 27_904
        assert_eq!(budget, 27_904);
    }

    // ── min(user_cap, model_budget) tests ──────────────────────────

    #[test]
    fn test_user_cap_smaller_than_model() {
        // User sets 50K, model has 200K → use 50K
        let core = make_core(
            Some(50_000),
            None,
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 50_000);
    }

    #[test]
    fn test_model_smaller_than_user_cap() {
        // User sets 500K, model has 128K → use model's budget
        let core = make_core(
            Some(500_000),
            None,
            Some(test_model_caps(128_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        // model_budget = 128_000 - 16_384 = 111_616
        assert_eq!(budget, 111_616);
    }

    #[test]
    fn test_user_cap_exactly_equals_model_budget() {
        // User sets 183_616 (200K-16K-384), model has 200K → min == user_cap
        // but user_cap = 183_616 and model_budget = 200_000 - 16_384 = 183_616
        // min(183_616, 183_616) = 183_616
        let core = make_core(
            Some(183_616),
            None,
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 183_616);
    }

    // ── No model capabilities fallback tests ────────────────────────

    #[test]
    fn test_no_model_caps_falls_back_to_history_max_tokens() {
        // No model capabilities → use history_max_tokens capped by user setting
        let core = make_core(
            Some(80_000),
            None,
            None,
            64_000,
        );
        let budget = core.context_trim_budget("test-model");
        // user_cap=80K, history_max_tokens=64K → min(80K, 64K) = 64K
        assert_eq!(budget, 64_000);
    }

    #[test]
    fn test_no_model_caps_zero_user_cap() {
        // No model capabilities + user_cap=0 → use history_max_tokens directly
        let core = make_core(
            Some(0),
            None,
            None,
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 128_000);
    }

    #[test]
    fn test_no_model_caps_default_fallback() {
        // No model capabilities, no user cap → min(DEFAULT_CONTEXT_WINDOW=200K, history=128K) = 128K
        let core = make_core(
            None,
            None,
            None,
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 128_000);
    }

    // ── Manifest layer tests ────────────────────────────────────────

    #[test]
    fn test_manifest_context_window_is_honored() {
        // Config=None, Manifest=150K, Model=200K → min(150K, 183K) = 150K
        let core = make_core(
            None,
            Some(150_000),
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 150_000);
    }

    #[test]
    fn test_manifest_context_window_capped_by_model() {
        // Config=None, Manifest=300K, Model=128K → min(300K, 111K) = 111K
        let core = make_core(
            None,
            Some(300_000),
            Some(test_model_caps(128_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 111_616);
    }

    #[test]
    fn test_manifest_zero_means_no_limit() {
        // Config=None, Manifest=0 → manifest says "no limit" → model full capacity
        // Model=500K → 500_000 - 16_384 = 483_616
        let core = make_core(
            None,
            Some(0),
            Some(test_model_caps(500_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 483_616);
    }

    // ── Edge case tests ─────────────────────────────────────────────

    #[test]
    fn test_user_cap_of_one_token() {
        // Extreme: user sets 1 token
        let core = make_core(
            Some(1),
            None,
            Some(test_model_caps(128_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        assert_eq!(budget, 1);
    }

    #[test]
    fn test_model_budget_zero_edge_case() {
        // Model with 0 context_window (invalid but defensive)
        let core = make_core(
            Some(100_000),
            None,
            Some(test_model_caps(0, 0)),
            128_000,
        );
        let budget = core.context_trim_budget("test-model");
        // effective_input_budget: 0 - 0 = 0; min(100K, 0) = 0
        assert_eq!(budget, 0);
    }

    // ── max_output_tokens_limit interaction ──────────────────────────

    #[test]
    fn test_max_output_tokens_limit_affects_model_budget() {
        // Model: 200K, max_output=16K, limit=8K → effective_input = 200K - min(16K, 8K) = 192K
        // User cap=100K → min(100K, 192K) = 100K
        let core = make_core(
            Some(100_000),
            None,
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        // Override max_output_tokens_limit for the model to 8K
        {
            let mut list = core.global_provider_list.write().unwrap();
            for p in list.iter_mut() {
                for m in p.models.iter_mut() {
                    m.max_output_tokens_limit = 8_192;
                }
            }
        }
        let budget = core.context_trim_budget("test-model");
        // model_budget = 200_000 - min(16_384, 8_192) = 200_000 - 8_192 = 191_808
        // min(100_000, 191_808) = 100_000
        assert_eq!(budget, 100_000);
    }
}
