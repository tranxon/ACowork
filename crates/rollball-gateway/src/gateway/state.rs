//! Gateway global state

use std::collections::HashMap;
use crate::vault::VaultFacade;

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
}

/// Gateway state — shared mutable state for the entire Gateway process
pub struct GatewayState {
    /// Installed agents (agent_id → AgentInfo)
    pub installed_agents: HashMap<String, AgentInfo>,
    /// Running agents (agent_id → RunningAgentInfo)
    pub running_agents: HashMap<String, RunningAgentInfo>,
    /// Vault facade for key storage and distribution
    pub vault: VaultFacade,
}

impl GatewayState {
    /// Create new empty state with vault at the given directory
    pub fn new(vault_dir: &str) -> Self {
        Self {
            installed_agents: HashMap::new(),
            running_agents: HashMap::new(),
            vault: VaultFacade::new(vault_dir),
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

    /// Add an installed agent
    pub fn add_installed(&mut self, info: AgentInfo) {
        self.installed_agents.insert(info.agent_id.clone(), info);
    }

    /// Remove an installed agent
    pub fn remove_installed(&mut self, agent_id: &str) -> Option<AgentInfo> {
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
        });
        assert!(state.is_running("com.example.weather"));
        
        state.remove_running("com.example.weather");
        assert!(!state.is_running("com.example.weather"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
