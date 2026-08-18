//! Agent process lifecycle manager

use crate::error::GatewayError;
use crate::gateway::state::{GatewayState, RunningAgentInfo};
use crate::lifecycle::process::{
    check_health, find_available_debug_port, kill_agent_process, spawn_agent_process,
};
use std::path::PathBuf;

/// System Agent ID — always auto-started with Gateway
pub const SYSTEM_AGENT_ID: &str = "com.acowork.system";

/// Lifecycle manager — controls Agent process lifecycle
pub struct LifecycleManager {
    /// Log file max size in MB before auto-split
    log_file_size_mb: u64,
    /// Maximum number of log files to keep (0 = unlimited)
    log_file_count: u64,
    /// ADR-033: MQTT broker port for Runtime to connect via MQTT.
    /// When set, Runtime receives --mqtt-port <port>.
    mqtt_port: Option<u16>,
}

impl LifecycleManager {
    pub fn new(
        log_file_size_mb: u64,
        log_file_count: u64,
        mqtt_port: Option<u16>,
    ) -> Self {
        // The previous `idle_timeout_secs` parameter was removed: the
        // decision to auto-sleep an idle Runtime is now owned by the
        // Runtime itself (see `acowork-runtime::agent::idle_watcher`).
        // The Gateway only observes the `sleeping` retained status and
        // surfaces it via `/api/agents` (see `AgentRegistry::sleeping_at`).
        Self {
            log_file_size_mb,
            log_file_count,
            mqtt_port,
        }
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
        let info = state
            .installed_agents
            .get(agent_id)
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
            dev_mode,
            debug_port,
            self.log_file_size_mb,
            self.log_file_count,
            self.mqtt_port,
        )
        .await?;

        let pid = child.id();

        state.add_running(RunningAgentInfo {
            agent_id: agent_id.to_string(),
            pid,
            started_at: chrono::Utc::now(),
            workspace: workspace.to_string_lossy().to_string(),
            connected: true,
            // ADR-033: MQTT-only agents are ready immediately
            // (status "online" = gRPC AgentReady equivalent).
            ready: self.mqtt_port.is_some(),
            dev_mode,
            debug_port,
            workspace_config_json: None,
            current_embed_dim: None,
            migration: None,
        });

        tracing::info!("Started agent: {} (PID: {})", agent_id, pid);
        Ok(())
    }

    /// Auto-start the System Agent (com.acowork.system) if installed.
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
                "System Agent (com.acowork.system) cannot be stopped".to_string(),
            ));
        }

        let running = state
            .running_agents
            .get(agent_id)
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

    // `check_idle_timeouts` removed: the auto-sleep decision is owned by
    // the Runtime (see `acowork-runtime::agent::idle_watcher`). The Gateway
    // observes the resulting `sleeping` retained status via the
    // `AgentRegistry` and surfaces `sleeping_at` to the Desktop through
    // `/api/agents` — no idle-tracking state lives here.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("acowork-test-lifecycle-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    #[test]
    fn test_lifecycle_manager_new() {
        let mgr = LifecycleManager::new(10, 20, None);
        assert_eq!(mgr.log_file_size_mb, 10);
        assert_eq!(mgr.log_file_count, 20);
    }

    #[tokio::test]
    async fn test_start_agent_not_installed() {
        let mut mgr = LifecycleManager::new(10, 20, None);
        let dir = temp_vault_dir("start");
        let mut state = GatewayState::new(&dir);
        let result = mgr.start_agent("com.test.unknown", &mut state, false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stop_agent_not_running() {
        let mut mgr = LifecycleManager::new(10, 20, None);
        let dir = temp_vault_dir("stop");
        let mut state = GatewayState::new(&dir);
        let result = mgr.stop_agent("com.test.unknown", &mut state).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_system_agent_id_constant() {
        assert_eq!(SYSTEM_AGENT_ID, "com.acowork.system");
    }

    #[tokio::test]
    async fn test_stop_system_agent_rejected() {
        let mut mgr = LifecycleManager::new(10, 20, None);
        let dir = temp_vault_dir("sysstop");
        let mut state = GatewayState::new(&dir);
        let result = mgr.stop_agent(SYSTEM_AGENT_ID, &mut state).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("System Agent") || err_msg.contains("com.acowork.system"));
    }

    #[tokio::test]
    async fn test_auto_start_system_agent_not_installed() {
        let mut mgr = LifecycleManager::new(10, 20, None);
        let dir = temp_vault_dir("autostart");
        let mut state = GatewayState::new(&dir);
        // System Agent not installed — should succeed gracefully with warning
        let result = mgr.auto_start_system_agent(&mut state).await;
        assert!(result.is_ok());
    }
}
