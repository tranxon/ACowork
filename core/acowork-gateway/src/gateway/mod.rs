//! Gateway main module
//!
//! The Gateway struct is the top-level orchestrator that ties together
//! gRPC server, lifecycle manager, package manager, and vault.

pub mod state;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::GatewayConfig;
use crate::cron::CronStore;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::interaction_store::InteractionStore;
use crate::compat::GlobalResourcePusher;
use crate::handlers::server::SharedState;
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
        config.validate()?;

        let idle_timeout = config.timeouts.idle_timeout_secs;
        let log_file_size_mb = config.log_file_size_mb;
        let log_file_count = config.log_file_count;
        let vault_dir = config.vault_dir.clone();
        let data_dir = config.data_dir.clone();

        // Ensure data directory exists before opening the database
        std::fs::create_dir_all(&data_dir).map_err(|e| {
            GatewayError::Config(format!(
                "Failed to create data directory '{}': {}",
                data_dir, e
            ))
        })?;

        // Build the gRPC endpoint URL that Runtime processes will use to connect.
        // ADR-033: gRPC disabled; hardcoded endpoint kept for Runtime spawn args only.
        // Runtime expects an HTTP URL like "http://127.0.0.1:19877".
        let gateway_grpc_endpoint = "http://127.0.0.1:19877".to_string();

        // ADR-033: MQTT port for Runtime lifecycle (if MQTT is enabled).
        let lifecycle_mqtt_port = if config.mqtt.enabled { Some(config.mqtt.port) } else { None };

        // Wire up the per-agent interaction store. Keys are agent_id, so the
        // timestamps survive agent stop/restart. Loaded eagerly so the
        // /api/agents sort order is correct from the first request.
        let interaction_store = InteractionStore::new(std::path::Path::new(&data_dir));
        let mut state = GatewayState::new(&vault_dir);
        state.interaction_store = Some(interaction_store.clone());
        state.last_interactions = interaction_store.load();

        Ok(Self {
            config,
            state,
            lifecycle: LifecycleManager::new(
                idle_timeout,
                gateway_grpc_endpoint,
                log_file_size_mb,
                log_file_count,
                lifecycle_mqtt_port,
            ),
        })
    }

    /// Auto-install bundled agents (System Agent, etc.) if not already installed.
    ///
    /// This is called during Gateway startup. It looks for bundled agents in:
    /// 1. The project source directory (../../examples/)
    /// 2. The ACOWORK_BUNDLED_AGENTS_DIR environment variable
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
        tracing::info!(
            "Auto-installing bundled System Agent from {:?}",
            system_agent_src
        );
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
        if let Ok(dir) = std::env::var("ACOWORK_BUNDLED_AGENTS_DIR") {
            let path = std::path::PathBuf::from(&dir);
            if path.exists() {
                return Some(path);
            }
        }

        // Try to find project root from CARGO_MANIFEST_DIR
        // CARGO_MANIFEST_DIR = core/acowork-gateway
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let project_root = manifest_dir.parent()?.parent()?;
        let bundled_dir = project_root.join("examples");

        if bundled_dir.exists() {
            return Some(bundled_dir);
        }

        None
    }

    /// Install an agent from a source directory.
    async fn install_agent_from_dir(
        &mut self,
        src_dir: &std::path::Path,
    ) -> Result<String, GatewayError> {
        use acowork_core::AgentManifest;

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
        let agent_pkg_dir = packages_dir.join(&agent_id);

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
                Ok(content) => match toml::from_str::<acowork_core::AgentManifest>(&content) {
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
                            self.state
                                .installed_agents
                                .get(&agent_id)
                                .map(|i| i.version.as_str())
                                .unwrap_or("?")
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to parse manifest at '{}': {}",
                            manifest_path.display(),
                            e
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "Failed to read manifest at '{}': {}",
                        manifest_path.display(),
                        e
                    );
                }
            }
        }

        let count = self.state.installed_agents.len();
        if count > 0 {
            tracing::info!("Restored {} installed agent(s) from disk", count);
        }

        // ADR-017: Load avatar cache and apply to in-memory manifest so
        // list_agents returns the correct avatar even for stopped agents.
        let data_dir = std::path::Path::new(&self.config.data_dir);
        let avatar_cache = crate::http::agent_config::load_avatar_cache(data_dir);
        if !avatar_cache.is_empty() {
            for (agent_id, entry) in &avatar_cache {
                if let Some(info) = self.state.installed_agents.get_mut(agent_id) {
                    info.manifest.avatar = entry.avatar.clone();
                    info.manifest.builtin_avatar = entry.builtin_avatar.clone();
                }
            }
            tracing::info!("Loaded avatar cache with {} entries", avatar_cache.len());
        }
    }

    /// Kill orphaned acowork-runtime processes left over from a previous Gateway run.
    ///
    /// When Gateway restarts, previously spawned runtime processes lose their
    /// connection and become useless orphans. This method finds them by scanning
    /// for acowork-runtime processes whose `--gateway-endpoint` argument
    /// matches this Gateway's endpoint.
    ///
    /// Since Gateway is single-instance per host (enforced by HTTP port probing),
    /// scoping by endpoint is a safety measure against false positives.
    fn cleanup_orphaned_runtimes(&self) -> usize {
        let grpc_endpoint_url = "http://127.0.0.1:19877".to_string();

        // Find all acowork-runtime processes
        let output = match std::process::Command::new("pgrep")
            .args(["-af", "acowork-runtime"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return 0, // pgrep not available, skip cleanup
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let my_pid = std::process::id();

        // Filter PIDs whose command line includes our gRPC endpoint
        let pids_to_kill: Vec<(u32, String)> = stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
                let pid: u32 = parts.first()?.trim().parse().ok()?;
                if pid == my_pid {
                    return None; // don't kill self
                }
                let cmdline = parts.get(1).map(|s| s.trim()).unwrap_or("");
                // Only kill runtimes connected to OUR gRPC endpoint
                if cmdline.contains(&grpc_endpoint_url) {
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
    /// This starts the gRPC server and enters the main event loop.
    /// Blocks until shutdown signal is received.
    /// The GatewayState is wrapped in Arc<RwLock> for concurrent access
    /// by multiple gRPC connection handlers.
    pub async fn run(
        &mut self,
        log_reload_handle: Option<crate::LogReloadHandle>,
    ) -> Result<(), GatewayError> {
        tracing::info!("Gateway starting");
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
        // (no gRPC connection). We kill them so the fresh Gateway can manage
        // agents from a clean state.
        let orphan_count = self.cleanup_orphaned_runtimes();
        if orphan_count > 0 {
            tracing::info!(count = orphan_count, "Cleaned up orphan runtime processes");
        }

        // Auto-install bundled agents (System Agent, etc.) if not installed
        self.auto_install_bundled_agents().await;

        // Auto-start the System Agent if installed
        if let Err(e) = self
            .lifecycle
            .auto_start_system_agent(&mut self.state)
            .await
        {
            tracing::warn!("Failed to auto-start System Agent: {}", e);
        }

        // Try to spawn the local embedding service (acowork-embed).
        // This is optional — if the binary is not found, embedding will
        // fall back to remote providers (Ollama / OpenAI-compatible API).
        // The embed process state is stored in GatewayState for the
        // HTTP embedding API to reference.
        let mut embed_child = None;
        let mut embed_supervisor_cfg: Option<
            crate::lifecycle::embed_supervisor::EmbedSupervisorConfig,
        > = None;
        {
            let data_dir = std::path::PathBuf::from(&self.config.data_dir);
            let models_dir = data_dir.join("models");
            let embed_port = 18080; // Default port for embedding service
            let hf_mirrors = self.config.hf_mirrors.clone();
            let embedding_model = self.config.embedding_model.clone();
            let onnx_variant = "onnx";
            let existing_health = crate::lifecycle::embed::check_embed_health(embed_port).await;
            if existing_health.is_some() {
                let embed_state = crate::lifecycle::embed::attach_existing_embed_process(
                    embed_port,
                    existing_health,
                );
                tracing::info!(
                    port = embed_state.port,
                    ready = embed_state.ready,
                    "Reusing existing embedding service"
                );
                self.state.embed_process = Some(embed_state);
                embed_supervisor_cfg =
                    Some(crate::lifecycle::embed_supervisor::EmbedSupervisorConfig {
                        data_dir,
                        models_dir,
                        port: embed_port,
                        hf_mirrors,
                        onnx_variant: onnx_variant.to_string(),
                        model_id: embedding_model.clone(),
                    });
            } else {
                match crate::lifecycle::embed::spawn_embed_process(
                    &data_dir,
                    &models_dir,
                    embed_port,
                    &hf_mirrors,
                    onnx_variant,
                    embedding_model.as_deref(),
                )
                .await
                {
                    Ok((embed_state, child)) => {
                        tracing::info!(
                            pid = embed_state.pid,
                            port = embed_state.port,
                            "Embedding service process spawned"
                        );
                        self.state.embed_process = Some(embed_state);
                        embed_child = Some(child);
                        embed_supervisor_cfg =
                            Some(crate::lifecycle::embed_supervisor::EmbedSupervisorConfig {
                                data_dir,
                                models_dir,
                                port: embed_port,
                                hf_mirrors,
                                onnx_variant: onnx_variant.to_string(),
                                model_id: embedding_model.clone(),
                            });
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to spawn embedding service (local ONNX embedding unavailable, will use remote fallback)"
                        );
                    }
                }
            }
        }

        // Load resource cache (provider_list.json + mcp_list.json) into memory.
        // These files are rebuilt by HTTP handlers when resources change.
        let cache_dir = std::path::PathBuf::from(&self.config.data_dir);
        self.state.resource_cache = crate::resource_cache::load_resource_cache(&cache_dir);

        // Wrap state in Arc<RwLock> for concurrent access in multi-connection mode.
        // std::mem::take replaces self.state with Default so the Arc takes ownership.
        // This is safe because run() is the terminal daemon method that blocks forever.
        let shared_state: SharedState = Arc::new(RwLock::new(std::mem::take(&mut self.state)));

        // Spawn embed process reaper — clears state when the child exits.
        // This is the single source of truth for embed process lifecycle:
        // when the child process exits (normally or by crash), the shared
        // state is updated atomically. HTTP handlers see embed_process=None
        // on the very next request, with no defensive PID polling needed.
        //
        // The reaper is PID-aware: if the supervisor has already replaced
        // this child with a new embed (different PID), we leave the new
        // state alone.
        if let Some(mut child) = embed_child {
            // Capture the PID before moving child into the async block.
            // `child.id()` returns Option<u32>; if the child has already
            // been reaped (None) we skip the reaper entirely.
            let child_pid = child.id();
            let state_for_reaper = shared_state.clone();
            tokio::spawn(async move {
                let Some(target_pid) = child_pid else {
                    return;
                };
                let exit_status = child.wait().await;
                tracing::warn!(
                    pid = target_pid,
                    exit_status = ?exit_status,
                    "Embedding service process exited"
                );
                let mut gw = state_for_reaper.write().await;
                let still_ours = gw
                    .embed_process
                    .as_ref()
                    .map(|eps| eps.pid == target_pid)
                    .unwrap_or(false);
                if still_ours {
                    gw.embed_process = None;
                } else {
                    tracing::debug!(
                        old_pid = target_pid,
                        current_pid = ?gw.embed_process.as_ref().map(|e| e.pid),
                        "Embed reaper: state already replaced by supervisor; leaving alone"
                    );
                }
            });
        }

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

        // Spawn the idle timeout checker in a background task
        let idle_timeout = self.config.timeouts.idle_timeout_secs;
        let _idle_handle = tokio::spawn(async move {
            if idle_timeout > 0 {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(idle_timeout.min(60)));
                loop {
                    interval.tick().await;
                    // Phase 2: check idle timeouts and stop idle agents
                    tracing::trace!("Idle timeout check (configured: {}s)", idle_timeout);
                }
            }
        });

        tracing::info!("Gateway entering gRPC event loop (async multi-connection)");

        // Clone HTTP config before moving into the task
        let http_config = self.config.http.clone();
        let data_dir_path = std::path::PathBuf::from(&self.config.data_dir);

        // Rebuild resource cache from MCP catalog at startup.
        // provider_list.json is loaded by load_resource_cache() above;
        // it is the source of truth for provider config. No rebuild needed.
        {
            let mut gw = shared_state.write().await;
            if let Ok(catalog) = crate::http::mcp_catalog_api::load_mcp_catalog(&data_dir_path) {
                crate::resource_cache::rebuild_and_save_mcp_cache(
                    &mut gw,
                    &data_dir_path,
                    &catalog,
                );
            }
            // Rebuild search_list cache from Vault search keys at startup
            crate::resource_cache::rebuild_and_save_search_cache(&mut gw, &data_dir_path);
        }

        // S3.1: Load cron scheduler entries from store
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

        // Start HTTP server in a separate tokio task (parallel with gRPC)
        let http_state = shared_state.clone();

        // Create unified global resource pusher for hot-push of provider_list,
        // search_config, MCP catalog, and user profile changes to running agents.
        let pusher: Option<Arc<GlobalResourcePusher>> = Some(Arc::new(GlobalResourcePusher::new(
            None,
            http_state.clone(),
            data_dir_path.clone(),
        )));

        // Start the embed supervisor. It watches the embed's SSE event
        // stream, updates `shared_state.embed_process.{active_model_id,
        // active_dimension, ready}` from the embed's state events, and
        // restarts the embed process on heartbeat timeout or connection
        // loss (with exponential backoff and a 5-attempts/5-min cap).
        // The HTTP API and gRPC pushers read the same `shared_state` via
        // a separate Arc clone, so updates are visible immediately.
        if let (Some(sup_cfg), Some(shared_arc)) =
            (embed_supervisor_cfg.take(), Some(shared_state.clone()))
        {
            let supervisor_pusher = pusher.clone();
            crate::lifecycle::embed_supervisor::start_embed_supervisor(
                sup_cfg,
                shared_arc,
                supervisor_pusher,
            );
        }

        // Spawn the LSP Relay process (acowork-lsp-relay).
        // This is optional — if the binary is not found, LSP functionality
        // is unavailable but the Gateway continues normally.
        // The relay runs as an independent process with its own HTTP server,
        // WebSocket LSP relay, and LSP process pool.
        // See ADR-019 for the full architecture rationale.
        let mut lsp_relay_child = None;
        let mut lsp_relay_supervisor_cfg: Option<
            crate::lifecycle::lsp_relay_supervisor::LspRelaySupervisorConfig,
        > = None;
        {
            let data_dir = std::path::PathBuf::from(&self.config.data_dir);
            let lsp_relay_port = crate::lifecycle::lsp_relay::LSP_RELAY_DEFAULT_PORT;
            let gateway_health_url = format!("http://127.0.0.1:{}/health", self.config.http.port);

            // Check if an LSP Relay is already running on the expected port
            let existing_health =
                crate::lifecycle::lsp_relay::check_lsp_relay_health(lsp_relay_port).await;
            if let Some(health) = existing_health {
                let relay_state = crate::lifecycle::lsp_relay::attach_existing_lsp_relay(
                    lsp_relay_port,
                    Some(health),
                );
                tracing::info!(
                    port = relay_state.port,
                    ready = relay_state.ready,
                    "Reusing existing LSP Relay process"
                );
                {
                    let mut gw = shared_state.write().await;
                    gw.lsp_relay_process = Some(relay_state);
                }
                lsp_relay_supervisor_cfg = Some(
                    crate::lifecycle::lsp_relay_supervisor::LspRelaySupervisorConfig {
                        data_dir,
                        port: lsp_relay_port,
                        gateway_health_url,
                        pusher: pusher.clone(),
                    },
                );
            } else {
                match crate::lifecycle::lsp_relay::spawn_lsp_relay(
                    &data_dir,
                    lsp_relay_port,
                    &gateway_health_url,
                )
                .await
                {
                    Ok((relay_state, child)) => {
                        tracing::info!(
                            pid = relay_state.pid,
                            port = relay_state.port,
                            "LSP Relay process spawned"
                        );
                        {
                            let mut gw = shared_state.write().await;
                            gw.lsp_relay_process = Some(relay_state);
                        }
                        lsp_relay_child = Some(child);
                        lsp_relay_supervisor_cfg = Some(
                            crate::lifecycle::lsp_relay_supervisor::LspRelaySupervisorConfig {
                                data_dir,
                                port: lsp_relay_port,
                                gateway_health_url,
                                pusher: pusher.clone(),
                            },
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to spawn LSP Relay (LSP functionality will be unavailable)"
                        );
                    }
                }
            }
        }

        // Spawn LSP Relay process reaper — clears state when the child exits.
        if let Some(mut child) = lsp_relay_child {
            let child_pid = child.id();
            let state_for_reaper = shared_state.clone();
            tokio::spawn(async move {
                let Some(target_pid) = child_pid else {
                    return;
                };
                let exit_status = child.wait().await;
                tracing::warn!(
                    pid = target_pid,
                    exit_status = ?exit_status,
                    "LSP Relay process exited"
                );
                let mut gw = state_for_reaper.write().await;
                let still_ours = gw
                    .lsp_relay_process
                    .as_ref()
                    .map(|eps| eps.pid == target_pid)
                    .unwrap_or(false);
                if still_ours {
                    gw.lsp_relay_process = None;
                }
            });
        }

        // Start the LSP Relay supervisor (SSE heartbeat monitoring + restart).
        if let Some(sup_cfg) = lsp_relay_supervisor_cfg.take() {
            crate::lifecycle::lsp_relay_supervisor::start_lsp_relay_supervisor(
                sup_cfg,
                shared_state.clone(),
                pusher.clone(),
            );
        }

        // ADR-033: Start MQTT broker + Gateway client BEFORE HTTP server
        // so it's available for chat handlers to publish control commands.
        let mqtt_config = self.config.mqtt.clone();
        let mqtt_broker_handle: Option<crate::mqtt::MqttBrokerHandle> = if mqtt_config.enabled {
            // ADR-033: start_broker_in_thread runs in a separate OS thread
            // because rumqttd creates its own tokio runtime internally.
            match crate::mqtt::start_broker_in_thread(&mqtt_config.host, mqtt_config.port) {
                Ok(h) => { tracing::info!(addr = %h.listen_addr, "MQTT broker started"); Some(h) }
                Err(e) => { tracing::error!(%e, "MQTT broker failed"); None }
            }
        } else { None };

        // ADR-033: Create runtime HTTP registry and agent registry.
        let runtime_http_registry = crate::http::proxy::new_shared_registry();
        let agent_registry = crate::mqtt::agent_registry::new_shared_registry();

        let mqtt_gw_client: Option<Arc<crate::mqtt::GatewayMqttClient>> = if mqtt_broker_handle.is_some() {
            let reg_for_dispatch = runtime_http_registry.clone();
            let agent_reg_for_dispatch = agent_registry.clone();
            let callback: crate::mqtt::MqttMessageCallback = Arc::new(move |topic, payload| {
                // Unified dispatch: try DataEnvelope (protobuf) first,
                // then fall through to plaintext (http_port, status).
                crate::mqtt::dispatch::handle_message(
                    &topic, &payload,
                    &reg_for_dispatch,
                    &agent_reg_for_dispatch,
                );
            });

            match crate::mqtt::GatewayMqttClient::new_publisher_with_callback(&mqtt_config.host, mqtt_config.port, callback).await {
                Ok(c) => {
                    tracing::info!("MQTT Gateway client connected");
                    let client = c.clone();
                    tokio::spawn(async move {
                        // Surface subscription errors instead of silently swallowing them —
                        // if either subscription fails, the Gateway's runtime_http_registry
                        // and agent_registry will stay empty and every reverse-proxy request
                        // will 503 (latency=0) with no diagnostic trail.
                        if let Err(e) = client
                            .subscribe("acowork/agents/+/http_port", crate::mqtt::MqttQoS::AtLeastOnce)
                            .await
                        {
                            tracing::error!(
                                topic = "acowork/agents/+/http_port",
                                error = %e,
                                "Failed to subscribe to Runtime http_port topic — Gateway will 503 every reverse-proxy request"
                            );
                        } else {
                            tracing::info!("Subscribed to acowork/agents/+/http_port");
                        }
                        if let Err(e) = client
                            .subscribe("acowork/agents/+/status", crate::mqtt::MqttQoS::AtLeastOnce)
                            .await
                        {
                            tracing::error!(
                                topic = "acowork/agents/+/status",
                                error = %e,
                                "Failed to subscribe to Runtime status topic — AgentRegistry will never be populated"
                            );
                        } else {
                            tracing::info!("Subscribed to acowork/agents/+/status");
                        }
                    });
                    Some(Arc::new(c))
                }
                Err(e) => { tracing::warn!(%e, "MQTT Gateway client failed"); None }
            }
        } else { None };

        // ADR-033: Start MQTT Global Resources Publisher.
        // Publishes providers, models, MCP catalog, searches, embedding models
        // to acowork/global/* Retained topics so Runtime can discover them.
        let mqtt_publisher_trigger: Option<crate::mqtt::MqttPublisherTrigger> = if let Some(ref client) = mqtt_gw_client {
            let publisher = crate::mqtt::MqttGlobalResourcesPublisher::new(
                client.as_ref().clone(),
                shared_state.clone(),
            );
            let handle = publisher.start();
            let trigger = handle.create_trigger();
            tracing::info!("MQTT Global Resources Publisher started");
            // Store the handle to keep the publisher loop alive.
            // We don't hold it explicitly — it's kept alive by the tokio task.
            Some(trigger)
        } else { None };

        // ADR-033: Start cron scheduler (uses MQTT for Intent delivery).
        // Must be started AFTER MQTT client is available.
        {
            let cron_mqtt = mqtt_gw_client.clone();
            let cron_gw_state = shared_state.clone();
            let _cron_handle = tokio::spawn(async move {
                crate::cron::run_cron_scheduler(cron_scheduler, cron_mqtt, cron_gw_state).await;
            });
        }

        let http_handle = tokio::spawn(async move {
            if let Err(e) = crate::http::server::start_http_server(
                &http_config,
                http_state,
                &data_dir_path,
                log_reload_handle,
                pusher,
                mqtt_gw_client,
                mqtt_publisher_trigger,
                Some(runtime_http_registry),
                Some(agent_registry),
            )
            .await
            {
                tracing::error!("HTTP server failed: {}", e);
            }
        });

        // S5.9: Wait for either SIGTERM/SIGINT or HTTP server exit.
        // On signal, all server tasks are aborted, triggering
        // PidFileGuard::Drop which cleans up the pidfile.
        let shutdown_result = tokio::select! {
            http_result = http_handle => {
                tracing::info!("HTTP server exited");
                http_result.map_err(|e| GatewayError::Config(format!("HTTP server task error: {}", e)))
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received shutdown signal, cleaning up...");

                // Kill all running Runtime (agent) processes before exiting.
                // This prevents orphaned Runtime processes when Gateway shuts
                // down normally. Collect PIDs under a short read-lock, then
                // release the lock before issuing async kill calls.
                {
                    let gw = shared_state.read().await;
                    let runtime_pids: Vec<(String, u32)> = gw
                        .running_agents
                        .iter()
                        .map(|(id, info)| (id.clone(), info.pid))
                        .collect();
                    // Drop the read lock before calling async kill operations
                    drop(gw);
                    for (agent_id, pid) in &runtime_pids {
                        tracing::info!(
                            agent_id = %agent_id,
                            pid = pid,
                            "Shutting down Runtime process"
                        );
                        if let Err(e) =
                            crate::lifecycle::process::kill_agent_process(*pid).await
                        {
                            tracing::warn!(
                                agent_id = %agent_id,
                                error = %e,
                                "Failed to kill Runtime process"
                            );
                        }
                    }
                }

                // Kill the embedding service process before exiting.
                // This prevents acowork-embed from becoming an orphan process.
                {
                    let gw = shared_state.read().await;
                    if let Some(ref embed_state) = gw.embed_process
                        && embed_state.pid != 0
                    {
                        tracing::info!(pid = embed_state.pid, "Shutting down embedding service");
                        if let Err(e) = crate::lifecycle::embed::kill_embed_process(embed_state.pid).await {
                            tracing::warn!(error = %e, "Failed to kill embedding service process");
                        }
                    }
                }

                // Kill the LSP Relay process before exiting.
                // This prevents acowork-lsp-relay from becoming an orphan process.
                {
                    let gw = shared_state.read().await;
                    if let Some(ref relay_state) = gw.lsp_relay_process
                        && relay_state.pid != 0
                    {
                        tracing::info!(pid = relay_state.pid, "Shutting down LSP Relay");
                        if let Err(e) = crate::lifecycle::lsp_relay::kill_lsp_relay(relay_state.pid).await {
                            tracing::warn!(error = %e, "Failed to kill LSP Relay process");
                        }
                    }
                }

                Ok(())
            }
        };

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
        uninstall::uninstall_package(agent_id, packages_dir, &mut self.state)?;
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
        self.lifecycle
            .start_agent(agent_id, &mut self.state, false)
            .await?;
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
        for dir in &[
            &self.config.vault_dir,
            &self.config.packages_dir,
            &self.config.data_dir,
        ] {
            std::fs::create_dir_all(dir).map_err(|e| {
                GatewayError::Config(format!("Failed to create directory '{}': {}", dir, e))
            })?;
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
        write!(
            f,
            "{} ({}) v{} [{}]",
            self.name, self.agent_id, self.version, status
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;

    fn test_config() -> GatewayConfig {
        GatewayConfig {
            config_source_path: None,
            vault_dir: std::env::temp_dir()
                .join("acowork-test-vault")
                .to_string_lossy()
                .to_string(),
            packages_dir: std::env::temp_dir()
                .join("acowork-test-packages")
                .to_string_lossy()
                .to_string(),
            data_dir: std::env::temp_dir()
                .join("acowork-test-data")
                .to_string_lossy()
                .to_string(),
            log_level: "info".to_string(),
            log_file_size_mb: 10,
            log_file_count: 20,
            timeouts: acowork_core::Timeouts::default(),
            max_iterations: 20,
            dev_mode: true,
            http: crate::config::HttpConfig::default(),
            default_provider: None,
            default_model: None,
            max_output_tokens_limit: 32768,
            embedding_model: None,
            hf_mirrors: Vec::new(),
            data_flow: crate::config::DataFlowConfig::default(),
            mqtt: crate::config::MqttConfig::default(),
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
