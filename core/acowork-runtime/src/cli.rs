//! CLI definitions for Agent Runtime
// ADR-040: gRPC-era dead code removed. Only standalone mode and startup
// entry point remain.
use crate::config::RuntimeConfig;
use crate::error::Result;
use clap::Parser;

use std::sync::Arc;

use acowork_core::logging::ChronoLocalTimer;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, reload, util::SubscriberInitExt};

/// Type alias for the reload handle used to dynamically change log level.
pub type LogReloadHandle = reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// Global reference to the SizeRollingFileAppender for runtime log rotation.
/// Set by init_tracing() and read by the LogRotate gRPC handler.
static FILE_APPENDER: std::sync::OnceLock<Arc<acowork_core::logging::SizeRollingFileAppender>> =
    std::sync::OnceLock::new();

/// Agent Runtime CLI
#[derive(Parser)]
#[command(name = "acowork-runtime")]
#[command(about = "Agent Runtime - unified execution engine for .agent packages")]
#[command(version)]
pub struct Cli {
    /// Agent ID (reverse-domain identifier, e.g., com.example.weather)
    #[arg(long, env = "ACOWORK_AGENT_ID")]
    pub agent_id: String,

    /// Path to .agent package (ZIP file or extracted directory)
    #[arg(long, env = "ACOWORK_PACKAGE_PATH")]
    pub package_path: String,

    /// Working directory for the agent
    #[arg(long, env = "ACOWORK_WORK_DIR")]
    pub work_dir: String,

    /// Enable developer mode (debug protocol)
    #[arg(long, default_value = "false")]
    pub dev_mode: bool,

    /// Debug Protocol port hint (used with --dev-mode).
    ///
    /// ADR-048: the legacy WebSocket listener was removed; the port is
    /// no longer bound by Runtime. The flag is kept for API stability
    /// (Gateway still assigns a per-agent port to avoid clashes with
    /// the LSP Relay default range starting at 19878) and historical
    /// compatibility with pre-ADR-048 Desktop configs.
    #[arg(long, default_value = "19878")]
    pub debug_port: u16,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", env = "ACOWORK_LOG_LEVEL")]
    pub log_level: String,

    /// Log file maximum size in MB before auto-split (0 = no split, default 10)
    #[arg(long, default_value = "10", env = "ACOWORK_LOG_FILE_SIZE_MB")]
    pub log_file_size_mb: u64,

    /// Maximum number of log files to keep (0 = unlimited, default 20)
    #[arg(long, default_value = "20", env = "ACOWORK_LOG_FILE_COUNT")]
    pub log_file_count: u64,

    /// Path to manifest.toml (overrides package-embedded manifest)
    #[arg(long)]
    pub manifest_path: Option<String>,

    /// Config directory for the agent
    #[arg(long, env = "ACOWORK_CONFIG_DIR")]
    pub config_dir: Option<String>,

    /// ADR-033: MQTT broker port for Runtime MQTT client.
    /// When set, Runtime connects to the Gateway's embedded MQTT broker.
    #[arg(long, env = "ACOWORK_MQTT_PORT")]
    pub mqtt_port: Option<u16>,

    /// ADR-033: Runtime localhost HTTP server port.
    /// When set, Runtime starts a local HTTP endpoint for Desktop discovery.
    #[arg(long, env = "ACOWORK_HTTP_PORT")]
    pub http_port: Option<u16>,

    /// ADR-055 D3: Gateway MQTT broker host (for remote / distributed
    /// deployments where the Gateway broker is not on 127.0.0.1).
    /// Defaults to 127.0.0.1 (single-machine topology).
    /// Injected by the Node Agent at spawn time (`--gateway-host`), or
    /// set directly in standalone mode.
    #[arg(long, env = "ACOWORK_GATEWAY_HOST")]
    pub gateway_host: Option<String>,

    /// ADR-055 D3/§6.4: base URL of the Node reverse proxy that fronts
    /// this Runtime's loopback HTTP server (e.g.
    /// `http://{node_advertise}:19900`). When set, the Runtime publishes
    /// its retained `http_endpoint` as `{base}/agents/{id}` so the
    /// Gateway reverse-proxies through the Node instead of directly to
    /// the loopback address. Injected by the Node Agent at spawn time
    /// (`--http-advertise-endpoint`); standalone mode omits it and falls
    /// back to the direct loopback endpoint. The Runtime only
    /// concatenates — it never learns the node-internal topology (§6.4).
    #[arg(long, env = "ACOWORK_HTTP_ADVERTISE_ENDPOINT")]
    pub http_advertise_endpoint: Option<String>,
}

impl Cli {
    /// Run the CLI
    pub fn run(self) -> Result<()> {
        // Print version info
        let version = env!("CARGO_PKG_VERSION");
        println!("ACowork Runtime v{version}");

        // Initialize tracing/logging and obtain reload handle
        let reload_handle = self.init_tracing();

        // Install global panic hook AFTER tracing is initialized so panic
        // messages are captured in both stderr and the rolling log file.
        acowork_core::logging::install_panic_hook();

        // Build runtime config from CLI args
        let config = RuntimeConfig::from_cli(&self);
        tracing::info!(

            agent_id = %config.agent_id,
            package_path = %config.package_path,
            work_dir = %config.work_dir,
            "Starting Agent Runtime"

        );

        // Create tokio runtime and run async main
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(crate::error::RuntimeError::Io)?;
        rt.block_on(async_main(config, reload_handle))
    }

    /// Initialize tracing subscriber with both stderr and file output.
    ///
    /// Logs are written to stderr (for Gateway capture) AND to
    /// `{work_dir}/logs/YYYYMMDD_HHMMSS.log` for user inspection.
    ///
    /// Returns a reload handle that allows dynamic log level changes
    /// at runtime (e.g. when Gateway pushes LogLevelUpdate).
    fn init_tracing(&self) -> Option<LogReloadHandle> {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&self.log_level));

        // Ensure the log directory exists under work_dir
        let log_dir = std::path::Path::new(&self.work_dir).join("logs");
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            // Fall back to stderr-only if we cannot create the log directory
            eprintln!(
                "WARN: failed to create log directory {:?}: {}; falling back to stderr-only",
                log_dir, e
            );
            return init_stderr_only(env_filter);
        }

        let max_mb = if self.log_file_size_mb > 0 {
            self.log_file_size_mb
        } else {
            10
        };
        let max_file_count = if self.log_file_count > 0 {
            self.log_file_count as usize
        } else {
            0
        };
        // File appender creation may fail (e.g. macOS sandbox EPERM on
        // $HOME paths, missing parent dir, full disk). Fall back to
        // stderr-only rather than panicking, otherwise the entire runtime
        // dies before any subsystem (HTTP, MQTT) can start.
        let file_appender = match acowork_core::logging::SizeRollingFileAppender::new(
            log_dir.clone(),
            max_mb,
            max_file_count,
        ) {
            Ok(appender) => Arc::new(appender),
            Err(e) => {
                eprintln!(
                    "WARN: failed to open log file in {:?}: {}; falling back to stderr-only",
                    log_dir, e
                );
                return init_stderr_only(env_filter);
            }
        };

        // Store for LogRotate gRPC handler
        let _ = FILE_APPENDER.set(file_appender.clone());
        let (filter, reload_handle) = reload::Layer::new(env_filter);
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_thread_ids(false)
            .with_file(false)
            .with_ansi(cfg!(not(windows))) // Enable ANSI on non-Windows, disable on Windows
            .with_timer(ChronoLocalTimer)
            .compact();
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(file_appender)
            .with_target(true)
            .with_thread_ids(true)
            .with_file(true)
            .with_ansi(false)
            .with_timer(ChronoLocalTimer);
        tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer)
            .with(file_layer)
            .init();
        Some(reload_handle)
    }
}

/// Initialize a stderr-only tracing subscriber with reload support.
///
/// Used as the fallback when the rolling file appender cannot be opened
/// (sandbox EPERM, missing parent dir, full disk). Keeping the reload
/// handle means the gateway can still push dynamic log-level updates
/// even when the file writer is unavailable.
fn init_stderr_only(env_filter: EnvFilter) -> Option<LogReloadHandle> {
    let (filter, reload_handle) = reload::Layer::new(env_filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_thread_ids(false)
                .with_file(false)
                .with_timer(ChronoLocalTimer)
                .compact(),
        )
        .init();
    Some(reload_handle)
}

/// Async entry point after tokio runtime is initialized.
///
/// Acts as the top-level phase orchestrator.  All logic lives in the
/// `startup::` sub-modules; this function merely sequences them.
async fn async_main(
    config: RuntimeConfig,
    log_reload_handle: Option<LogReloadHandle>,
) -> Result<()> {
    use crate::startup::{
        phase_a_init_agent, phase_b_init_session, phase_c_spawn_subsystems, phase_d_run,
    };

    // Phase 0: fail-fast validation of timeout configuration.
    config.validate().map_err(crate::error::RuntimeError::Config)?;

    // Phase A: per-agent initialization (package, gateway, provider, tools, embedding).
    let mut agent_ctx = phase_a_init_agent(&config).await?;

    if agent_ctx.mqtt_client.is_some() {
        // ── Gateway mode ────────────────────────────────────────────────────
        // Phase B: per-session initialization (conversation, AgentCore, SessionManager).
        let mut session_ctx = phase_b_init_session(&mut agent_ctx, &config).await?;

        // Phase C: spawn subsystems (chunk_relay, MCP auto-connect, DevMode).
        let handles =
            phase_c_spawn_subsystems(&mut agent_ctx, &mut session_ctx, &config).await?;

        // Phase D: announce ready + run Gateway message loop.
        phase_d_run(&mut agent_ctx, session_ctx, handles, &config, log_reload_handle).await
    } else {
        // ── Standalone mode ──────────────────────────────────────────────────
        use crate::agent::loop_::AgentLoop;
        tracing::info!("Running in standalone mode");
        let (mut agent_loop, _inbound_tx) = AgentLoop::new(
            config.clone(),
            agent_ctx.loaded.manifest.clone(),
            agent_ctx.provider.clone(),
            agent_ctx.active_tools.clone(),
            agent_ctx.budget.clone(),
            agent_ctx.chunk_tx.clone(),
            None, // no conversation session in standalone cold-start
        );

        agent_loop.core.embedding_provider = agent_ctx.emb_provider.clone();
        agent_loop.core.memory_session = Some(agent_ctx.memory_session.clone());
        // ADR-053: this branch bypasses `phase_b_init_session`, so the
        // agent-specific compaction prompt (prompts/summary.md) must be
        // injected here — mirroring the Phase B injection for Gateway mode.
        // The value was already loaded once in Phase A (see
        // `AgentBootContext::compaction_prompt`), so both modes resolve the
        // same package declaration.
        agent_loop.core.compaction_prompt = agent_ctx.compaction_prompt.clone();
        let work_dir_path = std::path::Path::new(&config.work_dir);
        agent_loop.init_memory_store(work_dir_path);

        // ADR-046: Mirror the Phase B attachment injection for the
        // standalone (CLI) path. Gateway mode runs through
        // `phase_b_init_session` which already wires the same
        // `RuntimeAttachmentService`; this branch only runs when
        // there is no MQTT client, so it must wire the blob store
        // itself. Without this, image-upload items received via the
        // standalone chat loop degrade silently to plain text.
        let attach_svc: Arc<dyn crate::usecases::AttachmentService> = Arc::new(
            crate::usecases::RuntimeAttachmentService::new(work_dir_path.to_path_buf()),
        );
        agent_loop.core.set_attachment_service(attach_svc);

        let _ = &agent_ctx.gateway_current_provider_id; // unused in standalone
        let mut ctx_builder = agent_ctx
            .context_builder
            .take()
            .expect("context_builder must be Some");
        run_chat_loop(&mut agent_loop, &mut ctx_builder).await
    }
}

/// Run interactive stdin chat loop
async fn run_chat_loop(
    agent_loop: &mut crate::agent::loop_::AgentLoop,

    context_builder: &mut crate::agent::context::ContextBuilder,
) -> Result<()> {
    use std::io::{self, BufRead, Write};
    println!("ACowork Agent Runtime — type messages and press Enter (Ctrl+C to exit)");
    println!();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(crate::error::RuntimeError::Io)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed == "/quit" || trimmed == "/exit" {
            println!("Goodbye!");
            return Ok(());
        }

        match agent_loop.run(trimmed, context_builder, None, None, Some(trimmed), None).await {
            Ok(response) => {
                println!(
                    "

--- Agent ---

{response}

"
                );
            }

            Err(e) => {
                tracing::error!(error = %e, "Agent loop error");
                println!(
                    "

--- Error ---

{e}

"
                );
            }
        }

        stdout.flush().ok();
    }

    Ok(())
}

// ADR-035 D9.2 used to own a local `truncate_tool_result_for_display`
// here. ADR-040 moved the canonical implementation to
// `crate::usecases::session_metadata_impl::truncate_tool_result` so
// the HTTP layer (`GET /sessions/{sid}/messages`) can route through
// `SessionMetadataService::get_messages` without depending on `cli`.
//
// No CLI path currently calls this; if a future CLI preview path
// (e.g. `acowork-cli cat conversations/<sid>.jsonl`) needs the same
// truncation rule, it should import the canonical function from the
// UseCase impl rather than re-introducing a duplicate here.

/// User-facing override for skill mode, persisted in `.agent_skills.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct AgentSkillsOverride {
    /// Whether to use progressive skill injection mode.
    #[serde(default)]
    progressive: Option<bool>,
}

/// Priority: `{work_dir}/.agent_skills.json` > manifest `[skills]` default.
pub(crate) fn resolve_skill_mode(
    manifest: &acowork_core::AgentManifest,

    work_dir: &str,
) -> acowork_core::SkillMode {
    let default_progressive = manifest.skills.progressive;

    // Check for user override in workspace
    let override_path = std::path::Path::new(work_dir).join(".agent_skills.json");
    if override_path.exists() {
        match std::fs::read_to_string(&override_path) {
            Ok(content) => match serde_json::from_str::<AgentSkillsOverride>(&content) {
                Ok(override_config) => {
                    if let Some(progressive) = override_config.progressive {
                        tracing::info!(
                            progressive = %progressive,
                            manifest_default = %default_progressive,
                            "Skill mode overridden by .agent_skills.json"
                        );
                        return if progressive {
                            acowork_core::SkillMode::Progressive
                        } else {
                            acowork_core::SkillMode::Manual
                        };
                    }
                }

                Err(e) => {
                    tracing::warn!(
                        path = %override_path.display(),
                        error = %e,
                        "Failed to parse .agent_skills.json, using manifest default"
                    );
                }
            },

            Err(e) => {
                tracing::warn!(
                    path = %override_path.display(),
                    error = %e,
                    "Failed to read .agent_skills.json, using manifest default"
                );
            }
        }
    }

    manifest.skill_mode()
}
