//! Agent process lifecycle manager

use std::path::PathBuf;
use crate::error::GatewayError;
use crate::gateway::state::{GatewayState, RunningAgentInfo};
use crate::lifecycle::process::{spawn_agent_process, kill_agent_process, check_health, find_available_debug_port};

/// System Agent ID — always auto-started with Gateway
pub const SYSTEM_AGENT_ID: &str = "com.rollball.system";

/// Lifecycle manager — controls Agent process lifecycle
pub struct LifecycleManager {
    /// Idle timeout in seconds (0 = no timeout)
    idle_timeout_secs: u64,
    /// Gateway gRPC endpoint URL passed to Runtime via --gateway-socket
    /// (e.g. "http://127.0.0.1:19877")
    gateway_grpc_endpoint: String,
    /// Log file max size in MB before auto-split
    log_file_size_mb: u64,
}

impl LifecycleManager {
    pub fn new(idle_timeout_secs: u64, gateway_grpc_endpoint: String, log_file_size_mb: u64) -> Self {
        Self { idle_timeout_secs, gateway_grpc_endpoint, log_file_size_mb }
    }

    /// Start an agent process
    ///
    /// If the agent declares `identity_deps` in its manifest, builds
    /// an identity delivery payload and writes it to the agent's
    /// workspace so the Runtime can inject it into the System Prompt
    /// during cold start.
    pub async fn start_agent(
        &mut self,
        agent_id: &str,
        state: &mut GatewayState,
        dev_mode: bool,
    ) -> Result<(), GatewayError> {
        // Check if already running
        if state.is_running(agent_id) {
            return Err(GatewayError::AgentAlreadyRunning(agent_id.to_string()));
        }

        // Check if installed
        let info = state.installed_agents.get(agent_id)
            .ok_or_else(|| GatewayError::AgentNotFound(agent_id.to_string()))?
            .clone();

        // Determine workspace directory
        let workspace = PathBuf::from(&info.install_path).join("workspace");
        std::fs::create_dir_all(&workspace)
            .map_err(|e| GatewayError::Lifecycle(format!("Failed to create workspace: {}", e)))?;

        // Build and deliver identity payload (cold-start injection)
        let identity_entries = self.build_identity_delivery(agent_id, state);
        if !identity_entries.is_empty() {
            let identity_path = workspace.join(".identity_delivery.json");
            let json = serde_json::to_string(&identity_entries)
                .map_err(|e| GatewayError::Lifecycle(
                    format!("Failed to serialize identity delivery: {}", e)
                ))?;
            std::fs::write(&identity_path, json)
                .map_err(|e| GatewayError::Lifecycle(
                    format!("Failed to write identity delivery: {}", e)
                ))?;
            tracing::info!(
                agent_id,
                entries = identity_entries.len(),
                "Identity delivery written to workspace"
            );
        }

        // Assign a per-agent debug port when running in dev mode
        let debug_port = if dev_mode {
            Some(find_available_debug_port(19878))
        } else {
            None
        };

        // Spawn the agent process
        let child = spawn_agent_process(
            agent_id,
            &info.install_path,
            &workspace,
            &self.gateway_grpc_endpoint,
            dev_mode,
            debug_port,
            self.log_file_size_mb,
        ).await?;

        let pid = child.id();
        
        state.add_running(RunningAgentInfo {
            agent_id: agent_id.to_string(),
            pid,
            started_at: chrono::Utc::now(),
            workspace: workspace.to_string_lossy().to_string(),
            connected: false,
            ready: false,
            dev_mode,
            debug_port,
        });

        tracing::info!("Started agent: {} (PID: {})", agent_id, pid);
        Ok(())
    }

    /// Auto-start the System Agent (com.rollball.system) if installed.
    ///
    /// Called during Gateway startup. The System Agent is a privileged
    /// agent that manages user identity and is always running.
    /// It cannot be stopped by normal `stop_agent` calls.
    pub async fn auto_start_system_agent(
        &mut self,
        state: &mut GatewayState,
    ) -> Result<(), GatewayError> {
        if !state.is_installed(SYSTEM_AGENT_ID) {
            tracing::warn!(
                "System Agent ({}) not installed — skipping auto-start",
                SYSTEM_AGENT_ID
            );
            return Ok(());
        }

        if state.is_running(SYSTEM_AGENT_ID) {
            tracing::debug!("System Agent already running");
            return Ok(());
        }

        tracing::info!("Auto-starting System Agent ({})", SYSTEM_AGENT_ID);
        self.start_agent(SYSTEM_AGENT_ID, state, false).await
    }

    /// Query identity_deps for an agent manifest.
    ///
    /// Returns the list of identity fields this agent depends on,
    /// as declared in its manifest. If no deps are declared,
    /// returns an empty list.
    pub fn get_identity_deps(
        &self,
        agent_id: &str,
        state: &GatewayState,
    ) -> Vec<String> {
        state.installed_agents
            .get(agent_id)
            .map(|info| info.manifest.identity_deps.clone())
            .unwrap_or_default()
    }

    /// Build an identity_delivery payload for a given agent.
    ///
    /// Queries the System Agent's Grafeo for the fields specified
    /// in the agent's identity_deps and returns them as IdentityEntry list.
    ///
    /// Current implementation uses well-known field definitions with
    /// placeholder values. When System Agent IPC is fully connected,
    /// this will query the System Agent's IdentityStore via IPC.
    pub fn build_identity_delivery(
        &self,
        agent_id: &str,
        state: &GatewayState,
    ) -> Vec<rollball_core::identity::IdentityEntry> {
        use rollball_core::identity::{IdentityEntry, find_field_def};

        let deps = self.get_identity_deps(agent_id, state);
        if deps.is_empty() {
            return Vec::new();
        }

        tracing::info!(
            "Building identity delivery for {}: fields {:?}",
            agent_id,
            deps
        );

        // Build entries from well-known field definitions.
        //
        // Value resolution (in priority order):
        // 1. Gateway data dir `.user_profile.json` `displayName` field (for display_name)
        // 2. Future: System Agent IdentityStore query
        let now = chrono::Utc::now().to_rfc3339();
        let data_dir = state.config.as_ref().map(|c| std::path::Path::new(&c.data_dir));
        let user_display_name = load_user_display_name(data_dir);
        let entries: Vec<IdentityEntry> = deps.iter().filter_map(|field| {
            find_field_def(field).map(|def| {
                let value = if field == "display_name" {
                    user_display_name.clone().unwrap_or_default()
                } else {
                    String::new()
                };
                IdentityEntry {
                    field: field.clone(),
                    value,
                    confidence: if field == "display_name" && user_display_name.is_some() { 1.0 } else { 0.0 },
                    category: def.category,
                    privacy: def.privacy,
                    source: "cold_start_delivery".to_string(),
                    updated_at: now.clone(),
                }
            })
        }).collect();

        if !entries.is_empty() {
            tracing::info!(
                agent_id,
                entries = entries.len(),
                "Identity delivery built"
            );
        }

        entries
    }

    /// Stop a running agent process
    pub async fn stop_agent(
        &mut self,
        agent_id: &str,
        state: &mut GatewayState,
    ) -> Result<(), GatewayError> {
        // System Agent cannot be stopped
        if agent_id == SYSTEM_AGENT_ID {
            return Err(GatewayError::Lifecycle(
                "System Agent (com.rollball.system) cannot be stopped".to_string()
            ));
        }

        let running = state.running_agents.get(agent_id)
            .ok_or_else(|| GatewayError::AgentNotRunning(agent_id.to_string()))?
            .clone();

        kill_agent_process(running.pid).await?;
        state.remove_running(agent_id);

        tracing::info!("Stopped agent: {} (was PID: {})", agent_id, running.pid);
        Ok(())
    }

    /// Check health of all running agents
    pub async fn health_check_all(&self, state: &GatewayState) -> Vec<(String, bool)> {
        let mut results = Vec::new();
        for (agent_id, info) in &state.running_agents {
            let healthy = check_health(info.pid).await;
            results.push((agent_id.clone(), healthy));
        }
        results
    }

    /// Check for idle agents that should be stopped
    pub fn check_idle_timeouts(&self, _state: &GatewayState) -> Vec<String> {
        if self.idle_timeout_secs == 0 {
            return Vec::new();
        }
        // Phase 1: return empty — idle tracking requires per-agent last-activity timestamps
        // Phase 2: implement with actual idle tracking
        Vec::new()
    }
}

/// Load the user's display name from Gateway's data directory.
///
/// Reads `{data_dir}/.user_profile.json` and extracts the `displayName` field.
/// Returns `None` if the file doesn't exist or can't be parsed.
fn load_user_display_name(data_dir: Option<&std::path::Path>) -> Option<String> {
    let data_dir = data_dir?;
    let path = data_dir.join(".user_profile.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed.get("displayName")?.as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("rollball-test-lifecycle-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    #[test]
    fn test_lifecycle_manager_new() {
        let mgr = LifecycleManager::new(300, "http://127.0.0.1:19877".to_string(), 10);
        assert_eq!(mgr.idle_timeout_secs, 300);
    }

    #[test]
    fn test_lifecycle_manager_zero_timeout() {
        let mgr = LifecycleManager::new(0, "http://127.0.0.1:19877".to_string(), 10);
        let dir = temp_vault_dir("zero");
        let state = GatewayState::new(&dir);
        let result = mgr.check_idle_timeouts(&state);
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_start_agent_not_installed() {
        let mut mgr = LifecycleManager::new(300, "http://127.0.0.1:19877".to_string(), 10);
        let dir = temp_vault_dir("start");
        let mut state = GatewayState::new(&dir);
        let result = mgr.start_agent("com.test.unknown", &mut state, false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stop_agent_not_running() {
        let mut mgr = LifecycleManager::new(300, "http://127.0.0.1:19877".to_string(), 10);
        let dir = temp_vault_dir("stop");
        let mut state = GatewayState::new(&dir);
        let result = mgr.stop_agent("com.test.unknown", &mut state).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_system_agent_id_constant() {
        assert_eq!(SYSTEM_AGENT_ID, "com.rollball.system");
    }

    #[tokio::test]
    async fn test_stop_system_agent_rejected() {
        let mut mgr = LifecycleManager::new(300, "http://127.0.0.1:19877".to_string(), 10);
        let dir = temp_vault_dir("sysstop");
        let mut state = GatewayState::new(&dir);
        let result = mgr.stop_agent(SYSTEM_AGENT_ID, &mut state).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("System Agent") || err_msg.contains("com.rollball.system"));
    }

    #[tokio::test]
    async fn test_auto_start_system_agent_not_installed() {
        let mut mgr = LifecycleManager::new(300, "http://127.0.0.1:19877".to_string(), 10);
        let dir = temp_vault_dir("autostart");
        let mut state = GatewayState::new(&dir);
        // System Agent not installed — should succeed gracefully with warning
        let result = mgr.auto_start_system_agent(&mut state).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_identity_deps_no_deps() {
        let mgr = LifecycleManager::new(300, "http://127.0.0.1:19877".to_string(), 10);
        let dir = temp_vault_dir("deps");
        let state = GatewayState::new(&dir);
        let deps = mgr.get_identity_deps("com.test.unknown", &state);
        assert!(deps.is_empty());
    }

    #[test]
    fn test_build_identity_delivery_no_deps() {
        let mgr = LifecycleManager::new(300, "http://127.0.0.1:19877".to_string(), 10);
        let dir = temp_vault_dir("delivery");
        let state = GatewayState::new(&dir);
        let entries = mgr.build_identity_delivery("com.test.unknown", &state);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_build_identity_delivery_with_known_fields() {
        use crate::gateway::state::AgentInfo;
        use rollball_core::identity::IdentityCategory;

        let mgr = LifecycleManager::new(300, "http://127.0.0.1:19877".to_string(), 10);
        let dir = temp_vault_dir("identity-fields");
        let mut state = GatewayState::new(&dir);

        // Install an agent with identity_deps
        let toml_str = r#"
            agent_id = "com.test.identity"
            version = "1.0.0"
            name = "Identity Test Agent"
            description = "Test agent with identity deps"
            author = "test"
            runtime_version = "0.1.0"
            identity_deps = ["display_name", "city", "language"]

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
        state.add_installed(AgentInfo {
            agent_id: "com.test.identity".to_string(),
            version: "1.0.0".to_string(),
            name: "Identity Test Agent".to_string(),
            install_path: dir.clone(),
            manifest,
        });

        let entries = mgr.build_identity_delivery("com.test.identity", &state);
        assert_eq!(entries.len(), 3);

        // Check that well-known fields have correct categories
        let display_name = entries.iter().find(|e| e.field == "display_name").unwrap();
        assert_eq!(display_name.category, IdentityCategory::Identity);
        assert_eq!(display_name.source, "cold_start_delivery");

        let city = entries.iter().find(|e| e.field == "city").unwrap();
        assert_eq!(city.category, IdentityCategory::Identity);

        let language = entries.iter().find(|e| e.field == "language").unwrap();
        assert_eq!(language.category, IdentityCategory::Preferences);
    }
}
