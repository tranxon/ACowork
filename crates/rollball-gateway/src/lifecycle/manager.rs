//! Agent process lifecycle manager

use std::path::PathBuf;
use crate::error::GatewayError;
use crate::gateway::state::{GatewayState, RunningAgentInfo};
use crate::lifecycle::process::{spawn_agent_process, kill_agent_process, check_health};

/// Lifecycle manager — controls Agent process lifecycle
pub struct LifecycleManager {
    /// Idle timeout in seconds (0 = no timeout)
    idle_timeout_secs: u64,
}

impl LifecycleManager {
    pub fn new(idle_timeout_secs: u64) -> Self {
        Self { idle_timeout_secs }
    }

    /// Start an agent process
    pub async fn start_agent(
        &mut self,
        agent_id: &str,
        state: &mut GatewayState,
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

        // Spawn the agent process
        let child = spawn_agent_process(
            agent_id,
            &info.install_path,
            &workspace,
        ).await?;

        let pid = child.id();
        
        state.add_running(RunningAgentInfo {
            agent_id: agent_id.to_string(),
            pid,
            started_at: chrono::Utc::now(),
            workspace: workspace.to_string_lossy().to_string(),
        });

        tracing::info!("Started agent: {} (PID: {})", agent_id, pid);
        Ok(())
    }

    /// Stop a running agent process
    pub async fn stop_agent(
        &mut self,
        agent_id: &str,
        state: &mut GatewayState,
    ) -> Result<(), GatewayError> {
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
        let mgr = LifecycleManager::new(300);
        assert_eq!(mgr.idle_timeout_secs, 300);
    }

    #[test]
    fn test_lifecycle_manager_zero_timeout() {
        let mgr = LifecycleManager::new(0);
        let dir = temp_vault_dir("zero");
        let state = GatewayState::new(&dir);
        let result = mgr.check_idle_timeouts(&state);
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_start_agent_not_installed() {
        let mut mgr = LifecycleManager::new(300);
        let dir = temp_vault_dir("start");
        let mut state = GatewayState::new(&dir);
        let result = mgr.start_agent("com.test.unknown", &mut state).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stop_agent_not_running() {
        let mut mgr = LifecycleManager::new(300);
        let dir = temp_vault_dir("stop");
        let mut state = GatewayState::new(&dir);
        let result = mgr.stop_agent("com.test.unknown", &mut state).await;
        assert!(result.is_err());
    }
}
