//! Gateway global state

use serde::Serialize;

use crate::budget::tracker::BudgetTracker;
use crate::capability::registry::CapabilityRegistry;
use crate::cron::CronScheduler;
use crate::cron::store::CronStore;
use crate::interaction_store::InteractionStore;
use crate::lifecycle::embed::EmbedProcessState;
use crate::rate::bucket::RateLimiter;
use crate::resource_cache::ResourceCache;
use crate::vault::VaultFacade;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
/// Debug-only handle to the MQTT broker.
///
/// Wrapped in `tokio::sync::Mutex` because the `MqttBrokerHandle::shutdown`
/// method requires `&mut self`. The HTTP debug handlers acquire this lock
/// briefly during disconnect/reconnect tests.
///
/// **None** when MQTT is disabled in the Gateway config.
pub type MqttBrokerControlHandle = tokio::sync::Mutex<Option<crate::mqtt::MqttBrokerHandle>>;

/// System Agent ID — always auto-started with the Gateway.
///
/// ADR-055 Phase 2b.3: moved here from `lifecycle/manager.rs` (deleted).
/// This is a Gateway policy constant (the System Agent is privileged and
/// cannot be stopped by normal stop commands), not a node concern.
pub const SYSTEM_AGENT_ID: &str = "com.acowork.system";

/// Information about an installed agent
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_id: String,
    pub version: String,
    pub name: String,
    pub install_path: String,
    pub manifest: acowork_core::AgentManifest,
    /// Which node hosts this installed agent (ADR-055 §6.5).
    /// `"local"` for the Gateway's own machine; a remote node_id once
    /// the installed-package inventory is aggregated from node retained
    /// info (Phase 2b.3 / Phase 3).
    pub node_id: String,
}

/// Runtime DevMode activation state.
///
/// ADR-048 follow-up: Decoupled from the startup `--dev-mode` flag.
/// DevMode can now be flipped on at runtime via
/// `POST /api/agents/{id}/debug/enable` (Gateway) →
/// `POST /api/debug/enable` (Runtime), without restarting the agent.
/// The Gateway tracks the activation in
/// [`RunningAgentInfo::debug_state`] so the Desktop can render the
/// Debug Panel + the "Enable Debug" button correctly even when the
/// agent was started without `--dev-mode`.
///
/// State transitions:
///
/// ```text
///        ┌──── CLI flag --dev-mode ────┐
///        │                            ▼
///   Disabled ────────── runtime enable ─────► Enabled
///        ▲                                     │
///        └────────── (no disable path) ────────┘
/// ```
///
/// We do NOT expose a "disable debug" path today; once DevMode is live
/// the only way to turn it off is to restart the agent. This matches
/// the Runtime's own contract (see `enable_debug_mode` early-return
/// when `runtime_debug_handles` is already set) and avoids forcing
/// teardown logic onto the SessionTask mid-iteration.
///
/// Serialised as the lowercase string `"disabled"` / `"enabled"` so
/// the TypeScript `AgentStore.dev_mode_state` mapping is direct (no
/// SCREAMING_SNAKE_CASE noise on the wire). The TypeScript side
/// already uses these literal strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DebugState {
    /// DevMode is not active. Either the agent was started without
    /// `--dev-mode` and no runtime `POST /api/debug/enable` has been
    /// observed, or the runtime enable call failed.
    Disabled,
    /// DevMode is live. Either the agent was started with
    /// `--dev-mode`, or the Gateway has successfully proxied a
    /// `POST /api/agents/{id}/debug/enable` call that returned 200.
    Enabled,
}

/// Information about a running agent
#[derive(Debug, Clone)]
pub struct RunningAgentInfo {
    pub agent_id: String,
    pub pid: u32,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub workspace: String,
    /// Which node hosts this running Runtime (ADR-055 §6.5). `"local"`
    /// while the Gateway spawns Runtimes directly; a remote node_id once
    /// lifecycle is delegated to the Node control plane (Phase 2b.3).
    pub node_id: String,
    /// Whether the Agent has completed the gRPC AgentHello handshake
    pub connected: bool,
    /// Whether the Agent has completed SessionTask initialization and is ready to receive messages
    pub ready: bool,
    /// Whether the agent was started in developer mode (Debug Protocol enabled at boot).
    ///
    /// ADR-048 follow-up: this is now **startup intent**, not current
    /// capability. To check whether DevMode is actually live for a
    /// running agent, read [`Self::debug_state`] instead. `dev_mode=true`
    /// at spawn time implies `debug_state=Enabled`; `dev_mode=false`
    /// does NOT imply `debug_state=Disabled` — the runtime enable path
    /// can flip DevMode on after the fact.
    pub dev_mode: bool,
    /// Whether DevMode is actually live for the running agent right now.
    ///
    /// ADR-048 follow-up: distinct from `dev_mode` so the Desktop can
    /// tell apart "agent was started in dev mode" from "DevMode was
    /// just enabled at runtime". Updated:
    ///   - At spawn: `Enabled` if `dev_mode=true`, else `Disabled`.
    ///   - After a successful `POST /api/agents/{id}/debug/enable`
    ///     proxy call (`proxy_debug_rpc` in `http/proxy.rs`): `Enabled`.
    pub debug_state: DebugState,
    /// Debug Protocol port hint (set when dev_mode is true).
    ///
    /// ADR-048: kept for API stability; Runtime no longer binds it as a
    /// WebSocket listener. See `dev_mode` doc above.
    pub debug_port: Option<u16>,
    /// In-memory cache of the agent's workspace config JSON (ADR-009: pass-through).
    /// Populated by Runtime via UpdateWorkspaceConfig gRPC after AgentHello.
    /// Cleared when agent disconnects. NOT persisted to disk.
    pub workspace_config_json: Option<String>,
    /// Current embedding dimension (reported by Runtime during AgentHello).
    /// Used by Gateway to detect which agents need dimension migration.
    pub current_embed_dim: Option<usize>,
    /// Current embedding migration state.
    /// None = no migration in progress; Some = migration active.
    pub migration: Option<AgentMigrationState>,
}

/// Per-agent embedding migration state tracked by Gateway.
#[derive(Debug, Clone)]
pub struct AgentMigrationState {
    /// Correlation request ID
    pub request_id: String,
    /// New embed model ID
    pub target_model_id: String,
    /// New embedding dimension
    pub target_dimension: usize,
    /// Current progress: (rebuilt, total_scanned, errors, phase, label)
    pub progress: Option<(u64, u64, u64, String, String)>,
    /// Whether migration is complete
    pub done: bool,
    /// Error message if migration failed
    pub error: Option<String>,
}

/// ADR-059: Gateway bootstrap runtime state.
///
/// Aggregates every bootstrap-specific concern that lives on the shared
/// `GatewayState`:
/// - the [`BootstrapOrchestrator`] that aggregates subsystem readiness
///   into the bootstrap snapshot consumed by MQTT + HTTP,
/// - per-subsystem readiness handles (e.g. `vault`) that HTTP handlers
///   use to demote / restore readiness without touching the registry.
///
/// Kept separate from `GatewayState`'s stable fields so ADR-059 phases
/// can extend bootstrap state without widening the general contract.
///
/// NOTE: distinct from the wire-level
/// `acowork_core::mqtt_proto::BootstrapState` (the serialisable
/// snapshot). This struct is pure in-process state.
#[derive(Default)]
pub struct BootstrapState {
    /// The bootstrap orchestrator (Phase 1.2 wires this in).
    ///
    /// `None` during very early construction; set once during
    /// `Gateway::run` after the subsystem registry is built. Read by
    /// the MQTT BootstrapPublisher (Phase 1.1) and the HTTP
    /// `/api/bootstrap` handler (Phase 1.3) to expose the aggregated
    /// readiness snapshot.
    pub orchestrator: Option<std::sync::Arc<crate::bootstrap::BootstrapOrchestrator>>,
    /// Readiness handle for the `vault` subsystem (Phase 5.4).
    ///
    /// Registered during `Gateway::run` alongside the other bootstrap
    /// subsystems; stored here so the HTTP vault lock/unlock handlers
    /// can demote (`mark_booting` on user lock) and restore
    /// (`mark_ready` on unlock) the vault's readiness without holding
    /// a reference to the bootstrap registry itself. `None` before
    /// registration.
    pub vault_readiness_handle: Option<crate::bootstrap::SubsystemHandle>,
}

/// Shared permission store type (same as gRPC server)
/// Gateway state — shared mutable state for the entire Gateway process
pub struct GatewayState {
    /// Installed agents (agent_id → AgentInfo)
    pub installed_agents: HashMap<String, AgentInfo>,
    /// Running agents (agent_id → RunningAgentInfo)
    pub running_agents: HashMap<String, RunningAgentInfo>,
    /// Vault facade for key storage and distribution
    pub vault: VaultFacade,
    /// Budget tracker for usage limits
    budget_tracker: Option<BudgetTracker>,
    /// Rate limiter for API call throttling
    rate_limiter: Option<RateLimiter>,
    /// Capability registry for Intent routing
    pub capability_registry: CapabilityRegistry,
    /// Cron scheduler for time-based triggers
    pub cron_scheduler: CronScheduler,
    /// Cron persistence store
    pub cron_store: Option<std::sync::Arc<CronStore>>,
    /// Gateway configuration snapshot (for Config API)
    pub config: Option<crate::config::GatewayConfig>,
    /// Resource cache — versioned provider and MCP lists for AgentHello diff sync.
    /// Loaded at startup and rebuilt by HTTP handlers when resources change.
    pub resource_cache: ResourceCache,
    /// Embedding service process state (None if not started).
    pub embed_process: Option<EmbedProcessState>,
    /// Last user-interaction timestamp per agent (`agent_id` -> UTC time).
    /// In-memory mirror of the on-disk interaction store; source of truth
    /// for the `GET /api/agents` sort order. Persists across agent
    /// stop/restart because the key is the install id, not a run-instance.
    pub last_interactions: HashMap<String, DateTime<Utc>>,
    /// Disk-backed persistence for `last_interactions`. `None` means
    /// in-memory only (tests, package-manager helpers). `Some` in the
    /// real Gateway after `Gateway::run` initialises it from `data_dir`.
    pub interaction_store: Option<InteractionStore>,
    /// ADR-XXX: MQTT broker control handle for debug endpoints.
    ///
    /// Holds the broker handle so the HTTP debug layer can request
    /// graceful shutdown (e.g. for testing reconnection paths).
    /// Always `Some` because initialised in `GatewayState::new`.
    /// The inner `Option` is `None` when MQTT is disabled in config.
    pub mqtt_broker_control: Arc<MqttBrokerControlHandle>,
    /// ADR-055 Phase 5a: broker CONNECT auth inputs, kept so the debug
    /// `/api/debug/mqtt/start` endpoint can restart the broker with the
    /// same credential state. `None` when MQTT auth is disabled.
    pub mqtt_broker_auth: Option<crate::mqtt::broker::BrokerAuth>,
    /// ADR-055 D3: resolved advertise host for constructing endpoints
    /// distributed to Runtime / Desktop (embed, LSP, broker).
    ///
    /// Config value > auto-detected non-loopback IP > "127.0.0.1".
    /// Set once at startup from `Gateway::run` via
    /// [`Self::set_advertise_host`]. Tests default to "127.0.0.1".
    pub advertise_host: String,
    /// MQTT publisher ready-barrier handle (Fix 1).
    ///
    /// The publisher defers its first retained publish until
    /// [`crate::mqtt::MqttPublisherHandle::mark_ready`] is called via this
    /// handle. Set once by `Gateway::run` after `start()` spawns the
    /// publisher loop; read by the vault auto-unlock task and the
    /// local-node ready task to coordinate the barrier. `None` when
    /// MQTT is disabled.
    pub mqtt_publisher_handle: Option<crate::mqtt::MqttPublisherHandle>,
    /// ADR-059: stable identifier of this Gateway instance.
    ///
    /// UUID v4 lowercase hex, generated fresh on every Gateway process
    /// start (NOT persisted across restarts — see ADR-059 §5.1:
    /// "重启后换发"). Published as `BootstrapState.instance_id` (and
    /// surfaced via `GET /api/bootstrap`) so clients can distinguish
    /// a fresh Gateway from a retained re-delivery of a previous
    /// instance.
    ///
    /// Initialised to `String::new()` by [`GatewayState::new`] and
    /// overwritten by [`Self::set_instance_id`] during `Gateway::new`
    /// before the state is wrapped in a `SharedState`.
    pub instance_id: String,
    /// ADR-059: Gateway bootstrap phase state — orchestrator + the
    /// subsystem handles it coordinates. Grouped in one struct so
    /// `GatewayState` stays a stable contract while this concern grows
    /// with each ADR-059 phase.
    pub bootstrap: BootstrapState,
    /// ADR-064: PM 独立进程状态（supervisor 管理）。
    ///
    /// 由 `Gateway::run` 启动 PM supervisor 后写入；`None` 表示 PM 进程
    /// 未启动（`pm.enabled=false`）或尚未 ready。HTTP 反代层
    /// （`http/pm_proxy.rs`）读取 `port` 构造代理目标；PM 未就绪时返回 503。
    pub pm_process: Option<crate::lifecycle::pm_supervisor::PmProcessState>,
    /// P3 T3-4: pm MCP HTTP 端点 URL（`http://{advertise_host}:{http.port}{pm.mcp_http_path}`）。
    ///
    /// 启动时在 `pm.auto_inject_mcp` 时设置；`Some` 表示
    /// `build_available_mcps` 应把 pm MCP 注入到 `acowork/global/mcps` 资源
    /// （每个 Agent 的 catalog），使 Agent 自动获得 `pm_*` 工具。
    pub pm_mcp_url: Option<String>,
}

impl GatewayState {
    /// Create new empty state with vault at the given directory
    pub fn new(vault_dir: &str) -> Self {
        Self {
            installed_agents: HashMap::new(),
            running_agents: HashMap::new(),
            vault: VaultFacade::new(vault_dir),
            budget_tracker: None,
            rate_limiter: None,
            capability_registry: CapabilityRegistry::new(),
            cron_scheduler: CronScheduler::new(),
            cron_store: None,
            config: None,
            resource_cache: ResourceCache::default(),
            embed_process: None,
            last_interactions: HashMap::new(),
            interaction_store: None,
            mqtt_broker_control: Arc::new(tokio::sync::Mutex::new(None)),
            mqtt_broker_auth: None,
            advertise_host: "127.0.0.1".to_string(),
            mqtt_publisher_handle: None,
            instance_id: String::new(),
            bootstrap: BootstrapState::default(),
            pm_process: None,
            pm_mcp_url: None,
        }
    }

    /// ADR-059: assign the Gateway's stable instance id.
    ///
    /// Called once during `Gateway::new` after the instance id is
    /// loaded (or freshly generated) from the data dir. The id is then
    /// used by the bootstrap orchestrator and by the MQTT/HTTP
    /// projections. Idempotent: a second call logs a warning and does
    /// NOT overwrite the existing id (mutating it mid-run would
    /// invalidate every already-published snapshot's `instance_id`).
    pub fn set_instance_id(&mut self, id: String) {
        if self.instance_id.is_empty() {
            self.instance_id = id;
        } else {
            tracing::warn!(
                current = %self.instance_id,
                ignored = %id,
                "GatewayState::set_instance_id called twice — keeping the first id"
            );
        }
    }

    /// ADR-059: attach the bootstrap orchestrator (Phase 1.2).
    ///
    /// Called once after the subsystem registry is built and the
    /// orchestrator is constructed. Subsequent calls replace the
    /// orchestrator; in normal Gateway operation this should only
    /// happen at construction time.
    pub fn set_bootstrap_orchestrator(
        &mut self,
        orchestrator: std::sync::Arc<crate::bootstrap::BootstrapOrchestrator>,
    ) {
        self.bootstrap.orchestrator = Some(orchestrator);
    }

    /// Check if an agent is installed
    pub fn is_installed(&self, agent_id: &str) -> bool {
        self.installed_agents.contains_key(agent_id)
    }

    /// Check if an agent is running
    pub fn is_running(&self, agent_id: &str) -> bool {
        self.running_agents.contains_key(agent_id)
    }

    /// Check if an agent is connected (gRPC AgentHello completed)
    pub fn is_connected(&self, agent_id: &str) -> bool {
        self.running_agents
            .get(agent_id)
            .map(|r| r.connected)
            .unwrap_or(false)
    }

    /// Set the connected state of a running agent
    pub fn set_agent_connected(&mut self, agent_id: &str, connected: bool) {
        if let Some(info) = self.running_agents.get_mut(agent_id) {
            info.connected = connected;
        }
    }

    /// Set the ready state of a running agent
    pub fn set_agent_ready(&mut self, agent_id: &str, ready: bool) {
        if let Some(info) = self.running_agents.get_mut(agent_id) {
            info.ready = ready;
        }
    }

    /// Add an installed agent
    pub fn add_installed(&mut self, info: AgentInfo) {
        // S4.2.2: Register capabilities from manifest on install
        self.capability_registry
            .register_from_manifest(&info.agent_id, &info.manifest);
        self.installed_agents.insert(info.agent_id.clone(), info);
    }

    /// Remove an installed agent
    pub fn remove_installed(&mut self, agent_id: &str) -> Option<AgentInfo> {
        // S4.2.3: Unregister capabilities on uninstall
        self.capability_registry.unregister_agent(agent_id);
        self.installed_agents.remove(agent_id)
    }

    /// ADR-055 §6.5: aggregate an installed-agent inventory entry
    /// reported by a node (retained `InstalledAgentInfo`). Rebuilds the
    /// `AgentInfo` from the `manifest.toml` payload and registers it
    /// (capabilities + install table). Idempotent: re-published retained
    /// entries on node (re)connect simply overwrite the same key.
    ///
    /// Returns `None` (and logs) when the embedded `manifest.toml` is
    /// invalid — the entry is skipped rather than poisoning the table.
    pub fn upsert_installed_from_node(
        &mut self,
        node_id: &str,
        entry: &acowork_core::mqtt_proto::InstalledAgentInfo,
    ) -> Option<String> {
        let manifest = match acowork_core::AgentManifest::from_toml(&entry.manifest_toml) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    node_id,
                    agent_id = %entry.agent_id,
                    error = %e,
                    "Installed-agent info carries invalid manifest.toml — ignoring"
                );
                return None;
            }
        };
        let agent_id = if entry.agent_id.is_empty() {
            manifest.agent_id.clone()
        } else {
            entry.agent_id.clone()
        };
        let info = AgentInfo {
            agent_id: agent_id.clone(),
            version: entry.version.clone(),
            name: entry.name.clone(),
            install_path: entry.install_path.clone(),
            manifest,
            node_id: node_id.to_string(),
        };
        self.add_installed(info);
        Some(agent_id)
    }

    /// Add a running agent
    pub fn add_running(&mut self, info: RunningAgentInfo) {
        self.running_agents.insert(info.agent_id.clone(), info);
    }

    /// Remove a running agent
    pub fn remove_running(&mut self, agent_id: &str) -> Option<RunningAgentInfo> {
        self.running_agents.remove(agent_id)
    }

    /// Get budget tracker (read-only)
    pub fn budget_tracker(&self) -> Option<&BudgetTracker> {
        self.budget_tracker.as_ref()
    }

    /// Get budget tracker (mutable)
    pub fn budget_tracker_mut(&mut self) -> Option<&mut BudgetTracker> {
        self.budget_tracker.as_mut()
    }

    /// Set budget tracker
    pub fn set_budget_tracker(&mut self, tracker: BudgetTracker) {
        self.budget_tracker = Some(tracker);
    }

    /// Get rate limiter (read-only)
    pub fn rate_limiter(&self) -> Option<&RateLimiter> {
        self.rate_limiter.as_ref()
    }

    /// Get rate limiter (mutable)
    pub fn rate_limiter_mut(&mut self) -> Option<&mut RateLimiter> {
        self.rate_limiter.as_mut()
    }

    /// Set rate limiter
    pub fn set_rate_limiter(&mut self, limiter: RateLimiter) {
        self.rate_limiter = Some(limiter);
    }

    /// ADR-055 D3: set the resolved advertise host used to construct
    /// embed / LSP / broker endpoints distributed to Runtime and
    /// Desktop. Called once at startup from `Gateway::run`.
    pub fn set_advertise_host(&mut self, host: String) {
        self.advertise_host = host;
    }

    /// Record a user-driven interaction for `agent_id` and persist
    /// if a disk-backed store is attached. Best-effort: a save failure
    /// is logged but does not propagate, so callers (HTTP handlers)
    /// stay non-blocking on persistence hiccups.
    pub fn touch_interaction(&mut self, agent_id: &str, when: DateTime<Utc>) {
        self.last_interactions.insert(agent_id.to_string(), when);
        if let Some(store) = &self.interaction_store
            && let Err(e) = store.save(&self.last_interactions)
        {
            tracing::warn!(
                error = %e,
                agent_id,
                "Failed to persist interaction store; in-memory state updated"
            );
        }
    }

    /// Look up the last user-interaction timestamp for `agent_id`.
    pub fn get_interaction(&self, agent_id: &str) -> Option<DateTime<Utc>> {
        self.last_interactions.get(agent_id).copied()
    }
}

impl Default for GatewayState {
    fn default() -> Self {
        Self::new("/tmp/acowork-vault-default")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("acowork-test-state-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    #[test]
    fn test_state_new() {
        let dir = temp_vault_dir("new");
        let state = GatewayState::new(&dir);
        assert!(state.installed_agents.is_empty());
        assert!(state.running_agents.is_empty());
        assert!(!state.vault.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_state_install_and_check() {
        let dir = temp_vault_dir("install");
        let mut state = GatewayState::new(&dir);
        assert!(!state.is_installed("com.example.weather"));

        let toml_str = r#"
            agent_id = "com.example.weather"
            version = "1.0.0"
            name = "Weather Agent"
            description = "Weather queries"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();

        state.add_installed(AgentInfo {
            agent_id: "com.example.weather".to_string(),
            version: "1.0.0".to_string(),
            name: "Weather Agent".to_string(),
            install_path: "/tmp/weather".to_string(),
            manifest,
            node_id: "local".to_string(),
        });
        assert!(state.is_installed("com.example.weather"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_state_running() {
        let dir = temp_vault_dir("running");
        let mut state = GatewayState::new(&dir);
        state.add_running(RunningAgentInfo {
            agent_id: "com.example.weather".to_string(),
            pid: 1234,
            started_at: chrono::Utc::now(),
            workspace: "/tmp/weather-workspace".to_string(),
            node_id: "local".to_string(),
            connected: false,
            ready: false,
            dev_mode: false,
            debug_state: DebugState::Disabled,
            debug_port: None,
            workspace_config_json: None,
            current_embed_dim: None,
            migration: None,
        });
        assert!(state.is_running("com.example.weather"));

        state.remove_running("com.example.weather");
        assert!(!state.is_running("com.example.weather"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_instance_id_default_is_empty() {
        let dir = temp_vault_dir("instance-id-default");
        let state = GatewayState::new(&dir);
        assert!(state.instance_id.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_set_instance_id_assigns() {
        let dir = temp_vault_dir("instance-id-set");
        let mut state = GatewayState::new(&dir);
        state.set_instance_id("instance-A".to_string());
        assert_eq!(state.instance_id, "instance-A");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_set_instance_id_is_idempotent() {
        let dir = temp_vault_dir("instance-id-idempotent");
        let mut state = GatewayState::new(&dir);
        state.set_instance_id("instance-A".to_string());
        // Second call must NOT overwrite — mutating the id mid-run
        // would invalidate every already-published snapshot's
        // instance_id.
        state.set_instance_id("instance-B".to_string());
        assert_eq!(state.instance_id, "instance-A");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
