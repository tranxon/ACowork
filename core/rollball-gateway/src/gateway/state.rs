//! Gateway global state

use std::collections::HashMap;
use std::sync::RwLock;
use crate::vault::VaultFacade;
use crate::budget::tracker::BudgetTracker;
use crate::rate::bucket::RateLimiter;
use crate::capability::registry::CapabilityRegistry;
use crate::cron::CronScheduler;
use crate::cron::store::CronStore;
use crate::permission_store::PermissionStore;
use rollball_core::permission::{PermissionMetrics, PermissionPolicyConfig};

/// Information about an installed agent
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_id: String,
    pub version: String,
    pub name: String,
    pub install_path: String,
    pub manifest: rollball_core::AgentManifest,
}

/// Information about a running agent
#[derive(Debug, Clone)]
pub struct RunningAgentInfo {
    pub agent_id: String,
    pub pid: u32,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub workspace: String,
    /// Whether the Agent has completed the gRPC AgentHello handshake
    pub connected: bool,
    /// Whether the Agent has completed SessionTask initialization and is ready to receive messages
    pub ready: bool,
    /// Whether the agent was started in developer mode (Debug Protocol enabled)
    pub dev_mode: bool,
    /// Debug WebSocket port (set when dev_mode is true)
    pub debug_port: Option<u16>,
}

/// Shared permission store type (same as IPC server)
pub type SharedPermissionStore = std::sync::Arc<PermissionStore>;

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
    /// Shared permission store (injected from IPC server at startup)
    /// This allows HTTP API and IPC server to share the same permission data.
    /// Set to None initially; populated by Gateway::run() before starting
    /// the HTTP server.
    pub permission_store: Option<SharedPermissionStore>,
    /// Gateway configuration snapshot (for Config API)
    pub config: Option<crate::config::GatewayConfig>,
    /// Shared IPC session manager (set during Gateway::run before IPC/HTTP start)
    pub ipc_sessions: Option<crate::http::routes::SharedSessionMgr>,
    /// Shared models.dev cache (set during Gateway::run before IPC/HTTP start).
    /// Allows IPC server to look up model capabilities with cache freshness.
    pub(crate) models_cache: Option<crate::http::models_api::ModelsCache>,
    /// Runtime-configurable permission policy overrides (S5.6).
    /// Shared between HTTP API and permission checker.
    pub permission_policy_config: std::sync::Arc<RwLock<PermissionPolicyConfig>>,
    /// Permission monitoring metrics (S5.7).
    /// Tracks cache hit rate, request latency, per-category stats.
    pub permission_metrics: std::sync::Arc<RwLock<PermissionMetrics>>,
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
            permission_store: None,
            config: None,
            ipc_sessions: None,
            models_cache: None,
            permission_policy_config: std::sync::Arc::new(RwLock::new(PermissionPolicyConfig::new())),
            permission_metrics: std::sync::Arc::new(RwLock::new(PermissionMetrics::new())),
        }
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
        self.capability_registry.register_from_manifest(
            &info.agent_id,
            &info.manifest,
        );
        self.installed_agents.insert(info.agent_id.clone(), info);
    }

    /// Remove an installed agent
    pub fn remove_installed(&mut self, agent_id: &str) -> Option<AgentInfo> {
        // S4.2.3: Unregister capabilities on uninstall
        self.capability_registry.unregister_agent(agent_id);
        self.installed_agents.remove(agent_id)
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
}

impl Default for GatewayState {
    fn default() -> Self {
        Self::new("/tmp/rollball-vault-default")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("rollball-test-state-{name}"));
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
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
        
        state.add_installed(AgentInfo {
            agent_id: "com.example.weather".to_string(),
            version: "1.0.0".to_string(),
            name: "Weather Agent".to_string(),
            install_path: "/tmp/weather".to_string(),
            manifest,
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
            connected: false,
            ready: false,
            dev_mode: false,
            debug_port: None,
        });
        assert!(state.is_running("com.example.weather"));
        
        state.remove_running("com.example.weather");
        assert!(!state.is_running("com.example.weather"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
