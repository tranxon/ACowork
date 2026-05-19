//! Gateway main module
//!
//! The Gateway struct is the top-level orchestrator that ties together
//! IPC server, lifecycle manager, package manager, and vault.

pub mod state;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::GatewayConfig;
use crate::cron::CronStore;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::ipc::server::SharedState;
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
    pub fn new(config: GatewayConfig) -> Result<Self, GatewayError> {
        let idle_timeout = config.idle_timeout_secs;
        let log_file_size_mb = config.log_file_size_mb;
        let vault_dir = config.vault_dir.clone();
        let data_dir = config.data_dir.clone();

        // Ensure data directory exists before opening the database
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| GatewayError::Config(format!(
                "Failed to create data directory '{}': {}", data_dir, e
            )))?;

        // Build the gRPC endpoint URL that Runtime processes will use to connect.
        // Runtime expects an HTTP URL like "http://127.0.0.1:19877".
        let grpc_addr = crate::grpc::server::default_grpc_addr();
        let gateway_grpc_endpoint = format!("http://{}", grpc_addr);

        Ok(Self {
            config,
            state: GatewayState::new(&vault_dir),
            lifecycle: LifecycleManager::new(idle_timeout, gateway_grpc_endpoint, log_file_size_mb),
        })
    }

    /// Auto-install bundled agents (System Agent, etc.) if not already installed.
    ///
    /// This is called during Gateway startup. It looks for bundled agents in:
    /// 1. The project source directory (../../examples/)
    /// 2. The ROLLBALL_BUNDLED_AGENTS_DIR environment variable
    ///
    /// Bundled agents are identified by `system = true` in their manifest.toml.
    async fn auto_install_bundled_agents(&mut self) {
        // Skip in production mode (bundled agents only for dev)
        if !self.config.dev_mode {
            tracing::debug!("Skipping bundled agents installation (dev_mode=false)");
            return;
        }

        // Check if System Agent is already installed
        if self.state.is_installed(crate::lifecycle::SYSTEM_AGENT_ID) {
            tracing::debug!("System Agent already installed, skipping bundled install");
            return;
        }

        // Find bundled agents directory
        let bundled_dir = Self::find_bundled_agents_dir();
        let Some(bundled_dir) = bundled_dir else {
            tracing::debug!("No bundled agents directory found, skipping auto-install");
            return;
        };

        // Find system agent in bundled directory
        let system_agent_src = bundled_dir.join("system-agent");
        if !system_agent_src.exists() {
            tracing::debug!("Bundled system-agent not found at {:?}", system_agent_src);
            return;
        }

        // Verify it has manifest.toml
        if !system_agent_src.join("manifest.toml").exists() {
            tracing::warn!("Bundled system-agent missing manifest.toml");
            return;
        }

        // Install the system agent
        tracing::info!("Auto-installing bundled System Agent from {:?}", system_agent_src);
        match self.install_agent_from_dir(&system_agent_src).await {
            Ok(agent_id) => {
                tracing::info!("Successfully auto-installed bundled agent: {}", agent_id);
                // Refresh installed agents state
                self.restore_installed_agents();
            }
            Err(e) => {
                tracing::warn!("Failed to auto-install bundled System Agent: {}", e);
            }
        }
    }

    /// Find the bundled agents directory.
    /// Returns Some(path) if found, None otherwise.
    fn find_bundled_agents_dir() -> Option<std::path::PathBuf> {
        // Try environment variable first
        if let Ok(dir) = std::env::var("ROLLBALL_BUNDLED_AGENTS_DIR") {
            let path = std::path::PathBuf::from(&dir);
            if path.exists() {
                return Some(path);
            }
        }

        // Try to find project root from CARGO_MANIFEST_DIR
        // CARGO_MANIFEST_DIR = core/rollball-gateway
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let project_root = manifest_dir.parent()?.parent()?;
        let bundled_dir = project_root.join("examples");

        if bundled_dir.exists() {
            return Some(bundled_dir);
        }

        None
    }

    /// Install an agent from a source directory.
    async fn install_agent_from_dir(&mut self, src_dir: &std::path::Path) -> Result<String, GatewayError> {
        use rollball_core::AgentManifest;

        // Read and parse manifest
        let manifest_path = src_dir.join("manifest.toml");
        let content = std::fs::read_to_string(&manifest_path)
            .map_err(|e| GatewayError::Config(format!("Failed to read manifest: {}", e)))?;

        let manifest: AgentManifest = toml::from_str(&content)
            .map_err(|e| GatewayError::Config(format!("Failed to parse manifest: {}", e)))?;

        let agent_id = manifest.agent_id.clone();
        let version = manifest.version.clone();

        // Copy agent files to packages directory
        let packages_dir = std::path::Path::new(&self.config.packages_dir);
        let agent_pkg_dir = packages_dir.join(format!("{}-{}", agent_id, version));

        // Remove existing directory if it exists
        let _ = std::fs::remove_dir_all(&agent_pkg_dir);
        std::fs::create_dir_all(&agent_pkg_dir)
            .map_err(|e| GatewayError::Config(format!("Failed to create package dir: {}", e)))?;

        // Copy all files from src_dir to package dir
        Self::copy_dir_recursive(src_dir, &agent_pkg_dir)
            .map_err(|e| GatewayError::Config(format!("Failed to copy agent files: {}", e)))?;

        // Create AgentInfo and add to state
        let info = crate::gateway::state::AgentInfo {
            agent_id: agent_id.clone(),
            version,
            name: manifest.name.clone(),
            install_path: agent_pkg_dir.to_string_lossy().to_string(),
            manifest,
        };

        self.state.add_installed(info);
        Ok(agent_id)
    }

    /// Recursively copy a directory
    fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let dst_path = dst.join(entry.file_name());
            if ty.is_dir() {
                std::fs::create_dir_all(&dst_path)?;
                Self::copy_dir_recursive(&entry.path(), &dst_path)?;
            } else {
                if let Some(parent) = dst_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(entry.path(), dst_path)?;
            }
        }
        Ok(())
    }

    /// Scan packages directory and restore installed agents from disk.
    ///
    /// On startup, the Gateway needs to rebuild its in-memory `installed_agents`
    /// map by reading `manifest.toml` from each subdirectory under `packages_dir`.
    /// Without this, agents installed in a previous session are invisible.
    fn restore_installed_agents(&mut self) {
        let packages_dir = std::path::Path::new(&self.config.packages_dir);
        if !packages_dir.exists() {
            return;
        }

        let Ok(entries) = std::fs::read_dir(packages_dir) else {
            return;
        };

        for entry in entries.flatten() {
            let agent_dir = entry.path();
            if !agent_dir.is_dir() {
                continue;
            }

            let manifest_path = agent_dir.join("manifest.toml");
            if !manifest_path.exists() {
                continue;
            }

            match std::fs::read_to_string(&manifest_path) {
                Ok(content) => {
                    match toml::from_str::<rollball_core::AgentManifest>(&content) {
                        Ok(manifest) => {
                            let info = crate::gateway::state::AgentInfo {
                                agent_id: manifest.agent_id.clone(),
                                version: manifest.version.clone(),
                                name: manifest.name.clone(),
                                install_path: agent_dir.to_string_lossy().to_string(),
                                manifest,
                            };
                            let agent_id = info.agent_id.clone();
                            self.state.add_installed(info);
                            tracing::info!(
                                "Restored installed agent: {} v{}",
                                agent_id,
                                self.state.installed_agents.get(&agent_id).map(|i| i.version.as_str()).unwrap_or("?")
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to parse manifest at '{}': {}",
                                manifest_path.display(), e
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to read manifest at '{}': {}",
                        manifest_path.display(), e
                    );
                }
            }
        }

        let count = self.state.installed_agents.len();
        if count > 0 {
            tracing::info!("Restored {} installed agent(s) from disk", count);
        }
    }

    /// Kill orphaned rollball-runtime processes left over from a previous Gateway run.
    ///
    /// When Gateway restarts, previously spawned runtime processes lose their IPC
    /// connection and become useless orphans. This method finds them by scanning
    /// /proc for rollball-runtime processes whose `--gateway-socket` argument
    /// matches this Gateway's socket path, and kills them.
    ///
    /// Scoping by socket path ensures we only kill orphans belonging to THIS
    /// Gateway instance, not runtimes managed by other concurrent Gateway instances.
    fn cleanup_orphaned_runtimes(&self) -> usize {
        // Find all rollball-runtime processes
        let output = match std::process::Command::new("pgrep")
            .args(["-af", "rollball-runtime"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return 0, // pgrep not available, skip cleanup
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let my_pid = std::process::id();

        // Filter PIDs whose command line includes our socket path
        let pids_to_kill: Vec<(u32, String)> = stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
                let pid: u32 = parts.first()?.trim().parse().ok()?;
                if pid == my_pid {
                    return None; // don't kill self
                }
                let cmdline = parts.get(1).map(|s| s.trim()).unwrap_or("");
                // Only kill runtimes that were connected to OUR socket path
                if cmdline.contains(&self.config.socket_path) {
                    Some((pid, cmdline.to_string()))
                } else {
                    None
                }
            })
            .collect();

        if pids_to_kill.is_empty() {
            return 0;
        }

        tracing::info!(
            count = pids_to_kill.len(),
            "Found {} orphaned runtime process(es) for this Gateway, cleaning up",
            pids_to_kill.len()
        );

        for (pid, _cmdline) in &pids_to_kill {
            // Try graceful kill first (SIGTERM)
            match std::process::Command::new("kill")
                .args(["-15", &pid.to_string()])
                .output()
            {
                Ok(_) => tracing::info!("Sent SIGTERM to orphaned runtime (PID {})", pid),
                Err(e) => tracing::warn!("Failed to kill orphaned runtime (PID {}): {}", pid, e),
            }
        }

        pids_to_kill.len()
    }

    /// Run the Gateway daemon (async, multi-connection)
    ///
    /// This starts the IPC server and enters the main event loop.
    /// Blocks until shutdown signal is received.
    /// The GatewayState is wrapped in Arc<RwLock> for concurrent access
    /// by multiple IPC connection handlers.
    pub async fn run(
        &mut self,
        log_reload_handle: Option<crate::LogReloadHandle>,
    ) -> Result<(), GatewayError> {
        tracing::info!("Gateway starting");
        tracing::info!("  Socket path: {}", self.config.socket_path);
        tracing::info!("  Vault dir: {}", self.config.vault_dir);
        tracing::info!("  Packages dir: {}", self.config.packages_dir);

        // Ensure directories exist
        self.ensure_dirs()?;

        // In dev_mode, auto-unlock vault with a default password
        // so that API keys can be stored/retrieved without manual unlock.
        // This is intentionally insecure — dev_mode is for local development only.
        if self.config.dev_mode {
            if let Err(e) = self.state.vault.unlock("dev-mode-unlock") {
                tracing::warn!("Failed to auto-unlock vault in dev_mode: {}", e);
            } else {
                tracing::info!("Vault auto-unlocked (dev_mode)");
            }
        }

        // Scan packages directory and restore installed agents from disk
        self.restore_installed_agents();

        // Clean up orphaned runtime processes from a previous Gateway run.
        // When Gateway restarts, previously running agents become orphaned
        // (no IPC connection). We kill them so the fresh Gateway can manage
        // agents from a clean state.
        let orphan_count = self.cleanup_orphaned_runtimes();
        if orphan_count > 0 {
            tracing::info!(count = orphan_count, "Cleaned up orphan runtime processes");
        }

        // Auto-install bundled agents (System Agent, etc.) if not installed
        self.auto_install_bundled_agents().await;

        // Auto-start the System Agent if installed
        if let Err(e) = self.lifecycle.auto_start_system_agent(&mut self.state).await {
            tracing::warn!("Failed to auto-start System Agent: {}", e);
        }


        // Wrap state in Arc<RwLock> for concurrent access in multi-connection mode.
        // std::mem::take replaces self.state with Default so the Arc takes ownership.
        // This is safe because run() is the terminal daemon method that blocks forever.
        let shared_state: SharedState =
            Arc::new(RwLock::new(std::mem::take(&mut self.state)));

        // S3.2: Open CronStore and load persisted cron entries
        {
            let cron_db_path = std::path::Path::new(&self.config.data_dir).join("cron_entries.db");
            match CronStore::open(&cron_db_path) {
                Ok(store) => {
                    let mut gw = shared_state.write().await;
                    if let Err(e) = gw.cron_scheduler.load_from_store(&store) {
                        tracing::warn!("Failed to load cron entries: {}", e);
                    }
                    gw.cron_store = Some(std::sync::Arc::new(store));
                }
                Err(e) => {
                    tracing::warn!("Failed to open cron store: {}", e);
                }
            }
        }

        // P0-2 fix: Store config snapshot in GatewayState for Config API
        {
            let mut gw = shared_state.write().await;
            gw.config = Some(self.config.clone());
        }

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

        // Clone HTTP config before moving into the task
        let http_config = self.config.http.clone();
        let data_dir_path = std::path::PathBuf::from(&self.config.data_dir);

        // Create shared session manager for both IPC and HTTP
        let session_mgr: crate::http::routes::SharedSessionMgr =
            Arc::new(tokio::sync::Mutex::new(crate::ipc::session::SessionManager::new()));
        let http_session_mgr = Some(session_mgr.clone());

        // Store session manager in shared state so HTTP API can access it
        // Also store models_cache so IPC server can look up model capabilities
        let models_cache: crate::http::models_api::ModelsCache =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));
        {
            let mut gw = shared_state.write().await;
            gw.ipc_sessions = Some(session_mgr.clone());
            gw.models_cache = Some(models_cache.clone());
        }

        // S3.1: Start cron scheduler tick loop
        let cron_scheduler = Arc::new(tokio::sync::Mutex::new({
            let gw = shared_state.read().await;
            std::mem::take(&mut gw.cron_scheduler.clone())
        }));
        // Sync back loaded entries into the shared scheduler
        {
            let mut gw = shared_state.write().await;
            gw.cron_scheduler = {
                let sched = cron_scheduler.lock().await;
                sched.clone()
            };
        }
        let cron_session_mgr = session_mgr.clone();
        let cron_gw_state = shared_state.clone();
        let _cron_handle = tokio::spawn(async move {
            crate::cron::run_cron_scheduler(
                cron_scheduler,
                cron_session_mgr,
                cron_gw_state,
            ).await;
        });

        // Create bridge channel for HTTP ↔ IPC message forwarding
        let (bridge_tx, _) = tokio::sync::broadcast::channel::<crate::http::routes::BridgeEvent>(256);
        let http_bridge_tx = Some(bridge_tx.clone());

        // S1.14 / Task #12: Create shared session_pending for HTTP ↔ gRPC bridge.
        // HTTP handlers store oneshot senders here; gRPC dispatch resolves them
        // when Runtime replies with IntentSend(action=session_response).
        let session_pending: crate::http::routes::SessionPendingRequests =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let http_session_pending = Some(session_pending.clone());
        let grpc_session_pending = Some(session_pending);

        // C1+C2: Create shared approval_pending for HTTP approval endpoint ↔ gRPC dispatch.
        // gRPC dispatch stores oneshot senders here when Runtime requests tool approval;
        // HTTP handler resolves them when Desktop App user clicks Allow/Deny.
        let approval_pending: crate::http::approval::ApprovalPendingRequests =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let http_approval_pending = Some(approval_pending.clone());
        let grpc_approval_pending = Some(approval_pending);

        // Task #12: Create shared gRPC session manager for Gateway→Runtime request-response.
        // Both the gRPC server and HTTP server share this instance.
        let grpc_session_mgr: crate::grpc::SharedGrpcSessionMgr =
            Arc::new(tokio::sync::Mutex::new(crate::grpc::server::GrpcSessionManager::new()));
        let http_grpc_session_mgr = Some(grpc_session_mgr.clone());

        // Start HTTP server in a separate tokio task (parallel with IPC)
        let http_state = shared_state.clone();
        let http_socket_path = socket_path.clone();
        let http_models_cache = models_cache.clone();
        let http_handle = tokio::spawn(async move {
            if let Err(e) = crate::http::server::start_http_server(
                &http_config,
                http_state,
                &http_socket_path,
                &data_dir_path,
                http_session_mgr,
                http_grpc_session_mgr,
                http_bridge_tx,
                http_models_cache,
                http_session_pending,
                http_approval_pending,
                log_reload_handle,
            ).await {
                tracing::error!("HTTP server failed: {}", e);
            }
        });

        // Task #12: Start gRPC server so HTTP API can reach Runtime via gRPC.
        // The gRPC server registers each connection in ipc_session_mgr,
        // so HTTP handlers find gRPC-connected agents via the same path.
        let grpc_state = shared_state.clone();
        let grpc_bridge_tx = Some(bridge_tx.clone());
        let (capability_tx, _) = tokio::sync::broadcast::channel::<rollball_core::protocol::GatewayResponse>(64);
        let grpc_handle = tokio::spawn(async move {
            let grpc_addr = crate::grpc::server::default_grpc_addr();
            if let Err(e) = crate::grpc::server::start_grpc_server(
                grpc_addr,
                grpc_state,
                grpc_session_mgr,
                session_mgr,
                capability_tx,
                grpc_bridge_tx,
                grpc_session_pending,
                grpc_approval_pending,
            ).await {
                tracing::error!("gRPC server failed: {}", e);
            }
        });

        // S5.9: Wait for either SIGTERM/SIGINT or server exit.
        // On signal, all server tasks are aborted, triggering
        // PidFileGuard::Drop which cleans up the pidfile.
        let shutdown_result = tokio::select! {
            grpc_result = grpc_handle => {
                tracing::info!("gRPC server exited");
                grpc_result.map_err(|e| GatewayError::Config(format!("gRPC server task error: {}", e)))
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received shutdown signal, cleaning up...");
                Ok(())
            }
        };

        // Clean up HTTP server on any exit path (triggers PidFileGuard::Drop for pidfile cleanup)
        http_handle.abort();

        shutdown_result?;

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
        self.lifecycle.start_agent(agent_id, &mut self.state, false).await?;
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

    /// Package an installed agent into .agent file (CLI command)
    pub fn package_agent(
        &self,
        agent_id: &str,
        output_dir: Option<&str>,
        sign: bool,
        key_dir: Option<&str>,
    ) -> Result<String, GatewayError> {
        let output = output_dir
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("./build"));
        let key = key_dir.map(std::path::PathBuf::from);

        let result = crate::package_manager::publish::build_package(
            agent_id,
            &output,
            sign,
            key.as_deref(),
            &self.state,
        )?;

        Ok(format!(
            "Package built: {} ({} bytes, signed: {})",
            result.output_path, result.file_size, result.signed
        ))
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
            log_file_size_mb: 10,
            idle_timeout_secs: 0,
            max_iterations: 20,
            iteration_timeout_ms: 30000,
            dev_mode: true,
            http: crate::config::HttpConfig::default(),
            default_provider: None,
            default_model: None,
            max_output_tokens_limit: 32_768,
        }
    }

    #[test]
    fn test_gateway_new() {
        let config = test_config();
        let gateway = Gateway::new(config).unwrap();
        assert!(gateway.list_agents().is_empty());
    }

    #[test]
    fn test_ensure_dirs() {
        let config = test_config();
        let gateway = Gateway::new(config).unwrap();
        assert!(gateway.ensure_dirs().is_ok());
    }

    #[test]
    fn test_list_agents_empty() {
        let config = test_config();
        let gateway = Gateway::new(config).unwrap();
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
