//! Runtime process lifecycle manager (ADR-055 §6.20 — migrated from
//! gateway `lifecycle/manager.rs`).
//!
//! Re-based from `GatewayError` / `SharedState` to
//! [`crate::error::NodeError`] / [`crate::state::SharedNodeState`].
//!
//! Note the scope change vs. the Gateway: the node is NOT the authority
//! for agent policy (e.g. "system agent cannot be stopped" or
//! "auto-start the system agent"). Those stay in the Gateway and arrive
//! here as plain `start` / `stop` control commands. The node simply
//! spawns/kills what it is told and keeps its local process table
//! truthful.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::{NodeError, Result};
use crate::process::spawn::{
    check_health, find_available_debug_port, find_available_http_port, kill_agent_process,
    spawn_agent_process,
};
use crate::state::{AgentSlot, InstalledAgent, SharedNodeState};

/// Runtime process manager — spawn/kill/reap/health for agent Runtime
/// processes hosted on this node.
pub struct ProcessManager {
    /// Log file max size in MB before auto-split.
    log_file_size_mb: u64,
    /// Maximum number of log files to keep (0 = unlimited).
    log_file_count: u64,
    /// ADR-033: MQTT broker port for Runtime to connect via MQTT.
    mqtt_port: Option<u16>,
    /// ADR-055 L3-5: Gateway MQTT broker host, forwarded to Runtimes.
    gateway_host: String,
    /// ADR-055 §6.3: Node reverse-proxy base URL injected into spawned
    /// Runtimes (`--http-advertise-endpoint`).
    http_advertise_endpoint: Option<String>,
}

impl ProcessManager {
    pub fn new(
        log_file_size_mb: u64,
        log_file_count: u64,
        mqtt_port: Option<u16>,
        gateway_host: String,
        http_advertise_endpoint: Option<String>,
    ) -> Self {
        Self {
            log_file_size_mb,
            log_file_count,
            mqtt_port,
            gateway_host,
            http_advertise_endpoint,
        }
    }

    /// Start an agent Runtime process.
    ///
    /// `wire_reaper` controls whether the spawned child's exit-handler
    /// removes the agent from the node process table. Pass `true` for
    /// the long-lived daemon path.
    pub async fn start_agent(
        &mut self,
        agent_id: &str,
        state: &SharedNodeState,
        dev_mode: bool,
        wire_reaper: bool,
    ) -> Result<()> {
        // Check if already running
        {
            let node = state.read().await;
            if node.is_running(agent_id) {
                return Err(NodeError::AgentAlreadyRunning(agent_id.to_string()));
            }
        }

        // Check if installed
        let info = {
            let node = state.read().await;
            node.installed_agents
                .get(agent_id)
                .ok_or_else(|| NodeError::AgentNotFound(agent_id.to_string()))?
                .clone()
        };

        // Determine workspace directory
        let workspace = PathBuf::from(&info.install_path).join("workspace");

        // Assign a per-agent debug port when running in dev mode
        let debug_port = if dev_mode {
            Some(find_available_debug_port(19878))
        } else {
            None
        };

        // ADR-055 §6.4: the Node allocates a concrete loopback HTTP
        // port so its reverse proxy has a stable `{id} → port` mapping.
        let http_port = find_available_http_port();

        let reaper_state = if wire_reaper {
            Some(state.clone())
        } else {
            None
        };
        let child = spawn_agent_process(
            agent_id,
            &info.install_path,
            &workspace,
            dev_mode,
            debug_port,
            self.log_file_size_mb,
            self.log_file_count,
            self.mqtt_port,
            &self.gateway_host,
            http_port,
            self.http_advertise_endpoint.as_deref(),
            reaper_state,
        )
        .await?;

        let pid = child.id();

        state.write().await.add_agent(AgentSlot {
            agent_id: agent_id.to_string(),
            pid,
            started_at: chrono::Utc::now(),
            workspace: workspace.to_string_lossy().to_string(),
            dev_mode,
            debug_port,
            http_port,
        });

        tracing::info!(
            "Started agent: {} (PID: {}, http_port: {})",
            agent_id,
            pid,
            http_port
        );
        Ok(())
    }

    /// Stop a running agent Runtime process.
    pub async fn stop_agent(
        &mut self,
        agent_id: &str,
        state: &SharedNodeState,
    ) -> Result<()> {
        let running = {
            let node = state.read().await;
            node.agents
                .get(agent_id)
                .ok_or_else(|| NodeError::AgentNotRunning(agent_id.to_string()))?
                .clone()
        };

        // Pre-emptively remove the entry so a subsequent kill failure
        // (e.g. PID already gone via idle auto-sleep) does NOT leave a
        // stale record. The reaper will find nothing and quietly exit.
        state.write().await.remove_agent(agent_id);

        if let Err(e) = kill_agent_process(running.pid).await {
            tracing::warn!(
                agent_id,
                pid = running.pid,
                error = %e,
                "kill_agent_process failed during stop_agent; \
                 process likely already exited (state already cleaned)"
            );
        }

        tracing::info!("Stopped agent: {} (was PID: {})", agent_id, running.pid);
        Ok(())
    }

    /// Check health of all running agents.
    pub async fn health_check_all(&self, state: &crate::state::NodeState) -> Vec<(String, bool)> {
        let mut results = Vec::new();
        for (agent_id, slot) in &state.agents {
            let healthy = check_health(slot.pid).await;
            results.push((agent_id.clone(), healthy));
        }
        results
    }

    /// Re-adopt orphan Runtime processes left running after a Node
    /// restart (ADR-055 §6.19).
    ///
    /// Scans the local process list for `acowork-runtime --agent-id {id}`
    /// command lines, and for every candidate whose agent is still in the
    /// install table rebuilds its [`AgentSlot`] so the reverse proxy can
    /// route `/agents/{id}/*` again. The `--work-dir` is re-derived from
    /// the install table (`{install_path}/workspace`) rather than parsed
    /// from the command line.
    ///
    /// Returns the agent_ids adopted (for the `node_readopted` diagnostic
    /// event). Residual processes for uninstalled agents are skipped, not
    /// killed — a best-effort scan never SIGKILLs on its own.
    pub async fn readopt_orphans(&self, state: &SharedNodeState) -> Vec<String> {
        let candidates = crate::process::reap::scan_runtime_processes().await;
        if candidates.is_empty() {
            return Vec::new();
        }

        let installed: HashMap<String, InstalledAgent> = {
            state.read().await.installed_agents.clone()
        };
        let (adopt, skip) = crate::process::reap::classify_candidates(candidates, &installed);

        for c in &skip {
            tracing::warn!(
                agent_id = %c.agent_id,
                pid = c.pid,
                "Re-adopt: skipping Runtime whose agent is not installed (residual process)"
            );
        }

        let mut adopted = Vec::new();
        for c in adopt {
            let workspace = installed
                .get(&c.agent_id)
                .map(|info| PathBuf::from(&info.install_path).join("workspace"))
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            state.write().await.add_agent(AgentSlot {
                agent_id: c.agent_id.clone(),
                pid: c.pid,
                started_at: chrono::Utc::now(),
                workspace,
                dev_mode: c.dev_mode,
                debug_port: None,
                http_port: c.http_port,
            });
            tracing::info!(
                agent_id = %c.agent_id,
                pid = c.pid,
                http_port = c.http_port,
                "Re-adopted orphan Runtime into node process table"
            );
            adopted.push(c.agent_id);
        }
        adopted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[test]
    fn test_process_manager_new() {
        let mgr = ProcessManager::new(10, 20, None, "127.0.0.1".to_string(), None);
        assert_eq!(mgr.log_file_size_mb, 10);
        assert_eq!(mgr.log_file_count, 20);
    }

    #[tokio::test]
    async fn test_start_agent_not_installed() {
        let mut mgr = ProcessManager::new(10, 20, None, "127.0.0.1".to_string(), None);
        let state: SharedNodeState = Arc::new(RwLock::new(crate::state::NodeState::new(16)));
        let result = mgr.start_agent("com.test.unknown", &state, false, false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stop_agent_not_running() {
        let mut mgr = ProcessManager::new(10, 20, None, "127.0.0.1".to_string(), None);
        let state: SharedNodeState = Arc::new(RwLock::new(crate::state::NodeState::new(16)));
        let result = mgr.stop_agent("com.test.unknown", &state).await;
        assert!(result.is_err());
    }
}
