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
            workspace_config_json: None,
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

}
