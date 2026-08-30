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

use acowork_core::protocol::{ModelCapabilitiesInfo, ProtocolType, ProviderListItem};
use acowork_core::providers::traits::{Provider, UsageInfo};
use acowork_core::rag::RagProvider;
use acowork_core::tools::traits::Tool;
use acowork_memory::admin::MemoryAdminService;
use acowork_memory::consolidation::SchedulerConfig;
#[cfg(feature = "grafeo-backend")]
use acowork_memory::types::{AutobioCategory, AutobiographicalNode, NodeStatus};
use acowork_memory::MemoryProvider;
#[cfg(feature = "grafeo-backend")]
use chrono::Utc;

use crate::config::RuntimeConfig;
use crate::debug::DebugObserverSlot;
use crate::embedding::EmbeddingProvider;
use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::memory::{MemoryManager, MemoryManagerConfig};
use crate::security::approval_gate::ApprovalGate;
use acowork_core::ShellApprovalThreshold;

/// A builtin tool entry — wraps the raw `Arc<dyn Tool>` together with
/// its per-agent `enabled` flag (ADR-029).
///
/// `enabled == false` tools are still present in
/// [`AgentCore::builtin_tools`] (so the frontend can render the full
/// list with checkboxes), but they are filtered out of
/// [`AgentCore::all_tools`] (the LLM dispatch list).
///
/// Clone is cheap because `Arc<dyn Tool>` is internally reference-counted.
///
/// **ADR-030 C3**: visibility was raised from `pub(crate)` to `pub` so that
/// `SessionMessage::AddDynamicBuiltinTool` (a `pub` enum) can carry this
/// type as a field. The runtime is a single crate (`acowork-runtime`) and
/// does not re-export `BuiltinToolEntry`, so this is effectively an
/// internal-only widening.
#[derive(Clone)]
pub struct BuiltinToolEntry {
    /// The tool implementation (cloned from the registry).
    pub tool: Arc<dyn Tool>,
    /// Whether this tool is enabled for the current agent.
    pub enabled: bool,
}

impl BuiltinToolEntry {
    /// Tool name (delegated to the underlying `Tool`).
    ///
    /// Returns a `String` rather than `&str` for backward compatibility
    /// with the many call sites that store the result in a local
    /// variable and compare against it. The `Tool` trait's `name()` API
    /// returns `String` so this is a thin allocation each call; given
    /// that builtin_tools is small (tens of entries), this is fine.
    pub fn name(&self) -> String {
        self.tool.name()
    }

    /// Tool specification (delegated to the underlying `Tool`).
    pub fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
        self.tool.spec()
    }
}

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
    /// rebuilding `all_tools` whenever `mcp_tools` or `builtin_tools`
    /// enabled flags change. These are the tools shipped with the
    /// runtime binary and registered in `crate::tools::builtin`. Each
    /// entry carries its own `enabled` flag (ADR-029); only `enabled`
    /// entries are merged into [`Self::all_tools`] by `rebuild_all_tools`.
    /// MCP tools are kept separate in [`Self::mcp_tools`].
    pub(crate) builtin_tools: Vec<BuiltinToolEntry>,
    /// MCP (Model Context Protocol) tool wrappers, populated when MCP servers
    /// have been connected. These are merged into [`all_tools`] at rebuild time.
    pub(crate) mcp_tools: Option<Vec<Arc<dyn Tool>>>,
    /// Merged tool list for dispatch — contains enabled built-in tools + MCP tools.
    pub(crate) all_tools: Vec<Arc<dyn Tool>>,
    /// Global provider list — full metadata including models, capabilities,
    /// base_url, protocol_type, compact_model for all configured providers.
    pub(crate) global_provider_list: Arc<RwLock<Vec<ProviderListItem>>>,
    /// Provider list version for diff sync with Gateway.
    pub(crate) provider_list_version: u64,
    /// Provider key vault (in-memory only, never persisted).
    pub(crate) provider_key_vault: Arc<RwLock<HashMap<String, String>>>,
    /// Search key vault (in-memory only, never persisted).
    ///
    /// Shared with `WebSearchEngine` - when `SessionManager::update_search_config`
    /// writes to this Arc (triggered by MQTT `acowork/global/searches`), the
    /// search engine reads the updated keys on the next `search()` call.
    pub(crate) search_key_vault: Arc<RwLock<HashMap<String, String>>>,
    /// Shared search provider list.
    ///
    /// Same sharing semantics as [`Self::search_key_vault`]. Read by
    /// `WebSearchEngine` at search time to determine which providers are
    /// configured and in what order.
    pub(crate) search_provider_list: Arc<RwLock<Vec<acowork_core::protocol::SearchProviderListItem>>>,

    /// Per-agent compatibility cache, shared across all provider instances
    /// (including those rebuilt by `build_provider_for`).  `None` when no
    /// provider was available at startup (noop fallback).
    pub(crate) compat_cache: Option<Arc<crate::providers::compat::CompatCache>>,

    /// Provider→compact_model mapping from provider_list at AgentHello.
    pub(crate) provider_compact_models: HashMap<String, Option<String>>,
    /// ADR-056: Global default compact model reference — `(provider_id, model_id)`.
    /// Set from `AvailableProviders.default_compact_model` at session init.
    /// Top-priority candidate in the distillation fallback chain
    /// (`resolve_distill_model`). `None` means no global override; runtime
    /// then falls back to `provider_compact_models` (Level 2) and finally the
    /// session's current chat model (Level 3).
    pub(crate) default_compact_model: Option<(String, String)>,
    /// LLM temperature override (from Gateway config via agent_config.json).
    /// Level 1 in the resolution chain.
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
    /// ADR-032: number of recent tool results preserved raw at every trigger
    /// System prompt override (from Gateway config).
    pub(crate) system_prompt_override: Option<String>,
    /// Agent-specific compaction prompt (from `prompts/summary.md` in the
    /// .agent package). `None` = use the built-in
    /// [`crate::prompt::COMPACTION_SYSTEM_PROMPT`] fallback.
    ///
    /// This is the package-declared summarization directive for context
    /// compaction and episode distillation. It is deliberately independent
    /// of `system_prompt_override` (which covers the MAIN dialog prompt
    /// only) — compaction is a summarization task with its own directive,
    /// and per-agent rules belong to the package, not the runtime config.
    pub(crate) compaction_prompt: Option<String>,
    /// Grafeo memory store (shared across all sessions of this agent).
    /// ADR-051 P4: Primary field is `memory_provider` (trait object).
    /// `memory_admin` is the admin interface for HTTP endpoints and
    /// embedding migration.
    pub(crate) memory_provider: Option<Arc<dyn MemoryProvider>>,
    /// Admin service for HTTP memory endpoints and embedding migration.
    /// ADR-051 P4: Replaces the former `grafeo_store` compat field.
    pub(crate) memory_admin: Option<Arc<dyn MemoryAdminService>>,
    /// RAG provider (enterprise knowledge retrieval, orthogonal to MemoryProvider).
    /// ADR-051 C3: New field, independent from memory_provider.
    pub(crate) rag_provider: Option<Arc<dyn RagProvider>>,
    /// Debug observer slot — Production (no-op) or Dev (real observer).
    pub(crate) debug_observer: DebugObserverSlot,
    /// ADR-046: blob store for `file_upload` / `image_upload` items
    /// (uploaded PDFs, images, …). Populated in Phase B of `session_init`
    /// via [`AgentCore::set_attachment_service`]; the agent loop reads
    /// this slot to convert `image_upload` items into multimodal
    /// `ContentPart::ImageUrl` payloads before the LLM call.
    pub(crate) attachment_service: Option<Arc<dyn crate::usecases::AttachmentService>>,
    /// Approval gate for shell command risk confirmation.
    pub(crate) approval_gate: Option<Arc<dyn ApprovalGate>>,
    /// Shell approval threshold: Low / Medium / High / Auto-approve.
    pub(crate) shell_approval_threshold: ShellApprovalThreshold,
    /// Shell risk rules (loaded from config dir on startup).
    pub(crate) shell_risk_rules: crate::security::shell_risk::ShellRiskRules,
    /// Memory session handle — shared between agent loop and memory tools.
    pub(crate) memory_session: Option<Arc<crate::memory::MemorySessionHandle>>,
    /// Embedding provider for vector-based memory retrieval.
    pub(crate) embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// P3-1: Retrieval quality metrics aggregator (shared across sessions).
    /// ADR-051 C3: Replaced grafeo MetricsAggregator with Runtime-internal
    /// RetrievalMetricsAggregator (data from acowork_memory::RetrievalMetrics).
    pub(crate) metrics_aggregator: Arc<std::sync::Mutex<crate::memory::RetrievalMetricsAggregator>>,
    // ADR-051 C3: consolidation_scheduler field removed.
    // Consolidation control is delegated to MemoryProvider trait.
    // Background task handle kept for lifecycle management.
    pub(crate) consolidation_bg_task: Option<crate::memory::ConsolidationBgTask>,
    /// ADR-051 P4: Runtime-internal consolidation timer.
    /// Replaces grafeo's `ConsolidationScheduler`. The timer implements
    /// idle-timeout + accumulation-threshold scheduling policy without
    /// holding a store reference. `notify_consolidation_active()` resets
    /// the idle timer so consolidation doesn't run during active use.
    pub(crate) consolidation_timer: Option<Arc<crate::memory::ConsolidationTimer>>,
    /// ADR-028: cumulative input tokens across every LLM call made by this
    /// agent process. Sourced from `accumulate_llm_usage` on every LLM call
    /// and from `merge_token_totals` on every `list_sessions` scan. Not
    /// persisted; on process restart the next session-list scan rebuilds the
    /// baseline via atomic-max merge with the on-disk `SessionTokens`.
    pub(crate) agent_total_input_tokens: AtomicU64,
    /// ADR-028: cumulative output tokens across every LLM call made by this
    /// agent process. See [`Self::agent_total_input_tokens`] for semantics.
    pub(crate) agent_total_output_tokens: AtomicU64,

    /// ADR-061 §10.2: Shared queue for the `context_retrieve` tool (the
    /// manual recall channel after compaction). Created in agent_init
    /// and passed to the tool at registration time.
    pub(crate) retrieve_queue:
        crate::agent::context_compression::RetrieveQueue,
}

impl AgentCore {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_observer(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        builtin_tools: Vec<BuiltinToolEntry>,
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

        // Compute initial `all_tools` now (enabled builtin + MCP later).
        // We can't fully rebuild yet because MCP hasn't been connected,
        // but seeding with the enabled builtin subset keeps dispatch
        // working from the very first LLM call.
        let initial_all_tools: Vec<Arc<dyn Tool>> = builtin_tools
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.tool.clone())
            .collect();

        Self {
            config: config.clone(),
            manifest,
            provider,
            builtin_tools,
            mcp_tools: None,
            all_tools: initial_all_tools,
            global_provider_list: Arc::new(RwLock::new(Vec::new())),
            provider_list_version: 0,
            provider_key_vault: Arc::new(RwLock::new(HashMap::new())),
            search_key_vault: Arc::new(RwLock::new(HashMap::new())),
            search_provider_list: Arc::new(RwLock::new(Vec::new())),
            compat_cache: None,
            provider_compact_models: HashMap::new(),
            default_compact_model: None,
            temperature_override: None,
            manifest_temperature,
            context_window_override: None,
            manifest_context_window,
            approval_timeout_secs: None,
            system_prompt_override: None,
            // Populated in Phase B of session_init from prompts/summary.md
            // (see `load_compaction_prompt`); None until then.
            compaction_prompt: None,
            memory_provider: None,
            memory_admin: None,
            rag_provider: None,
            memory_session: None,
            debug_observer: observer,
            approval_gate: None,
            shell_approval_threshold,
            shell_risk_rules: crate::security::shell_risk::ShellRiskRules::load(
                std::path::Path::new(&config.work_dir),
            )
            .unwrap_or_default(),
            embedding_provider: None,
            metrics_aggregator: Arc::new(std::sync::Mutex::new(
                crate::memory::RetrievalMetricsAggregator::with_defaults(1.0),
            )),
            consolidation_bg_task: None,
            consolidation_timer: None,
            // ADR-046: blob store slot is empty by default; Phase B
            // populates it via `set_attachment_service` once the work_dir
            // is known. Until then, multimodal image parts cannot be
            // derived — chat messages fall back to plain text.
            attachment_service: None,
            // ADR-028: counters start at 0; the next `list_sessions` scan
            // rebuilds the baseline via `merge_token_totals`.
            agent_total_input_tokens: AtomicU64::new(0),
            agent_total_output_tokens: AtomicU64::new(0),
            retrieve_queue: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
        }
    }

    pub(crate) fn new(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        builtin_tools: Vec<BuiltinToolEntry>,
    ) -> Self {
        Self::new_with_observer(
            config,
            manifest,
            provider,
            builtin_tools,
            DebugObserverSlot::production(),
        )
    }

    /// Rebuild `all_tools` from the current `builtin_tools` (filtered by
    /// `enabled`) plus `mcp_tools`. Called whenever either set changes:
    /// - Agent startup (after `builtin_tools` is initialized with flags)
    /// - MCP connect/disconnect
    /// - `RuntimeConfigUpdate.builtin_tools_enabled` toggle
    ///   (ADR-061 §10.2: `context_retrieve` is always registered, so
    ///   there is no platform-tool hot-reload path anymore)
    pub(crate) fn rebuild_all_tools(&mut self) {
        let mut merged: Vec<Arc<dyn Tool>> = self
            .builtin_tools
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.tool.clone())
            .collect();
        if let Some(ref mcp) = self.mcp_tools {
            merged.extend(mcp.clone());
        }
        self.all_tools = merged;
    }

    /// Rewrite `builtin_tools` enabled flags from a desired
    /// `(name, enabled)` list - the **single policy** behind every
    /// builtin-tools enabled write path (SessionTask hot-update via
    /// `apply_builtin_tools_update`, SessionManager template sync via
    /// `apply_builtin_tools_enabled`).
    ///
    /// Policy (all of it delegated to
    /// [`crate::agent_config::apply_builtin_tools_patch`] with an empty
    /// patch, so the persistence layer and the in-memory layer can
    /// never diverge):
    /// - Platform-protected names ([`PLATFORM_PROTECTED_TOOLS`]) are
    ///   filtered out of the resolution - their enabled flag is
    ///   platform-managed (registered unconditionally by
    ///   [`crate::tools::builtin::all_builtin_tools`], ADR-061 §10.2),
    ///   never user-toggleable. Their registered slots keep the current
    ///   flag.
    /// - Names not currently registered are dropped (defensive against
    ///   drift between the persisted file and the code registry).
    /// - Only the `enabled` flag is rewritten; the wrapped `tool` impl
    ///   (security decorators applied during `ToolRegistry::activate`)
    ///   is preserved.
    ///
    /// Does NOT rebuild `all_tools` / `ContextBuilder.tool_definitions`;
    /// dependent refreshes are owned by the caller (one refresh site
    /// per layer - see `session_task::refresh_builtin_tools_dependents`).
    pub(crate) fn apply_builtin_enabled_entries(
        &mut self,
        entries: &[crate::agent_config::AgentToolEntry],
    ) {
        let resolved = crate::agent_config::apply_builtin_tools_patch(entries, &[]);
        let resolved_map: std::collections::HashMap<String, bool> = resolved
            .iter()
            .map(|e| (e.name.clone(), e.enabled))
            .collect();
        for entry in self.builtin_tools.iter_mut() {
            let name = entry.name().to_string();
            if let Some(&enabled) = resolved_map.get(&name) {
                entry.enabled = enabled;
            }
        }
    }

    pub fn config(&self) -> &RuntimeConfig { &self.config }
    pub fn manifest(&self) -> &acowork_core::AgentManifest { &self.manifest }
    pub fn provider(&self) -> &Arc<dyn Provider> { &self.provider }

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
            "Embedding provider updated at runtime via SidecarEndpointUpdate"
        );
    }

    /// Clear the embedding provider (set to `None`).
    ///
    /// Called when the embed sidecar goes down (SidecarEndpointUpdate with
    /// empty endpoint). The memory store should gracefully degrade -
    /// embedding-dependent operations will fail until a new provider is
    /// pushed (ADR-030 review ISSUE-2 fix).
    pub fn clear_embedding_provider(&mut self) {
        let old_name = self
            .embedding_provider
            .as_ref()
            .map(|p| p.name())
            .unwrap_or("none")
            .to_string();
        self.embedding_provider = None;
        tracing::info!(
            old_provider = %old_name,
            "Embedding provider cleared (embed sidecar unavailable)"
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

        // ADR-061 §10.2: the `tool_compression_enabled` toggle and its
        // platform-tools hot-reload are deleted; `context_retrieve` is
        // always registered and `context_abandon` is never registered.
    }

    pub fn init_memory_provider(&mut self, work_dir: &std::path::Path) {
        if self.memory_provider.is_some() {
            tracing::debug!("init_memory_provider: already initialized, skipping");
            return;
        }
        let memory_dir = work_dir.join("memory");
        if let Err(e) = std::fs::create_dir_all(&memory_dir) {
            tracing::warn!(error = %e, dir = %memory_dir.display(), "Failed to create memory directory, memory features disabled");
            return;
        }
        #[cfg(feature = "grafeo-backend")]
        {
            self.init_grafeo_backend(&memory_dir);
        }

        #[cfg(not(feature = "grafeo-backend"))]
        {
            let _ = &memory_dir;
            tracing::warn!(
                "init_memory_provider: grafeo-backend feature not enabled, memory features disabled"
            );
        }
    }

    /// Create and initialise a GrafeoStore as the memory provider.
    ///
    /// ADR-051 P4: Feature-gated behind `grafeo-backend`. When the feature
    /// is disabled, `init_memory_provider` logs a warning and skips.
    #[cfg(feature = "grafeo-backend")]
    fn init_grafeo_backend(&mut self, memory_dir: &std::path::Path) {
        use acowork_grafeo::grafeo::GrafeoStore;
        use acowork_grafeo::types::{GrafeoConfig, DEFAULT_EMBEDDING_DIM};

        let db_path = memory_dir.join("private.grafeo");
        let embedding_dim = self.embedding_provider.as_ref().map(|p| p.dimension()).unwrap_or_else(|| {
            tracing::warn!(
                default_dim = DEFAULT_EMBEDDING_DIM,
                "⚠️ Embedding provider unavailable - opening GrafeoStore with default dim {}. \
                 If the on-disk store was created with a different dim, vector search will fail \
                 (HNSW index creation will warn) and memory will fall back to text-only search. \
                 Restart runtime after the embedding service is back online to use vector search.",
                DEFAULT_EMBEDDING_DIM
            );
            DEFAULT_EMBEDDING_DIM
        });
        let config = GrafeoConfig { db_path: db_path.clone(), embedding_dim };
        match GrafeoStore::open(&config) {
            Ok(store) => {
                let graph = store.db().graph_store();
                let existing: usize = ["Episodic", "Knowledge", "Procedural", "Autobiographical"]
                    .iter().map(|l| graph.nodes_by_label(l).len()).sum();
                tracing::info!(path = %db_path.display(), existing_nodes = existing, "Grafeo memory store opened");
                let store_arc = Arc::new(store);
                self.bootstrap_autobiographical_from_manifest(&*store_arc);
                if let Some(ref session) = self.memory_session {
                    session.set_provider(store_arc.clone());
                }
                self.memory_admin = Some(store_arc.clone());
                self.memory_provider = Some(store_arc);
                self.start_consolidation_pipeline();
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %db_path.display(), "Failed to open Grafeo memory store, memory features disabled");
            }
        }
    }

    pub fn memory_provider(&self) -> Option<&Arc<dyn MemoryProvider>> {
        self.memory_provider.as_ref()
    }

    /// Admin service for HTTP memory endpoints and embedding migration.
    /// ADR-051 P4: Replaces the former `grafeo_store()` accessor.
    pub fn memory_admin(&self) -> Option<&Arc<dyn MemoryAdminService>> {
        self.memory_admin.as_ref()
    }

    /// Backward-compat alias for memory_provider(). ADR-051 C3.
    pub fn memory_store(&self) -> Option<&Arc<dyn MemoryProvider>> {
        self.memory_provider.as_ref()
    }

    #[cfg(feature = "grafeo-backend")]
    fn bootstrap_autobiographical_from_manifest(&self, provider: &dyn MemoryProvider) {
        match provider.find_autobiographical_by_category(AutobioCategory::Identity) {
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
            if let Err(e) = provider.store_autobiographical(&node) {
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
            if let Err(e) = provider.store_autobiographical(&node) {
                tracing::warn!(capability = %cap_key, error = %e, "Failed to bootstrap Autobiographical/Capability node");
            }
        }
        tracing::info!(identity_count = identity_entries.len(), capability_count = manifest.capabilities.len(), "Bootstrapped Autobiographical nodes from manifest");
    }

    pub fn init_memory_manager(&self) -> MemoryManager {
        MemoryManager::new(MemoryManagerConfig::default())
    }

    pub fn start_consolidation_pipeline(&mut self) {
        let Some(ref provider) = self.memory_provider else {
            tracing::debug!("Cannot start consolidation: memory provider not initialized");
            return;
        };
        let Some(ref embedding) = self.embedding_provider else {
            tracing::warn!("Cannot start consolidation pipeline: embedding provider not available. Background memory consolidation (generalization, conflict resolution) is disabled until embedding service is back.");
            return;
        };
        if self.consolidation_bg_task.is_some() {
            tracing::debug!("Consolidation pipeline already running");
            return;
        }
        use crate::memory::consolidation_bg::{ConsolidationParams, start_consolidation_pipeline};
        use std::time::Duration;
        let model = {
            let list = self.global_provider_list.read().unwrap();
            list.iter().flat_map(|p| p.models.iter()).next().map(|m| m.id.clone()).unwrap_or_else(|| "default".to_string())
        };
        let params = ConsolidationParams {
            provider: provider.clone(), llm_provider: self.provider.clone(), model,
            embedding_provider: embedding.clone(), scheduler_config: SchedulerConfig::default(),
            poll_interval: Duration::from_secs(60),
            work_dir: Some(std::path::PathBuf::from(&self.config.work_dir)),
        };
        let (timer, bg_task) = start_consolidation_pipeline(params);
        self.consolidation_bg_task = Some(bg_task);
        self.consolidation_timer = Some(timer);
        // ADR-051 C3: Also notify the provider to start its internal consolidation.
        if let Some(ref provider) = self.memory_provider {
            let _ = provider.start_consolidation(&SchedulerConfig::default());
        }
        tracing::info!("Consolidation background pipeline started");
    }

    pub async fn notify_consolidation_active(&self) {
        // ADR-051 P4: Reset the Runtime-internal ConsolidationTimer's idle
        // timer so consolidation doesn't run during active use.
        if let Some(ref timer) = self.consolidation_timer {
            timer.notify_active().await;
        }
        // Also notify the Provider (for engines that manage their own
        // scheduling internally). GrafeoStore's impl is a no-op today.
        if let Some(ref provider) = self.memory_provider {
            provider.notify_consolidation_active().await;
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

    /// ADR-056: Whether the global default compact model's provider is
    /// actually usable right now. A provider is usable when either:
    ///
    ///   1. it has a non-empty API key in the in-memory vault (cloud
    ///      providers), or
    ///   2. it is a local provider — no key required, reachable via a
    ///      local base_url (Ollama native protocol, or any base_url
    ///      pointing at localhost / 127.0.0.1 / 0.0.0.0 / ::1).
    ///
    /// This is what `resolve_distill_model` consults before accepting
    /// Level 1 — without the local-provider branch, a user-chosen Ollama
    /// default (ADR-056 §2.3 "chat 用 deepseek,蒸馏用本地 qwen2.5:0.5b")
    /// would always be rejected and the feature would never fire.
    pub fn is_default_compact_provider_available(&self) -> bool {
        let Some((pid, _)) = self.default_compact_model.as_ref() else {
            return false;
        };
        // 1) Cloud provider with a configured key.
        if self
            .get_provider_api_key(pid)
            .map(|k| !k.is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        // 2) Local provider — no key required, but must still be present in
        //    the provider list with a reachable local base_url.
        let list = self.global_provider_list.read().unwrap();
        list.iter()
            .find(|p| p.id == *pid)
            .is_some_and(|p| {
                matches!(p.protocol_type, ProtocolType::Ollama)
                    || crate::providers::is_local_base_url(&p.base_url)
            })
    }

    pub fn set_debug_mode(&mut self, observer: crate::debug::DebugObserverImpl) {
        tracing::info!(is_dev = crate::debug::observer::DebugObserver::is_dev_mode(&observer), "AgentCore::set_debug_mode called (observer pipeline)");
        self.debug_observer = DebugObserverSlot::dev(observer);
    }

    /// Tear down the DevMode observer on this `AgentCore`, restoring
    /// production-mode semantics. Symmetric counterpart to
    /// [`Self::set_debug_mode`] — used by the runtime
    /// `POST /api/debug/disable` flow (ADR-048 follow-up) to exit
    /// DevMode without an agent restart.
    ///
    /// After this call:
    /// - The `debug_observer` slot is `Production`; all `on_*` hooks
    ///   become no-ops at the compiler-discriminated level (same
    ///   zero-overhead guarantee as a never-enabled agent).
    /// - The pending-injection slot is also cleared, so any in-flight
    ///   rewind/patch dispatched before disable arrives cannot fire
    ///   after the toggle.
    pub fn clear_debug_mode(&mut self) {
        if !self.debug_observer.is_dev_mode() {
            tracing::debug!(
                "AgentCore::clear_debug_mode: no-op (observer already in Production)"
            );
            return;
        }
        tracing::info!("AgentCore::clear_debug_mode: dropping DevMode observer");
        self.debug_observer = DebugObserverSlot::production();
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

    /// ADR-046: bind the blob store used to read uploaded files. Called
    /// from Phase B of `session_init` once the workspace services are
    /// in place. `None` means image uploads are silently dropped at the
    /// multimodal layer (the metadata system entry is still written to
    /// JSONL — the agent will see the filename but not the picture).
    pub fn attachment_service(&self) -> Option<&Arc<dyn crate::usecases::AttachmentService>> {
        self.attachment_service.as_ref()
    }
    pub fn set_attachment_service(
        &mut self,
        svc: Arc<dyn crate::usecases::AttachmentService>,
    ) {
        self.attachment_service = Some(svc);
    }
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
            search_key_vault: self.search_key_vault.clone(),
            search_provider_list: self.search_provider_list.clone(),
            compat_cache: self.compat_cache.clone(),
            provider_compact_models: self.provider_compact_models.clone(),
            default_compact_model: self.default_compact_model.clone(),
            temperature_override: self.temperature_override,
            manifest_temperature: self.manifest_temperature,
            context_window_override: self.context_window_override,
            manifest_context_window: self.manifest_context_window,
            approval_timeout_secs: self.approval_timeout_secs,
            system_prompt_override: self.system_prompt_override.clone(),
            compaction_prompt: self.compaction_prompt.clone(),
            memory_provider: self.memory_provider.clone(),
            memory_admin: self.memory_admin.clone(),
            rag_provider: self.rag_provider.clone(),
            memory_session: self.memory_session.clone(),
            debug_observer: self.debug_observer.clone_production(),
            approval_gate: self.approval_gate.clone(),
            shell_approval_threshold: self.shell_approval_threshold,
            shell_risk_rules: self.shell_risk_rules.clone(),
            embedding_provider: self.embedding_provider.clone(),
            metrics_aggregator: self.metrics_aggregator.clone(),
            consolidation_bg_task: None, // sessions don't own bg task
            consolidation_timer: self.consolidation_timer.clone(), // shared timer for idle reset
            attachment_service: self.attachment_service.clone(),
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
            retrieve_queue: self.retrieve_queue.clone(),
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

    // ── apply_builtin_enabled_entries (ADR-052 §3.5 shared policy) ────
    //
    // The tests below pin the policy helper that BOTH the per-session
    // hot-update path (`session_task::apply_builtin_tools_update`) and
    // the SessionManager template-sync path
    // (`SessionManager::apply_builtin_tools_enabled`) funnel through.
    // Without these, the two paths could drift silently and reintroduce
    // the "registry vs persistence" bug 2 regression.

    use crate::agent_config::AgentToolEntry;

    /// Pre-populate `builtin_tools` with a small mixed set so the
    /// enabled-rewrite path has both platform and non-platform entries
    /// to work with. Mirrors a real boot where `context_retrieve` is
    /// always registered (ADR-061 §10.2).
    fn seed_with_mixed_builtins(core: &mut AgentCore) {
        use crate::tools::builtin::build_platform_protected_tools;
        // Add a non-platform tool for the rewrite to land on.
        struct DummyTool;
        #[async_trait::async_trait]
        impl acowork_core::tools::traits::Tool for DummyTool {
            fn name(&self) -> String {
                "shell".to_string()
            }
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: "shell".to_string(),
                    description: "test".to_string(),
                    input_schema: serde_json::json!({}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: String::new(),
                    error: None,
                    token_usage: None,
                })
            }
        }
        core.builtin_tools.push(BuiltinToolEntry::with_resolved_enabled(
            true,
            Arc::new(DummyTool),
        ));
        for tool in build_platform_protected_tools(
            &core.config.work_dir,
            core.retrieve_queue.clone(),
        ) {
            core.builtin_tools
                .push(BuiltinToolEntry::with_resolved_enabled(false, tool));
        }
        core.rebuild_all_tools();
    }

    #[test]
    fn apply_builtin_enabled_entries_filters_platform_tools_out_of_resolution() {
        // The shared policy MUST delegate to
        // `agent_config::apply_builtin_tools_patch` with an empty patch,
        // so platform-protected names are stripped from the resolution
        // map regardless of what the incoming `entries` say. If this
        // ever changes, the platform-protected UX invariant is broken.
        let mut core = make_core(Some(8192), None, None, 0);
        seed_with_mixed_builtins(&mut core);

        // Hostile patch tries to disable both platform tools.
        core.apply_builtin_enabled_entries(&[
            AgentToolEntry::new("shell", false),
            AgentToolEntry::new("context_retrieve", false),
            AgentToolEntry::new("context_abandon", false),
        ]);

        // Non-platform tool follows the patch verbatim.
        let shell = core
            .builtin_tools
            .iter()
            .find(|e| e.name() == "shell")
            .expect("shell must still be registered");
        assert!(
            !shell.enabled,
            "non-platform tool must follow the patch"
        );

        // Platform tools are filtered OUT of the resolution map, so
        // their registered slots keep whatever enabled flag they had
        // before the call (here, force-enabled by `with_resolved_enabled`
        // -> true). Only names actually registered are checked:
        // `context_abandon` is no longer registered at all (ADR-061
        // §10.2), so it is simply absent rather than disabled.
        for name in crate::tools::registry::PLATFORM_PROTECTED_TOOLS {
            let Some(entry) = core.builtin_tools.iter().find(|e| e.name() == *name)
            else {
                continue;
            };
            assert!(
                entry.enabled,
                "{name} must keep its prior enabled flag (platform tools are filter-out, not force-disable)"
            );
        }
    }

    #[test]
    fn apply_builtin_enabled_entries_drops_unknown_names() {
        // Defensive: an `entries` payload that mentions a tool name not
        // currently in `builtin_tools` must be silently dropped, not
        // panic and not auto-register it (that would let a hostile
        // `agent_tools.json` smuggle arbitrary tools into the registry).
        let mut core = make_core(Some(8192), None, None, 0);
        seed_with_mixed_builtins(&mut core);
        let before = core.builtin_tools.len();

        core.apply_builtin_enabled_entries(&[
            AgentToolEntry::new("shell", false),
            AgentToolEntry::new("totally_made_up_tool", true),
        ]);

        assert_eq!(
            core.builtin_tools.len(),
            before,
            "unknown names must not introduce new registry entries"
        );
    }

    #[test]
    fn apply_builtin_enabled_entries_preserves_wrapped_tool_impl() {
        // The shared policy only rewrites the `enabled` flag. The
        // wrapped `tool` impl (security decorators applied during
        // `ToolRegistry::activate`) must survive untouched.
        let mut core = make_core(Some(8192), None, None, 0);
        seed_with_mixed_builtins(&mut core);

        let shell_before: Arc<dyn acowork_core::tools::traits::Tool> = core
            .builtin_tools
            .iter()
            .find(|e| e.name() == "shell")
            .unwrap()
            .tool
            .clone();

        core.apply_builtin_enabled_entries(&[AgentToolEntry::new("shell", false)]);

        let shell_after: Arc<dyn acowork_core::tools::traits::Tool> = core
            .builtin_tools
            .iter()
            .find(|e| e.name() == "shell")
            .unwrap()
            .tool
            .clone();
        assert!(
            Arc::ptr_eq(&shell_before, &shell_after),
            "the wrapped tool impl must be the same Arc (security decorators preserved)"
        );
    }


    // ── Consolidation timer integration tests (ADR-051 P4) ──────────

    /// G1: Verify that `start_consolidation_pipeline` stores the timer
    /// in `AgentCore.consolidation_timer` and that `notify_consolidation_active`
    /// resets the timer's idle clock.
    ///
    /// This is the integration-level regression for the P0 bug where the
    /// timer was created but immediately dropped (not stored), causing
    /// `notify_consolidation_active` to be a no-op and consolidation to
    /// fire unconditionally on every idle-timeout tick.
    #[tokio::test]
    async fn test_consolidation_timer_stored_and_notify_resets_idle() {
        use acowork_core::embedding::EmbeddingProvider;
        use acowork_grafeo::GrafeoStore;

        let mut core = make_core(Some(8192), None, None, 0);

        // Set up a real in-memory GrafeoStore as the memory provider.
        let store: Arc<dyn acowork_memory::MemoryProvider> =
            Arc::new(GrafeoStore::new_in_memory().unwrap());
        core.memory_provider = Some(store);

        // Set up a dummy embedding provider (required by start_consolidation_pipeline).
        struct DummyEmbeddingProvider;
        #[async_trait::async_trait]
        impl EmbeddingProvider for DummyEmbeddingProvider {
            fn name(&self) -> &str { "dummy" }
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, acowork_core::embedding::EmbeddingError> {
                Ok(vec![0.0; 384])
            }
            async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, acowork_core::embedding::EmbeddingError> {
                Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
            }
            fn dimension(&self) -> usize { 384 }
            async fn is_available(&self) -> bool { true }
        }
        core.embedding_provider = Some(Arc::new(DummyEmbeddingProvider));

        // Before starting: timer is None.
        assert!(core.consolidation_timer.is_none());
        assert!(core.consolidation_bg_task.is_none());

        // Start the consolidation pipeline.
        core.start_consolidation_pipeline();

        // After starting: timer must be stored.
        let timer = core.consolidation_timer
            .clone()
            .expect("consolidation_timer must be Some after start_consolidation_pipeline");
        assert!(core.consolidation_bg_task.is_some());

        // Wait a moment to let some "idle" time accumulate.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Call notify_consolidation_active - should reset idle timer.
        core.notify_consolidation_active().await;

        // Verify idle is near 0 via public accessor.
        let idle_secs = timer.idle_secs().await;
        assert!(
            idle_secs < 5,
            "Idle should be < 5s after notify_consolidation_active, got {idle_secs}s"
        );

        // Verify should_run does NOT trigger via idle (recently active, 0 pending).
        timer.update_pending_count(0).await;
        let trigger = timer.should_run().await;
        assert_eq!(trigger, None, "Should not trigger with 0 pending and recent activity");

        // Clean up: abort the bg task.
        if let Some(bg) = core.consolidation_bg_task.take() {
            bg.abort();
        }
    }

    /// G1b: Verify that `clone_for_session` shares the same timer Arc.
    /// When a session clone calls `notify_consolidation_active`, the
    /// original AgentCore's timer must also be reset (shared Arc).
    #[tokio::test]
    async fn test_consolidation_timer_shared_across_session_clone() {
        use acowork_core::embedding::EmbeddingProvider;
        use acowork_grafeo::GrafeoStore;

        let mut core = make_core(Some(8192), None, None, 0);

        let store: Arc<dyn acowork_memory::MemoryProvider> =
            Arc::new(GrafeoStore::new_in_memory().unwrap());
        core.memory_provider = Some(store);

        struct DummyEmbeddingProvider;
        #[async_trait::async_trait]
        impl EmbeddingProvider for DummyEmbeddingProvider {
            fn name(&self) -> &str { "dummy" }
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, acowork_core::embedding::EmbeddingError> {
                Ok(vec![0.0; 384])
            }
            async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, acowork_core::embedding::EmbeddingError> {
                Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
            }
            fn dimension(&self) -> usize { 384 }
            async fn is_available(&self) -> bool { true }
        }
        core.embedding_provider = Some(Arc::new(DummyEmbeddingProvider));

        core.start_consolidation_pipeline();
        let original_timer = core.consolidation_timer.clone().unwrap();

        // Clone the core (simulates session creation).
        let session_core = core.clone();

        // The session clone must share the same timer Arc.
        let session_timer = session_core.consolidation_timer
            .as_ref()
            .expect("session clone must have consolidation_timer");
        assert!(
            Arc::ptr_eq(session_timer, &original_timer),
            "session clone must share the same timer Arc (not a copy)"
        );

        // Session clone resets the timer.
        session_core.notify_consolidation_active().await;

        // The original core's timer must reflect the reset.
        let idle_secs = original_timer.idle_secs().await;
        assert!(
            idle_secs < 5,
            "Original timer must reflect session clone's notify_active (shared Arc)"
        );

        // Clean up.
        if let Some(bg) = core.consolidation_bg_task.take() {
            bg.abort();
        }
    }

    /// G9: Verify that `AgentCore.rag_provider` is None by default
    /// and can be set / accessed. This tests the accessor path that
    /// `session_init` uses to propagate the RAG provider from boot
    /// context.
    #[test]
    fn test_rag_provider_default_none_and_settable() {
        let mut core = make_core(Some(8192), None, None, 0);

        // Default: no RAG provider.
        assert!(core.rag_provider.is_none());

        // Set a dummy RAG provider.
        use acowork_core::rag::RagProvider;
        struct DummyRag;
        #[async_trait::async_trait]
        impl RagProvider for DummyRag {
            fn name(&self) -> &str { "dummy_rag" }
            async fn query(&self, _query: &str) -> Vec<acowork_core::rag::AnnotatedRagResult> {
                Vec::new()
            }
            async fn query_with_params(
                &self,
                _query: &str,
                _top_k: Option<u32>,
                _score_threshold: Option<f32>,
                _filters: Option<serde_json::Value>,
            ) -> Vec<acowork_core::rag::AnnotatedRagResult> {
                Vec::new()
            }
        }
        core.rag_provider = Some(Arc::new(DummyRag));

        // Verify it's accessible.
        assert!(core.rag_provider.is_some());
        assert_eq!(
            core.rag_provider.as_ref().unwrap().name(),
            "dummy_rag"
        );

        // Verify it survives clone_for_session.
        let session_core = core.clone();
        assert!(session_core.rag_provider.is_some());
        assert_eq!(
            session_core.rag_provider.as_ref().unwrap().name(),
            "dummy_rag"
        );
    }
}
