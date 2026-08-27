//! Gateway main module
//!
//! The Gateway struct is the top-level orchestrator that ties together
//! gRPC server, lifecycle manager, package manager, and vault.

pub mod state;
pub mod node_manager;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::GatewayConfig;
use crate::cron::CronStore;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::interaction_store::InteractionStore;
use crate::handlers::server::SharedState;
use crate::gateway::state::SYSTEM_AGENT_ID;

/// Gateway — the top-level orchestrator
///
/// Owns all sub-systems and drives the event loop.
///
/// `state` is a [`SharedState`] (`Arc<RwLock<GatewayState>>`) from the
/// very first moment after construction. Wrapping the state in a shared
/// handle at construction time (rather than after a `std::mem::take` in
/// `run()`) eliminates the entire family of "self.state is a stand-in,
/// writes go nowhere" bugs that arise when an owned field is converted
/// to a shared handle partway through initialisation.
pub struct Gateway {
    config: GatewayConfig,
    state: SharedState,
}

impl Gateway {
    /// Create a new Gateway instance with the given configuration
    ///
    /// Construction runs entirely on a single thread before any async
    /// runtime exists, so we first build an owned `GatewayState` and run
    /// all synchronous initialisation (`interaction_store` setup,
    /// `restore_installed_agents`) directly against it. We then wrap the
    /// fully-initialised state in a `SharedState`. From this point on,
    /// every code path sees the same shared handle — there is no
    /// `std::mem::take`/`mem::swap` dance and no "two views of the state".
    pub fn new(config: GatewayConfig) -> Result<Self, GatewayError> {
        config.validate()?;

        let vault_dir = config.vault_dir.clone();
        let data_dir = config.data_dir.clone();

        // Ensure data directory exists before opening the database
        std::fs::create_dir_all(&data_dir).map_err(|e| {
            GatewayError::Config(format!(
                "Failed to create data directory '{}': {}",
                data_dir, e
            ))
        })?;

        // Wire up the per-agent interaction store. Keys are agent_id, so the
        // timestamps survive agent stop/restart. Loaded eagerly so the
        // /api/agents sort order is correct from the first request.
        let interaction_store = InteractionStore::new(std::path::Path::new(&data_dir));
        let mut state = GatewayState::new(&vault_dir);
        state.interaction_store = Some(interaction_store.clone());
        state.last_interactions = interaction_store.load();

        // ADR-055 §6.5: installed agents are no longer restored by an
        // on-disk packages scan (L2-9). The local node publishes its
        // installed-package inventory (retained InstalledAgentInfo) after
        // it starts; the Gateway aggregates those into `installed_agents`
        // via the MQTT dispatch path.

        let gateway = Self {
            config,
            state: Arc::new(RwLock::new(state)),
        };

        Ok(gateway)
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
        if self.state.read().await.is_installed(SYSTEM_AGENT_ID) {
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
                // The local node re-discovers the copied package from its
                // packages dir on startup and publishes the retained
                // installed info; no in-memory refresh is needed here.
                tracing::info!("Successfully auto-installed bundled agent: {}", agent_id);
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

        // Copy agent files to packages directory. The local node
        // re-discovers the copied package from its packages dir on startup
        // and publishes the retained installed info — the Gateway does NOT
        // add it to installed_agents directly (ADR-055 §6.5 / L2-9).
        let packages_dir = std::path::Path::new(&self.config.packages_dir);
        let agent_pkg_dir = packages_dir.join(&agent_id);

        let _ = std::fs::remove_dir_all(&agent_pkg_dir);
        std::fs::create_dir_all(&agent_pkg_dir)
            .map_err(|e| GatewayError::Config(format!("Failed to create package dir: {}", e)))?;

        Self::copy_dir_recursive(src_dir, &agent_pkg_dir)
            .map_err(|e| GatewayError::Config(format!("Failed to copy agent files: {}", e)))?;

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

    /// Kill orphaned acowork-runtime processes left over from a previous Gateway run.
    ///
    /// When Gateway restarts, previously spawned runtime processes lose their
    /// MQTT connection (or fail to reconnect) and become useless orphans. This
    /// method finds them by scanning for `acowork-runtime` processes whose
    /// `--mqtt-port <N>` argument matches this Gateway's MQTT port.
    ///
    /// If MQTT is disabled on this Gateway, runtime processes are never given
    /// `--mqtt-port` and orphans cannot be distinguished by port — we keep them.
    ///
    /// Since Gateway is single-instance per host (enforced by HTTP port probing),
    /// scoping by MQTT port is a safety measure against false positives.
    fn cleanup_orphaned_runtimes(&self) -> usize {
        // ADR-033: gRPC endpoint no longer passed to Runtime. Use MQTT port
        // as the unique cmdline marker tying a runtime to this Gateway.
        let mqtt_marker = self
            .config
            .mqtt
            .enabled
            .then(|| format!("--mqtt-port {}", self.config.mqtt.port));

        // Find all acowork-runtime processes (full argv via `ps` — the
        // `pgrep -af` variant prints bare PIDs on macOS and silently
        // defeats every marker match).
        let procs = crate::gateway::node_manager::find_procs_by_cmdline("acowork-runtime");
        let my_pid = std::process::id();

        // Filter PIDs whose command line matches our MQTT marker (if any).
        let pids_to_kill: Vec<(u32, String)> = procs
            .into_iter()
            .filter_map(|(pid, cmdline)| {
                if pid == my_pid {
                    return None; // don't kill self
                }
                match &mqtt_marker {
                    Some(marker) if cmdline.contains(marker.as_str()) => {
                        Some((pid, cmdline))
                    }
                    _ => None,
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
    /// This starts the HTTP server, MQTT broker, embed/LSP supervisors and
    /// the main event loop. Blocks until shutdown signal is received.
    ///
    /// `self.state` is already a [`SharedState`] at construction time, so
    /// there is no `std::mem::take`/`mem::swap` dance here — every
    /// mutation goes through the shared lock and every reader sees the
    /// same data.
    pub async fn run(
        &mut self,
        log_reload_handle: Option<crate::LogReloadHandle>,
    ) -> Result<(), GatewayError> {
        tracing::info!("Gateway starting");
        tracing::info!("  Vault dir: {}", self.config.vault_dir);
        tracing::info!("  Packages dir: {}", self.config.packages_dir);

        // Ensure directories exist
        self.ensure_dirs()?;

        // ADR-055 §6.5: installed agents are no longer restored by an
        // on-disk packages scan (L2-9). The local node publishes its
        // retained installed inventory after it starts; the Gateway
        // aggregates those via the MQTT dispatch path.

        // Clean up orphaned runtime processes from a previous Gateway run.
        let orphan_count = self.cleanup_orphaned_runtimes();
        if orphan_count > 0 {
            tracing::info!(count = orphan_count, "Cleaned up orphan runtime processes");
        }

        // Auto-install bundled agents (System Agent, etc.) if not installed
        self.auto_install_bundled_agents().await;

        // `self.state` is already a SharedState. Clone the handle so the
        // rest of `run()` can move it into long-lived tasks (reapers,
        // supervisors, HTTP server) while `&mut self` continues to be
        // available for the occasional self-state mutation.
        let shared_state: SharedState = self.state.clone();

        // System Agent auto-start now happens AFTER the local node is up
        // and its installed inventory has been aggregated (see below).

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
                self.state.write().await.embed_process = Some(embed_state);
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
                        self.state.write().await.embed_process = Some(embed_state);
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
        // Writes go through the SharedState write lock so the very next
        // HTTP/MQTT request sees the loaded data — no more "loaded but
        // invisible" race where `self.state` is a stand-in.
        let cache_dir = std::path::PathBuf::from(&self.config.data_dir);
        let loaded_cache = crate::resource_cache::load_resource_cache(&cache_dir);
        shared_state.write().await.resource_cache = loaded_cache;

        // `shared_state` was constructed *earlier* in this function
        // (see `let shared_state: SharedState = self.state.clone();`)
        // so the System Agent's reaper can be wired up.

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
            // ADR-055 D3: resolve the advertise host once at startup
            // (config > auto-detected non-loopback IP > 127.0.0.1) and
            // cache it on GatewayState so every endpoint constructor
            // (embed / LSP / AgentHello) reads a single source of truth.
            let advertise = crate::config::resolve_advertise_host(&self.config);
            tracing::info!(advertise_host = %advertise, "Resolved advertise host (ADR-055 D3)");
            gw.advertise_host = advertise;
        }

        // Idle-timeout decision is owned by the Runtime now (see
        // `acowork-runtime::agent::idle_watcher`); the Gateway only
        // observes the `sleeping` retained status that the Runtime
        // publishes and stamps `AgentInfo.sleeping_at` for the
        // /api/agents listing. No background checker is spawned here.

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

        // Start the embed supervisor. It watches the embed's SSE event
        // stream, updates `shared_state.embed_process.{active_model_id,
        // active_dimension, ready}` from the embed's state events, and
        // restarts the embed process on heartbeat timeout or connection
        // loss (with exponential backoff and a 5-attempts/5-min cap).
        if let (Some(sup_cfg), Some(shared_arc)) =
            (embed_supervisor_cfg.take(), Some(shared_state.clone()))
        {
            crate::lifecycle::embed_supervisor::start_embed_supervisor(
                sup_cfg,
                shared_arc,
            );
        }

        // ADR-055 §6.7 (Phase 4): the LSP relay is NO LONGER hosted
        // here — each Node hosts its own node-local relay and publishes
        // the retained `acowork/nodes/{node_id}/lsps` topic. The Gateway
        // subscribes to that topic (node_registry) and serves
        // `GET /api/agents/{id}/lsp-endpoint` from it.

        // ADR-033: Start MQTT broker + Gateway client BEFORE HTTP server
        // so it's available for chat handlers to publish control commands.
        let mqtt_config = self.config.mqtt.clone();
        // ADR-055 Phase 5a: prepare the credential stores + internal
        // tokens BEFORE the broker starts so the CONNECT auth handler
        // installs with the full state. The HTTP token doubles as the
        // Desktop MQTT password, so it is generated whenever either
        // channel enables auth (the HTTP handler layer itself stays
        // permissive in this phase — the token is only consumed by the
        // broker CONNECT handler and /api/status).
        let http_auth = Arc::new(crate::http::auth::HttpAuth::new(
            http_config.auth_enabled || mqtt_config.auth_enabled,
        ));
        if let Err(e) = http_auth.write_token_file(&data_dir_path) {
            tracing::warn!(error = %e, "Failed to write http_token file");
        }
        let enrollment_tokens = crate::mqtt::new_shared_enrollment_store(&data_dir_path);
        let node_tokens = crate::mqtt::new_shared_node_token_store(&data_dir_path);
        let publisher_token = crate::mqtt::enrollment::generate_token();
        let broker_auth = crate::mqtt::broker::BrokerAuth {
            auth_enabled: mqtt_config.auth_enabled,
            enrollment_tokens: enrollment_tokens.clone(),
            node_tokens: node_tokens.clone(),
            publisher_token: publisher_token.clone(),
            http_token: http_auth.token().map(str::to_string),
        };

        let mut mqtt_broker_handle: Option<crate::mqtt::MqttBrokerHandle> = if mqtt_config.enabled {
            // ADR-033: start_broker runs in a separate OS thread
            // because rumqttd creates its own tokio runtime internally.
            let auth = if mqtt_config.auth_enabled {
                Some(broker_auth.clone())
            } else {
                None
            };
            match crate::mqtt::start_broker_with_auth(&mqtt_config.host, mqtt_config.port, auth) {
                Ok(h) => { tracing::info!(addr = %h.listen_addr, "MQTT broker started"); Some(h) }
                Err(e) => { tracing::error!(%e, "MQTT broker failed"); None }
            }
        } else { None };

        // ADR-XXX Debug: Share broker handle with HTTP debug endpoints so they can
        // trigger graceful shutdown for connection-recovery tests.
        if let Some(h) = mqtt_broker_handle.take() {
            {
                let gw = shared_state.write().await;
                let mut ctrl = gw.mqtt_broker_control.lock().await;
                *ctrl = Some(h);
            }
            tracing::info!("MQTT broker handle registered with debug control");
        }
        // ADR-055 Phase 5a: share the auth state so a debug-triggered
        // broker restart keeps the same credential model.
        {
            let mut gw = shared_state.write().await;
            gw.mqtt_broker_auth = Some(broker_auth);
        }

        // ADR-033: Create runtime HTTP registry and agent registry.
        let runtime_http_registry = crate::http::proxy::new_shared_registry();
        let agent_registry = crate::mqtt::agent_registry::new_shared_registry();
        // ADR-055: node registry — the Gateway's view of Node Agents
        // (LWT-driven online state + retained info snapshots).
        let node_registry = crate::mqtt::node_registry::new_shared_registry();

        // Track whether the broker actually started — used to gate the
        // Gateway-side publisher without needing to re-check the broker
        // handle (which has been moved into the debug control slot above).
        let mqtt_broker_started = {
            let ctrl = shared_state.read().await;
            let guard = ctrl.mqtt_broker_control.lock().await;
            guard.is_some()
        };

        // ADR-055 §6.2: node control client — created after the MQTT
        // client exists; shared between dispatch (NodeEvent correlation)
        // and the HTTP handlers (command issue) via this slot.
        let node_control_slot: std::sync::Arc<
            tokio::sync::Mutex<Option<crate::mqtt::node_control::NodeControlClient>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let mqtt_gw_client: Option<Arc<crate::mqtt::GatewayMqttClient>> = if mqtt_broker_started {
            let reg_for_dispatch = runtime_http_registry.clone();
            let agent_reg_for_dispatch = agent_registry.clone();
            let node_reg_for_dispatch = node_registry.clone();
            // Pass the client into dispatch so the status re-publish
            // path (plain text → protobuf DataEnvelope) has a way to
            // call `publish_envelope`. Note: the client is created
            // AFTER this callback registration, so we capture an
            // `Option<Arc<…>>` that the callback re-borrows each time
            // via the closure below. Easier: re-use the *same* client
            // — we already have `mqtt_gw_client` in scope, but the
            // callback needs to own its clone. Build the callback with
            // a one-shot placeholder, then wire it after `client`
            // exists via the message_callback's set-after-connect
            // pattern. For now, capture the *eventual* client via
            // `Option<Arc<…>>` by deferring construction:
            let dispatch_client: Option<Arc<crate::mqtt::GatewayMqttClient>> = None;
            let dispatch_client_slot: std::sync::Arc<
                tokio::sync::Mutex<Option<Arc<crate::mqtt::GatewayMqttClient>>>,
            > = std::sync::Arc::new(tokio::sync::Mutex::new(dispatch_client));
            let slot_for_cb = dispatch_client_slot.clone();
            let node_control_slot_for_cb = node_control_slot.clone();
            let state_for_dispatch = shared_state.clone();
            // ADR-055 Phase 5a: enroll dispatch needs the credential
            // stores + the auth flag. Cloned BEFORE the move-closure so
            // the originals stay available (local-node token pre-issue
            // below).
            let enrollment_tokens_for_dispatch = enrollment_tokens.clone();
            let node_tokens_for_dispatch = node_tokens.clone();
            let auth_enabled_for_dispatch = mqtt_config.auth_enabled;
            let callback: crate::mqtt::MqttMessageCallback = Arc::new(move |topic, payload| {
                // Plain-text dispatch (http_port, status, ready, …)
                let slot = slot_for_cb.clone();
                let node_control_slot = node_control_slot_for_cb.clone();
                let topic = topic.clone();
                let payload = payload.clone();
                let reg_for_dispatch = reg_for_dispatch.clone();
                let agent_reg_for_dispatch = agent_reg_for_dispatch.clone();
                let node_reg_for_dispatch = node_reg_for_dispatch.clone();
                let state_for_dispatch = state_for_dispatch.clone();
                let enrollment_tokens_for_cb = enrollment_tokens_for_dispatch.clone();
                let node_tokens_for_cb = node_tokens_for_dispatch.clone();
                tokio::spawn(async move {
                    let client = slot.lock().await.clone();
                    let node_control = node_control_slot.lock().await.clone();
                    crate::mqtt::dispatch::handle_message(
                        &topic, &payload,
                        &reg_for_dispatch,
                        &agent_reg_for_dispatch,
                        &node_reg_for_dispatch,
                        client.as_ref(),
                        &state_for_dispatch,
                        node_control.as_ref(),
                        Some(&enrollment_tokens_for_cb),
                        Some(&node_tokens_for_cb),
                        auth_enabled_for_dispatch,
                    );
                });
            });

            // ADR-055 Phase 5a: when MQTT auth is enabled the broker
            // rejects credential-less connections — the publisher
            // presents the internal startup token (client_id as the
            // username; the CONNECT check keys on client_id + password).
            let publisher_result = if mqtt_config.auth_enabled {
                crate::mqtt::GatewayMqttClient::new_publisher_with_callback_and_credentials(
                    &mqtt_config.host,
                    mqtt_config.port,
                    callback,
                    acowork_core::defaults::GATEWAY_MQTT_PUBLISHER_CLIENT_ID,
                    &publisher_token,
                )
                .await
            } else {
                crate::mqtt::GatewayMqttClient::new_publisher_with_callback(
                    &mqtt_config.host,
                    mqtt_config.port,
                    callback,
                )
                .await
            };
            match publisher_result {
                Ok(c) => {
                    tracing::info!("MQTT Gateway client connected (persistent subscriptions handled by ConnAck handler)");
                    let client = Arc::new(c.clone());
                    // Backfill the dispatch slot so the status re-publish
                    // path can call `publish_envelope`. Until now the
                    // slot was `None` and dispatch silently dropped
                    // re-publishes (still updating the in-process
                    // AgentRegistry, which is what the Gateway itself
                    // needs).
                    *dispatch_client_slot.lock().await = Some(client.clone());
                    // Backfill the node control client so dispatch can
                    // correlate NodeEvent results (ADR-055 §6.2).
                    *node_control_slot.lock().await = Some(
                        crate::mqtt::node_control::NodeControlClient::new(client.clone()),
                    );
                    Some(client)
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

        // Keep a handle on the publisher trigger for the dev-mode vault
        // auto-unlock task (below): the publisher emits its initial
        // retained snapshot immediately, before the unlock completes, so
        // every ProviderRef.api_key is empty in that snapshot. The unlock
        // task republishes once keys become readable. (The original is
        // moved into the HTTP server task further down.)
        let unlock_republish_trigger = mqtt_publisher_trigger.clone();

        // ADR-033: Start cron scheduler (uses MQTT for Intent delivery).
        // Must be started AFTER MQTT client is available.
        {
            let cron_mqtt = mqtt_gw_client.clone();
            let cron_gw_state = shared_state.clone();
            let cron_node_control = node_control_slot.lock().await.clone();
            let _cron_handle = tokio::spawn(async move {
                crate::cron::run_cron_scheduler(
                    cron_scheduler,
                    cron_mqtt,
                    cron_gw_state,
                    cron_node_control,
                )
                .await;
            });
        }

        // Start the HTTP API as early as possible: the desktop app's
        // readiness probe (10 s) must see :19876 listening before the
        // node / System Agent startup dance completes. Every dependency
        // (broker, Gateway client, registries, node-control slot) is
        // ready at this point.
        let node_control = node_control_slot.lock().await.clone();
        let http_node_registry = node_registry.clone();
        let http_handle = tokio::spawn(async move {
            if let Err(e) = crate::http::server::start_http_server(
                &http_config,
                http_state,
                &data_dir_path,
                log_reload_handle,
                mqtt_gw_client,
                mqtt_publisher_trigger,
                Some(runtime_http_registry),
                Some(agent_registry),
                node_control,
                Some(http_node_registry),
                http_auth,
            )
            .await
            {
                tracing::error!("HTTP server failed: {}", e);
            }
        });

        // ADR-055 §6.11: ensure a local Node Agent is running and
        // supervise it (orphan cleanup → reuse window → spawn + reaper).
        // Must run AFTER the MQTT broker + Gateway client are up so the
        // retained `acowork/nodes/local/status` reuse window works.
        // ADR-055 Phase 5a: when MQTT auth is enabled, pre-issue the
        // local node's long-lived credential BEFORE the spawn so the
        // child can connect + enroll on first boot (the record carries
        // a placeholder machine_uid, claimed at enroll time).
        let local_node_token = if mqtt_config.auth_enabled {
            Some(
                node_tokens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .upsert(acowork_core::node::LOCAL_NODE_ID, ""),
            )
        } else {
            None
        };
        let local_node_supervisor: Option<std::sync::Arc<crate::gateway::node_manager::LocalNodeSupervisor>> =
            if mqtt_broker_started {
                match crate::gateway::node_manager::ensure_local_node(
                    &mqtt_config.host,
                    mqtt_config.port,
                    &self.config.packages_dir,
                    node_registry.clone(),
                    local_node_token,
                    self.config.node_proxy_port,
                    self.config.node_lsp_relay_port,
                )
                .await
                {
                    Ok(supervisor) => Some(supervisor),
                    Err(e) => {
                        tracing::warn!(error = %e, "Local node agent supervision failed");
                        None
                    }
                }
            } else {
                None
            };

        // ADR-055 §6.2: auto-start the System Agent once the local node is
        // up and its retained installed inventory has been aggregated.
        // Runs in a background task — the inventory wait can take up to
        // 10 s and must not delay the HTTP API / readiness probe. (When
        // the broker is disabled the node-control slot is None, so this
        // is a no-op.)
        {
            let sa_slot = node_control_slot.clone();
            let sa_state = shared_state.clone();
            tokio::spawn(async move {
                // Take the node-control handle WITHOUT holding the slot
                // lock across the wait loop below. A `MutexGuard` born in
                // an `if let` condition lives until the end of the if
                // block — holding it here would stall every dispatch
                // task that locks the same slot for the full 10 s wait
                // (observed as a ~10 s delay in installed-inventory
                // aggregation and System Agent auto-start).
                let nc_opt = sa_slot.lock().await.clone();
                if let Some(nc) = nc_opt {
                    // Bounded wait for the node's retained installed info.
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    while !sa_state.read().await.is_installed(SYSTEM_AGENT_ID)
                        && tokio::time::Instant::now() < deadline
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    if !sa_state.read().await.is_installed(SYSTEM_AGENT_ID) {
                        tracing::warn!("System Agent not installed — skipping auto-start");
                    } else {
                        match nc
                            .start_agent(acowork_core::node::LOCAL_NODE_ID, SYSTEM_AGENT_ID, false)
                            .await
                        {
                            Ok(event) => {
                                if let Err(e) =
                                    crate::mqtt::node_control::NodeControlClient::check_reply(
                                        SYSTEM_AGENT_ID,
                                        &event,
                                    )
                                {
                                    tracing::warn!("Failed to auto-start System Agent: {}", e);
                                } else {
                                    let mut gw = sa_state.write().await;
                                    let workspace = gw
                                        .installed_agents
                                        .get(SYSTEM_AGENT_ID)
                                        .map(|i| {
                                            std::path::PathBuf::from(&i.install_path)
                                                .join("workspace")
                                                .to_string_lossy()
                                                .to_string()
                                        })
                                        .unwrap_or_default();
                                    gw.add_running(crate::gateway::state::RunningAgentInfo {
                                        agent_id: SYSTEM_AGENT_ID.to_string(),
                                        pid: 0,
                                        started_at: chrono::Utc::now(),
                                        workspace,
                                        node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                                        connected: false,
                                        ready: false,
                                        dev_mode: false,
                                        debug_state: crate::gateway::state::DebugState::Disabled,
                                        debug_port: None,
                                        workspace_config_json: None,
                                        current_embed_dim: None,
                                        migration: None,
                                    });
                                    tracing::info!("Auto-started System Agent via local node");
                                }
                            }
                            Err(e) => tracing::warn!("Failed to auto-start System Agent: {}", e),
                        }
                    }
                }
            });
        }

        // In dev_mode, auto-unlock the vault in the background. Unlock runs
        // Argon2id (a deliberately slow KDF, ~1 s) under the GatewayState
        // write lock, so it must start AFTER the HTTP server is up — a
        // boot-time unlock would serialize all startup writes behind it
        // (the broker / node / System Agent path does not touch the vault;
        // only provider-credential HTTP handlers do, and they queue behind
        // the 1 s unlock at most). This is intentionally insecure —
        // dev_mode is for local development only.
        if self.config.dev_mode {
            let vault_state = self.state.clone();
            let unlock_republish = unlock_republish_trigger;
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    vault_state.blocking_write().vault.unlock("dev-mode-unlock")
                })
                .await;
                match result {
                    Ok(Ok(())) => {
                        tracing::info!("Vault auto-unlocked (dev_mode)");
                        // The initial retained acowork/global/providers
                        // snapshot predates this unlock, so every api_key
                        // field was empty (get_provider fails while the
                        // vault is locked) and nothing else triggers a
                        // republish until a provider changes. Republish
                        // now so Runtimes receive the decrypted keys.
                        if let Some(ref trigger) = unlock_republish {
                            trigger.trigger();
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("Failed to auto-unlock vault in dev_mode: {}", e)
                    }
                    Err(e) => tracing::warn!("Vault auto-unlock task panicked: {}", e),
                }
            });
        } else {
            // No background unlock in non-dev mode — nothing to follow up.
            drop(unlock_republish_trigger);
        }

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

                // ADR-055 §6.2: Runtime processes are hosted by the node —
                // the Gateway no longer kills them directly. The local node
                // is shut down below (its supervisor SIGTERMs the node
                // process group); any surviving Runtime is reaped by the
                // next Gateway startup's orphan cleanup.

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

                // ADR-055 §6.11: shut down the local Node Agent.
                if let Some(supervisor) = &local_node_supervisor {
                    supervisor.shutdown().await;
                }

                Ok(())
            }
        };

        shutdown_result?;

        Ok(())
    }

    /// List installed agents.
    ///
    /// Returns a snapshot of installed agents and their current running
    /// state. Marked `async` because `self.state` is a tokio
    /// [`SharedState`] (the daemon-side handlers already use it under
    /// `read().await`).
    pub async fn list_agents(&self) -> Vec<AgentListEntry> {
        let state = self.state.read().await;
        state
            .installed_agents
            .values()
            .map(|info| AgentListEntry {
                agent_id: info.agent_id.clone(),
                name: info.name.clone(),
                version: info.version.clone(),
                running: state.is_running(&info.agent_id),
            })
            .collect()
    }

    /// Package an installed agent into .agent file (CLI command).
    ///
    /// Disabled until Phase 3 — publish build is delegated to the node.
    pub async fn package_agent(
        &self,
        _agent_id: &str,
        _output_dir: Option<&str>,
        _sign: bool,
        _key_dir: Option<&str>,
    ) -> Result<String, GatewayError> {
        Err(GatewayError::Lifecycle(
            "Publish build is not available in the node topology yet (ADR-055 Phase 3)".to_string(),
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
            advertise_host: None,
            node_proxy_port: None,
            node_lsp_relay_port: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_gateway_new() {
        let config = test_config();
        let gateway = Gateway::new(config).unwrap();
        assert!(gateway.list_agents().await.is_empty());
    }

    #[test]
    fn test_ensure_dirs() {
        let config = test_config();
        let gateway = Gateway::new(config).unwrap();
        assert!(gateway.ensure_dirs().is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_list_agents_empty() {
        let config = test_config();
        let gateway = Gateway::new(config).unwrap();
        let list = gateway.list_agents().await;
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
