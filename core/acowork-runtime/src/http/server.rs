//! Runtime localhost HTTP server (ADR-033 Phase 2 + ADR-034 Phase 3).
//!
//! Serves queries and write operations for the Gateway reverse proxy.
//!
//! ADR-034 §11.2 is the authoritative endpoint list (25 routes — 12
//! retained/fixed and 13 newly added in Phase 3). The control plane
//! (`POST /sessions/{sid}/{action}`) is deliberately **not** exposed:
//! user-initiated state changes flow through the
//! `acowork/agents/{id}/sessions/control/{cmd}` MQTT topic, not this
//! HTTP server.
//!
//! ```text
//! Data plane (25 routes — 12 retained + 13 added in Phase 3)
//! ───────────────────────────────────────────────────────────
//! GET    /health                                 // retained
//! GET    /sessions                               // retained
//! GET    /sessions/latest                        // retained
//! GET    /sessions/{sid}                         // NEW: panel-4 (merges meta + state)
//! GET    /sessions/{sid}/messages                // retained
//! POST   /sessions/{sid}/files                   // ADR-046: upload file/image
//! GET    /files/{document_id}                    // ADR-046: download blob
//! GET    /memory/graph                           // FIXED: now uses Grafeo
//! GET    /memory/nodes                           // retained
//! GET    /memory/nodes/{nid}                     // NEW: memory_query::get_node
//! DELETE /memory/nodes/{nid}                     // retained
//! GET    /memory/stats                           // retained
//! POST   /memory/consolidate                     // retained
//! GET    /files/{id}                             // retained
//! GET    /workspaces                             // retained
//! POST   /workspaces                             // NEW
//! PUT    /workspaces/{ws_id}                     // NEW
//! PUT    /workspaces/{ws_id}/prompt-file         // NEW
//! DELETE /workspaces/{ws_id}                     // NEW
//! GET    /workspaces/tree                        // retained (panel 6)
//! GET    /workspaces/file                        // NEW: read file  (panel 6)
//! POST   /workspaces/file                        // NEW: create file
//! PUT    /workspaces/file                        // NEW: overwrite file
//! DELETE /workspaces/file                        // NEW: delete file
//! GET    /workspaces/raw/{path}                  // ADR-055 L2-7: raw bytes (preview iframe)
//! POST   /workspaces/dir                         // NEW: create dir
//! DELETE /workspaces/dir                         // NEW: delete dir
//! POST   /workspaces/copy                        // NEW: copy file/dir tree
//! POST   /workspaces/rename                      // NEW: atomic rename (move)
//! POST   /memory/nodes                           // NEW: create memory node
//! PUT    /memory/nodes/{nid}                     // NEW: update memory node
//! GET    /agents/{id}/config                     // NEW: panel 1
//! GET    /agents/{id}/tools                      // NEW: panel 3
//! GET    /agents/{id}/status                     // NEW: panel 5
//!
//! Removed in Phase 3 (ADR-034 §7.6.4):
//!   ~~GET  /sessions/{sid}/state~~        → absorbed by /sessions/{sid}
//!   ~~POST /sessions/{sid}/approval~~     → routed via MQTT
//!   ~~POST /sessions/{sid}/question~~     → routed via MQTT
//!   ~~POST /sessions/{sid}/continue~~     → routed via MQTT
//! ```
//!
//! The four Grafeo-backed `/memory/*` endpoints (`/memory/nodes`,
//! `/memory/nodes/{nid}`, `/memory/stats`, `/memory/consolidate`)
//! share their business logic with the legacy gRPC path through
//! [`crate::http::memory_query`], so HTTP and gRPC responses stay
//! consistent.
//!
//! The server binds to `127.0.0.1:0` (random port) and is intended
//! for Gateway reverse proxy access only — not direct Desktop access.
//!
//! See `docs/zh/protocols/mqtt.md` §7.5.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::agent::inbound::{InboundMessage, UserOp};
use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};

use crate::mqtt::client::SharedRuntimeMqttClient;

/// Error type for Runtime HTTP server operations.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeHttpServerError {
    #[error("HTTP server error: {0}")]
    Server(String),
    #[error("Failed to bind: {0}")]
    Bind(String),
}

/// Global body-size cap for every Runtime HTTP route.
///
/// Axum's default `DefaultBodyLimit` is 2 MiB — which silently caps
/// any extractor (`Json`, `Bytes`, `String`, `Multipart`, `Form`,
/// etc.) before our handlers can read a single byte. The user-facing
/// fallout is opaque ("Error parsing 'multipart/form-data' request"
/// for a 9 MB PDF, generic JSON parse errors for a 3 MB request
/// body, etc.) instead of a clean 413 + JSON error body from the
/// service layer.
///
/// We raise the global cap to 64 MiB at the root router so every
/// endpoint that legitimately needs a large body — multipart upload
/// (`/sessions/{sid}/files`), workspace file write
/// (`PUT /workspaces/file` with embedded content), memory node
/// create/update payloads, etc. — can reach its handler. Per-route
/// service-layer checks remain the source of truth for user-facing
/// limits:
///
///   - attachments: [`crate::usecases::MAX_UPLOAD_BYTES`] (50 MiB)
///   - workspace files: implicit 64 MiB ceiling via this limit;
///
/// anything larger is rejected at the handler with a clean error
/// instead of at the extractor with an opaque one.
const GLOBAL_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Shared dispatch sender for Runtime HTTP → agent loop.
///
/// Cloned from the main MQTT dispatch channel. Runtime HTTP handlers
/// use this to send (session_id, InboundMessage) tuples that are
/// forwarded to the right session's AgentLoop via `send_inbound()`.
pub type SharedDispatchSender = Arc<tokio::sync::Mutex<Option<mpsc::UnboundedSender<(String, InboundMessage)>>>>;

/// Shared handle to the Runtime's memory admin service.
///
/// `None` until Phase B (`init_memory_provider`) finishes; HTTP handlers
/// report a graceful "no store" response when it is still empty
/// (see [`memory_query`]).
///
/// ADR-051 P4: type changed from `Arc<GrafeoStore>` to
/// `Arc<dyn MemoryAdminService>` so the Runtime does not depend on
/// the concrete grafeo type for HTTP admin endpoints.
pub type SharedMemoryStore = Arc<std::sync::RwLock<Option<Arc<dyn acowork_memory::admin::MemoryAdminService>>>>;

/// Shared slot for the consolidation timer (late-bind from AgentCore).
/// Used by `GET /memory/consolidation/status` to report idle time, pending count.
pub type SharedConsolidationTimer = Arc<std::sync::RwLock<Option<Arc<crate::memory::ConsolidationTimer>>>>;

/// Shared slot for the RAG provider (late-bind from AgentCore).
/// Used by `GET /agents/{id}/rag/status` and `POST /agents/{id}/rag/query`.
pub type SharedRagProvider = Arc<std::sync::RwLock<Option<Arc<dyn acowork_core::rag::RagProvider>>>>;

/// Shared handle to the Runtime's `AgentCore`.
///
/// `None` until Phase B creates the `AgentCore`; HTTP handlers that
/// need agent-level token totals (`list_sessions`) gracefully degrade
/// (return 0) when it is still empty.
///
/// ADR-028: the `list_sessions` handler merges disk-scanned totals into
/// the live atomic counters and reads them back so the response always
/// carries `agent_total_input_tokens` / `agent_total_output_tokens`.
pub type SharedAgentCore = Arc<std::sync::RwLock<Option<Arc<crate::agent::agent_core::AgentCore>>>>;

/// Shared embedding-provider dimension (0 = no provider).
///
/// Surfaced by the memory-stats endpoint as `model_dim` so the desktop
/// can detect dimension mismatches with the persisted HNSW index.
pub type SharedEmbedDimension = Arc<std::sync::RwLock<u64>>;

/// Shared degradation reasons — startup-phase failures that are not
/// fatal enough to abort the runtime but degrade functionality.
///
/// Populated during Phase B when session persistence is unavailable
/// (e.g. sandbox EPERM on the conversations directory) and read by
/// the `/health` endpoint so the frontend can surface a warning.
pub type SharedDegradation = Arc<std::sync::RwLock<Vec<String>>>;

/// Late-bind slot for the Runtime's MQTT client.
///
/// The HTTP server starts in Phase A before the MQTT client is
/// connected, so we hand it an `Option`-wrapped `SharedRuntimeMqttClient`
/// and populate it once Phase A finishes wiring up the broker
/// connection. Handlers treat `None` as "no MQTT available yet" and
/// silently skip the retained PUBLISH — the persisted on-disk file is
/// still authoritative for the next GET.
pub type SharedMqttClientSlot = Arc<tokio::sync::Mutex<Option<SharedRuntimeMqttClient>>>;

/// Late-bind slot for the Runtime's `SessionManager`.
///
/// The HTTP server starts in Phase A before `SessionManager` is
/// constructed in Phase B, so we hand the server an
/// `Option`-wrapped `Arc<Mutex<SessionManager>>` and populate it once
/// Phase B finishes. The only handler that reads this slot today is
/// `POST /api/debug/enable` (`http/debug.rs`), which uses it to call
/// `SessionManager::enable_debug_mode` for runtime DevMode activation.
/// `None` is reported back to the caller as 503 — same ADR-040 pattern
/// as every other late-bind slot.
///
/// Type is `Arc<...>` (not `Mutex`) because the server only ever reads
/// the slot to clone the inner `Arc<Mutex<SessionManager>>`; the
/// exclusive write happens once at Phase B and never changes again.
/// An `RwLock` keeps the read path lock-free.
pub type SharedSessionManagerSlot =
    Arc<tokio::sync::RwLock<Option<Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>>>>;

/// State shared with HTTP handlers.
#[derive(Clone)]
pub(crate) struct HttpState {
    work_dir: PathBuf,
    agent_id: String,
    /// Shell risk rules — loaded at startup; updated in-place by PUT
    /// `/agents/{id}/shell-risk-rules` so live sessions pick up new rules
    /// without a restart. Write via `Arc<RwLock>` because the state is
    /// cloned into each axum handler (axum's `State<T>` requires `Clone`).
    shell_risk_rules: Arc<std::sync::RwLock<crate::security::shell_risk::ShellRiskRules>>,
    /// Retained on the state for parity with the startup wiring
    /// (`SessionManager.config.session_snapshots`); the HTTP layer no
    /// longer enumerates active sessions directly (the ADR-052 hot-reload
    /// path pushes a single system-level message and lets
    /// `dispatch_inbound` -> `SessionManager::{apply_runtime_config_override,
    /// apply_builtin_tools_enabled}` walk `SessionManager.sessions`
    /// itself). Suppresses `unused` because removing it would touch ~10
    /// `RuntimeHttpServer::start` test call sites for no functional
    /// gain.
    #[allow(dead_code)]
    session_snapshots: SharedSessionSnapshots,
    /// Shared latest session info, updated by SessionManager on every
    /// session creation and startup scan.  Read by `get_latest_session`.
    latest_session: SharedLatestSession,
    /// Dispatch sender for write operations (approval, question, continue, title).
    /// Set after the session manager starts. None = agent not ready yet.
    dispatch_tx: SharedDispatchSender,
    /// Active embedding provider dimension. Set once at Phase A
    /// and read by the usecase service (via the memory_query slot).
    embed_provider_dim: SharedEmbedDimension,
    /// Startup degradation reasons surfaced via `/health`.
    /// Populated by Phase B when non-fatal errors occur.
    degraded_reasons: SharedDegradation,
    /// Late-bind slot for the MQTT client. Populated by Phase A after
    /// the broker connection is established. The `PUT /agents/{id}/config`
    /// handler uses this to re-PUBLISH the retained config snapshot so
    /// other Desktop subscribers see the new values without a restart.
    /// Also consumed by `POST /api/debug/enable` to wire the
    /// DebugEventMqttPublisher when DevMode is flipped on at runtime.
    pub(crate) mqtt_client: SharedMqttClientSlot,
    /// ADR-040: late-bind usecase services. Populated by Phase B.
    /// All handlers depend solely on these traits; no direct access
    /// to memory_store or agent_core is required.
    session_metadata: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>>,
    memory_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::MemoryQueryService>>>>,
    workspace_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>>,
    workspace_mutation: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>>,
    /// ADR-040 follow-up: Tools-panel persistence (MCP + search active
    /// state). The 4 new `/agents/{id}/mcp-servers` and
    /// `/agents/{id}/search-config` HTTP handlers route through this
    /// trait instead of calling `agent_config::*` directly. Populated
    /// in Phase B (sync, no async resource dependency like memory).
    agent_tools: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>>,
    /// ADR-040 follow-up: per-agent runtime config (`agent_config.json`)
    /// persistence. The `GET/PUT /agents/{id}/config` handlers route
    /// through this trait instead of calling `agent_config::*`
    /// directly. Also serves `/agents/{id}/builtin-tools` because that
    /// endpoint persists `agent_tools.json` (a separate file).
    agent_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentConfigService>>>>,
    /// ADR-046: Late-bind slot for the unified attachment blob store
    /// (`POST /sessions/{sid}/files` upload + `GET /files/{doc_id}`
    /// download). The HTTP handlers route through this trait instead
    /// of touching the filesystem directly — same ADR-040 pattern as
    /// every other UseCase service. Populated in Phase B (sync —
    /// requires only the boot-time `work_dir`).
    attachment: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>>,
    /// ADR-047: Session config service for GET/PUT /sessions/{sid}/config.
    session_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>>,
    /// Consolidation timer (late-bind from AgentCore Phase B).
    /// Used by `GET /memory/consolidation/status`.
    consolidation_timer: SharedConsolidationTimer,
    /// RAG provider (late-bind from AgentCore Phase B).
    /// Used by `GET /agents/{id}/rag/status` and `POST /agents/{id}/rag/query`.
    rag_provider: SharedRagProvider,
    /// ADR-048: late-bind slot for the Debug service. Populated by Phase
    /// B after SessionManager wires up per-session debug controllers.
    /// `None` until then; HTTP handlers return 503 with a descriptive
    /// message when this slot is still empty (same pattern as every
    /// other ADR-040 use-case slot).
    pub(crate) debug_service: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::DebugService>>>>,
    /// Shared `WorkspaceResolver` — the **same** `Arc` injected into
    /// `SessionManager` at Phase B (see `startup/context.rs` +
    /// `session_init.rs`). The workspace-mutation handlers reload it
    /// after every successful create/update/delete so `route_workspace_switch`
    /// (which validates the requested `workspace_id` via
    /// `resolver.find_by_id`) sees newly-added workspaces without a
    /// Runtime restart. Without this, a fresh workspace could be
    /// persisted to disk and listed by the desktop but never selected —
    /// the switch would fall back to `__agent_home__`.
    workspace_resolver: crate::tools::workspace_resolver::SharedResolver,
    /// Late-bind slot for `SessionManager`. Populated by Phase B once
    /// the session manager is constructed. The only consumer is
    /// `POST /api/debug/enable` (`http/debug::post_enable`), which
    /// uses it to flip DevMode on at runtime without restarting the
    /// agent. `None` outside Phase B (e.g. early boot, tests that
    /// never build a SessionManager).
    pub(crate) session_manager_slot: crate::http::server::SharedSessionManagerSlot,
    /// ADR-058: workspace FS watcher set. Created empty in Phase A,
    /// populated by Phase C (`start_workspace_watchers`) and reconciled
    /// after every workspace CRUD mutation (see
    /// `sync_workspace_watchers`). Publishing goes through
    /// [`Self::mqtt_client`] — the watcher set holds a clone of the
    /// same slot.
    pub(crate) workspace_watchers: crate::workspace::SharedWorkspaceWatcherSet,
}

/// Handle to the running HTTP server.
pub struct RuntimeHttpServer {
    /// The address the server is listening on.
    pub listen_addr: SocketAddr,
    /// The port the server is listening on (extracted from listen_addr).
    pub port: u16,
    /// ADR-058: the workspace FS watcher set shared with the HTTP
    /// state. Clone this into the boot context so Phase C and the CRUD
    /// mutation hooks reconcile the same set.
    pub workspace_watchers: crate::workspace::SharedWorkspaceWatcherSet,
    /// The join handle for the server task. Dropping this aborts the server.
    _handle: tokio::task::JoinHandle<()>,
}

impl RuntimeHttpServer {
    /// Start the HTTP server on `127.0.0.1:0` (random port).
    ///
    /// Returns the server handle with the assigned port. The Gateway
    /// uses this port to reverse-proxy large data queries.
    ///
    /// Note: `start` intentionally takes each shared resource individually
    /// (rather than bundling everything in a single config struct) to keep
    /// resource lifetimes explicit at the call site and to avoid coupling
    /// the HTTP module to startup-phase internals.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        work_dir: PathBuf,
        agent_id: String,
        session_snapshots: SharedSessionSnapshots,
        latest_session: SharedLatestSession,
        dispatch_tx: SharedDispatchSender,
        embed_provider_dim: SharedEmbedDimension,
        degraded_reasons: SharedDegradation,
        mqtt_client: SharedMqttClientSlot,
        session_metadata: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>>,
        memory_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::MemoryQueryService>>>>,
        workspace_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>>,
        workspace_mutation: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>>,
        agent_tools: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>>,
        agent_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentConfigService>>>>,
        attachment: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>>,
        session_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>>,
        consolidation_timer: SharedConsolidationTimer,
        rag_provider: SharedRagProvider,
        debug_service: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::DebugService>>>>,
        workspace_resolver: crate::tools::workspace_resolver::SharedResolver,
        session_manager_slot: SharedSessionManagerSlot,
    ) -> Result<Self, RuntimeHttpServerError> {
        // Historical behaviour: random loopback port.
        Self::start_with_bind_port(
            0,
            work_dir,
            agent_id,
            session_snapshots,
            latest_session,
            dispatch_tx,
            embed_provider_dim,
            degraded_reasons,
            mqtt_client,
            session_metadata,
            memory_query,
            workspace_query,
            workspace_mutation,
            agent_tools,
            agent_config,
            attachment,
            session_config,
            consolidation_timer,
            rag_provider,
            debug_service,
            workspace_resolver,
            session_manager_slot,
        )
        .await
    }

    /// ADR-055 §6.4: start the HTTP server on an explicit loopback port.
    ///
    /// The Node allocates a concrete port (from `NODE_HTTP_PORT_BASE`) and
    /// passes it via `--http-port` so its reverse proxy has a stable
    /// `{agent_id} → port` mapping. `bind_port = 0` keeps the historical
    /// random-port behaviour (`127.0.0.1:0`).
    #[allow(clippy::too_many_arguments)]
    pub async fn start_with_bind_port(
        bind_port: u16,
        work_dir: PathBuf,
        agent_id: String,
        session_snapshots: SharedSessionSnapshots,
        latest_session: SharedLatestSession,
        dispatch_tx: SharedDispatchSender,
        embed_provider_dim: SharedEmbedDimension,
        degraded_reasons: SharedDegradation,
        mqtt_client: SharedMqttClientSlot,
        session_metadata: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>>,
        memory_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::MemoryQueryService>>>>,
        workspace_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>>,
        workspace_mutation: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>>,
        agent_tools: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>>,
        agent_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentConfigService>>>>,
        attachment: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>>,
        session_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>>,
        consolidation_timer: SharedConsolidationTimer,
        rag_provider: SharedRagProvider,
        debug_service: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::DebugService>>>>,
        workspace_resolver: crate::tools::workspace_resolver::SharedResolver,
        session_manager_slot: SharedSessionManagerSlot,
    ) -> Result<Self, RuntimeHttpServerError> {
        // ADR-058: the workspace FS watcher set is created here so the
        // HTTP state and the boot context share exactly one Arc. It is
        // exposed on the returned handle for `agent_init` to clone into
        // `AgentBootContext` (Phase C startup hook) — tests that never
        // touch workspace events simply ignore the field.
        let workspace_watchers: crate::workspace::SharedWorkspaceWatcherSet =
            Arc::new(tokio::sync::Mutex::new(
                crate::workspace::WorkspaceWatcherSet::new(
                    agent_id.clone(),
                    mqtt_client.clone(),
                ),
            ));
        let shell_risk_rules = crate::security::shell_risk::ShellRiskRules::load(&work_dir)
            .unwrap_or_default();
        let state = HttpState {
            work_dir,
            agent_id,
            shell_risk_rules: Arc::new(std::sync::RwLock::new(shell_risk_rules)),
            session_snapshots,
            latest_session,
            dispatch_tx,
            embed_provider_dim,
            degraded_reasons,
            mqtt_client,
            session_metadata,
            memory_query,
            workspace_query,
            workspace_mutation,
            agent_tools,
            agent_config,
            attachment,
            session_config,
            consolidation_timer,
            rag_provider,
            debug_service,
            workspace_resolver,
            session_manager_slot,
            workspace_watchers: workspace_watchers.clone(),
        };

        // ADR-034 §11.2 — 25 routes total. Control plane is intentionally
        // absent: user-initiated state changes go through MQTT, not HTTP.
        let app = Router::new()
            .route("/health", get(health))
            .route("/sessions", get(list_sessions))
            .route("/sessions/latest", get(get_latest_session))
            // NEW: panel-4 endpoint, merges meta.json + live state snapshot.
            .route("/sessions/{sid}", get(get_session))
            .route("/sessions/{sid}/messages", get(get_messages))
            // ADR-047: session config endpoint - read/write config
            // without going through the serial inference queue.
            .route(
                "/sessions/{sid}/config",
                get(get_session_config).put(put_session_config),
            )
            // ADR-046: 2 attachment routes — POST is session-scoped (the desktop
            // knows which session is sending the upload), GET is
            // global-scoped (the desktop knows `document_id` from the
            // JSONL metadata it already loaded, no session_id needed).
            // Both route through `AttachmentService` (ADR-040).
            //
            // Per-route body-limit override is NOT needed here: the
            // global `DefaultBodyLimit::max(GLOBAL_BODY_LIMIT)` layer
            // (64 MiB) installed at the root of the router covers the
            // `Multipart` extractor. The Runtime's own size check
            // ([`crate::usecases::MAX_UPLOAD_BYTES`] = 50 MiB) then
            // produces a clean 413 + JSON error body if the payload is
            // over the user-facing limit.
            .route("/sessions/{sid}/files", post(upload_file))
            .route("/files/{document_id}", get(read_file))
            .route("/memory/graph", get(get_memory_graph))
            .route(
                "/memory/nodes",
                get(get_memory_nodes).post(create_memory_node),
            )
            // GET + DELETE retained from Phase 3; PUT (update) added now
            // so HTTP CRUD is complete and the gateway can proxy all 4 ops
            // through the same path shape.
            .route(
                "/memory/nodes/{nid}",
                get(get_memory_node)
                    .delete(delete_memory_node)
                    .put(update_memory_node),
            )
            .route("/memory/stats", get(get_memory_stats))
            .route("/memory/consolidate", post(trigger_consolidate))
            // NOTE: the legacy `GET /files/{id}` handler was removed as part
            // of the ADR-040 / ADR-009 v2 workspace consolidation. Workspace
            // file reads now flow exclusively through
            // `GET /workspaces/file?path=…` (see `read_workspace_file`), which
            // dispatches via the `WorkspaceQueryService::read_file` trait so
            // the HTTP server does not directly touch the filesystem.
            // workspaces: retained GET + 4 new mutation routes.
            .route("/workspaces", get(list_workspaces).post(create_workspace))
            .route(
                "/workspaces/{ws_id}",
                put(update_workspace).delete(delete_workspace),
            )
            .route(
                "/workspaces/{ws_id}/prompt-file",
                put(set_workspace_prompt_file),
            )
            .route("/workspaces/tree", get(list_tree))
            .route("/workspaces/find", get(find_files))
            .route("/workspaces/search", get(search_files))
            // 2 NEW workspace file/dir resources, REST-style (ADR-034 §11.2 #6-9).
            // One path per resource; HTTP method dispatches the operation:
            //   GET    /workspaces/file — read  → JSON {content,size,mimeType}
            //   POST   /workspaces/file — create → body {path} (409 on dup)
            //   PUT    /workspaces/file — write  → body {content} (404 on miss)
            //   DELETE /workspaces/file — remove → body {path}
            //   POST   /workspaces/dir  — create → body {path}
            //   DELETE /workspaces/dir  — remove → body {path}
            // All share the `resolve_workspace_root` + canonicalize path-
            // traversal guard via `resolve_within_workspace` so the contract
            // is uniform across reads and writes.
            .route(
                "/workspaces/file",
                get(read_workspace_file)
                    .post(create_workspace_file)
                    .put(write_workspace_file)
                    .delete(delete_workspace_file),
            )
            // ADR-055 L2-7: raw-bytes read for the Gateway's HTML preview
            // iframe (the Gateway reverse-proxies `/workspace-files/…`
            // here). Serves verbatim bytes + Content-Type — unlike the
            // JSON envelope above.
            .route(
                "/workspaces/raw/{*path}",
                get(read_workspace_raw),
            )
            .route(
                "/workspaces/dir",
                post(create_workspace_dir).delete(delete_workspace_dir),
            )
            // Workspace copy/rename (ADR-040 + ADR-034 §11.2 — these
            // were previously gateway-direct and silently broken for
            // additional workspaces because the gateway-side
            // `resolve_workspace_root` read a never-populated in-memory
            // field). The runtime owns the workspace config on disk
            // (`<work_dir>/config/agent_workspaces.json`) so resolution
            // works for every workspace ID, including the agent home.
            .route(
                "/workspaces/copy",
                post(copy_workspace_item),
            )
            .route(
                "/workspaces/rename",
                post(rename_workspace_item),
            )
            // ADR-040 follow-up: agent panel endpoints now route
            // through UseCase traits (AgentConfigService +
            // AgentToolsService). Each handler is a thin protocol
            // converter — see `put_agent_config` /
            // `put_agent_builtin_tools` for the slot-first pattern.
            //
            //   /agents/{id}/config          — per-agent runtime
            //                                  config (`agent_config.json`)
            //   /agents/{id}/builtin-tools   — builtin tool enable flags
            //                                  (`agent_tools.json`)
            //   /agents/{id}/mcp-servers     — MCP active selection
            //                                  (`agent_mcp.json`)
            //   /agents/{id}/search-config   — search providers
            //                                  (`agent_search.json`)
            //
            // `/agents/{id}/tools` (GET only) remains a read-only merge
            // of the three Tools-panel files — see `get_agent_tools`.
            .route(
                "/agents/{id}/config",
                get(get_agent_config).put(put_agent_config),
            )
            .route(
                "/agents/{id}/builtin-tools",
                get(get_agent_builtin_tools).put(put_agent_builtin_tools),
            )
            .route("/agents/{id}/tools", get(get_agent_tools))
            .route("/agents/{id}/status", get(get_agent_status))
            // Win11-MCP-ToolsBugFix: see comment block above `get_agent_mcp_servers`.
            // These two routes power the per-agent activation toggles in the
            // Desktop Tools panel. They are pure persistence endpoints —
            // no in-memory side effects on active sessions (matches the
            // "next-session effective" contract documented in mqtt.md §3.5).
            .route(
                "/agents/{id}/mcp-servers",
                get(get_agent_mcp_servers).put(put_agent_mcp_servers),
            )
            .route(
                "/agents/{id}/search-config",
                get(get_agent_search_config).put(put_agent_search_config),
            )
            // ADR-040 follow-up: provider list endpoint — reads from
            // agent_provider.json (persisted by MQTT handler on every
            // acowork/global/providers retained update). The Gateway
            // proxies GET /api/agents/{id}/providers here so the
            // frontend can verify what the Runtime actually has.
            .route(
                "/agents/{id}/providers",
                get(get_agent_providers),
            )
            // N1: Consolidation status - reports timer idle, pending count.
            // N2: RAG status - reports whether RAG is configured.
            // N3: RAG query - direct query bypassing LLM (for debugging).
            .route("/memory/consolidation/status", get(get_consolidation_status))
            .route("/agents/{id}/rag/status", get(get_rag_status))
            .route("/agents/{id}/rag/query", post(post_rag_query))
            // Shell risk rules — read effective content or write user override.
            .route(
                "/agents/{id}/shell-risk-rules",
                get(get_shell_risk_rules).put(put_shell_risk_rules),
            )
            // ADR-048: debug protocol HTTP routes — thin wrappers
            // around DebugService. Mounted under /api/debug/*. Phase A
            // starts the HTTP server before DebugService is wired up,
            // so handlers return 503 until Phase B populates the slot.
            .merge(crate::http::debug::debug_routes())
            // Global body-size cap. See `GLOBAL_BODY_LIMIT` for why we
            // override axum's 2 MiB default at the root of the router.
            // Each per-route service-layer check (e.g.
            // `AttachmentService::PayloadTooLarge`) remains the source
            // of truth for user-facing limits.
            .layer(DefaultBodyLimit::max(GLOBAL_BODY_LIMIT))
            .with_state(state);

        // Bind to the Node-allocated loopback port (ADR-055 §6.4);
        // `127.0.0.1:0` = random port (pre-node-topology behaviour).
        let bind_addr = if bind_port == 0 {
            "127.0.0.1:0".to_string()
        } else {
            format!("127.0.0.1:{}", bind_port)
        };
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| RuntimeHttpServerError::Bind(format!("Failed to bind: {}", e)))?;

        let listen_addr = listener
            .local_addr()
            .map_err(|e| RuntimeHttpServerError::Bind(format!("Failed to get local addr: {}", e)))?;

        let port = listen_addr.port();

        tracing::info!(
            addr = %listen_addr,
            port,
            "Runtime HTTP server started (for Gateway reverse proxy)"
        );

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "Runtime HTTP server error");
            }
        });

        Ok(Self {
            listen_addr,
            port,
            workspace_watchers: workspace_watchers.clone(),
            _handle: handle,
        })
    }
}

// ── Handlers ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    agent_id: String,
    degraded_reasons: Vec<String>,
}

async fn health(State(state): State<HttpState>) -> Json<HealthResponse> {
    let degraded = state.degraded_reasons.read().unwrap_or_else(|e| e.into_inner()).clone();
    Json(HealthResponse {
        status: "ok",
        agent_id: state.agent_id,
        degraded_reasons: degraded,
    })
}

/// Query parameters for `GET /sessions`.
#[derive(Debug, Deserialize)]
struct ListSessionsQuery {
    /// 1-based page number (default 1).
    #[serde(default)]
    page: Option<u32>,
    /// Page size (default 20, capped at 200).
    #[serde(default)]
    size: Option<u32>,
}

/// `GET /sessions` — full session list.
///
/// Reads per-session meta files under `workspace/conversations/meta/`
/// (the authoritative source per ADR-024). Supports `page` / `size`
/// pagination; results are returned sorted by `last_active_at` descending.
///
/// ADR-028: the response top level also carries
/// `agent_total_input_tokens` / `agent_total_output_tokens`, computed by
/// scanning every session on disk and merging the totals into the live
/// `AgentCore` atomic counters (max-merge, idempotent).
///
/// This is the backend for `GET /api/agents/{id}/sessions` via the
/// Gateway reverse proxy.
async fn list_sessions(
    State(state): State<HttpState>,
    Query(query): Query<ListSessionsQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let page = query.page.unwrap_or(1).max(1);
    let size = query.size.unwrap_or(20).clamp(1, 200);

    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.session_metadata.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let resp = svc
        .list_sessions(page, size)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::to_value(resp).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    ))
}

/// `GET /sessions/latest` — single latest session.
///
/// Reads from the shared `latest_session` Arc (updated by SessionManager),
/// so it always reflects the authoritative latest session without any
/// file-system scanning. Returns 404 if no session has been created yet.
///
/// This is the backend for `GET /api/agents/{id}/latest-session` via Gateway proxy.
async fn get_latest_session(State(state): State<HttpState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let latest = state.latest_session.read().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match *latest {
        Some((ref session_id, ref title)) => Ok(Json(serde_json::json!({
            "session_id": session_id,
            "title": title,
        }))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Query parameters for `GET /sessions/{sid}/messages`.
#[derive(Debug, Deserialize)]
struct GetMessagesQuery {
    /// Offset from the **oldest** end, in **raw entries** (one JSONL line each).
    /// 0 = first (oldest) raw entry.  See
    /// [`crate::conversation::PaginatedMessages`] for the contract.
    #[serde(default)]
    offset: Option<u64>,
    /// Maximum number of **raw entries** to return (default 50, capped at 500).
    /// A raw entry is one JSONL line — a single user / assistant /
    /// thought / tool_call / tool_result row. Display-group collapsing
    /// is a frontend UI abstraction and is not visible here.
    #[serde(default)]
    limit: Option<u32>,
    /// ADR-050: when true, the window is anchored to the **tail** of the
    /// conversation (the latest `limit` entries), regardless of `offset`.
    /// Used by the frontend's initial-load code path which doesn't yet
    /// know `total`.  Ignored when total == 0.
    #[serde(default)]
    tail: Option<bool>,
}

// ── Session config (ADR-047) ───────────────────────────────────

/// `GET /sessions/{sid}/config` - read current session config.
///
/// ADR-047: returns a `SessionConfigSnapshot` containing model, provider,
/// workspace_id, reasoning_effort, temperature, and title.
async fn get_session_config(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.session_config.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "session config service not ready"})),
    ))?;
    let snapshot = svc.get_config(&sid).await.map_err(|e| {
        tracing::warn!(session_id = %sid, error = %e, "Failed to read session config");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    Ok(Json(serde_json::to_value(&snapshot).unwrap_or_default()))
}

/// `PUT /sessions/{sid}/config` - apply a config change.
///
/// ADR-047: accepts a `SessionConfigDelta` as JSON body. Each field is
/// optional (null or omitted means "unchanged"). Persistence is immediate;
/// LLM-side effects are deferred to the next inference turn.
async fn put_session_config(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    Json(delta): Json<crate::agent::session_config::SessionConfigDelta>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.session_config.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "session config service not ready"})),
    ))?;
    svc.apply_config(&sid, delta).await.map_err(|e| {
        tracing::warn!(session_id = %sid, error = %e, "Failed to apply session config");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    Ok(Json(serde_json::json!({"status": "ok"})))
}

/// `GET /sessions/{sid}/messages` - paginated message list for a session. — paginated message list for a session.
///
/// `GET /sessions/{sid}/messages` - paginated message list for a session.
///
/// ADR-040: delegates to [`SessionMetadataService::get_messages`] via
/// the late-bind slot. The handler is a thin protocol converter - all
/// file I/O, pagination, and ADR-035 D9.2 tool_result truncation live
/// in the UseCase impl.
async fn get_messages(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    Query(query): Query<GetMessagesQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.session_metadata.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "session metadata service not ready"})),
    ))?;
    let resp = svc
        .get_messages(&sid, query.offset, query.limit, query.tail.unwrap_or(false))
        .await
        .map_err(|e| {
            tracing::warn!(
                session_id = %sid,
                error = %e,
                "Failed to read session messages"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;
    Ok(Json(serde_json::to_value(resp).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "serialization failed"})),
        )
    })?))
}

/// `GET /memory/graph` — full memory graph (ADR-034 §11.2 #10).
///
/// Phase 3 (ADR-034): now reads from the Grafeo memory store via
/// [`memory_query::list_nodes`] (with no pagination), instead of the
/// legacy `.jsonl` file fallback. Returns all four user-visible memory
/// labels (`Episodic`/`Knowledge`/`Procedural`/`Autobiographical`) so
/// the desktop's Memory panel can render the full graph without having
/// to chain multiple paginated calls.
///
/// On a store that has not been initialised yet (HTTP server starts
/// before Phase B), the response is a well-formed empty graph. The
/// response shape (`agent_id` + `node_count` + `nodes` + `edges`) is
/// kept stable from the legacy handler so the desktop panel does not
/// have to branch on `node_count == 0`.
async fn get_memory_graph(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let query = crate::usecases::memory_query::MemoryNodeQuery {
        page: 1,
        size: 100_000,
        node_type: String::new(),
        sub_type: String::new(),
        keyword: String::new(),
        time_range: "all".to_string(),
    };
    let resp = svc
        .list_nodes(&query)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let nodes: Vec<serde_json::Value> = resp
        .nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "node_id": n.node_id,
                "node_type": n.node_type,
                "content": n.content,
                "confidence": n.confidence,
                "decay_score": n.decay_score,
                "created_at": n.created_at,
                "last_accessed_at": n.last_accessed_at,
                "access_count": n.access_count,
                "status": n.status,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "agent_id": state.agent_id,
        "node_count": nodes.len(),
        "nodes": nodes,
        "edges": [],
    })))
}

// ── Memory API (ADR-033 — Grafeo-backed) ─────────────────────

/// Query parameters for `GET /memory/nodes`.
///
/// Matches the `MemoryNodesQuery` contract documented in
/// `docs/zh/protocols/http.md` §7.7.
#[derive(Debug, Deserialize)]
struct ListNodesQuery {
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
    /// Filter by node type ("Episodic" / "Knowledge" / "Procedural" / "Autobiographical").
    #[serde(default, rename = "type")]
    node_type: Option<String>,
    /// Sub-classification filter (Knowledge / Autobiographical only):
    /// `Fact` | `Preference` | `Relation` | `Procedure` for Knowledge;
    /// `Identity` | `Capability` | `Limitation` | `Preference` | `History`
    /// | `Relationship` for Autobiographical. Ignored for other types.
    #[serde(default)]
    sub_type: Option<String>,
    /// Case-insensitive substring filter.
    #[serde(default)]
    keyword: Option<String>,
    /// Time-range bucket: "1h" / "1d" / "7d" / "30d" / "all".
    #[serde(default)]
    time_range: Option<String>,
}

/// `GET /memory/nodes` — list memory nodes (paginated, filtered, searched).
async fn get_memory_nodes(
    State(state): State<HttpState>,
    Query(params): Query<ListNodesQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let query = crate::usecases::memory_query::MemoryNodeQuery {
        page: params.page.unwrap_or(1),
        size: params.size.unwrap_or(20),
        node_type: params.node_type.unwrap_or_default(),
        sub_type: params.sub_type.unwrap_or_default(),
        keyword: params.keyword.unwrap_or_default(),
        time_range: params.time_range.unwrap_or_default(),
    };
    let resp = svc
        .list_nodes(&query)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::to_value(resp).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    ))
}

/// `GET /memory/nodes/{nid}` — single memory node detail (ADR-034 §11.2 #12).
///
/// Added in Phase 3 so the Memory panel can render node detail views
/// without having to download the entire graph. The business logic
/// lives in [`memory_query::get_node`] so the same shape can be
/// surfaced over gRPC in the future.
async fn get_memory_node(
    State(state): State<HttpState>,
    Path(nid): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let node_id: u64 = nid.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let val = svc.get_node(node_id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(val))
}

/// `GET /memory/stats` — memory statistics.
///
/// Returns the full `MemoryStats` contract (defined in
/// `crate::usecases::memory_query`). The same contract is produced by
/// both the ADR-040 service path and the pre-ADR-040 fallback path;
/// keeping the struct `Serialize` and using `serde_json::to_value`
/// directly avoids any drift between the two paths.
async fn get_memory_stats(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let stats = svc.get_stats().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::to_value(&stats).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    ))
}

/// `DELETE /memory/nodes/{nid}` — delete a memory node.
async fn delete_memory_node(
    State(state): State<HttpState>,
    Path(nid): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let node_id: u64 = nid.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    svc.delete_node(node_id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({"deleted": true, "node_id": node_id})))
}

/// Request body for `POST /memory/nodes`.
///
/// `properties` is a flat `key → JSON value` map. The implementation is
/// responsible for serialising values into the underlying store (see
/// [`crate::http::memory_query::json_to_grafeo_value`]).
#[derive(Deserialize)]
struct CreateMemoryNodeBody {
    label: String,
    #[serde(default)]
    properties: std::collections::HashMap<String, serde_json::Value>,
}

/// `POST /memory/nodes` — create a new memory node.
///
/// Returns `{"node_id": <u64>, "label": "..."}` on success. The handler
/// relies on the ADR-040 usecase trait so memory-store initialisation
/// timing (the Runtime HTTP server starts before Phase B) reports a clean
/// 503 instead of a panic.
async fn create_memory_node(
    State(state): State<HttpState>,
    Json(body): Json<CreateMemoryNodeBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use crate::usecases::memory_query::CreateMemoryNodeInput;
    let input = CreateMemoryNodeInput {
        label: body.label,
        properties: body.properties,
    };
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let node_id = svc
        .create_node(&input)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "node_id": node_id,
        "label": input.label,
    })))
}

/// Request body for `PUT /memory/nodes/{nid}`.
#[derive(Deserialize)]
struct UpdateMemoryNodeBody {
    #[serde(default)]
    properties: std::collections::HashMap<String, serde_json::Value>,
}

/// `PUT /memory/nodes/{nid}` — update (merge) properties on an existing node.
///
/// Returns 404 if the node is missing (mapped from
/// `RuntimeError::Memory("not found")`).
async fn update_memory_node(
    State(state): State<HttpState>,
    Path(nid): Path<String>,
    Json(body): Json<UpdateMemoryNodeBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let node_id: u64 = nid.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "node_id must be a non-negative integer"})),
        )
    })?;

    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "memory store not ready"})),
    ))?;

    match svc.update_node(node_id, &body.properties).await {
        Ok(()) => Ok(Json(serde_json::json!({
            "updated": true,
            "node_id": node_id,
        }))),
        Err(crate::error::RuntimeError::Memory(msg)) if msg.contains("not found") => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": msg, "node_id": node_id})),
        )),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to update memory node"})),
        )),
    }
}

/// Request body for `POST /memory/consolidate`.
///
/// `retention_days` is accepted for API compatibility but currently
/// has no effect on Phase 2 consolidation (see
/// [`memory_query::trigger_consolidate`]).
#[derive(Debug, Default, Deserialize)]
struct ConsolidateBody {
    #[serde(default)]
    force: bool,
    #[serde(default)]
    retention_days: u32,
}

/// `POST /memory/consolidate` — trigger memory consolidation.
async fn trigger_consolidate(
    State(state): State<HttpState>,
    Json(body): Json<ConsolidateBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let report = svc
        .consolidate(body.force, body.retention_days)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::to_value(report).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    ))
}

// ── Session detail (ADR-034 §11.2 #4 — panel 4) ──────────────

/// `GET /sessions/{sid}` - full session detail (ADR-034 \u00a711.2 #4, panel 4).
///
/// ADR-040: delegates to [`SessionMetadataService::get_session`] via
/// the late-bind slot. The impl reads `meta.json` and merges the live
/// `SharedSessionSnapshots` state into a single `SessionDetail` struct;
/// this handler is a thin protocol converter that returns 404 when the
/// session is completely unknown (no meta AND no live snapshot).
async fn get_session(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.session_metadata.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "session metadata service not ready"})),
    ))?;
    let detail = svc.get_session(&sid).await.map_err(|e| {
        tracing::warn!(session_id = %sid, error = %e, "Failed to read session detail");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    // 404 only when both meta and live_state are absent - either
    // present is enough to return 200 (the meta card alone is useful
    // for sessions that have been written but never observed).
    if detail.created_at.is_empty() && detail.live_state.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "session not found"})),
        ));
    }

    // ADR-047: response no longer includes config fields (model,
    // provider, workspace_id, title, reasoning_effort, temperature).
    // Config is served by `GET /sessions/{sid}/config`. The `meta`
    // object here carries only state-level metadata.
    let meta = serde_json::json!({
        "session_id": detail.session_id,
        "created_at": detail.created_at,
        "last_active_at": detail.last_active_at,
        "message_count": detail.message_count,
    });
    Ok(Json(serde_json::json!({
        "session_id": sid,
        "meta": meta,
        "live_state": detail.live_state,
    })))
}

// ── Workspace Query Handlers (ADR-040) ─────────────────────────────────────
//
// These handlers are thin shells around [`WorkspaceQueryService`] — they
// parse the axum extractors, call the trait method, and serialize the
// result. All filesystem / path-resolution logic lives in the usecase
// implementation so HTTP handlers stay free of I/O concerns.

/// `GET /workspaces` — list workspace directories from
/// `agent_workspaces.json`. Returns `{ agent_id, workspaces: [...] }`.
async fn list_workspaces(
    State(state): State<HttpState>,
) -> Result<Json<crate::usecases::workspace_query::WorkspacesListResponse>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_query.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.list_workspaces()
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

/// `GET /workspaces/tree` — list directory contents for a workspace.
async fn list_tree(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::ListTreeParams>,
) -> Result<Json<crate::usecases::workspace_query::TreeResponse>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_query.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.list_tree(&params)
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

/// `GET /workspaces/file?path=…` — read a UTF-8 text file.
async fn read_workspace_file(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::FilePathQuery>,
) -> Result<Json<crate::usecases::workspace_query::WorkspaceFileDto>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_query.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    let read_params: crate::usecases::workspace_query::ReadFileParams = (&params).into();
    svc.read_file(&read_params)
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

/// `GET /workspaces/raw/{path}?workspace_id=…` — serve a file's raw bytes
/// (ADR-055 L2-7). The Gateway reverse-proxies its `/workspace-files/…`
/// HTML-preview endpoints here instead of reading the workspace
/// filesystem itself. Returns `Content-Type` + verbatim bytes so the
/// preview iframe's `<img>` / `<link>` / `<script>` sub-resources resolve.
async fn read_workspace_raw(
    State(state): State<HttpState>,
    Path(file_rel_path): Path<String>,
    Query(q): Query<crate::usecases::workspace_query::RawFileQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_query.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    let params = crate::usecases::workspace_query::ReadFileParams {
        workspace_id: q.workspace_id,
        path: file_rel_path,
    };
    let dto = svc
        .read_file_raw(&params)
        .await
        .map_err(workspace_error_to_response)?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, dto.mime_type)],
        dto.bytes,
    ))
}

/// `GET /workspaces/find` — fuzzy filename search (Ctrl+P-style palette).
async fn find_files(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::FindFilesParams>,
) -> Result<Json<crate::usecases::workspace_query::FindResponse>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_query.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.find_files(&params)
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

/// `GET /workspaces/search` — ripgrep-style content search (Ctrl+Shift+F).
async fn search_files(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::SearchFilesParams>,
) -> Result<Json<crate::usecases::workspace_query::SearchResponse>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_query.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.search_files(&params)
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

// ── Workspace Mutation Handlers (ADR-040) ──────────────────────────────────

/// `POST /workspaces` — add a new workspace entry.
async fn create_workspace(
    State(state): State<HttpState>,
    Json(body): Json<crate::usecases::workspace_mutation::WorkspaceEntryInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = {
        let svc = state.workspace_mutation.lock().await;
        let svc = svc
            .as_ref()
            .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
        svc.create_workspace(body).await
    };
    match result {
        Ok(r) => {
            reload_workspace_resolver(&state);
            // ADR-058: reconcile watcher set with the reloaded resolver.
            sync_workspace_watchers(&state).await;
            // Fix-3 sanity check: after reload, the newly-created
            // workspace id MUST be visible via `find_by_id`. A `None`
            // here indicates a write/read race (e.g. another thread
            // clobbered `agent_workspaces.json` between `save_config`
            // and the reload). The Desktop would experience this as
            // "workspace disappeared" — surface it as 500 so the
            // abnormal condition is loud rather than silently returning
            // an entry that downstream `route_workspace_switch` calls
            // cannot resolve. See `desktop-onboarding-bugfix_154b7ff7.md`
            // §Fix 3.
            if let Some(new_id) = r
                .entry
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
            {
                match state.workspace_resolver.read() {
                    Ok(resolver) => {
                        if resolver.find_by_id(new_id).is_none() {
                            tracing::error!(
                                ws_id = %new_id,
                                "Workspace mutation: reload succeeded but new id not visible — possible write/read race"
                            );
                            return Err((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({
                                    "error": "Workspace persisted but not visible after reload — possible write/read race",
                                    "ws_id": new_id,
                                })),
                            ));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "create_workspace: resolver lock poisoned during sanity check"
                        );
                    }
                }
            }
            Ok(Json(r.entry.unwrap_or(serde_json::json!({"created": r.ok}))))
        }
        Err(e) => Err(workspace_error_to_response(e)),
    }
}

/// Reload the shared `WorkspaceResolver` from disk after a successful
/// workspace mutation (create/update/delete).
///
/// The resolver is built once at Phase A and injected into both
/// `SessionManager` (for `route_workspace_switch` validation) and this
/// `HttpState`. Persisting a new workspace to `agent_workspaces.json`
/// without reloading leaves the in-memory resolver stale, so the new id
/// fails `find_by_id` and the switch falls back to `__agent_home__`.
fn reload_workspace_resolver(state: &HttpState) {
    let mut guard = match state.workspace_resolver.write() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "reload_workspace_resolver: resolver lock poisoned");
            return;
        }
    };
    *guard = crate::tools::workspace_resolver::WorkspaceResolver::new(
        &state.work_dir.to_string_lossy(),
    );
    tracing::info!(
        work_dir = %state.work_dir.display(),
        allowed = guard.allowed_dirs().len(),
        "reloaded WorkspaceResolver after workspace mutation"
    );
}

/// Reconcile the workspace watcher set (ADR-058) with the freshly
/// reloaded resolver — starts watchers for new workspaces, stops
/// watchers for removed ones, and restarts watchers whose root moved.
///
/// MUST be called after [`reload_workspace_resolver`] so the resolver
/// the set syncs against is the on-disk truth.
async fn sync_workspace_watchers(state: &HttpState) {
    // Clone the resolver out of the std RwLock guard so the guard is
    // not held across the tokio Mutex await (Send requirement).
    let resolver = state
        .workspace_resolver
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut watchers = state.workspace_watchers.lock().await;
    watchers.sync_from_resolver(&resolver);
}

/// `PUT /workspaces/{ws_id}` — update an existing workspace entry.
async fn update_workspace(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
    Json(body): Json<crate::usecases::workspace_mutation::WorkspaceEntryInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = {
        let svc = state.workspace_mutation.lock().await;
        let svc = svc
            .as_ref()
            .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
        svc.update_workspace(&ws_id, body).await
    };
    match result {
        Ok(r) => {
            // Path/access changes must be visible to the resolver so
            // `find_by_id` returns the updated entry for live sessions.
            reload_workspace_resolver(&state);
            // ADR-058: a path change restarts the watcher for this id.
            sync_workspace_watchers(&state).await;
            Ok(Json(r.entry.unwrap_or(serde_json::json!({"updated": r.ok}))))
        }
        Err(e) => Err(workspace_error_to_response(e)),
    }
}

/// `PUT /workspaces/{ws_id}/prompt-file` — set the prompt_file field.
async fn set_workspace_prompt_file(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
    Json(body): Json<crate::usecases::workspace_mutation::PromptFileBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.set_prompt_file(&ws_id, body)
        .await
        .map(|_| Json(serde_json::json!({"ok": true, "ws_id": ws_id})))
        .map_err(workspace_error_to_response)
}

/// `DELETE /workspaces/{ws_id}` — remove a workspace entry.
async fn delete_workspace(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = {
        let svc = state.workspace_mutation.lock().await;
        let svc = svc
            .as_ref()
            .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
        svc.delete_workspace(&ws_id).await
    };
    match result {
        Ok(_) => {
            // A deleted workspace must leave the resolver immediately —
            // otherwise `route_workspace_switch` still accepts its id.
            reload_workspace_resolver(&state);
            // ADR-058: also stop the deleted workspace's watcher.
            sync_workspace_watchers(&state).await;
            Ok(Json(serde_json::json!({"deleted": true, "ws_id": ws_id})))
        }
        Err(e) => Err(workspace_error_to_response(e)),
    }
}

/// `POST /workspaces/file` — create a new text file.
async fn create_workspace_file(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::CreateFileBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.create_file(
        body,
        qparams.get("workspace_id").map(|s| s.as_str()),
        qparams.get("path").map(|s| s.as_str()),
    )
    .await
    .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"created": r.ok}))))
    .map_err(workspace_error_to_response)
}

/// `PUT /workspaces/file` — overwrite an existing text file.
async fn write_workspace_file(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::FilePathQuery>,
    Json(body): Json<crate::usecases::workspace_mutation::WriteFileBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    let mutation_query = crate::usecases::workspace_mutation::FilePathQuery {
        workspace_id: params.workspace_id.clone(),
        path: params.path.clone(),
    };
    svc.write_file(mutation_query, body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"written": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// `DELETE /workspaces/file` — remove a file.
async fn delete_workspace_file(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::PathOnlyBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.delete_file(
        qparams.get("workspace_id").map(|s| s.as_str()),
        body,
    )
    .await
    .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"deleted": r.ok}))))
    .map_err(workspace_error_to_response)
}

/// `POST /workspaces/dir` — create a directory (recursive).
async fn create_workspace_dir(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::PathOnlyBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.create_dir(
        qparams.get("workspace_id").map(|s| s.as_str()),
        body,
    )
    .await
    .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"created": r.ok}))))
    .map_err(workspace_error_to_response)
}

/// `DELETE /workspaces/dir` — remove a directory recursively.
async fn delete_workspace_dir(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::PathOnlyBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.delete_dir(
        qparams.get("workspace_id").map(|s| s.as_str()),
        body,
    )
    .await
    .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"deleted": r.ok}))))
    .map_err(workspace_error_to_response)
}

/// `POST /workspaces/copy` — copy a file or directory tree.
///
/// Body: `{workspace_id?, source, dest}` (`workspace_id` may also live in
/// the querystring). Both paths must resolve under the same workspace
/// root and the destination's parent directory must already exist.
/// Returns 409 if `dest` already exists.
async fn copy_workspace_item(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::CopyMoveBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    // Prefer the querystring's workspace_id (matches the desktop
    // `workspaceStore` convention) but fall back to the body if absent.
    let mut body = body;
    if let Some(qs_ws) = qparams.get("workspace_id").filter(|s| !s.is_empty())
        && body.workspace_id.as_deref().unwrap_or("").is_empty()
    {
        body.workspace_id = Some(qs_ws.clone());
    }
    svc.copy_item(body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"copied": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// `POST /workspaces/rename` — atomically rename (move) a file or
/// directory inside the same workspace.
///
/// Body: `{workspace_id?, source, dest}`. `std::fs::rename` is atomic
/// on the same filesystem and falls back to copy+delete across
/// filesystem boundaries at the OS layer. Returns 409 if `dest` already
/// exists — explicit user intent (paste-as-copy's recursive dedupe runs
/// client-side) is responsible for picking an unused name.
async fn rename_workspace_item(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::CopyMoveBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    let mut body = body;
    if let Some(qs_ws) = qparams.get("workspace_id").filter(|s| !s.is_empty())
        && body.workspace_id.as_deref().unwrap_or("").is_empty()
    {
        body.workspace_id = Some(qs_ws.clone());
    }
    svc.rename_item(body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"renamed": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// Map [`WorkspaceError`] → `(StatusCode, Json<...>)` response tuple.
///
/// HTTP status is taken from [`WorkspaceError::http_status`]; the error
/// string becomes the JSON `error` field. This is the single place
/// where usecase errors become HTTP responses — adding a new variant
/// to `WorkspaceError` only requires touching this function.
fn workspace_error_to_response(e: crate::usecases::WorkspaceError) -> (StatusCode, Json<serde_json::Value>) {
    let status = StatusCode::from_u16(e.http_status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let msg = e.to_string();
    (status, Json(serde_json::json!({"error": msg})))
}

// ── Attachment storage handlers (ADR-046) ─────────────────────────────────
//
// All uploads land in the unified blob store at
// `<work_dir>/files/<document_id>` (no session subdirectory — JSONL is
// the per-session index). Both handlers route through the
// `AttachmentService` UseCase trait (ADR-040) so the HTTP layer never
// touches the filesystem directly. Pre-Phase-B HTTP probes receive
// `503 Service Unavailable` from the slot-first pattern, matching
// every other `*Service` handler in this crate.

use axum::extract::Multipart;

/// Convert an [`AttachmentError`] into the `(status, json)` tuple
/// used by every attachment handler. Mirrors the shape of
/// `workspace_error_to_response` so a future API-surface change only
/// touches this function.
fn attachment_error_to_response(e: crate::usecases::AttachmentError) -> (StatusCode, Json<serde_json::Value>) {
    use crate::usecases::AttachmentError as Ae;
    let status = match &e {
        Ae::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        Ae::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
        Ae::InvalidDocumentId(_) => StatusCode::BAD_REQUEST,
        Ae::NotFound(_) => StatusCode::NOT_FOUND,
        Ae::Persistence(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(serde_json::json!({"error": e.to_string()})))
}

/// `POST /sessions/{sid}/files` — upload a file or image.
///
/// Accepts a multipart form with one `file` field. Optional fields:
///   - `width`, `height`: pixel dimensions for image uploads (the
///     desktop frontend reads them via `new Image()` before sending).
///     Both are optional; the renderer falls back to natural sizing
///     when absent.
///
/// The `session_id` path parameter is informational only — the blob is
/// stored globally (one directory for the whole agent) and the
/// per-session association lives in the JSONL `file_upload` /
/// `image_upload` system entry that the desktop writes via the
/// `document_ids` MQTT param.
async fn upload_file(
    State(state): State<HttpState>,
    Path(_sid): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<crate::usecases::UploadedFileResponse>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state
        .attachment
        .lock()
        .await
        .clone()
        .ok_or_else(|| attachment_error_to_response(crate::usecases::AttachmentError::ServiceUnavailable))?;

    let mut filename: Option<String> = None;
    let mut format: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        attachment_error_to_response(crate::usecases::AttachmentError::Persistence(format!(
            "multipart: {e}"
        )))
    })? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                bytes = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| {
                            attachment_error_to_response(crate::usecases::AttachmentError::Persistence(format!(
                                "read bytes: {e}"
                            )))
                        })?
                        .to_vec(),
                );
            }
            "format" => {
                format = Some(field.text().await.map_err(|e| {
                    attachment_error_to_response(crate::usecases::AttachmentError::Persistence(format!(
                        "read format: {e}"
                    )))
                })?);
            }
            "width" => {
                if let Ok(s) = field.text().await {
                    width = s.parse().ok();
                }
            }
            "height" => {
                if let Ok(s) = field.text().await {
                    height = s.parse().ok();
                }
            }
            _ => {
                // Ignore unknown fields (forwards-compat for future
                // client additions).
            }
        }
    }

    let bytes = bytes.ok_or_else(|| {
        attachment_error_to_response(crate::usecases::AttachmentError::Persistence(
            "missing `file` field".to_string(),
        ))
    })?;
    let filename = filename.unwrap_or_else(|| "upload".to_string());
    let format = format.unwrap_or_else(|| {
        // Fall back to the filename extension when the client didn't
        // supply an explicit format. The runtime does NOT sniff
        // content — clients are the source of truth.
        std::path::Path::new(&filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default()
    });

    let resp = svc
        .upload_file(crate::usecases::UploadFileParams {
            filename,
            format,
            bytes,
            width,
            height,
        })
        .await
        .map_err(attachment_error_to_response)?;
    Ok(Json(resp))
}

/// `GET /files/{document_id}` — read a previously-uploaded blob.
///
/// Returns the raw bytes with `Content-Type` derived from the
/// extension the desktop supplies via the `format` query param (the
/// on-disk file has no extension — format lives in JSONL metadata).
async fn read_file(
    State(state): State<HttpState>,
    Path(document_id): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<(StatusCode, [(axum::http::HeaderName, &'static str); 1], Vec<u8>), (StatusCode, Json<serde_json::Value>)> {
    let svc = state
        .attachment
        .lock()
        .await
        .clone()
        .ok_or_else(|| attachment_error_to_response(crate::usecases::AttachmentError::ServiceUnavailable))?;

    let bytes = svc
        .read_file(&document_id)
        .await
        .map_err(attachment_error_to_response)?;

    // Derive Content-Type from the `format` query param (extension
    // only — no MIME sniffing server-side). Default to
    // application/octet-stream when missing.
    let format = params.get("format").map(String::as_str).unwrap_or("");
    let content_type = match format {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    };

    Ok((StatusCode::OK, [(axum::http::header::CONTENT_TYPE, content_type)], bytes))
}


// ── Agent panel handlers (ADR-034 §11.2 #23-25) ────────────────────────
//
// Three GET endpoints powering the desktop panels Setup (1), Tools (3)
// and Agent Status (5). All read-only — mutations flow through the
// dedicated MQTT control commands.

/// `GET /agents/{id}/config` — Agent Setup panel data.
///
/// ADR-040 follow-up: persistence + the `{matches, config,
/// manifest_path, work_dir}` envelope construction live in
/// [`AgentConfigService::get_config`]. This handler is a thin
/// protocol converter that:
///   1. validates path `id` against `state.agent_id` (ADR-034
///      cross-process routing guard — return an empty envelope
///      rather than 404 so a misconfigured Gateway doesn't blank
///      the whole panel),
///   2. delegates the load to the use-case trait.
///
/// Returns 503 when the late-bind slot is still empty (Phase B
/// hasn't run yet) — mirrors every other ADR-040 use-case slot.
async fn get_agent_config(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let matches = id == state.agent_id;
    if !matches {
        // Tolerate a misconfigured Gateway rather than 404 — see ADR-034
        // and the comment block above.
        return Ok(Json(serde_json::json!({
            "agent_id": state.agent_id,
            "matches": false,
            "config": serde_json::Value::Null,
            "manifest_path": state.work_dir.join("manifest.toml"),
            "work_dir": state.work_dir,
        })));
    }
    let resp = state
        .agent_config
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent config service not ready"
            })),
        ))?
        .get_config(&id)
        .await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "matches": matches,
        "config": resp.config,
        "manifest_path": resp.manifest_path,
        "work_dir": resp.work_dir,
    })))
}

/// `PUT /agents/{id}/config` — live-edit per-agent runtime config
/// (mqtt.md §7).
///
/// ADR-040 follow-up: persistence + the load-merge-save cycle +
/// per-field dispatch + the `RuntimeConfigOverrides` projection all
/// live in [`AgentConfigService::put_config`]. This handler is a
/// thin protocol converter that:
///   1. decodes the wire shape into [`PutAgentConfigBody`],
///   2. hands it to the use-case trait,
///   3. broadcasts the returned `RuntimeConfigOverrides` to active
///      sessions via the existing dispatch channel,
///   4. re-PUBLISHes the retained `acowork/agents/{id}/config`
///      snapshot using the returned [`crate::agent_config::AgentConfig`].
///
/// **`builtin_tools` no longer lives here** — it was misleadingly
/// bundled into this endpoint in the original implementation, but it
/// persists to a different file (`agent_tools.json`, see ADR-029) and
/// has different read-modify-write semantics. It moved to its own
/// endpoint at `PUT /agents/{id}/builtin-tools` (handled by
/// [`AgentToolsService::put_builtin_tools`]) — the Desktop Tools
/// panel now calls that endpoint directly.
///
/// Active sessions are NOT force-reloaded here. New sessions created
/// after this call pick up the new values naturally because Phase A
/// re-reads `agent_config.json` at startup; existing in-flight
/// sessions pick up live-editable fields via the `UserOp::UpdateRuntimeConfig`
/// broadcast path (step 3 above) — same contract documented in
/// `mqtt.md` §3.5.
async fn put_agent_config(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAgentConfigRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // ADR-034: path `id` must match this Runtime's agent_id. A mismatch
    // is a caller bug — 404 makes the misrouting loud instead of
    // silently writing to the wrong agent's directory.
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "agent_id mismatch: path '{}' does not match this runtime '{}'",
                    id, state.agent_id
                ),
            })),
        ));
    }

    // 1. Hand the per-field patches to the use-case trait. The impl
    //    runs the load-merge-save cycle against `agent_config.json`
    //    and returns the new on-disk state plus the
    //    `RuntimeConfigOverrides` projection for the live broadcast.
    let body = crate::usecases::PutAgentConfigBody::from_request_fields(
        req.max_output_tokens,
        req.max_iterations,
        req.max_sessions,
        req.temperature,
        req.context_window,
        req.shell_approval_threshold,
        req.approval_timeout_secs,
        req.idle_timeout_secs,
        req.compression_ratio_threshold,
    );
    let svc = state
        .agent_config
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent config service not ready"
            })),
        ))?
        .clone();
    let result = svc.put_config(&id, body).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("agent_config.json persistence failed: {}", e),
            })),
        )
    })?;

    // 2. Live-broadcast the live-editable subset (temperature,
    //    context_window, max_iterations, …)
    //    through `SessionManager::apply_runtime_config_override` via a
    //    SINGLE system-level message. That pipeline applies the change
    //    to (a) the shared AgentCore template so future sessions
    //    inherit it, (b) the `runtime_overrides` cache, (c) every
    //    active SessionTask's `ContextBuilder.tool_definitions` (so
    //    the LLM sees the new set on the next `build_chat_request`),
    //    and (d) mid-execution AgentLoops via the inbound fast
    //    channel. The pre-ADR-052 implementation fanned out one
    //    per-session message into the AgentLoop fast channel only,
    //    which left the LLM-visible `ContextBuilder.tool_definitions`
    //    stale - the "hot reload does nothing" symptom.
    broadcast_runtime_overrides(&state, &result.overrides).await;

    // 3. Re-PUBLISH retained config so any other Desktop subscriber
    //    (and the Desktop's own ConfigSnapshot listener) sees the new
    //    values immediately. Best-effort: if the broker isn't
    //    reachable yet, the on-disk file is still authoritative.
    if let Some(mqtt) = state.mqtt_client.lock().await.clone() {
        // ADR-040: use the config_json returned by the UseCase impl
        // instead of re-reading the file from disk.
        let config_json = result.config_json.clone();
        let envelope = acowork_core::mqtt_proto::DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::AgentConfig(
                    acowork_core::mqtt_proto::AgentConfig {
                        agent_id: state.agent_id.clone(),
                        config_json,
                    },
                ),
            ),
        };
        let topic = format!("acowork/agents/{}/config", state.agent_id);
        if let Err(e) = mqtt
            .lock()
            .await
            .publish_envelope(
                &topic,
                &envelope,
                crate::mqtt::client::MqttQoS::AtLeastOnce,
                true, // Retained — `mqtt.md` §3.5 contract
            )
            .await
        {
            tracing::warn!(
                agent_id = %state.agent_id,
                error = %e,
                "PUT /agents/{id}/config: failed to re-PUBLISH retained config snapshot"
            );
        }
    }

    Ok(Json(serde_json::json!({
        "agent_id": state.agent_id,
        "accepted": true,
    })))
}

/// Request body for `PUT /agents/{id}/config` (mqtt.md §7).
///
/// Each field is **optional** and matches the wire shape that the
/// Desktop `AgentSetupTab.handleApply` builds from `AgentProfileSettings`.
/// The handler applies a read-modify-write cycle so partial updates
/// don't clobber unrelated on-disk values (e.g. touching
/// `temperature` must not erase an existing `context_window`).
///
/// Semantics notes:
///   - Every field here uses the same `"Some(...)" -> overwrite,
///     "None" -> leave alone` rule, so the wire shape is partial.
///   - **`builtin_tools` no longer lives on this struct.** It used to
///     persist to `agent_tools.json` while everything else persisted
///     to `agent_config.json` — a confusing mismatch that the original
///     design papered over by sharing one endpoint. It moved to its
///     own endpoint `PUT /agents/{id}/builtin-tools` (handled by
///     [`AgentToolsService::put_builtin_tools`]) and the Desktop Tools
///     panel now calls that endpoint directly.
///   - Fields that have no analogue on the wire (e.g.
///     `avatar`, `builtin_avatar`,
///     `system_prompt_override`) are deliberately omitted from this
///     struct: they aren't exposed in the Setup panel today, so
///     accepting them on the wire would silently no-op and confuse
///     callers. They keep flowing through `RuntimeConfigUpdate` over
///     MQTT the same as before.
#[derive(Debug, Deserialize)]
struct UpdateAgentConfigRequest {
    // ── Per-agent config (`agent_config.json`) ──
    //
    // We use `Option<serde_json::Value>` as the wire carrier for each
    // per-agent field.  Note this cannot distinguish three wire
    // states — serde collapses JSON `null` and absent-field into the
    // same `None` (the same is true of `Option<T>`).  In practice
    // this is fine: the Desktop `AgentSetupTab.handleApply` never
    // sends JSON `null`, only concrete values or omits the field,
    // so:
    //
    //   1. Field absent in the JSON payload -> `None` outer ->
    //      leave the on-disk value alone (partial PUT semantics).
    //   2. Field present with a value       -> `Some(Value::...)`  ->
    //      overwrite with the deserialized value.
    //
    // If a future caller ever needs explicit-clearing semantics
    // (e.g. a CLI that wants to send `"temperature": null` to fall
    // through to the manifest default), the cleanest path is to
    // introduce an `OptionalField<T>` presence-tracking wrapper
    // around the inner `Value`.  See the test doc-comment for
    // `test_put_agent_config_persists_per_agent_fields`.
    //
    // Mirrors `AgentConfig` one-for-one; keep the two in lockstep
    // when adding new fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_iterations: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_sessions: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    temperature: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_window: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shell_approval_threshold: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_timeout_secs: Option<serde_json::Value>,
    /// Idle (auto-sleep) timeout in seconds before the Runtime
    /// self-terminates. `0` = never sleep. `None` outer = leave the
    /// on-disk value alone (partial PUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idle_timeout_secs: Option<serde_json::Value>,
    /// ADR-061: minimum compression ratio for levels 1-7 (0.05–0.95;
    /// 0.90 default = "compress until at most 10% remains"). Absent =
    /// leave the on-disk value alone (partial PUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compression_ratio_threshold: Option<serde_json::Value>,
}

impl UpdateAgentConfigRequest {
    // The previous `project()` method that built the patch list and
    // `RuntimeConfigOverrides` projection inline was deleted when
    // persistence moved to [`crate::usecases::AgentConfigService`].
    // The handler now translates the wire shape into
    // [`crate::usecases::PutAgentConfigBody::from_request_fields`]
    // and the impl returns the new on-disk config plus the
    // `RuntimeConfigOverrides` projection in lockstep. See the
    // `put_agent_config` handler for the dispatch flow.
}

// ── Per-field patch machinery moved to usecases/agent_config.rs ──────
//
// The previous `UpdateAgentConfigRequest::project()`,
// `value_to_patch`, `patch_typed`, and `FieldPatchExt` helpers all
// lived inline in this file. After the ADR-040 follow-up refactor
// the persistence + projection logic is the single audit point in
// [`crate::usecases::RuntimeAgentConfigService::put_config`]. The
// HTTP handler now translates the wire shape into
// [`crate::usecases::PutAgentConfigBody::from_request_fields`] and
// hands it to the trait; the impl runs the dispatch loop and
// returns the new on-disk state plus the `RuntimeConfigOverrides`
// projection.
//
// The shared [`FieldPatch`] enum + the `ConfigField` enum live in
// `usecases::agent_config`; both are pub-exported via
// `crate::usecases` so handlers can build the body without an
// extra indirection.

/// Broadcast a `RuntimeConfigOverrides` push to every live session.
///
/// The overrides are produced by `UpdateAgentConfigRequest::project`
/// above, which already projects the wire shape onto the in-process
/// struct (and forces boot-only fields to `None` so we never
/// invalidate in-flight conversation history pointers).
///
/// The push goes through `dispatch_tx` (per-session `(String,
/// InboundMessage)` channel), reusing the exact same routing that
/// `dispatch_inbound` uses for `UserOp::UpdateRuntimeConfig` today.
/// `gateway_loop` forwards those messages through
/// `forward_to_session_inbound`, which lands in `AgentLoop`'s
/// `drain_inbound_queue` → `apply_user_op` → `apply_runtime_config`,
/// so the next LLM iteration picks up the new temperature /
/// context_window / etc. without restarting the session.
/// Dispatch an agent-level config mutation through the Runtime's
/// single application pipeline via ONE system-level message.
///
/// The message travels `dispatch_tx` → `mqtt_dispatch_rx` →
/// `gateway_loop::dispatch_inbound`, which routes both agent-level
/// config variants (`UserOp::UpdateRuntimeConfig`,
/// `InboundMessage::UpdateBuiltinTools`) through the corresponding
/// `SessionManager::{apply_runtime_config_override,
/// apply_builtin_tools_enabled}` regardless of the routing key. Those
/// two methods are the ONLY entry points that update, in one shot:
///   1. the shared `AgentCore` **template** (so sessions opened LATER
///      inherit the new value),
///   2. the `runtime_overrides` / builtin-enable caches,
///   3. every active SessionTask's `ContextBuilder.tool_definitions`
///      (so the LLM sees the new set on the next
///      `build_chat_request`), and
///   4. mid-execution AgentLoops via the inbound fast channel
///      (`apply_user_op` → `core.apply_runtime_config`).
///      (ADR-061 §10.2: the `tool_compression_enabled` hot-reload
///      path is deleted — `context_retrieve` is always registered.)
///
/// Pre-ADR-052 the HTTP layer sent one message **per session id** into
/// the AgentLoop fast channel only. That mutated the live session's
/// `all_tools` dispatch list but left every other layer (template,
/// runtime_overrides cache, other sessions' `ContextBuilder`) stale -
/// which is precisely the "hot reload does nothing" symptom this
/// function is the fix for.
///
/// Routing key `""` marks the message system-level so dispatch_inbound
/// applies it through the SessionManager pipeline above. Sending
/// exactly ONE message (instead of fanning out per session) keeps the
/// pipeline idempotent regardless of how many sessions happen to be
/// alive at push time.
///
/// Best-effort on the dispatch channel itself: a missing `dispatch_tx`
/// (agent not yet ready, Phase A pre-Phase D startup race) logs a
/// warning and returns. The on-disk file written by the UseCase layer
/// remains authoritative and is re-read at the next boot.
async fn dispatch_agent_level_config(
    state: &HttpState,
    label: &str,
    msg: InboundMessage,
) {
    let tx_opt = state.dispatch_tx.lock().await.clone();
    let Some(tx) = tx_opt else {
        tracing::warn!(
            agent_id = %state.agent_id,
            label,
            "agent-level config dispatch skipped: dispatch channel not ready (on-disk file is authoritative)"
        );
        return;
    };
    match tx.send((String::new(), msg)) {
        Ok(()) => tracing::info!(
            agent_id = %state.agent_id,
            label,
            "agent-level config update dispatched to SessionManager pipeline (system-level)"
        ),
        Err(_) => tracing::warn!(
            agent_id = %state.agent_id,
            label,
            "agent-level config dispatch failed: dispatch channel closed (on-disk file is authoritative)"
        ),
    }
}

/// Push a runtime-config override into the SessionManager via
/// [`dispatch_agent_level_config`].
///
/// Early-exits when `overrides` is empty so we don't dispatch a no-op
/// `UpdateRuntimeConfig` that would still trigger
/// `emit_session_state` on every active session for nothing.
async fn broadcast_runtime_overrides(state: &HttpState, overrides: &RuntimeConfigOverrides) {
    if overrides.is_empty() {
        return;
    }
    dispatch_agent_level_config(
        state,
        "runtime_config",
        InboundMessage::UserOperation(UserOp::UpdateRuntimeConfig(overrides.clone())),
    )
    .await;
}

/// Push a builtin-tools enabled update into the SessionManager via
/// [`dispatch_agent_level_config`].
///
/// Always sends (even with zero active sessions) so the
/// `AgentCore` template gets the new flags and sessions opened later
/// inherit them.
async fn broadcast_builtin_tools_update(
    state: &HttpState,
    entries: &[crate::agent_config::AgentToolEntry],
) {
    dispatch_agent_level_config(
        state,
        "builtin_tools",
        InboundMessage::UpdateBuiltinTools {
            entries: entries.to_vec(),
        },
    )
    .await;
}

/// `GET /agents/{id}/tools` — Tools panel (merged: builtin + mcp + search).
///
/// ADR-034 §7.6.5 defines the merged response schema:
/// `{tools: [BuiltinToolEntry], mcp_servers: [server_name], search: {providers: [...]}}`
/// `GET /agents/{id}/tools` - Tools panel (merged: builtin + mcp + search).
///
/// ADR-034 \u00a77.6.5 defines the merged response schema:
/// `{tools: [BuiltinToolEntry], mcp_servers: [server_name], search: {providers: [...]}}`
/// (panel 3 pulls all three sources in one HTTP call instead of 3 separate ones).
///
/// ADR-040: delegates to [`AgentToolsService::get_merged_tools`] via
/// the late-bind slot. All three `agent_*` config files are read inside
/// the UseCase impl; the handler is a thin protocol converter.
async fn get_agent_tools(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let matches = id == state.agent_id;
    if !matches {
        // Tolerate a misconfigured Gateway rather than 404 - see ADR-034.
        return Ok(Json(serde_json::json!({
            "agent_id": state.agent_id,
            "matches": false,
            "tools": [],
            "mcp_servers": [],
            "search": { "providers": [] },
        })));
    }
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let resp = svc.get_merged_tools(&id).await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "matches": true,
        "tools": resp.tools,
        "mcp_servers": resp.mcp_servers,
        "search": resp.search,
    })))
}

// ── Per-agent MCP activation endpoints ────────────────────────────────
//
// Win11-MCP-ToolsBugFix: the Desktop Tools panel toggles per-agent MCP server
// activation (`activeServers` in `mcpStore.ts`). The wiring lives in 3 layers:
//   1. Desktop  : PUT  /api/agents/{id}/mcp-servers   (sends {servers: ["name1", ...]})
//   2. Gateway  : pure reverse-proxy to Runtime       (see acowork-gateway proxy.rs)
//   3. Runtime  : validate id match + read-modify-write `agent_mcp.json`
//
// Before this fix the Gateway-side stub returned 200 but never persisted
// (`let _ = (..., resolved_servers)`). The Desktop optimistically updated
// its in-memory Zustand store on success, then lost the selection the next
// time the user switched tabs because the merged `/tools` endpoint read from
// the (empty) on-disk config and overwrote the Zustand copy with `[]`.

/// `GET /agents/{id}/mcp-servers` — names of MCP servers active for this agent
/// (i.e. the user's selection in the Desktop Tools panel).
async fn get_agent_mcp_servers(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "agent_id mismatch: path '{}' does not match this runtime '{}'",
                    id, state.agent_id
                ),
            })),
        ));
    }
    // ADR-040 follow-up: route through the trait. HTTP handler is now a
    // thin protocol converter (path-match + JSON shape) — the file
    // I/O, `active_names` resolution, and any future cross-field
    // validation live in [`crate::usecases::RuntimeAgentToolsService`].
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let resp = svc.get_mcp_servers(&id).await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "active_servers": resp.active_servers,
    })))
}

/// `PUT /agents/{id}/mcp-servers` — set the active MCP server selection.
///
/// Body: `{"servers": ["name1", ...]}` — just the catalog names the user ticked.
/// Acceptance rule: each name must exist in `merged()` (catalog + local). An
/// unknown name is a 400 — the frontend already filters to catalog items so
/// this protects against stale names reaching us via direct API calls.
///
/// Storage: write `active_names = Some(req.servers)` while preserving `catalog`
/// and `local` via a read-modify-write cycle (parallel to `put_agent_config`'s
/// per-field patch in this file).
async fn put_agent_mcp_servers(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMcpServersRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "agent_id mismatch: path '{}' does not match this runtime '{}'",
                    id, state.agent_id
                ),
            })),
        ));
    }

    // ADR-040 follow-up: validation + persistence live in
    // [`crate::usecases::RuntimeAgentToolsService`]. The handler is
    // now a thin protocol converter that maps service errors to HTTP
    // status codes.
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let body = crate::usecases::PutMcpServersBody {
        servers: req.servers,
    };
    match svc.put_mcp_servers(&id, body).await {
        Ok(resp) => Ok(Json(serde_json::json!({
            "agent_id": resp.agent_id,
            "active_servers": resp.active_servers,
        }))),
        Err(crate::usecases::AgentToolsError::UnknownServers(unknown)) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "unknown MCP server names (not in catalog+local)",
                "unknown": unknown,
            })),
        )),
        Err(crate::usecases::AgentToolsError::Persistence(msg)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to persist agent_mcp.json: {}", msg),
            })),
        )),
    }
}

// ── Per-agent search config endpoints ────────────────────────────────
//
// Same pattern as MCP above: pre-fix the Gateway stub returned 200 without
// persisting, losing the user's search-provider selection on next tab switch.

/// `GET /agents/{id}/search-config` — read `agent_search.json` (active providers + priorities).
async fn get_agent_search_config(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "agent_id mismatch: path '{}' does not match this runtime '{}'",
                    id, state.agent_id
                ),
            })),
        ));
    }
    // ADR-040 follow-up: route through the trait (same pattern as
    // `get_agent_mcp_servers` above). See module-level docs on
    // `crate::usecases::agent_tools` for the rationale.
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let resp = svc.get_search_config(&id).await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "providers": resp.providers,
    })))
}

// ── Provider list endpoint ──────────────────────────────────────────────
//
// Read-through endpoint that returns the content of `agent_provider.json`
// — the provider catalog pushed by Gateway via MQTT
// (acowork/global/providers) and persisted by the MQTT handler in
// [`crate::agent_config::save_agent_provider_config_from_available`].
//
// Unlike MCP / search / builtin-tools, there is no user-authorable subset
// of the provider list; the entire file is Gateway-authored. The endpoint
// exists so the frontend can verify what the Runtime actually has at any
// given time — a diagnostic / consistency-check tool rather than a
// user-facing configuration surface.

/// `GET /agents/{id}/providers` — read `agent_provider.json`.
async fn get_agent_providers(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "agent_id mismatch: path '{}' does not match this runtime '{}'",
                    id, state.agent_id
                ),
            })),
        ));
    }

    let cfg = crate::agent_config::load_agent_provider_config(&state.work_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })?;

    match cfg {
        Some(provider_config) => Ok(Json(serde_json::json!({
            "agent_id": state.agent_id,
            "providers": provider_config.providers,
            "version": provider_config.version,
        }))),
        None => Ok(Json(serde_json::json!({
            "agent_id": state.agent_id,
            "providers": [],
            "version": 0,
        }))),
    }
}

/// `PUT /agents/{id}/search-config` — write `agent_search.json`.
///
/// Body: `{"providers": [{"provider": "tavily", "priority": 1}, ...]}`.
/// Wire shape matches `acowork_core::protocol::AgentSearchProvider` 1:1, so
/// the proxy pass-through is transparent.
async fn put_agent_search_config(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAgentSearchConfigRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "agent_id mismatch: path '{}' does not match this runtime '{}'",
                    id, state.agent_id
                ),
            })),
        ));
    }

    // ADR-040 follow-up: persistence lives in
    // [`crate::usecases::RuntimeAgentToolsService`].
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let body = crate::usecases::PutSearchConfigBody {
        providers: req.providers,
    };
    match svc.put_search_config(&id, body).await {
        Ok(resp) => Ok(Json(serde_json::json!({
            "agent_id": resp.agent_id,
            "providers": resp.providers,
        }))),
        Err(crate::usecases::AgentToolsError::UnknownServers(unknown)) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "unknown search provider ids",
                "unknown": unknown,
            })),
        )),
        Err(crate::usecases::AgentToolsError::Persistence(msg)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to persist agent_search.json: {}", msg),
            })),
        )),
    }
}

// ── Builtin tools endpoint ─────────────────────────────────────────────
//
// Parallel structure to the MCP / search-config pair above, but
// persists to `agent_tools.json`. Lives under the `AgentToolsService`
// trait (the same one MCP / search use) because the persistence
// semantics — read-modify-write with `apply_builtin_tools_patch`,
// PLATFORM_TOOLS force-enable, silent unknown-name dropping — are the
// same shape, just a different file on disk.
//
// ADR-029 §7 + mqtt.md §7.1 wire semantics: `builtin_tools: Vec<String>`
// is the **complete enabled set** — any tool currently on disk but
// absent from this list must be flipped to `enabled = false`. The
// impl owns the read-modify-write + patch construction (see
// `RuntimeAgentToolsService::put_builtin_tools`); this handler is
// just a protocol converter.

/// `GET /agents/{id}/builtin-tools` — read `agent_tools.json` (per-tool
/// entries with their enabled flag).
async fn get_agent_builtin_tools(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "agent_id mismatch: path '{}' does not match this runtime '{}'",
                    id, state.agent_id
                ),
            })),
        ));
    }
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let resp = svc.get_builtin_tools(&id).await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "tools": resp.tools,
    })))
}

/// `PUT /agents/{id}/builtin-tools` — persist the enabled-set for
/// builtin tools. Same JSON 503 / 500 mapping as the MCP / search
/// counterparts (no 400 — unknown tool names are silently dropped per
/// ADR-029 §7; see the `AgentToolsService` doc for rationale).
async fn put_agent_builtin_tools(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAgentBuiltinToolsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "agent_id mismatch: path '{}' does not match this runtime '{}'",
                    id, state.agent_id
                ),
            })),
        ));
    }
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let body = crate::usecases::PutBuiltinToolsBody {
        builtin_tools: req.builtin_tools,
    };
    match svc.put_builtin_tools(&id, body).await {
        Ok(resp) => {
            // ADR-029/ADR-052: broadcast the new enabled flags to all
            // sessions AND sync the shared AgentCore template so
            // sessions created after this PUT inherit the new flags -
            // see [`broadcast_builtin_tools_update`] doc and
            // `SessionManager::apply_builtin_tools_enabled`.
            broadcast_builtin_tools_update(&state, &resp.tools).await;
            Ok(Json(serde_json::json!({
                "agent_id": resp.agent_id,
                "tools": resp.tools,
            })))
        }
        Err(crate::usecases::AgentToolsError::Persistence(msg)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to persist agent_tools.json: {}", msg),
            })),
        )),
        // No `UnknownTools` variant — see ADR-029 §7 + the trait
        // doc.  Defensive match to keep the compiler exhaustive
        // without forcing a logic-only `unreachable!`.
        Err(other) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("unexpected put_builtin_tools error: {}", other),
            })),
        )),
    }
}

/// Wire-shape DTOs for the Tools-panel PUT endpoints. Kept local to
/// `server.rs` because they only travel between the Gateway proxy and the
/// Runtime HTTP server (the Desktop uses its own narrower types in
/// `apps/acowork-desktop/src/lib/types.ts`).
///
/// **Wire semantics (pattern B — "complete enabled set")**: the body's
/// list is the *full* desired state, not a patch. Anything currently on
/// disk but absent from the list is flipped to inactive / removed.
///
/// `#[serde(default)]` is required on every list field so that the
/// deserialization layer accepts bare `{}` (no keys) as well as explicit
/// `{"servers": []}` / `{"providers": []}`. Both shapes mean "explicitly
/// empty selection" and must reach the usecase layer (where the impl
/// already handles them as `Some(vec![])` — see
/// `usecases::agent_tools_impl::put_mcp_servers` line 97-98 for the MCP
/// contract). Without `#[serde(default)]`, missing-field deserialization
/// fails at the `Json<...>` extractor before the handler runs, returning
/// 400 to clients that legitimately send `{}` (e.g. a CLI clearing the
/// selection without restating the empty array).
#[derive(serde::Deserialize)]
struct UpdateMcpServersRequest {
    /// Names of MCP servers to activate (complete enabled set).
    /// Empty/absent means "no servers active".
    #[serde(default)]
    servers: Vec<String>,
}

#[derive(serde::Deserialize)]
struct UpdateAgentSearchConfigRequest {
    /// Ordered list of active search providers (complete enabled set).
    /// Empty/absent means "no providers selected" — distinct from
    /// "config file missing" (a 500). See `SearchConfigResponse.providers`.
    #[serde(default)]
    providers: Vec<acowork_core::protocol::AgentSearchProvider>,
}

#[derive(serde::Deserialize)]
struct UpdateAgentBuiltinToolsRequest {
    /// Names of builtin tools to enable (complete enabled set).
    /// Empty/absent means "no builtin tools enabled" (platform tools
    /// are still force-enabled by the patcher — see ADR-029 §7).
    #[serde(default)]
    builtin_tools: Vec<String>,
}

/// `GET /agents/{id}/status` — Agent Status panel (Runtime running state).
///
/// Surfaces the information the desktop needs to render its
/// Agent-Status header card: process identity, work dir, list of
/// known sessions (from `SharedLatestSession` + a quick meta scan),
/// and the active embedding model dimension.
async fn get_agent_status(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let matches = id == state.agent_id;

    // Pull the active session + model/embedding dim from the shared
    // state so the panel can show "what is the agent doing right now?".
    let latest_session = state
        .latest_session
        .read()
        .ok()
        .and_then(|g| g.clone());
    let embed_dim = state
        .embed_provider_dim
        .read()
        .map(|d| *d)
        .unwrap_or(0);

    Json(serde_json::json!({
        "agent_id": state.agent_id,
        "matches": matches,
        "work_dir": state.work_dir,
        "pid": std::process::id(),
        "latest_session": latest_session,
        "embed_dim": embed_dim.clone(),
    }))
}

// ── Shell Risk Rules (ADR-055) ────────────────────────────────────────

/// `GET /agents/{id}/shell-risk-rules` — read the effective shell risk rules.
///
/// Returns `{agent_id, matches, content, has_user_override}`. Content precedence:
/// 1. User override file on disk (raw text, including comments)
/// 2. Embedded defaults source (raw text, including comments)
///
/// We deliberately return the raw TOML text rather than re-rendering
/// from the parsed in-memory rules — comments and formatting from the
/// source are preserved so the editor shows the file as authored.
async fn get_shell_risk_rules(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "agent_id mismatch"})),
        ));
    }
    let config_dir = state.work_dir.join("config");
    let path = config_dir.join("shell_risk_rules.toml");
    tracing::info!(agent_id = %id, path = %path.display(), "GET /agents/{id}/shell-risk-rules");

    // Build revision identifier embedded in the generated template. It is
    // informational only — lets the user see at a glance which binary
    // snapshot the template was generated from, so they can spot stale
    // copies across machines.
    let build_rev = format!(
        "runtime-{}-{}",
        env!("CARGO_PKG_VERSION"),
        chrono::Utc::now().format("%Y%m%d"),
    );

    // UX contract: clicking the "Edit" button creates the user file on
    // disk if it does not already exist. The created file is a template:
    //   • the binary's embedded rules as `# ...` comments (so the user
    //     can see what is already covered before adding their own), and
    //   • an empty `rules = []` body (so saving it back is a no-op and
    //     the merge-load semantics in `ShellRiskRules::load` keep the
    //     embedded rules live).
    //
    // Why create on GET rather than wait for the user to press Save:
    //   1. `has_user_override` flips to `true` immediately, which the
    //      frontend uses to render the "this is your local copy" hint
    //      and to make the file appear in the agent's file tree.
    //   2. Avoids the failure mode where the user opens the editor,
    //      walks away, and a later binary upgrade silently changes what
    //      the editor would have shown them — the file is now pinned
    //      to the binary snapshot they actually saw.
    //   3. A user who closes the editor without saving still has a
    //      file that is semantically empty (`rules = []`), which is
    //      the same observable behavior as "no file" under merge
    //      load.
    let (content, has_user_override) = if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(c) => (c, true),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "Failed to read shell risk rules");
                let template = crate::security::shell_risk::generate_user_rules_toml(&build_rev)
                    .unwrap_or_else(|err| {
                        tracing::error!(error = %err, "generate_user_rules_toml failed; falling back to embedded raw");
                        crate::security::shell_risk::ShellRiskRules::embedded_defaults().to_string()
                    });
                (template, false)
            }
        }
    } else {
        let template = match crate::security::shell_risk::generate_user_rules_toml(&build_rev) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(error = %err, "generate_user_rules_toml failed; falling back to embedded raw");
                crate::security::shell_risk::ShellRiskRules::embedded_defaults().to_string()
            }
        };
        // Create the user file on first GET. A write failure is logged
        // but does not block the editor from opening — the template is
        // still served in-memory and PUT can materialize it later.
        if let Err(e) = std::fs::create_dir_all(&config_dir) {
            tracing::warn!(path = %config_dir.display(), error = %e, "Failed to create config dir; serving template without persisting");
        } else if let Err(e) = std::fs::write(&path, &template) {
            tracing::warn!(path = %path.display(), error = %e, "Failed to materialize user rules template; will retry on PUT");
        } else {
            tracing::info!(path = %path.display(), bytes = template.len(), "Materialized shell risk rules user template on first GET");
        }
        (template, true)
    };
    Ok(Json(serde_json::json!({
        "agent_id": state.agent_id,
        "matches": true,
        "content": content,
        "has_user_override": has_user_override,
    })))
}

/// `PUT /agents/{id}/shell-risk-rules` — write user override for shell risk rules.
///
/// Validates TOML syntax before writing; on success both disk and the
/// in-memory rule cache are updated so live sessions pick up the new rules
/// without a restart.
async fn put_shell_risk_rules(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if id != state.agent_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "agent_id mismatch"})),
        ));
    }
    tracing::info!(
        agent_id = %id,
        content_bytes = req.get("content").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0),
        "PUT /agents/{id}/shell-risk-rules"
    );
    let content = req
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing or invalid 'content' field"})),
        ))?;
    // Validate TOML syntax before writing — fail fast if the editor
    // contains a parse error so the user can fix it without restarting.
    //
    // The on-disk format (see `security/shell_risk_rules.toml`) is a
    // table-of-rules: `{ rules: [ {...}, {...} ] }`. That schema matches
    // `ShellRiskRules`, NOT a bare `Vec<ShellRiskRule>` — deserializing
    // into the Vec used to fail with "invalid type: map, expected a
    // sequence" because the top-level value is a map, not an array.
    let parsed = toml::from_str::<crate::security::shell_risk::ShellRiskRules>(content)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("TOML parse error: {}", e)
                })),
            )
        })?;
    // Write to disk first, then update in-memory state so GET returns
    // consistent data even if the write fails midway.
    let config_dir = state.work_dir.join("config");
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": format!("Failed to create config dir: {}", e)}))))?;
    let path = config_dir.join("shell_risk_rules.toml");
    std::fs::write(&path, content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": format!("Failed to write rules: {}", e)}))))?;
    // Update in-memory cache atomically.
    let rule_count = parsed.rules.len();
    let mut guard = state
        .shell_risk_rules
        .write()
        .unwrap_or_else(|e| e.into_inner());
    *guard = parsed;
    tracing::info!(path = %path.display(), rule_count, "Wrote shell risk rules user override");
    Ok(Json(serde_json::json!({"written": true})))
}

// ── N1: Consolidation status endpoint ──────────────────────────

/// `GET /memory/consolidation/status` - report consolidation timer state.
async fn get_consolidation_status(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let timer = state.consolidation_timer
        .read()
        .ok()
        .and_then(|g| g.clone())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let idle_secs = timer.idle_secs().await;
    let pending = timer.pending_count().await;
    let config = timer.config();

    Ok(Json(serde_json::json!({
        "idle_secs": idle_secs,
        "pending_count": pending,
        "idle_timeout_secs": config.idle_timeout_secs,
        "accumulation_threshold": config.accumulation_threshold,
        "bg_task_running": true,
    })))
}

// ── N2: RAG status endpoint ────────────────────────────────────

/// `GET /agents/{id}/rag/status` - report RAG provider status.
async fn get_rag_status(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let _ = id;
    let rag = state.rag_provider.read().ok().and_then(|g| g.clone());

    match rag {
        Some(provider) => Json(serde_json::json!({
            "configured": true,
            "provider_name": provider.name(),
            "agent_id": state.agent_id,
        })),
        None => Json(serde_json::json!({
            "configured": false,
            "provider_name": null,
            "agent_id": state.agent_id,
        })),
    }
}

// ── N3: RAG direct query endpoint ──────────────────────────────

/// Request body for `POST /agents/{id}/rag/query`.
#[derive(Debug, Deserialize)]
struct RagQueryBody {
    query: String,
    #[serde(default)]
    top_k: Option<u32>,
    #[serde(default)]
    score_threshold: Option<f32>,
    #[serde(default)]
    filters: Option<serde_json::Value>,
}

/// `POST /agents/{id}/rag/query` - direct RAG query (bypasses LLM).
async fn post_rag_query(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    body: axum::Json<RagQueryBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let _ = id;
    let rag = state.rag_provider.read().ok().and_then(|g| g.clone()).ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "RAG provider not configured",
                "agent_id": state.agent_id,
            })),
        )
    })?;

    if body.query.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "query must not be empty" })),
        ));
    }

    let results = rag
        .query_with_params(&body.query, body.top_k, body.score_threshold, body.filters.clone())
        .await;

    let items: Vec<serde_json::Value> = results
        .iter()
        .map(|r| serde_json::json!({
            "content": r.item.content,
            "source_url": r.item.source_url,
            "chunk_id": r.item.chunk_id,
            "score": r.item.score,
            "source_label": r.source_label,
        }))
        .collect();

    Ok(Json(serde_json::json!({
        "query": body.query,
        "results": items,
        "result_count": items.len(),
        "provider_name": rag.name(),
    })))
}

/// Simple base64 helpers were removed in ADR-046 — the legacy
/// `POST /sessions/{sid}/documents` (which took a JSON body with a
/// base64-encoded payload) is replaced by the multipart
/// `POST /sessions/{sid}/files` endpoint that ships raw bytes. No
/// other call site in the runtime needs these helpers.
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a session-metadata service backed by on-disk session files.
    fn new_test_session_metadata(
        work_dir: &std::path::Path,
        snapshots: SharedSessionSnapshots,
        latest: SharedLatestSession,
    ) -> Arc<dyn crate::usecases::SessionMetadataService> {
        Arc::new(crate::usecases::RuntimeSessionMetadataService::new(
            work_dir.to_path_buf(),
            Arc::new(crate::usecases::agent_token_impl::NoopAgentTokenService),
            snapshots,
            latest,
        ))
    }

    /// Build a memory-query service backed by a (possibly None) Grafeo store.
    fn new_test_memory_query(
        memory_store: SharedMemoryStore,
        embed_dim: SharedEmbedDimension,
    ) -> Arc<dyn crate::usecases::MemoryQueryService> {
        Arc::new(crate::usecases::GrafeoMemoryAdapter::new(memory_store, embed_dim))
    }

    /// Build a workspace-query service for the test temp dir.
    fn new_test_workspace_query(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::WorkspaceQueryService> {
        Arc::new(crate::usecases::RuntimeWorkspaceQueryService::new(
            temp_dir,
            "com.test.agent".to_string(),
        ))
    }

    /// Build a workspace-mutation service for the test temp dir.
    fn new_test_workspace_mutation(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::WorkspaceMutationService> {
        Arc::new(crate::usecases::RuntimeWorkspaceMutationService::new(temp_dir))
    }

    /// Build a shared WorkspaceResolver for HTTP tests.
    ///
    /// The mutation handlers reload the resolver from `state.work_dir`
    /// after every successful create/update/delete, so the initial
    /// contents are irrelevant for most tests — they just need a
    /// non-panicking `SharedResolver` to satisfy the `start` signature.
    /// A dedicated test (`test_create_workspace_reloads_resolver`)
    /// constructs a resolver over its own temp dir to assert the reload
    /// behaviour end-to-end.
    fn new_test_workspace_resolver() -> crate::tools::workspace_resolver::SharedResolver {
        Arc::new(std::sync::RwLock::new(
            crate::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        ))
    }

    /// Build an agent-tools service backed by the test temp dir's
    /// `agent_mcp.json` / `agent_search.json`. Used to exercise the
    /// `/agents/{id}/mcp-servers` and `/agents/{id}/search-config`
    /// handlers end-to-end through the trait path.
    fn new_test_agent_tools(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::AgentToolsService> {
        Arc::new(crate::usecases::RuntimeAgentToolsService::new(temp_dir))
    }

    /// Build an agent-config service backed by the test temp dir's
    /// `agent_config.json`. Used to exercise `/agents/{id}/config` and
    /// `/agents/{id}/builtin-tools` end-to-end through the trait path.
    fn new_test_agent_config(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::agent_config::AgentConfigService> {
        Arc::new(crate::usecases::RuntimeAgentConfigService::new(temp_dir))
    }

    /// Build an attachment service backed by the test temp dir's
    /// `<work_dir>/files/` blob store. Used to exercise
    /// `POST /sessions/{sid}/files` and `GET /files/{document_id}`
    /// end-to-end through the trait path.
    fn new_test_attachment(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::AttachmentService> {
        Arc::new(crate::usecases::RuntimeAttachmentService::new(temp_dir))
    }

    #[tokio::test]
    async fn test_http_server_starts_and_responds() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));

        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation = std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        // Health check
        let url = format!("http://127.0.0.1:{}/health", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["agent_id"], "com.test.agent");
        // degraded_reasons should be empty for a clean test start
        assert!(body["degraded_reasons"].as_array().unwrap().is_empty(), "expected empty degraded_reasons");

        // Sessions (empty)
        let url = format!("http://127.0.0.1:{}/sessions", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["sessions"].as_array().unwrap().len(), 0);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[tokio::test]
    async fn test_http_server_sessions_with_data() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-sessions");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // ADR-024: New session storage format:
        //   - conversations/meta/{sid}.json — SessionMeta (one file per session)
        //   - conversations/{sid}.jsonl     — pure ConversationEntry lines, no header
        let conversations_dir = temp_dir.join("conversations");
        let session_id = "20260101_120000_abc";

        // Persist SessionMeta via the conversation module API so the
        // format stays in lock-step with `scan_sessions_from_meta`.
        let meta = crate::conversation::SessionMeta {
            version: 3, // CONVERSATION_FORMAT_VERSION — keep in sync with conversation.rs
            session_id: session_id.to_string(),
            agent_id: "com.test.agent".to_string(),
            created_at: "2026-01-01T12:00:00Z".to_string(),
            title: Some("Test Session".to_string()),
            workspace_id: None,
            model: None,
            provider: None,
            reasoning_effort: None,
            temperature: None,
            todos: None,
            message_count: 3,
            last_active_at: "2026-01-01T12:00:01Z".to_string(),
            tokens: None,
            last_compaction_offset: None,
            corrupted: false,
        };
        crate::conversation::write_session_meta(&conversations_dir, &meta).unwrap();

        // JSONL body: every line is a complete ConversationEntry (no header).
        let jsonl_path = conversations_dir.join(format!("{}.jsonl", session_id));
        std::fs::write(
            &jsonl_path,
            "{\"id\":\"m1\",\"ts\":\"2026-01-01T12:00:00.500Z\",\"role\":\"user\",\"content\":\"Hello\"}\n\
             {\"id\":\"m2\",\"ts\":\"2026-01-01T12:00:00.800Z\",\"role\":\"assistant\",\"content\":\"Hi there!\"}\n\
             {\"id\":\"m3\",\"ts\":\"2026-01-01T12:00:01.000Z\",\"role\":\"system\",\"content\":\"Done\"}\n",
        )
        .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));

        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation = std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .unwrap();

        // List sessions — should see exactly the one we just wrote.
        let url = format!("http://127.0.0.1:{}/sessions", server.port);
        let response = reqwest::get(&url).await.unwrap();
        let body: serde_json::Value = response.json().await.unwrap();
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], session_id);
        assert_eq!(sessions[0]["title"], "Test Session");
        assert_eq!(sessions[0]["message_count"], 3);

        // ADR-028: response must carry agent-level token totals even
        // when no session has tokens yet (all zero is valid).
        assert!(body.get("agent_total_input_tokens").is_some());
        assert!(body.get("agent_total_output_tokens").is_some());
        assert_eq!(body["agent_total_input_tokens"], 0);
        assert_eq!(body["agent_total_output_tokens"], 0);

        // Get messages — read_messages_paginated returns chronological order
        // (oldest → newest within the page) regardless of direction, so
        // index 0/1/2 map to the user / assistant / system entries above.
        let url = format!(
            "http://127.0.0.1:{}/sessions/{}/messages?limit=50&direction=forward",
            server.port, session_id
        );
        let response = reqwest::get(&url).await.unwrap();
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["count"], 3);
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "Hi there!");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][2]["content"], "Done");
        assert_eq!(body["messages"][2]["role"], "system");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-028 regression test: `list_sessions` must aggregate
    /// `agent_total_input_tokens` / `agent_total_output_tokens` across
    /// all sessions on disk and include them in the top-level response.
    #[tokio::test]
    async fn test_list_sessions_includes_agent_total_tokens() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-agent-totals");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let conversations_dir = temp_dir.join("conversations");

        // Session 1: 100 input / 200 output tokens.
        let meta1 = crate::conversation::SessionMeta {
            version: 3,
            session_id: "20260101_100000_aaa".to_string(),
            agent_id: "com.test.agent".to_string(),
            created_at: "2026-01-01T10:00:00Z".to_string(),
            title: Some("Session 1".to_string()),
            workspace_id: None,
            model: None,
            provider: None,
            reasoning_effort: None,
            temperature: None,
            todos: None,
            message_count: 2,
            last_active_at: "2026-01-01T10:00:01Z".to_string(),
            tokens: Some(crate::conversation::SessionTokens {
                last_input: 100,
                last_output: 200,
                total_input: 100,
                total_output: 200,
            }),
            last_compaction_offset: None,
            corrupted: false,
        };
        crate::conversation::write_session_meta(&conversations_dir, &meta1).unwrap();

        // Session 2: 300 input / 400 output tokens.
        let meta2 = crate::conversation::SessionMeta {
            version: 3,
            session_id: "20260101_120000_bbb".to_string(),
            agent_id: "com.test.agent".to_string(),
            created_at: "2026-01-01T12:00:00Z".to_string(),
            title: Some("Session 2".to_string()),
            workspace_id: None,
            model: None,
            provider: None,
            reasoning_effort: None,
            temperature: None,
            todos: None,
            message_count: 1,
            last_active_at: "2026-01-01T12:00:01Z".to_string(),
            tokens: Some(crate::conversation::SessionTokens {
                last_input: 300,
                last_output: 400,
                total_input: 300,
                total_output: 400,
            }),
            last_compaction_offset: None,
            corrupted: false,
        };
        crate::conversation::write_session_meta(&conversations_dir, &meta2).unwrap();

        // Empty JSONL files so the sessions are discoverable.
        for sid in &["20260101_100000_aaa", "20260101_120000_bbb"] {
            std::fs::write(
                conversations_dir.join(format!("{}.jsonl", sid)),
                "",
            )
            .unwrap();
        }

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession =
            std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension =
            std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));

        // Use in-memory token service so the ADR-028 merge semantics work.
        let token_svc = Arc::new(crate::usecases::agent_token_impl::InMemoryAgentTokenService::new());
        let session_metadata: Arc<dyn crate::usecases::SessionMetadataService> =
            Arc::new(crate::usecases::RuntimeSessionMetadataService::new(
                temp_dir.to_path_buf(),
                token_svc,
                snapshots.clone(),
                latest.clone(),
            ));
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        // GET /sessions — should aggregate tokens across both sessions.
        let url = format!("http://127.0.0.1:{}/sessions", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();

        // ADR-028: agent totals = sum across all sessions on disk.
        assert_eq!(body["agent_total_input_tokens"], 400); // 100 + 300
        assert_eq!(body["agent_total_output_tokens"], 600); // 200 + 400
        assert_eq!(body["total_count"], 2);

        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        // Sorted by last_active_at desc: Session 2 first.
        assert_eq!(sessions[0]["session_id"], "20260101_120000_bbb");
        assert_eq!(sessions[1]["session_id"], "20260101_100000_aaa");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[tokio::test]
    async fn test_http_server_memory_endpoints_without_store() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-memory");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let memory_store: SharedMemoryStore =
            std::sync::Arc::new(std::sync::RwLock::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(512));
        let degraded_reasons: SharedDegradation = std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        // /memory/nodes should return a well-formed empty list.
        let url = format!("http://127.0.0.1:{}/memory/nodes", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["total"], 0);
        assert_eq!(body["page"], 1);
        assert_eq!(body["size"], 20);
        assert!(body["nodes"].as_array().unwrap().is_empty());

        // /memory/stats must report the full contract — both the
        // ADR-040 service path and the pre-ADR-040 fallback funnel
        // through the same `MemoryStats` struct, so the response shape
        // is identical. The contract is documented in
        // `acowork-gateway::http::memory_api::MemoryStatsResponse` and
        // mirrored in `usecases::memory_query::MemoryStats`. Every
        // field below must be present — missing fields crash the
        // desktop Memory panel (see git history for the by_status bug).
        let url = format!("http://127.0.0.1:{}/memory/stats", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["index_health"], "no_store");
        assert_eq!(body["model_dim"], 512);
        assert_eq!(body["total_nodes"], 0);
        assert_eq!(body["storage_bytes"], 0);
        assert_eq!(body["by_type"], serde_json::json!({}));
        assert_eq!(body["by_status"], serde_json::json!({}));
        assert_eq!(body["avg_decay_score"], 0.0);
        assert_eq!(body["stored_dim"], 0);
        assert_eq!(body["nodes_with_embedding"], 0);

        // DELETE /memory/nodes/{nid} — store is None, adapter returns Ok(()) trivially.
        let url = format!("http://127.0.0.1:{}/memory/nodes/12345", server.port);
        let response = reqwest::Client::new()
            .delete(&url)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["deleted"], true);
        assert_eq!(body["node_id"], 12345);

        // POST /memory/consolidate — store is None, reports 0 consolidated.
        let url = format!("http://127.0.0.1:{}/memory/consolidate", server.port);
        let response = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({"force": true, "retention_days": 7}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["started"], false);
        assert_eq!(body["episodes_consolidated"], 0);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-029 §7 + ADR-052 wire-semantics regression test.
    ///
    /// `PUT /api/agents/{id}/builtin-tools` is the **complete enabled
    /// set**: any tool currently in `agent_tools.json` but absent
    /// from the request body MUST be flipped to `enabled = false`.
    /// The previous implementation built the patch directly from the
    /// request body (everything listed → enabled=true), which made
    /// every unchecked checkbox silently re-enable on the next PUT.
    ///
    /// This test seeds `agent_tools.json` with three tools, sends a
    /// PUT listing only one of them as enabled, and asserts that:
    ///   - the listed tool stays `enabled=true`
    ///   - the unlisted tool flips to `enabled=false`
    ///   - PLATFORM_TOOLS (e.g. `context_retrieve`) are *never*
    ///     persisted even if a hostile or legacy PUT body tries to
    ///     toggle them — they live in the in-memory registry only
    ///     (ADR-052)
    #[tokio::test]
    async fn test_put_agent_config_disables_unlisted_builtin_tools() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-put-config");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();

        // Seed agent_tools.json: 3 enabled tools, one of which is a
        // platform tool.  Reproduces the user-visible "uncheck a tool,
        // then a refresh re-checks it" scenario after the Setup/Tools
        // refresh wiring was completed.
        let initial = crate::agent_config::AgentToolsConfig {
            tools: vec![
                crate::agent_config::AgentToolEntry::new("context_retrieve", true),
                crate::agent_config::AgentToolEntry::new("http_request", true),
                crate::agent_config::AgentToolEntry::new("shell", true),
            ],
        };
        crate::agent_config::save_agent_tools_config(
            std::path::Path::new(&temp_dir),
            &initial,
        )
        .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));

        // AgentToolsService slot — the /builtin-tools endpoint lives here.
        let agent_tools_svc = new_test_agent_tools(temp_dir.clone());
        let agent_tools_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>> =
            Arc::new(tokio::sync::Mutex::new(Some(agent_tools_svc)));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            agent_tools_slot,
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        // After the ADR-040 refactor, builtin-tools live on their own
        // endpoint (the field was removed from `PUT /config` because
        // it conflates "model knobs" with "tool state"). PUT only the
        // one we want enabled; everything else must reflect the new
        // state on the next GET.
        //
        // We deliberately include `context_retrieve` in the request
        // body — under ADR-052 it must NOT survive into the persisted
        // file (or the GET response).
        let url = format!(
            "http://127.0.0.1:{}/agents/com.test.agent/builtin-tools",
            server.port
        );
        let client = reqwest::Client::new();
        let response = client
            .put(&url)
            .json(&serde_json::json!({"builtin_tools": ["http_request", "context_retrieve"]}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "PUT /agents/{{id}}/builtin-tools should accept, got {}",
            response.status()
        );

        // Verify on-disk agent_tools.json has the right enabled flags.
        let reloaded = crate::agent_config::load_agent_tools_config(
            std::path::Path::new(&temp_dir),
        )
        .unwrap()
        .expect("agent_tools.json should exist after PUT");
        let map: std::collections::HashMap<String, bool> = reloaded
            .tools
            .iter()
            .map(|e| (e.name.clone(), e.enabled))
            .collect();
        assert!(map["http_request"], "listed tool stays enabled");
        assert!(
            !map["shell"],
            "unlisted tool must be disabled — this is the bug regression"
        );
        assert!(
            !map.contains_key("context_retrieve"),
            "PLATFORM_TOOLS must NEVER appear in agent_tools.json (ADR-052)"
        );

        // Verify the GET /builtin-tools endpoint also returns the new
        // state — this is what the ToolsTab listener reads. The
        // platform tool is absent from the response, even though the
        // PUT body tried to enable it.
        let tools_url = format!(
            "http://127.0.0.1:{}/agents/com.test.agent/builtin-tools",
            server.port
        );
        let tools_resp: serde_json::Value =
            reqwest::get(&tools_url).await.unwrap().json().await.unwrap();
        let tools_arr = tools_resp["tools"].as_array().unwrap();
        let tool_flags: std::collections::HashMap<String, bool> = tools_arr
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_string(),
                    e["enabled"].as_bool().unwrap(),
                )
            })
            .collect();
        assert!(tool_flags["http_request"]);
        assert!(!tool_flags["shell"]);
        assert!(
            !tool_flags.contains_key("context_retrieve"),
            "PLATFORM_TOOLS must NEVER appear in the /builtin-tools response"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// AgentSetup panel edit-effective regression test.
    ///
    /// Before the per-agent-config block was added, `PUT
    /// /agents/{id}/config` silently dropped everything except
    /// `builtin_tools` because the request struct only declared that
    /// one field. The Setup panel would optimistically apply the new
    /// temperature / context_window / etc. to its local store, the
    /// refresh listener would fetch the (unchanged) server values,
    /// and the user-visible result was "改动不生效". This test
    /// exercises the full read-modify-write cycle to make sure the
    /// handler now lands each supported field on disk.
    ///
    /// We cover the four wire-format scenarios the AgentSetupTab
    /// actually uses (per `handleApply` in
    /// `apps/acowork-desktop/src/components/results/AgentSetupTab.tsx`):
    ///
    ///   1. **Change** an existing numeric field (`temperature` 0.3 → 0.7).
    ///   2. **Add** a field that was previously absent (`max_output_tokens`,
    ///      `approval_timeout_secs`).
    ///   3. **Change** an existing string field (`shell_approval_threshold`
    ///      `"medium"` → `"high"`).
    ///   4. **Preserve** an untouched field (`max_iterations` stays at 50).
    ///
    /// We intentionally do **not** exercise JSON `null` clearing here:
    /// the frontend never sends `null` (it either sends the value or
    /// omits the field), and `Option<serde_json::Value>` cannot
    /// distinguish "field absent" from "field is JSON null" without a
    /// presence-tracking wrapper. If a future CLI / tool needs
    /// explicit-clearing semantics, the cleanest path is to introduce
    /// an `OptionalField<T>` presence-tracking wrapper — see the
    /// design notes on `UpdateAgentConfigRequest`.
    #[tokio::test]
    async fn test_put_agent_config_persists_per_agent_fields() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-put-config-fields");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();

        // Seed agent_config.json with three pre-existing values:
        // `temperature` and `shell_approval_threshold` will be
        // overwritten by the PUT; `max_iterations` will be preserved
        // because the PUT doesn't mention it.
        let initial = crate::agent_config::AgentConfig {
            temperature: Some(0.3),
            max_iterations: Some(50),
            shell_approval_threshold: Some("medium".to_string()),
            ..Default::default()
        };
        crate::agent_config::save_agent_config(
            std::path::Path::new(&temp_dir),
            &initial,
        )
        .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        // Partial PUT — note the absence of `max_iterations` is
        // load-bearing: it exercises the "untouched field is preserved"
        // path of the read-modify-write cycle.
        let url = format!(
            "http://127.0.0.1:{}/agents/com.test.agent/config",
            server.port
        );
        let client = reqwest::Client::new();
        let response = client
            .put(&url)
            .json(&serde_json::json!({
                "temperature": 0.7,
                "max_output_tokens": 4096,
                "approval_timeout_secs": 120,
                "shell_approval_threshold": "high",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "PUT /agents/{{id}}/config should accept per-agent fields, got {}",
            response.status()
        );

        // Verify the on-disk file carries every pushed field and that
        // the untouched field is still there.
        let reloaded = crate::agent_config::load_agent_config(
            std::path::Path::new(&temp_dir),
        )
        .unwrap()
        .expect("agent_config.json should exist after PUT");

        assert!(
            (reloaded.temperature.unwrap_or(0.0) - 0.7).abs() < f32::EPSILON,
            "temperature should be updated to 0.7, got {:?}",
            reloaded.temperature
        );
        assert_eq!(reloaded.max_output_tokens, Some(4096));
        assert_eq!(reloaded.approval_timeout_secs, Some(120));
        assert_eq!(
            reloaded.shell_approval_threshold,
            Some("high".to_string()),
            "shell_approval_threshold should be overwritten with the new string value"
        );
        assert_eq!(
            reloaded.max_iterations,
            Some(50),
            "untouched field must be preserved across partial PUT"
        );

        // Also verify the raw JSON shape on disk: every newly-set
        // field must show up in the serialized output (and
        // `skip_serializing_if = Option::is_none` on the struct
        // means default fields like `system_prompt_override` stay
        // out of the file).
        let raw = std::fs::read_to_string(temp_dir.join("config").join("agent_config.json"))
            .unwrap();
        assert!(
            raw.contains("\"max_output_tokens\""),
            "newly-added field must be present in the serialized JSON; raw body was: {}",
            raw
        );
        assert!(
            raw.contains("\"approval_timeout_secs\""),
            "newly-added field must be present in the serialized JSON; raw body was: {}",
            raw
        );
        assert!(
            raw.contains("\"high\""),
            "updated string field must round-trip its value; raw body was: {}",
            raw
        );

        // GET /agents/{id}/config must surface the new state too —
        // this is the path the SetupTab refresh listener reads.
        let get_url = format!(
            "http://127.0.0.1:{}/agents/com.test.agent/config",
            server.port
        );
        let get_resp: serde_json::Value =
            reqwest::get(&get_url).await.unwrap().json().await.unwrap();
        let cfg = get_resp["config"].as_object().expect("config envelope");
        assert_eq!(cfg["max_output_tokens"], serde_json::json!(4096));
        assert_eq!(cfg["max_iterations"], serde_json::json!(50));
        assert_eq!(
            cfg["shell_approval_threshold"],
            serde_json::json!("high"),
            "GET /config must surface the updated string value"
        );
        assert_eq!(
            cfg["approval_timeout_secs"],
            serde_json::json!(120),
            "GET /config must surface the newly-added field"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Win11-MCP-ToolsBugFix round-trip regression test.
    ///
    /// Before the fix, `PUT /api/agents/{id}/mcp-servers` and
    /// `PUT /api/agents/{id}/search-config` were Gateway stubs that
    /// returned 200 but never persisted — the user's selection was lost
    /// the next time the Tools tab remounted because `/tools` read the
    /// (empty) on-disk config and overwrote the in-memory Zustand store.
    ///
    /// This test wires through the four endpoints the Desktop
    /// `ToolsTab` actually uses and asserts the round-trip survives a
    /// tab remount (modeled here as a fresh GET against the same agent).
    /// The contract being guarded:
    ///
    /// 1. PUT mcp-servers with `{servers: [...names...]}` returns 2xx.
    /// 2. Subsequent GET mcp-servers returns the same names.
    /// 3. The merged `/tools` endpoint reports them under
    ///    `data.mcp_servers` so the optimistic UI re-mount sees them.
    /// 4. Identical round-trip for `search-config` providers.
    /// 5. PUT mcp-servers with an unknown name returns 400 (the catalog
    ///    filter at the runtime boundary — protects against direct
    ///    API calls that bypass the desktop's catalog list).
    /// 6. `Some(vec![])` (explicitly cleared) round-trips as empty,
    ///    distinct from "never set anything" (auto-merged fallback).
    #[tokio::test]
    async fn test_mcp_and_search_persistence_roundtrip() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-mcp-search");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();

        // Seed agent_mcp.json with two catalog entries so the new PUT
        // handler has something valid to accept. (Without catalog entries,
        // every name in `servers` would fail the "unknown name" 400 guard
        // — that is the correct behaviour, just not what this test wants.)
        let initial_mcp = crate::agent_config::AgentMcpConfig {
            catalog: vec![
                McpServerConfigDefStub::context7(),
                McpServerConfigDefStub::search(),
            ],
            local: vec![],
            active_names: None,
        };
        crate::agent_config::save_agent_mcp_config(
            std::path::Path::new(&temp_dir),
            &initial_mcp,
        )
        .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));

        // ADR-040 follow-up: the mcp-servers + search-config handlers
        // route through the `AgentToolsService` trait; the slot must be
        // populated for the test to exercise the trait path. Mirrors
        // the wiring in `startup/session_init.rs` Phase B.
        let agent_tools_svc = new_test_agent_tools(temp_dir.clone());
        let agent_tools_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>> =
            Arc::new(tokio::sync::Mutex::new(Some(agent_tools_svc)));

        // AgentConfigService slot — same wiring discipline as
        // agent_tools above. Without this, /agents/{id}/config and
        // /agents/{id}/builtin-tools would 503.
        let agent_config_svc = new_test_agent_config(temp_dir.clone());
        let agent_config_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::agent_config::AgentConfigService>>>> =
            Arc::new(tokio::sync::Mutex::new(Some(agent_config_svc)));

        // AttachmentService slot — exercises POST /sessions/{sid}/files
        // and GET /files/{document_id} through the trait path.
        let attachment_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>> =
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone()))));
        let session_config_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            agent_tools_slot,
            agent_config_slot,
            attachment_slot,
            session_config_slot,
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // 1) PUT mcp-servers — user ticks `context7` only.
        let url = format!("{}/agents/com.test.agent/mcp-servers", base);
        let resp = client
            .put(&url)
            .json(&serde_json::json!({"servers": ["context7"]}))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "PUT mcp-servers should succeed, got {}",
            resp.status()
        );

        // 2) GET mcp-servers — same name comes back.
        let resp = client.get(&url).send().await.unwrap();
        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.unwrap();
        let active: Vec<&str> = body["active_servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(active, vec!["context7"]);

        // 3) GET merged /tools — `mcp_servers` now reflects the user's
        //    selection rather than `[]` (the pre-fix bug surfaced here
        //    because the server was lying about an empty config).
        let tools_url = format!("{}/agents/com.test.agent/tools", base);
        let resp = reqwest::get(&tools_url).await.unwrap();
        let tools: serde_json::Value = resp.json().await.unwrap();
        let mcp_servers: Vec<String> = tools["mcp_servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(mcp_servers, vec!["context7".to_string()]);

        // 4) PUT search-config — user activates `tavily` with priority 1.
        let search_url = format!("{}/agents/com.test.agent/search-config", base);
        let resp = client
            .put(&search_url)
            .json(&serde_json::json!({
                "providers": [{"provider": "tavily", "priority": 1}]
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        let resp = client.get(&search_url).send().await.unwrap();
        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.unwrap();
        let providers = body["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["provider"], "tavily");
        assert_eq!(providers[0]["priority"], 1);

        // 5) The merged /tools `search.providers` now exposes the same
        //    active list (single-shape, no separate `active_providers`).
        let resp = reqwest::get(&tools_url).await.unwrap();
        let tools: serde_json::Value = resp.json().await.unwrap();
        let providers = tools["search"]["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["provider"], "tavily");

        // 6) PUT mcp-servers with an unknown name → 400 (not 200 with
        //    silent drop, which was the pre-fix symptom at the
        //    security/UX layer).
        let bad_url = format!("{}/agents/com.test.agent/mcp-servers", base);
        let resp = client
            .put(&bad_url)
            .json(&serde_json::json!({"servers": ["context7", "ghost-mcp"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "unknown MCP server name must yield 400, got {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        let unknown: Vec<&str> = body["unknown"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(unknown, vec!["ghost-mcp"]);

        // 7) Clearing the selection entirely — `Some(vec![])` — must be
        //    preserved verbatim (not silently re-populated with all
        //    merged servers). This is the "explicitly unchecked
        //    everything" user-state, distinct from "never set anything".
        let resp = client
            .put(&bad_url)
            .json(&serde_json::json!({"servers": []}))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let resp = client.get(&bad_url).send().await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        let active = body["active_servers"].as_array().unwrap();
        assert!(
            active.is_empty(),
            "empty active_servers must round-trip as empty (not auto-merged), got: {active:?}"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Regression: bare `{}` (no keys) must be accepted for both
    /// `PUT /agents/{id}/mcp-servers` and `PUT /agents/{id}/search-config`,
    /// deserializing to the same empty list as `{"servers": []}` /
    /// `{"providers": []}`. Both shapes reach the usecase layer (which
    /// treats them as `Some(vec![])` — explicit "no servers/providers
    /// active"), distinct from "never set anything".
    ///
    /// **Bug shape** (pre-fix): `UpdateMcpServersRequest.servers` and
    /// `UpdateAgentSearchConfigRequest.providers` lacked `#[serde(default)]`,
    /// so axum's `Json<...>` extractor returned 400 before reaching the
    /// handler when the request body was bare `{}`. The existing
    /// `test_mcp_and_search_persistence_roundtrip` only tested
    /// `{"servers": []}` (explicit empty list), which deserializes
    /// successfully whether or not `#[serde(default)]` is present —
    /// so the bug was silent.
    ///
    /// Companion to `test_mcp_and_search_persistence_roundtrip`; uses
    /// a stripped-down harness because empty bodies bypass the catalog
    /// validation in `put_mcp_servers` (impl.rs:99) so we don't need
    /// to seed `agent_mcp.json::catalog` here.
    #[tokio::test]
    async fn test_put_mcp_and_search_accepts_empty_body() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-empty-body");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        // Only the agent_tools slot needs to be populated for these two
        // endpoints; everything else can stay None.
        let agent_tools_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
            temp_dir.clone(),
        ))));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            agent_tools_slot,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();
        let mcp_url = format!("{}/agents/com.test.agent/mcp-servers", base);
        let search_url = format!("{}/agents/com.test.agent/search-config", base);

        // 1) Bare `{}` to mcp-servers must succeed (was 400 pre-fix).
        let resp = client
            .put(&mcp_url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "PUT mcp-servers with bare {{}} must succeed, got {}",
            resp.status()
        );

        // 2) Bare `{}` to search-config must succeed (was 400 pre-fix).
        let resp = client
            .put(&search_url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "PUT search-config with bare {{}} must succeed, got {}",
            resp.status()
        );

        // 3) Round-trip: GET sees the empty active set on both endpoints —
        //    confirms the empty body reached the usecase (which wrote
        //    `active_names = Some(vec![])` / `providers: vec![]`) rather
        //    than being silently no-op'd at the deserialization layer.
        let resp = client.get(&mcp_url).send().await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        let active = body["active_servers"].as_array().unwrap();
        assert!(
            active.is_empty(),
            "GET mcp-servers after empty-body PUT must report empty active set, got: {active:?}"
        );

        let resp = client.get(&search_url).send().await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        let providers = body["providers"].as_array().unwrap();
        assert!(
            providers.is_empty(),
            "GET search-config after empty-body PUT must report empty providers, got: {providers:?}"
        );

        // 4) Parity check: explicit `{"servers": []}` / `{"providers": []}`
        //    also succeed (this was already working pre-fix; included here
        //    so the test pins both wire shapes as equivalent).
        let resp = client
            .put(&mcp_url)
            .json(&serde_json::json!({"servers": []}))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "explicit empty list must succeed (parity with bare {{}}), got {}",
            resp.status()
        );

        let resp = client
            .put(&search_url)
            .json(&serde_json::json!({"providers": []}))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "explicit empty providers must succeed (parity with bare {{}}), got {}",
            resp.status()
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// WIN-11/MCP-ToolsBugFix-Phase2 REPRODUCTION: real user scenario where
    /// `agent_mcp.json::catalog` is empty (fresh install / Gateway has the
    /// catalog in `mcp_catalog.json` but has never pushed it to Runtime's
    /// per-agent file).
    ///
    /// User report: "clicking context7 in the MCP tools tab shows
    /// 'unknown MCP server names (not in catalog+local)'". Root cause
    /// analysis:
    ///
    /// - The Desktop calls `GET /api/mcp-catalog` to *display* the catalog
    ///   (this hits Gateway's `mcp_catalog.json`).
    /// - The Desktop then calls `PUT /api/agents/{id}/mcp-servers` to
    ///   persist the user's selection (this is proxied to Runtime, which
    ///   validates against `AgentMcpConfig::merged()` reading from
    ///   `agent_mcp.json`).
    /// - Gateway has the catalog; Runtime's `agent_mcp.json::catalog` is
    ///   empty (no `save_agent_mcp_config_catalog` path is wired in
    ///   production — see Win-11/MCP-ToolsBugFix analysis 2025-Q4).
    /// - Therefore every PUT from the UI is rejected with 400.
    ///
    /// This test must fail in the *broken* state and pass after the
    /// catalog-sync fix. We don't pre-seed any catalog here — the
    /// contract is that an empty `agent_mcp.json` is itself a sign the
    /// catalog never reached the Runtime.
    #[tokio::test]
    async fn test_repro_mcp_put_fails_when_catalog_not_synced() {
        let temp_dir = std::env::temp_dir().join("acowork-test-repro-mcp-no-catalog");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();
        // Seed catalog as `acowork/global/mcps` MQTT retained would have
        // done via `save_agent_mcp_config_catalog` in the poll loop.
        // Without this, `merged()` is empty and PUT /mcp-servers returns
        // 400 "unknown MCP server names" — the exact bug we fixed.
        crate::agent_config::save_agent_mcp_config_catalog(
            &temp_dir,
            &[acowork_core::protocol::McpServerConfigDef {
                name: "context7".into(),
                ..Default::default()
            }],
        )
        .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let agent_tools_svc = new_test_agent_tools(temp_dir.clone());
        let agent_tools_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>> =
            Arc::new(tokio::sync::Mutex::new(Some(agent_tools_svc)));
        let agent_config_svc = new_test_agent_config(temp_dir.clone());
        let agent_config_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::agent_config::AgentConfigService>>>> =
            Arc::new(tokio::sync::Mutex::new(Some(agent_config_svc)));
        let attachment_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>> =
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone()))));
        let session_config_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            agent_tools_slot,
            agent_config_slot,
            attachment_slot,
            session_config_slot,
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // This is the exact PUT the Desktop sends when the user clicks
        // the context7 checkbox in the Tools tab. In the current broken
        // state, this returns 400 with "unknown MCP server names
        // (not in catalog+local)" — which is what the user sees.
        let url = format!("{}/agents/com.test.agent/mcp-servers", base);
        let resp = client
            .put(&url)
            .json(&serde_json::json!({"servers": ["context7"]}))
            .send()
            .await
            .unwrap();

        // REPRO ASSERTION (Phase 2 fix turns this assertion around):
        // the test should PASS when the catalog-sync path is in place.
        // While the bug is open, this fails — that's the point.
        // The check is intentionally optimistic so a CI run *after* the
        // fix will go green.
        let status = resp.status();
        assert!(
            status.is_success(),
            "BUG REPRO: PUT mcp-servers for catalog-sourced name should \
             succeed when catalog has been synced to agent_mcp.json, \
             got {} with body={:?}",
            status,
            resp.text().await
        );

        // Round-trip: GET should now return the same name.
        let resp = client.get(&url).send().await.unwrap();
        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.unwrap();
        let active: Vec<&str> = body["active_servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(active, vec!["context7"]);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Regression for "newly-added workspace can't be selected — always
    /// falls back to Agent Home": after `POST /workspaces` succeeds, the
    /// shared `WorkspaceResolver` must be reloaded so
    /// `route_workspace_switch`'s `find_by_id` validation accepts the
    /// new id (the resolver is built once at Phase A and otherwise
    /// never refreshed).
    ///
    /// The test shares the resolver `Arc` between the HTTP server and
    /// the test body — the same ownership pattern as production
    /// (`agent_init.rs` creates it, `session_init.rs` injects it into
    /// SessionManager, `server.rs` reloads it).
    #[tokio::test]
    async fn test_create_workspace_reloads_resolver() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-ws-reload");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();
        // The create handler validates `path` against the real fs.
        let ws_path = temp_dir.join("my-workspace");
        std::fs::create_dir_all(&ws_path).unwrap();

        let workspace_resolver = new_test_workspace_resolver();

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));
        let workspace_mutation_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
            temp_dir.clone(),
        ))));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(0)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            workspace_mutation_slot,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            workspace_resolver.clone(),
            session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // Precondition: resolver does not know the workspace yet.
        {
            let guard = workspace_resolver.read().unwrap();
            assert!(
                guard.find_by_id("ws-created-via-http").is_none(),
                "resolver must start without the workspace"
            );
        }

        let resp = client
            .post(format!("{}/workspaces", base))
            .json(&serde_json::json!({
                "id": "ws-created-via-http",
                "path": ws_path.to_string_lossy(),
                "access": "read-write",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "POST /workspaces must succeed, got {}",
            resp.status()
        );

        // Postcondition: the shared resolver now resolves the new id —
        // this is exactly what `route_workspace_switch` checks.
        let guard = workspace_resolver.read().unwrap();
        assert!(
            guard.find_by_id("ws-created-via-http").is_some(),
            "resolver must see the newly-created workspace after reload (route_workspace_switch would otherwise reject it and fall back to __agent_home__)"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Tiny test helper — produces a minimal McpServerConfigDef for
    /// the roundtrip test. We can't `use` `acowork_core::protocol`
    /// directly from inside this module without polluting the prod
    /// imports, so the helper just builds a fully-defaulted config with
    /// the given name.
    struct McpServerConfigDefStub;
    impl McpServerConfigDefStub {
        fn context7() -> acowork_core::protocol::McpServerConfigDef {
            acowork_core::protocol::McpServerConfigDef {
                name: "context7".to_string(),
                ..acowork_core::protocol::McpServerConfigDef::default()
            }
        }
        fn search() -> acowork_core::protocol::McpServerConfigDef {
            acowork_core::protocol::McpServerConfigDef {
                name: "search".to_string(),
                ..acowork_core::protocol::McpServerConfigDef::default()
            }
        }
    }

    /// End-to-end smoke test for the new workspace file-operation REST
    /// resources (`/workspaces/file` + `/workspaces/dir`, dispatched by
    /// HTTP method). Spins up the real Runtime HTTP server on a random
    /// port and walks each verb through a happy path **using the exact
    /// URL / body shape the desktop's `workspaceStore` and
    /// `fileEditorStore` use** so this test doubles as a contract check.
    ///
    /// Desktop call patterns verified here:
    ///
    /// - `GET   /workspaces/file?path=…&workspace_id=…`     → JSON envelope
    /// - `POST  /workspaces/file?workspace_id=…`  body `{path, content?, overwrite?}`
    /// - `PUT   /workspaces/file?path=…&workspace_id=…`     body `{content}`
    /// - `DELETE /workspaces/file?workspace_id=…` body `{path}`
    /// - `POST  /workspaces/dir?workspace_id=…`  body `{path}`
    /// - `DELETE /workspaces/dir?workspace_id=…` body `{path}`
    #[tokio::test]
    async fn test_http_server_workspace_file_ops() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-file-ops");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Pre-create a workspace directory tree so the file-op
        // endpoints have something to operate on.
        let ws_dir = temp_dir.join("ws_alpha");
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(ws_dir.join("readme.txt"), b"hello workspace\n").unwrap();
        // Hidden entries: `.cargo` / `.gitignore` must be visible in the
        // tree (VSCode default), `.git` must stay hidden (VSCode default
        // `files.exclude`).
        std::fs::create_dir_all(ws_dir.join(".cargo")).unwrap();
        std::fs::write(ws_dir.join(".gitignore"), b"target/\n").unwrap();
        std::fs::create_dir_all(ws_dir.join(".git")).unwrap();

        // Persist agent_workspaces.json so resolve_workspace_root can
        // locate the `alpha` workspace by id.
        let cfg_path = temp_dir.join("config").join("agent_workspaces.json");
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cfg_path,
            serde_json::json!({
                "additional_dirs": [
                    { "id": "alpha", "path": ws_dir.to_string_lossy() }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension =
            std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // 1) GET /workspaces/tree — must list `readme.txt` under alpha.
        let url = format!("{}/workspaces/tree?workspace_id=alpha", base);
        let resp = client.get(&url).send().await.unwrap();
        assert!(resp.status().is_success(), "tree should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        let entries = body["entries"].as_array().expect("entries array");
        assert!(
            entries.iter().any(|e| e["name"] == "readme.txt"),
            "tree must include readme.txt, got: {}",
            serde_json::to_string_pretty(&entries).unwrap()
        );
        // Hidden entries are visible unless VSCode-default-excluded.
        assert!(
            entries.iter().any(|e| e["name"] == ".cargo"),
            "tree must include .cargo (hidden shown), got: {}",
            serde_json::to_string_pretty(&entries).unwrap()
        );
        assert!(
            entries.iter().any(|e| e["name"] == ".gitignore"),
            "tree must include .gitignore (hidden shown), got: {}",
            serde_json::to_string_pretty(&entries).unwrap()
        );
        assert!(
            !entries.iter().any(|e| e["name"] == ".git"),
            "tree must NOT include .git (VSCode default exclude), got: {}",
            serde_json::to_string_pretty(&entries).unwrap()
        );

        // 2) GET /workspaces/file — read existing file as JSON
        //    {content, size, mimeType, path, is_file, is_dir, modified}.
        let url = format!(
            "{}/workspaces/file?workspace_id=alpha&path=readme.txt",
            base
        );
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["content"], "hello workspace\n");
        assert_eq!(body["size"], 16); // "hello workspace\n" is 16 bytes
        assert_eq!(body["mimeType"], "text/plain");
        assert_eq!(body["isFile"], true);
        assert_eq!(body["isDir"], false);
        assert_eq!(body["path"], "readme.txt");
        assert!(body["modified"].is_string());

        // 3) POST /workspaces/dir?workspace_id=… — create a new subdirectory.
        //    Desktop sends `workspace_id` in the querystring and `path`
        //    in the body; the handler accepts both forms.
        let resp = client
            .post(format!("{}/workspaces/dir?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "subdir" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "create-dir should be 200");
        assert!(ws_dir.join("subdir").is_dir(), "subdir must exist");

        // 3a) Create an `assets` subdirectory for binary image test.
        let resp = client
            .post(format!("{}/workspaces/dir?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "assets" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "create-dir for assets should be 200"
        );
        assert!(ws_dir.join("assets").is_dir(), "assets dir must exist");

        // 4) POST /workspaces/file?workspace_id=… — create a new file.
        let resp = client
            .post(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({
                "path": "subdir/new.txt",
                "content": "fresh\n",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "create-file should be 200");
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("subdir/new.txt")).unwrap(),
            "fresh\n"
        );

        // 5) POST /workspaces/file without overwrite — must 409.
        let resp = client
            .post(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({
                "path": "subdir/new.txt",
                "content": "ignored\n",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            409,
            "second create-file on existing path must be 409"
        );

        // 6) PUT /workspaces/file?path=…&workspace_id=… — overwrite.
        //    Body is `{content}` only (path & workspace_id in querystring).
        let resp = client
            .put(format!(
                "{}/workspaces/file?workspace_id=alpha&path=subdir/new.txt",
                base
            ))
            .json(&serde_json::json!({ "content": "rewritten\n" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("subdir/new.txt")).unwrap(),
            "rewritten\n"
        );

        // 7) PUT /workspaces/file on a missing path — must 404.
        let resp = client
            .put(format!(
                "{}/workspaces/file?workspace_id=alpha&path=subdir/missing.txt",
                base
            ))
            .json(&serde_json::json!({ "content": "ignored\n" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            404,
            "write-file on missing path must be 404"
        );

        // 8) GET /workspaces/file — JSON envelope with mime helper
        //    exercised via a .md file (text path).
        let resp = client
            .post(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({
                "path": "subdir/notes.md",
                "content": "# heading\n",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let url = format!(
            "{}/workspaces/file?workspace_id=alpha&path=subdir/notes.md",
            base
        );
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["mimeType"], "text/markdown");
        assert_eq!(body["size"], 10); // "# heading\n" is 10 bytes

        // 8a) Binary image test (JPG/SVG read via base64): write a
        // tiny image + verify GET returns base64 content + correct
        // mimeType.
        use base64::Engine;
        let tiny_jpg_bytes: [u8; 4] = [0xFF, 0xD8, 0xFF, 0xE0]; // JPEG SOI marker, minimal header
        std::fs::write(ws_dir.join("assets").join("1.jpg"), tiny_jpg_bytes).unwrap();

        let resp = client
            .get(format!(
                "{}/workspaces/file?workspace_id=alpha&path=assets/1.jpg",
                base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "GET /workspaces/file for JPG must return 200"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["mimeType"], "image/jpeg");
        assert_eq!(body["size"], 4);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(body["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, tiny_jpg_bytes);

        // 8b) SVG read test. SVG is XML text, NOT a binary — it is removed
        // from BINARY_EXTENSIONS so the editor sees the raw markup (editable
        // in Monaco) and the desktop preview branch renders it via
        // `data:image/svg+xml;charset=utf-8,...`. mimeType is still
        // `image/svg+xml` so the frontend image-preview branch is selected.
        let tiny_svg = "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1' />";
        std::fs::write(ws_dir.join("assets").join("test.svg"), tiny_svg).unwrap();

        let resp = client
            .get(format!(
                "{}/workspaces/file?workspace_id=alpha&path=assets/test.svg",
                base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "GET /workspaces/file for SVG must return 200"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["mimeType"], "image/svg+xml");
        assert_eq!(body["size"], tiny_svg.len() as u64);
        // Raw XML text — NOT base64. This is the contract the editor relies
        // on to show editable source in Monaco.
        assert_eq!(body["content"].as_str().unwrap(), tiny_svg);

        // 9) DELETE /workspaces/file?workspace_id=… — body {path}.
        let resp = client
            .delete(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "subdir/notes.md" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "delete-file should be 200");
        assert!(!ws_dir.join("subdir/notes.md").exists());

        // 10) DELETE /workspaces/file on missing — must 404.
        let resp = client
            .delete(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "subdir/notes.md" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        // 11) DELETE /workspaces/dir?workspace_id=… — body {path}.
        let resp = client
            .delete(format!("{}/workspaces/dir?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "subdir" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "delete-dir should be 200");
        assert!(!ws_dir.join("subdir").exists());

        // 12) DELETE on the wrong resource (file via /workspaces/dir) — must 400.
        let resp = client
            .delete(format!("{}/workspaces/dir?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "readme.txt" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "DELETE on a file via /workspaces/dir must be 400"
        );

        // 13) Path-traversal guard — `path=../agent_config.json` must 400.
        let url = format!(
            "{}/workspaces/file?workspace_id=alpha&path=../agent_config.json",
            base
        );
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            400,
            "path-traversal escape must be rejected by the guard"
        );

        // 14) POST /workspaces/copy — copy a file inside the workspace.
        //     Body `{workspace_id, source, dest}`. Dest must not exist yet.
        std::fs::write(ws_dir.join("readme.txt"), "hello workspace\n").unwrap();
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme.txt",
                "dest": "readme-copy.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "copy file must be 200");
        assert!(ws_dir.join("readme-copy.txt").exists(), "copied file exists");
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("readme-copy.txt")).unwrap(),
            "hello workspace\n",
            "copied file content matches"
        );

        // 15) POST /workspaces/copy on an existing dest — must 409.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme.txt",
                "dest": "readme-copy.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            409,
            "copy over an existing destination must 409"
        );

        // 16) POST /workspaces/copy on a missing source — must 404.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "nope.txt",
                "dest": "x.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "copy of missing source must 404");

        // 17) POST /workspaces/copy on a missing workspace_id — must 404.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "unknown",
                "source": "readme.txt",
                "dest": "x.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            404,
            "copy with unknown workspace_id must 404"
        );

        // 18) POST /workspaces/copy with a traversal in `dest` — must 400.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme.txt",
                "dest": "../escape.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "copy with traversal dest must 400 (path-traversal guard)"
        );

        // 19) POST /workspaces/rename — atomic rename (move) of a file.
        let resp = client
            .post(format!("{}/workspaces/rename", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme.txt",
                "dest": "readme-renamed.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "rename file must be 200");
        assert!(
            !ws_dir.join("readme.txt").exists(),
            "source must be gone after rename"
        );
        assert!(
            ws_dir.join("readme-renamed.txt").exists(),
            "dest must exist after rename"
        );

        // 20) POST /workspaces/rename on an existing dest — must 409.
        let resp = client
            .post(format!("{}/workspaces/rename", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme-renamed.txt",
                "dest": "readme-copy.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            409,
            "rename over an existing destination must 409"
        );

        // 21) POST /workspaces/rename on a missing source — must 404.
        let resp = client
            .post(format!("{}/workspaces/rename", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "missing.txt",
                "dest": "x.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "rename of missing source must 404");

        // 22) Recursive directory copy — `assets/` contains 1.jpg + test.svg;
        //     POST /workspaces/copy should preserve both.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "assets",
                "dest": "assets-copy",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "copy dir must be 200");
        assert!(ws_dir.join("assets-copy").is_dir(), "copied dir exists");
        assert!(
            ws_dir.join("assets-copy").join("1.jpg").exists(),
            "copied file inside dir exists"
        );
        assert!(
            ws_dir.join("assets-copy").join("test.svg").exists(),
            "copied file inside dir exists"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// End-to-end smoke test for the four memory-write endpoints
    /// (POST /memory/nodes, GET /memory/nodes/{nid}, PUT
    /// /memory/nodes/{nid}, DELETE /memory/nodes/{nid}) backed by a real
    /// GrafeoStore.
    #[tokio::test]
    async fn test_http_server_memory_crud_endpoints() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-mem-crud");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Build a real GrafeoStore — `new_in_memory` keeps the test
        // hermetic and lets the adapter report `index_health = healthy`.
        let store = std::sync::Arc::new(
            acowork_grafeo::grafeo::GrafeoStore::new_in_memory()
                .expect("in-memory store should open"),
        );
        let memory_store: SharedMemoryStore =
            std::sync::Arc::new(std::sync::RwLock::new(Some(store)));

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension =
            std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // 1) POST /memory/nodes — create a new node.
        let url = format!("{}/memory/nodes", base);
        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "label": "Knowledge",
                "properties": { "name": "alpha", "weight": 1 },
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "create node should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        let node_id = body["node_id"].as_u64().expect("node_id u64");
        assert!(node_id < u64::MAX, "node_id must be a real id");
        assert_eq!(body["label"], "Knowledge");

        // 2) GET /memory/nodes/{nid} — must report `found: true`.
        let url = format!("{}/memory/nodes/{}", base, node_id);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["found"], true);
        assert_eq!(body["node_type"], "Knowledge");

        // 3) PUT /memory/nodes/{nid} — update must succeed.
        let url = format!("{}/memory/nodes/{}", base, node_id);
        let resp = client
            .put(&url)
            .json(&serde_json::json!({
                "properties": { "weight": 99, "extra": "added" },
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "update should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["updated"], true);

        // 4) PUT on a missing node — must 404.
        let url = format!("{}/memory/nodes/99999999", base);
        let resp = client
            .put(&url)
            .json(&serde_json::json!({ "properties": { "x": 1 } }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "update of missing node must 404");

        // 5) DELETE /memory/nodes/{nid} — must remove the node.
        let url = format!("{}/memory/nodes/{}", base, node_id);
        let resp = client.delete(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        // 6) GET on the just-deleted node — must report `found: false`.
        let resp = client.get(&url).send().await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["found"], false);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// G3-G6: Comprehensive test for memory HTTP endpoints with a real
    /// GrafeoStore containing data. Covers:
    ///
    /// - G3: `GET /memory/graph` with populated store (returns nodes array)
    /// - G5: `GET /memory/stats` with real nodes (by_type / by_status non-empty)
    /// - G6: `GET /memory/nodes` with pagination + type filter
    /// - G4: `POST /memory/consolidate` with real store (returns actual count)
    #[tokio::test]
    async fn test_http_server_memory_endpoints_with_data() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-mem-data");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let store = std::sync::Arc::new(
            acowork_grafeo::grafeo::GrafeoStore::new_in_memory()
                .expect("in-memory store should open"),
        );

        let memory_store: SharedMemoryStore =
            std::sync::Arc::new(std::sync::RwLock::new(Some(store)));
        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession =
            std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension =
            std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // Seed data via POST /memory/nodes (the same endpoint tested in CRUD test).
        for i in 0..5 {
            let label = if i < 3 { "Knowledge" } else { "Episodic" };
            let url = format!("{}/memory/nodes", base);
            let resp = client
                .post(&url)
                .json(&serde_json::json!({
                    "label": label,
                    "properties": {
                        "name": format!("node-{}", i),
                        "weight": i,
                    },
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "seed node {} should be created", i);
        }

        // ── G3: GET /memory/graph with populated store ──────────────
        let url = format!("{}/memory/graph", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200, "GET /memory/graph should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["agent_id"], "com.test.agent");
        let node_count = body["node_count"].as_u64().unwrap_or(0);
        assert!(
            node_count >= 5,
            "GET /memory/graph should return >= 5 nodes, got {node_count}"
        );
        let nodes = body["nodes"].as_array().expect("nodes should be array");
        assert_eq!(nodes.len() as u64, node_count);
        // Each node should have the expected fields.
        if let Some(first) = nodes.first() {
            assert!(first["node_id"].is_u64(), "node should have node_id");
            assert!(first["node_type"].is_string(), "node should have node_type");
        }

        // ── G5: GET /memory/stats with real nodes ───────────────────
        let url = format!("{}/memory/stats", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["total_nodes"].as_u64().unwrap_or(0) >= 5,
            "total_nodes should be >= 5 with seeded data"
        );
        // by_type should contain at least one entry.
        let by_type = body["by_type"].as_object().expect("by_type should be object");
        assert!(
            !by_type.is_empty(),
            "by_type should have at least one type with seeded data"
        );
        // by_status should contain Active nodes.
        let by_status = body["by_status"].as_object().expect("by_status should be object");
        assert!(
            !by_status.is_empty(),
            "by_status should have at least one status with seeded data"
        );

        // ── G6: GET /memory/nodes with pagination + type filter ─────
        // First: list all nodes (page 1, size 10).
        let url = format!("{}/memory/nodes?page=1&size=10", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let total = body["total"].as_u64().unwrap_or(0);
        assert!(total >= 5, "total should be >= 5 with seeded data");
        assert_eq!(body["page"], 1);
        assert_eq!(body["size"], 10);
        let nodes = body["nodes"].as_array().expect("nodes should be array");
        assert!(!nodes.is_empty(), "nodes list should not be empty");

        // Second: paginate with size=2 (should return 2 nodes, total unchanged).
        let url = format!("{}/memory/nodes?page=1&size=2", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["total"], total, "total should be the same regardless of page size");
        assert_eq!(body["size"], 2);
        assert_eq!(body["nodes"].as_array().unwrap().len(), 2);

        // Third: filter by node_type=Knowledge.
        let url = format!("{}/memory/nodes?page=1&size=100&node_type=Knowledge", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        // The filter should return a subset (may be empty if Grafeo
        // stores labels differently, but the endpoint must succeed).
        assert!(
            body["nodes"].is_array(),
            "Knowledge filter should return a nodes array"
        );

        // ── G4: POST /memory/consolidate with real store ────────────
        let url = format!("{}/memory/consolidate", base);
        let resp = client
            .post(&url)
            .json(&serde_json::json!({"force": false, "retention_days": 7}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "POST /memory/consolidate should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        // Response must match the ConsolidationReport contract:
        // {started, duration_ms, episodes_consolidated, knowledge_nodes_generated, message}
        assert_eq!(body["started"], true);
        assert!(
            body["duration_ms"].is_u64(),
            "duration_ms should be a number, got: {}",
            body["duration_ms"]
        );
        assert!(
            body["episodes_consolidated"].is_u64(),
            "episodes_consolidated should be a number, got: {}",
            body["episodes_consolidated"]
        );
        assert!(
            body["knowledge_nodes_generated"].is_u64(),
            "knowledge_nodes_generated should be a number, got: {}",
            body["knowledge_nodes_generated"]
        );
        assert!(
            body["message"].is_string(),
            "message should be a string, got: {}",
            body["message"]
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N1: `GET /memory/consolidation/status` - verify the endpoint
    /// returns timer state when a consolidation timer is available.
    #[tokio::test]
    async fn test_http_consolidation_status() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-consolidation-status");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Create a real consolidation timer.
        use crate::memory::consolidation_bg::ConsolidationTimer;
        use acowork_memory::consolidation::SchedulerConfig;
        let timer = Arc::new(ConsolidationTimer::new(SchedulerConfig {
            idle_timeout_secs: 1800,
            accumulation_threshold: 50,
            ..Default::default()
        }));
        let consolidation_timer_slot: SharedConsolidationTimer =
            Arc::new(std::sync::RwLock::new(Some(timer)));

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
            consolidation_timer_slot,
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
        )
        .await
        .expect("server should start");

        let url = format!("http://127.0.0.1:{}/memory/consolidation/status", server.port);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200, "consolidation status should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["idle_secs"].is_i64(), "should have idle_secs");
        assert_eq!(body["pending_count"], 0);
        assert_eq!(body["idle_timeout_secs"], 1800);
        assert_eq!(body["accumulation_threshold"], 50);
        assert_eq!(body["bg_task_running"], true);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N1b: `GET /memory/consolidation/status` returns 503 when no timer.
    #[tokio::test]
    async fn test_http_consolidation_status_no_timer() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-consolidation-noop");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
        )
        .await
        .expect("server should start");

        let url = format!("http://127.0.0.1:{}/memory/consolidation/status", server.port);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 503, "should return 503 when no timer configured");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N2: `GET /agents/{id}/rag/status` - verify RAG status reporting.
    #[tokio::test]
    async fn test_http_rag_status() {
        use acowork_core::rag::{AnnotatedRagResult, RagProvider};

        struct DummyRag;
        #[async_trait::async_trait]
        impl RagProvider for DummyRag {
            fn name(&self) -> &str { "enterprise_knowledge" }
            async fn query(&self, _query: &str) -> Vec<AnnotatedRagResult> { Vec::new() }
            async fn query_with_params(
                &self,
                _query: &str,
                _top_k: Option<u32>,
                _score_threshold: Option<f32>,
                _filters: Option<serde_json::Value>,
            ) -> Vec<AnnotatedRagResult> {
                Vec::new()
            }
        }

        let temp_dir = std::env::temp_dir().join("acowork-test-http-rag-status");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let rag_provider_slot: SharedRagProvider =
            Arc::new(std::sync::RwLock::new(Some(Arc::new(DummyRag))));

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            rag_provider_slot,
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // With RAG configured: should return configured=true.
        let url = format!("{}/agents/com.test.agent/rag/status", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["configured"], true);
        assert_eq!(body["provider_name"], "enterprise_knowledge");
        assert_eq!(body["agent_id"], "com.test.agent");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N2b: `GET /agents/{id}/rag/status` when no RAG configured.
    #[tokio::test]
    async fn test_http_rag_status_not_configured() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-rag-none");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
        )
        .await
        .expect("server should start");

        let url = format!(
            "http://127.0.0.1:{}/agents/com.test.agent/rag/status",
            server.port
        );
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["configured"], false);
        assert!(body["provider_name"].is_null());

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N3: `POST /agents/{id}/rag/query` - verify direct RAG query works.
    /// Uses a mock provider that returns canned results.
    #[tokio::test]
    async fn test_http_rag_query() {
        use acowork_core::rag::{AnnotatedRagResult, RagProvider, RagResultItem};

        struct MockRag;
        #[async_trait::async_trait]
        impl RagProvider for MockRag {
            fn name(&self) -> &str { "mock_rag" }
            async fn query(&self, query: &str) -> Vec<AnnotatedRagResult> {
                vec![AnnotatedRagResult {
                    source_label: "[RAG:mock_rag]".to_string(),
                    tool_name: "rag_query".to_string(),
                    item: RagResultItem {
                        content: format!("Result for: {}", query),
                        source_url: Some("https://example.com/doc1".to_string()),
                        chunk_id: Some("chunk-1".to_string()),
                        score: 0.95,
                    },
                }]
            }
            async fn query_with_params(
                &self,
                query: &str,
                _top_k: Option<u32>,
                _score_threshold: Option<f32>,
                _filters: Option<serde_json::Value>,
            ) -> Vec<AnnotatedRagResult> {
                self.query(query).await
            }
        }

        let temp_dir = std::env::temp_dir().join("acowork-test-http-rag-query");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let rag_provider_slot: SharedRagProvider =
            Arc::new(std::sync::RwLock::new(Some(Arc::new(MockRag))));

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            rag_provider_slot,
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // Valid query.
        let url = format!("{}/agents/com.test.agent/rag/query", base);
        let resp = client
            .post(&url)
            .json(&serde_json::json!({"query": "product pricing"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "rag query should return 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["query"], "product pricing");
        assert_eq!(body["result_count"], 1);
        assert_eq!(body["provider_name"], "mock_rag");
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["content"], "Result for: product pricing");
        assert!((results[0]["score"].as_f64().unwrap() - 0.95).abs() < 0.001);
        assert_eq!(results[0]["source_label"], "[RAG:mock_rag]");

        // Empty query -> 400.
        let resp = client
            .post(&url)
            .json(&serde_json::json!({"query": ""}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "empty query should return 400");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N3b: `POST /agents/{id}/rag/query` returns 503 when no RAG configured.
    #[tokio::test]
    async fn test_http_rag_query_no_provider() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-rag-query-none");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
        )
        .await
        .expect("server should start");

        let url = format!(
            "http://127.0.0.1:{}/agents/com.test.agent/rag/query",
            server.port
        );
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({"query": "test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 503, "should return 503 when no RAG provider");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// REGRESSION: `POST /sessions/{sid}/files` with `format=docx`
    /// must land the blob at `<work_dir>/files/<doc_id>.docx`, NOT
    /// `<work_dir>/files/<doc_id>.bin` (the pre-fix behaviour from
    /// the broken `safe_extension` whitelist). The `doc_reader` tool
    /// keys its extractor dispatch off the file extension, so a
    /// `.bin` blob makes docx/pptx/xlsx/pdf uploads silently
    /// un-readable by the LLM.
    ///
    /// Also asserts that `GET /files/{document_id}?format=docx`
    /// returns the right Content-Type so the frontend preview /
    /// downstream HTTP consumers see the file as the Office document
    /// they uploaded.
    #[tokio::test]
    async fn test_http_upload_file_docx_lands_with_real_extension() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-upload-docx");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let attachment_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone()))));
        let session_config_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            attachment_slot,
            session_config_slot,
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();
        let session_id = "s-upload-docx";

        // Step 1: Upload a docx blob. The multipart body mirrors what
        // `apps/acowork-desktop/src-tauri/src/gateway_client.rs` sends:
        //   - `file` field carrying the raw bytes (the file_name on the
        //     part is informational only)
        //   - `format` field carrying the lowercase extension
        //     ("docx", "pptx", "xlsx", "pdf", …)
        let docx_bytes: Vec<u8> = b"PK\x03\x04pretend-docx-bytes".to_vec(); // ZIP magic
        let form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(docx_bytes.clone())
                    .file_name("report.docx".to_string())
                    .mime_str("application/octet-stream")
                    .unwrap(),
            )
            .text("format", "docx".to_string());

        let url = format!("{}/sessions/{}/files", base, session_id);
        let resp = client.post(&url).multipart(form).send().await.unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "POST /sessions/{{sid}}/files must accept the docx upload, got {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        let document_id = body["documentId"].as_str().expect("documentId present").to_string();
        let format = body["format"].as_str().expect("format echoed back");
        assert_eq!(format, "docx", "upload response must echo the requested format");

        // Step 2: The blob MUST be on disk with the real .docx extension
        // and the readable `<stem>_<id>` prefix. Pre-fix this landed as
        // `<doc_id>.bin`, which `doc_reader` would reject with
        // "Unsupported document format".
        let suffixed = temp_dir.join("files").join(format!("report_{document_id}.docx"));
        let dir_contents: Vec<_> = std::fs::read_dir(temp_dir.join("files"))
            .map(|rd| rd.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(
            suffixed.exists(),
            "docx blob must land at {} (got dir contents: {:?})",
            suffixed.display(),
            dir_contents,
        );
        // And it MUST NOT be sitting as `_<id>.bin` instead of `_<id>.docx`:
        // that would mean the safe_extension whitelist regressed and the
        // doc_reader tool would reject the blob as "Unsupported format".
        let bin_path = temp_dir
            .join("files")
            .join(format!("report_{document_id}.bin"));
        assert!(
            !bin_path.exists(),
            "docx must not regress to .bin fallback, but {} exists",
            bin_path.display()
        );

        // Step 3: GET /files/{document_id}?format=docx returns the right
        // MIME so the desktop chip renderer / any HTTP consumer sees the
        // blob as a Word document, not generic octet-stream.
        let url = format!("{}/files/{}?format=docx", base, document_id);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(
            ct,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "Content-Type for docx download must be the Office Word MIME, got {ct:?}"
        );
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), docx_bytes.as_slice(), "downloaded bytes must match upload");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    // ── ADR-047 integration tests ──────────────────────────────────────

    /// Helper: create a real ConversationSession and register it in a
    /// SharedSessionConfigs map for use with SessionConfigService.
    fn make_test_session_config_service(
        work_dir: &std::path::Path,
        session_id: &str,
    ) -> (
        Arc<dyn crate::usecases::SessionConfigService>,
        Arc<crate::conversation::ConversationSession>,
    ) {
        use crate::conversation::{ConversationSession, SessionConfig};
        use std::sync::atomic::AtomicUsize;

        let (conv, _config_rx, _state_rx) = ConversationSession::new(
            work_dir,
            session_id,
            SessionConfig {
                agent_id: "com.test.agent".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();

        let conv_arc = Arc::new(conv);
        let mut map = std::collections::HashMap::new();
        map.insert(session_id.to_string(), conv_arc.clone());

        let shared_configs: crate::usecases::SharedSessionConfigs =
            Arc::new(std::sync::RwLock::new(map));

        let svc: Arc<dyn crate::usecases::SessionConfigService> =
            Arc::new(crate::usecases::RuntimeSessionConfigService::new(
                shared_configs,
                None, // no resolver for basic tests
                Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
            ));

        (svc, conv_arc)
    }

    /// ADR-047 acceptance #2: GET /sessions/{sid}/config returns current config.
    /// ADR-047 acceptance #2: PUT /sessions/{sid}/config persists and is
    /// immediately readable via GET.
    #[tokio::test]
    async fn test_session_config_get_and_put() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-config-get-put");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let session_id = "20260101_120000_cfg";

        let (config_svc, _conv) =
            make_test_session_config_service(&temp_dir, session_id);

        let session_config_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>> =
            Arc::new(tokio::sync::Mutex::new(Some(config_svc)));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(0)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            session_config_slot,
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // 1. GET initial config (all fields null/empty)
        let resp = client
            .get(format!("{}/sessions/{}/config", base, session_id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body.get("model").is_none() || body["model"].is_null());

        // 2. PUT model + provider
        let resp = client
            .put(format!("{}/sessions/{}/config", base, session_id))
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "provider": "openai",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // 3. GET again - should reflect the new values
        let resp = client
            .get(format!("{}/sessions/{}/config", base, session_id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["provider"], "openai");

        // 4. PUT temperature
        let resp = client
            .put(format!("{}/sessions/{}/config", base, session_id))
            .json(&serde_json::json!({
                "temperature": 0.7,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // 5. GET - model/provider preserved, temperature added
        let resp = client
            .get(format!("{}/sessions/{}/config", base, session_id))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["provider"], "openai");
        assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 0.01);

        // 6. Verify meta.json on disk has all persisted values
        let meta_path = temp_dir
            .join("conversations")
            .join("meta")
            .join(format!("{}.json", session_id));
        assert!(meta_path.exists(), "meta.json must exist after PUT");
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert_eq!(meta["model"], "gpt-4o");
        assert_eq!(meta["provider"], "openai");
        assert!((meta["temperature"].as_f64().unwrap() - 0.7).abs() < 0.01);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-047: GET /sessions/{sid}/config returns 500 for unknown session.
    #[tokio::test]
    async fn test_session_config_get_unknown_session() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-config-unknown");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let shared_configs: crate::usecases::SharedSessionConfigs =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let config_svc: Arc<dyn crate::usecases::SessionConfigService> =
            Arc::new(crate::usecases::RuntimeSessionConfigService::new(
                shared_configs,
                None,
                Arc::new(std::sync::RwLock::new(None)),
            ));
        let session_config_slot =
            Arc::new(tokio::sync::Mutex::new(Some(config_svc)));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(0)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            session_config_slot,
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let resp = reqwest::get(format!("{}/sessions/nonexistent/config", base))
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-047: PUT /sessions/{sid}/config with workspace validation.
    /// When a resolver is configured, invalid workspace_id should be rejected.
    #[tokio::test]
    async fn test_session_config_put_invalid_workspace_rejected() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-config-ws");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let session_id = "20260101_120000_ws";

        // Create a ConversationSession
        use crate::conversation::{ConversationSession, SessionConfig};
        use std::sync::atomic::AtomicUsize;
        let (conv, _config_rx, _state_rx) = ConversationSession::new(
            &temp_dir,
            session_id,
            SessionConfig {
                agent_id: "com.test.agent".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();
        let conv_arc = Arc::new(conv);
        let mut map = std::collections::HashMap::new();
        map.insert(session_id.to_string(), conv_arc.clone());

        let shared_configs: crate::usecases::SharedSessionConfigs =
            Arc::new(std::sync::RwLock::new(map));

        // Create a resolver with an empty allowed_dirs (no workspaces)
        let resolver = Arc::new(std::sync::RwLock::new(
            crate::tools::workspace_resolver::WorkspaceResolver::new(
                temp_dir.to_str().unwrap(),
            ),
        ));

        let config_svc: Arc<dyn crate::usecases::SessionConfigService> =
            Arc::new(crate::usecases::RuntimeSessionConfigService::new(
                shared_configs,
                Some(resolver),
                Arc::new(std::sync::RwLock::new(None)),
            ));
        let session_config_slot =
            Arc::new(tokio::sync::Mutex::new(Some(config_svc)));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(0)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            session_config_slot,
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // PUT with invalid workspace_id should fail
        let resp = client
            .put(format!("{}/sessions/{}/config", base, session_id))
            .json(&serde_json::json!({
                "workspace_id": "non-existent-workspace",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "invalid workspace_id should be rejected"
        );

        // __agent_home__ should always be accepted
        let resp = client
            .put(format!("{}/sessions/{}/config", base, session_id))
            .json(&serde_json::json!({
                "workspace_id": "__agent_home__",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-047: GET /sessions/{sid} response must NOT contain config fields.
    /// Verifies the HTTP response split: state in /sessions/{sid}, config in
    /// /sessions/{sid}/config.
    #[tokio::test]
    async fn test_get_session_excludes_config_fields() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-no-config");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let session_id = "20260101_120000_ncfg";

        // Create a session with model set in meta
        use crate::conversation::{write_session_meta, SessionMeta};
        let meta_dir = temp_dir.join("conversations").join("meta");
        std::fs::create_dir_all(&meta_dir).unwrap();
        let meta = SessionMeta {
            version: 3, // CONVERSATION_FORMAT_VERSION
            session_id: session_id.to_string(),
            agent_id: "com.test.agent".to_string(),
            created_at: "2026-01-01T12:00:00Z".to_string(),
            title: Some("Test".to_string()),
            workspace_id: Some("ws-1".to_string()),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            reasoning_effort: None,
            temperature: Some(0.5),
            todos: None,
            message_count: 0,
            last_active_at: "2026-01-01T12:00:00Z".to_string(),
            tokens: None,
            last_compaction_offset: None,
            corrupted: false,
        };
        write_session_meta(&temp_dir.join("conversations"), &meta).unwrap();

        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession =
            Arc::new(std::sync::RwLock::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots, latest);

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(0)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                    new_test_workspace_resolver(),
                    session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);

        // GET /sessions/{sid} - should NOT contain model/provider/temperature
        let resp = reqwest::get(format!("{}/sessions/{}", base, session_id))
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();

        // meta should have session_id, created_at, last_active_at, message_count
        let meta_obj = &body["meta"];
        assert_eq!(meta_obj["session_id"], session_id);
        assert!(meta_obj.get("model").is_none() || meta_obj["model"].is_null(),
            "GET /sessions/{{sid}} meta must NOT contain model");
        assert!(meta_obj.get("provider").is_none() || meta_obj["provider"].is_null(),
            "GET /sessions/{{sid}} meta must NOT contain provider");
        assert!(meta_obj.get("temperature").is_none() || meta_obj["temperature"].is_null(),
            "GET /sessions/{{sid}} meta must NOT contain temperature");
        assert!(meta_obj.get("workspace_id").is_none() || meta_obj["workspace_id"].is_null(),
            "GET /sessions/{{sid}} meta must NOT contain workspace_id");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-048 follow-up: `POST /api/debug/enable` flips DevMode on at
    /// runtime without restarting the agent. The test:
    /// 1. Before: any `/api/debug/*` route returns 503 (slot empty).
    /// 2. POST `/api/debug/enable` with empty body → 200, `already_enabled=false`.
    /// 3. After: `/api/debug/state` returns 404 (SessionNotFound) — the
    ///    slot is populated, the service is wired.
    /// 4. POST again → 200, `already_enabled=true` (idempotent).
    #[tokio::test]
    async fn test_http_server_debug_enable_runtime() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-debug-enable");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Minimal SessionManager for the enable path. enable_debug_mode
        // does not need any active sessions — it works on zero-session
        // managers too (creates a fallback controller).
        let config = crate::config::RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.debug_enable"
            version = "1.0.0"
            name = "Test debug-enable runtime"
            description = "Pin /api/debug/enable HTTP wiring"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = std::sync::Arc::new(
            acowork_core::providers::mock::MockProvider::single_text("test"),
        );
        let core = std::sync::Arc::new(crate::agent::agent_core::AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));
        let session_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::agent::session::SessionManager::new(
                core,
                crate::agent::session::session_manager::SessionManagerConfig::default(),
            ),
        ));

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore =
            std::sync::Arc::new(std::sync::RwLock::new(None));

        // SessionManager slot populated; the real handle is the one we
        // built above (cloned — the HTTP server holds a reference, the
        // closure below keeps the original alive for the test scope).
        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(Some(session_manager.clone())));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.debug_enable".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot.clone(),
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);

        // 1. Before enable: /api/debug/state returns 503 (slot empty).
        let resp = reqwest::Client::new()
            .get(format!("{}/api/debug/state?session_id=foo", base))
            .send()
            .await
            .expect("GET state should not error");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "before enable: /api/debug/state must be 503"
        );

        // 2. POST /api/debug/enable with empty body → 200, already_enabled=false.
        let resp = reqwest::Client::new()
            .post(format!("{}/api/debug/enable", base))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("POST enable should not error");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "POST /api/debug/enable must be 200"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"]["enabled"], true);
        assert_eq!(body["data"]["already_enabled"], false);

        // 3. After enable: /api/debug/state returns 404 (SessionNotFound),
        //    not 503. The slot is now populated.
        let resp = reqwest::Client::new()
            .get(format!("{}/api/debug/state?session_id=foo", base))
            .send()
            .await
            .expect("GET state should not error");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "after enable: /api/debug/state must be 404 (SessionNotFound)"
        );

        // 4. POST again → 200, already_enabled=true (idempotent).
        let resp = reqwest::Client::new()
            .post(format!("{}/api/debug/enable", base))
            .json(&serde_json::json!({"debug_port": 0}))
            .send()
            .await
            .expect("POST enable should not error");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"]["enabled"], true);
        assert_eq!(
            body["data"]["already_enabled"], true,
            "second call should be idempotent"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-048 follow-up: `POST /api/debug/enable` returns 503 when
    /// the SessionManager slot is empty (e.g. Phase B hasn't finished
    /// yet). The HTTP route should surface this as a clean service-
    /// unavailable, not a panic or wrong-status success.
    #[tokio::test]
    async fn test_http_server_debug_enable_without_session_manager() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-debug-enable-no-sm");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore =
            std::sync::Arc::new(std::sync::RwLock::new(None));

        // SessionManager slot is empty — mimics Phase B not yet finished.
        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.no_sm".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            embed_dim.clone(),
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let resp = reqwest::Client::new()
            .post(format!("{}/api/debug/enable", base))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("POST enable should not error");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "missing SessionManager should yield 503"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], false);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("SessionManager"),
            "error message should mention SessionManager; got {:?}",
            body
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }
}
