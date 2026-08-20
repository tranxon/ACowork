//! Agent process lifecycle manager

use crate::error::GatewayError;
use crate::gateway::state::{GatewayState, RunningAgentInfo};
use crate::handlers::server::SharedState;
use crate::lifecycle::process::{
    check_health, find_available_debug_port, kill_agent_process, spawn_agent_process,
};
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use tokio::sync::RwLock;

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
    ///
    /// `state` is a `SharedState` (`Arc<RwLock<GatewayState>>`) — the
    /// reaper task needs its own handle to remove the agent when the
    /// child exits, so we cannot take `&mut GatewayState` here.
    /// Callers that hold `&mut GatewayState` wrap it with
    /// `Arc::new(tokio::sync::RwLock::new(state))` or pass an existing
    /// `SharedState` clone.
    ///
    /// `wire_reaper` controls whether the spawned child's exit-handler
    /// removes the agent from `state.running_agents`. Pass `true` for
    /// production HTTP handlers (long-lived daemon) where the gateway
    /// outlives the agent process. Pass `false` for short-lived CLI
    /// commands (`gateway start`/`stop`) where the reaper is irrelevant
    /// and avoiding the wrapper clone lets us `try_unwrap` the state on
    /// return.
    pub async fn start_agent(
        &mut self,
        agent_id: &str,
        state: &SharedState,
        dev_mode: bool,
        wire_reaper: bool,
    ) -> Result<(), GatewayError> {
        // Check if already running
        {
            let gw = state.read().await;
            if gw.is_running(agent_id) {
                return Err(GatewayError::AgentAlreadyRunning(agent_id.to_string()));
            }
        }

        // Check if installed
        let info = {
            let gw = state.read().await;
            gw.installed_agents
                .get(agent_id)
                .ok_or_else(|| GatewayError::AgentNotFound(agent_id.to_string()))?
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

        // Spawn the agent process. Pass a `SharedState` clone to the
        // reaper so it can clean up `running_agents` when the child
        // exits — fixes the bug where an auto-sleep Runtime left a
        // dead PID behind and `POST /api/agents/{id}/stop` returned 500.
        //
        // `wire_reaper = false` is for short-lived CLI invocations where
        // the reaper would outlive the caller's ownership of `state`.
        let reaper_state = if wire_reaper { Some(state.clone()) } else { None };
        let child = spawn_agent_process(
            agent_id,
            &info.install_path,
            &workspace,
            dev_mode,
            debug_port,
            self.log_file_size_mb,
            self.log_file_count,
            self.mqtt_port,
            reaper_state,
        )
        .await?;

        let pid = child.id();

        state.write().await.add_running(RunningAgentInfo {
            agent_id: agent_id.to_string(),
            pid,
            started_at: chrono::Utc::now(),
            workspace: workspace.to_string_lossy().to_string(),
            // `connected` flips to `true` when the Runtime completes the MQTT
            // handshake and the Gateway's `handle_agent_hello` callback fires
            // (`handlers/server.rs:324`). It is NOT tied to the spawn returning
            // — writing `true` here would let `/api/agents` report a connected
            // agent before the Runtime's MQTT client has actually talked to
            // the broker, which is the same class of race as the (formerly
            // broken) `ready: true` below.
            connected: false,
            // `ready` flips to `true` only when the Runtime publishes
            // `acowork/agents/{id}/ready = "true"` after Phase B has fully
            // populated `session_metadata_slot` / `session_config_slot` /
            // `memory_query_slot` / `workspace_query_slot` and Phase C has
            // spawned the chunk-relay / DevMode / MCP subsystems. Until then,
            // the HTTP server boots with all of those slots `None` and every
            // handler returns 503 (see `http/server.rs:533, 595, 620, 648`).
            // Reporting `ready=true` at spawn time made the Desktop dispatch
            // HTTP requests into that 503 window and surfaced them as
            // "Session 加载失败" right after Stop → Start. The dispatch path
            // (`mqtt/dispatch.rs` plaintext `acowork/agents/+/ready`) catches
            // the published retained message and calls `set_agent_ready`.
            ready: false,
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
        state: &SharedState,
    ) -> Result<(), GatewayError> {
        let (installed, running) = {
            let gw = state.read().await;
            (gw.is_installed(SYSTEM_AGENT_ID), gw.is_running(SYSTEM_AGENT_ID))
        };

        if !installed {
            tracing::warn!(
                "System Agent ({}) not installed — skipping auto-start",
                SYSTEM_AGENT_ID
            );
            return Ok(());
        }

        if running {
            tracing::debug!("System Agent already running");
            return Ok(());
        }

        tracing::info!("Auto-starting System Agent ({})", SYSTEM_AGENT_ID);
        self.start_agent(SYSTEM_AGENT_ID, state, false, true).await
    }

    /// Stop a running agent process
    ///
    /// Takes `SharedState` so the reaper can also see the cleanup if the
    /// manual kill races with the Runtime's own exit (idempotent: both
    /// paths call `remove_running`, and the second call is a no-op).
    pub async fn stop_agent(
        &mut self,
        agent_id: &str,
        state: &SharedState,
    ) -> Result<(), GatewayError> {
        // System Agent cannot be stopped
        if agent_id == SYSTEM_AGENT_ID {
            return Err(GatewayError::Lifecycle(
                "System Agent (com.acowork.system) cannot be stopped".to_string(),
            ));
        }

        let running = {
            let gw = state.read().await;
            gw.running_agents
                .get(agent_id)
                .ok_or_else(|| GatewayError::AgentNotRunning(agent_id.to_string()))?
                .clone()
        };

        // Pre-emptively remove the entry so a subsequent `kill_agent_process`
        // failure (e.g. PID already gone because the Runtime exited via
        // idle auto-sleep) does NOT leave a stale record. The reaper will
        // find nothing and quietly exit.
        state.write().await.remove_running(agent_id);

        // Then try to deliver SIGTERM/taskkill. If the process is already
        // gone this returns Err — that's fine, the state is already clean.
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
        let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));
        let result = mgr.start_agent("com.test.unknown", &state, false, false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stop_agent_not_running() {
        let mut mgr = LifecycleManager::new(10, 20, None);
        let dir = temp_vault_dir("stop");
        let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));
        let result = mgr.stop_agent("com.test.unknown", &state).await;
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
        let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));
        let result = mgr.stop_agent(SYSTEM_AGENT_ID, &state).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("System Agent") || err_msg.contains("com.acowork.system"));
    }

    #[tokio::test]
    async fn test_auto_start_system_agent_not_installed() {
        let mut mgr = LifecycleManager::new(10, 20, None);
        let dir = temp_vault_dir("autostart");
        let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));
        // System Agent not installed — should succeed gracefully with warning
        let result = mgr.auto_start_system_agent(&state).await;
        assert!(result.is_ok());
    }
}
