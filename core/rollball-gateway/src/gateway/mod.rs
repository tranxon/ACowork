//! Gateway main module
//!
//! The Gateway struct is the top-level orchestrator that ties together
//! IPC server, lifecycle manager, package manager, and vault.

pub mod state;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::ipc::server::{IpcServer, SharedState};
use crate::lifecycle::manager::LifecycleManager;
use crate::package_manager::install;
use crate::package_manager::uninstall;
use crate::package_manager::upgrade;
use crate::permission_store::PermissionStore;
use crate::cli::PermissionAction;
use rollball_core::permission::Permission;

/// Gateway — the top-level orchestrator
///
/// Owns all sub-systems and drives the event loop.
pub struct Gateway {
    config: GatewayConfig,
    state: GatewayState,
    lifecycle: LifecycleManager,
    perm_store: PermissionStore,
}

impl Gateway {
    /// Create a new Gateway instance with the given configuration
    pub fn new(config: GatewayConfig) -> Self {
        let idle_timeout = config.idle_timeout_secs;
        let vault_dir = config.vault_dir.clone();
        let perm_store = PermissionStore::open_in_memory()
            .expect("Failed to create permission store");
        Self {
            config,
            state: GatewayState::new(&vault_dir),
            lifecycle: LifecycleManager::new(idle_timeout),
            perm_store,
        }
    }

    /// Run the Gateway daemon (async, multi-connection)
    ///
    /// This starts the IPC server and enters the main event loop.
    /// Blocks until shutdown signal is received.
    /// The GatewayState is wrapped in Arc<RwLock> for concurrent access
    /// by multiple IPC connection handlers.
    pub async fn run(&mut self) -> Result<(), GatewayError> {
        tracing::info!("Gateway starting");
        tracing::info!("  Socket path: {}", self.config.socket_path);
        tracing::info!("  Vault dir: {}", self.config.vault_dir);
        tracing::info!("  Packages dir: {}", self.config.packages_dir);

        // Ensure directories exist
        self.ensure_dirs()?;

        // Wrap state in Arc<RwLock> for concurrent access in multi-connection mode.
        // std::mem::take replaces self.state with Default so the Arc takes ownership.
        // This is safe because run() is the terminal daemon method that blocks forever.
        let shared_state: SharedState =
            Arc::new(RwLock::new(std::mem::take(&mut self.state)));

        let socket_path = self.config.socket_path.clone();

        // Spawn the idle timeout checker in a background task
        let idle_timeout = self.config.idle_timeout_secs;
        let _idle_handle = tokio::spawn(async move {
            if idle_timeout > 0 {
                let mut interval = tokio::time::interval(
                    std::time::Duration::from_secs(idle_timeout.min(60))
                );
                loop {
                    interval.tick().await;
                    // Phase 2: check idle timeouts and stop idle agents
                    tracing::trace!("Idle timeout check (configured: {}s)", idle_timeout);
                }
            }
        });

        tracing::info!("Gateway entering IPC event loop (async multi-connection)");

        // Run the IPC server (async, multi-connection)
        let ipc_server = IpcServer::new(&socket_path);
        ipc_server.listen(shared_state).await?;

        Ok(())
    }

    /// Install a .agent package
    pub fn install_package(&mut self, package_path: &str) -> Result<String, GatewayError> {
        let packages_dir = std::path::Path::new(&self.config.packages_dir);
        install::install_package(
            std::path::Path::new(package_path),
            packages_dir,
            &mut self.state,
            self.config.dev_mode,
        )?;
        Ok(format!("Package installed: {}", package_path))
    }

    /// Uninstall an agent
    pub fn uninstall_package(&mut self, agent_id: &str) -> Result<String, GatewayError> {
        let packages_dir = std::path::Path::new(&self.config.packages_dir);
        uninstall::uninstall_package(
            agent_id,
            packages_dir,
            &mut self.state,
        )?;
        Ok(format!("Agent uninstalled: {}", agent_id))
    }

    /// Upgrade an agent
    pub fn upgrade_package(
        &mut self,
        agent_id: &str,
        package_path: &str,
    ) -> Result<String, GatewayError> {
        let packages_dir = std::path::Path::new(&self.config.packages_dir);
        upgrade::upgrade_package(
            agent_id,
            std::path::Path::new(package_path),
            packages_dir,
            &mut self.state,
        )?;
        Ok(format!("Agent upgraded: {}", agent_id))
    }

    /// Start an agent
    pub async fn start_agent(&mut self, agent_id: &str) -> Result<String, GatewayError> {
        self.lifecycle.start_agent(agent_id, &mut self.state).await?;
        Ok(format!("Agent started: {}", agent_id))
    }

    /// Stop a running agent
    pub async fn stop_agent(&mut self, agent_id: &str) -> Result<String, GatewayError> {
        self.lifecycle.stop_agent(agent_id, &mut self.state).await?;
        Ok(format!("Agent stopped: {}", agent_id))
    }

    /// List installed agents
    pub fn list_agents(&self) -> Vec<AgentListEntry> {
        self.state
            .installed_agents
            .values()
            .map(|info| AgentListEntry {
                agent_id: info.agent_id.clone(),
                name: info.name.clone(),
                version: info.version.clone(),
                running: self.state.is_running(&info.agent_id),
            })
            .collect()
    }

    /// Handle permission CLI subcommands
    pub fn handle_permission_cli(&self, action: PermissionAction) -> Result<String, GatewayError> {
        match action {
            PermissionAction::Revoke { agent_id, permission } => {
                let perm = Permission::parse(&permission)
                    .ok_or_else(|| GatewayError::Package(format!("Invalid permission: {}", permission)))?;
                let count = self.perm_store.revoke(&agent_id, Some(&perm))
                    .map_err(|e| GatewayError::Package(format!("Failed to revoke: {}", e)))?;
                if count > 0 {
                    tracing::info!("Revoked permission '{}' from agent {}", permission, agent_id);
                    Ok(format!("Revoked '{}' from {} ({} grant(s) removed)", permission, agent_id, count))
                } else {
                    Ok(format!("No matching grant found for '{}' on {}", permission, agent_id))
                }
            }
            PermissionAction::Reset { agent_id } => {
                let count = self.perm_store.reset(&agent_id)
                    .map_err(|e| GatewayError::Package(format!("Failed to reset: {}", e)))?;
                tracing::info!("Reset all permissions for agent {}", agent_id);
                Ok(format!("Reset {} permission grant(s) for {}", count, agent_id))
            }
            PermissionAction::List { agent_id } => {
                let grants = self.perm_store.query_grants(&agent_id)
                    .map_err(|e| GatewayError::Package(format!("Failed to list: {}", e)))?;
                if grants.is_empty() {
                    Ok(format!("No permissions granted for {}", agent_id))
                } else {
                    let lines: Vec<String> = grants.iter()
                        .map(|g| format!("  {} (by {}, {})",
                            g.permission.to_permission_string(),
                            g.authorized_by,
                            if g.expires_at.is_some() { "temporary" } else { "permanent" }
                        ))
                        .collect();
                    Ok(format!("Permissions for {}:\n{}", agent_id, lines.join("\n")))
                }
            }
        }
    }

    /// Ensure all required directories exist
    fn ensure_dirs(&self) -> Result<(), GatewayError> {
        for dir in &[&self.config.vault_dir, &self.config.packages_dir, &self.config.data_dir] {
            std::fs::create_dir_all(dir)
                .map_err(|e| GatewayError::Config(format!("Failed to create directory '{}': {}", dir, e)))?;
        }
        Ok(())
    }
}

/// Agent list entry for display
#[derive(Debug, Clone)]
pub struct AgentListEntry {
    pub agent_id: String,
    pub name: String,
    pub version: String,
    pub running: bool,
}

impl std::fmt::Display for AgentListEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = if self.running { "running" } else { "stopped" };
        write!(f, "{} ({}) v{} [{}]", self.name, self.agent_id, self.version, status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;

    fn test_config() -> GatewayConfig {
        GatewayConfig {
            socket_path: "/tmp/test-gateway.sock".to_string(),
            vault_dir: std::env::temp_dir().join("rollball-test-vault").to_string_lossy().to_string(),
            packages_dir: std::env::temp_dir().join("rollball-test-packages").to_string_lossy().to_string(),
            data_dir: std::env::temp_dir().join("rollball-test-data").to_string_lossy().to_string(),
            log_level: "info".to_string(),
            idle_timeout_secs: 0,
            max_iterations: 20,
            iteration_timeout_ms: 30000,
            dev_mode: true,
        }
    }

    #[test]
    fn test_gateway_new() {
        let config = test_config();
        let gateway = Gateway::new(config);
        assert!(gateway.list_agents().is_empty());
    }

    #[test]
    fn test_ensure_dirs() {
        let config = test_config();
        let gateway = Gateway::new(config);
        assert!(gateway.ensure_dirs().is_ok());
    }

    #[test]
    fn test_list_agents_empty() {
        let config = test_config();
        let gateway = Gateway::new(config);
        let list = gateway.list_agents();
        assert!(list.is_empty());
    }

    #[test]
    fn test_agent_list_entry_display() {
        let entry = AgentListEntry {
            agent_id: "com.example.weather".to_string(),
            name: "Weather Agent".to_string(),
            version: "1.0.0".to_string(),
            running: true,
        };
        let display = format!("{}", entry);
        assert!(display.contains("Weather Agent"));
        assert!(display.contains("running"));
    }
}
