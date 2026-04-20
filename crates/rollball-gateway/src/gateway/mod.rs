//! Gateway main module
//!
//! The Gateway struct is the top-level orchestrator that ties together
//! IPC server, lifecycle manager, package manager, and vault.

pub mod state;

use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::ipc::server::IpcServer;
use crate::lifecycle::manager::LifecycleManager;
use crate::package_manager::install;
use crate::package_manager::uninstall;
use crate::package_manager::upgrade;

/// Gateway — the top-level orchestrator
///
/// Owns all sub-systems and drives the event loop.
pub struct Gateway {
    config: GatewayConfig,
    state: GatewayState,
    lifecycle: LifecycleManager,
}

impl Gateway {
    /// Create a new Gateway instance with the given configuration
    pub fn new(config: GatewayConfig) -> Self {
        let idle_timeout = config.idle_timeout_secs;
        let vault_dir = config.vault_dir.clone();
        Self {
            config,
            state: GatewayState::new(&vault_dir),
            lifecycle: LifecycleManager::new(idle_timeout),
        }
    }

    /// Run the Gateway daemon (blocking)
    ///
    /// This starts the IPC server and enters the main event loop.
    /// Blocks until shutdown signal is received.
    pub async fn run(&mut self) -> Result<(), GatewayError> {
        tracing::info!("Gateway starting");
        tracing::info!("  Socket path: {}", self.config.socket_path);
        tracing::info!("  Vault dir: {}", self.config.vault_dir);
        tracing::info!("  Packages dir: {}", self.config.packages_dir);

        // Ensure directories exist
        self.ensure_dirs()?;

        // Start IPC server in a background task
        let socket_path = self.config.socket_path.clone();

        // Phase 1: run IPC server synchronously in the main task
        // Phase 2: run in a separate task and use channels for communication
        let mut ipc_server = IpcServer::new(&socket_path);

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

        tracing::info!("Gateway entering IPC event loop");

        // Run the IPC server (blocking)
        // Phase 2 will use Arc<Mutex<GatewayState>> for true async
        ipc_server.run(&mut self.state)?;

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
