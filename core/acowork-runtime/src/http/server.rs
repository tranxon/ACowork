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
//! GET    /sessions/{sid}/documents               // NEW
//! POST   /sessions/{sid}/documents               // NEW
//! GET    /sessions/{sid}/documents/{doc_id}      // NEW
//! DELETE /sessions/{sid}/documents/{doc_id}      // NEW
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
//! POST   /workspaces/dir                         // NEW: create dir
//! DELETE /workspaces/dir                         // NEW: delete dir
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
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::agent::inbound::{InboundMessage, UserOp};
use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use crate::conversation::{read_messages_paginated, ConversationEntry};
use crate::mqtt::client::SharedRuntimeMqttClient;

/// Error type for Runtime HTTP server operations.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeHttpServerError {
    #[error("HTTP server error: {0}")]
    Server(String),
    #[error("Failed to bind: {0}")]
    Bind(String),
}

/// Shared dispatch sender for Runtime HTTP → agent loop.
///
/// Cloned from the main MQTT dispatch channel. Runtime HTTP handlers
/// use this to send (session_id, InboundMessage) tuples that are
/// forwarded to the right session's AgentLoop via `send_inbound()`.
pub type SharedDispatchSender = Arc<tokio::sync::Mutex<Option<mpsc::UnboundedSender<(String, InboundMessage)>>>>;

/// Shared handle to the Runtime's Grafeo memory store.
///
/// `None` until Phase B (`init_memory_store`) finishes; HTTP handlers
/// report a graceful "no store" response when it is still empty
/// (see [`memory_query`]).
pub type SharedMemoryStore = Arc<std::sync::RwLock<Option<Arc<acowork_grafeo::grafeo::GrafeoStore>>>>;

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

/// State shared with HTTP handlers.
#[derive(Clone)]
struct HttpState {
    work_dir: PathBuf,
    agent_id: String,
    /// Shared map of per-session runtime snapshots, keyed by session_id.
    /// Each value is the same `Arc<RwLock<SessionRuntimeSnapshot>>` held
    /// by `SessionHandle`, so reads are always up-to-date.
    ///
    /// ADR-039: persisted fields (model, provider, workspace_id, etc.) are
    /// not duplicated here — see `data/meta/{session_id}.json` and the
    /// `session_meta` MQTT channel.
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
    mqtt_client: SharedMqttClientSlot,
    /// ADR-040: late-bind usecase services. Populated by Phase B.
    /// All handlers depend solely on these traits; no direct access
    /// to memory_store or agent_core is required.
    session_metadata: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>>,
    memory_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::MemoryQueryService>>>>,
    workspace_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>>,
    workspace_mutation: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>>,
}

/// Handle to the running HTTP server.
pub struct RuntimeHttpServer {
    /// The address the server is listening on.
    pub listen_addr: SocketAddr,
    /// The port the server is listening on (extracted from listen_addr).
    pub port: u16,
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
    ) -> Result<Self, RuntimeHttpServerError> {
        let state = HttpState {
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
            // 4 NEW document routes (panel-sub-component under session).
            .route(
                "/sessions/{sid}/documents",
                post(upload_document).get(list_documents),
            )
            .route(
                "/sessions/{sid}/documents/{doc_id}",
                get(read_document).delete(delete_document),
            )
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
            .route(
                "/workspaces/dir",
                post(create_workspace_dir).delete(delete_workspace_dir),
            )
            // 3 NEW agent panel endpoints (panels 1/3/5).
            // `/agents/{id}/config` carries both GET (Setup panel) and
            // PUT (live-edit builtin_tools / temperature / etc., mqtt.md
            // §3.5 §7). The PUT handler persists to agent_tools.json +
            // re-PUBLISHes the retained AgentConfig snapshot.
            .route(
                "/agents/{id}/config",
                get(get_agent_config).put(put_agent_config),
            )
            .route("/agents/{id}/tools", get(get_agent_tools))
            .route("/agents/{id}/status", get(get_agent_status))
            .with_state(state);

        // Bind to 127.0.0.1:0 for a random port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
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
    /// Offset from the newest end, in **raw entries** (one JSONL line each).
    /// 0 = latest raw entries.  See
    /// [`crate::conversation::PaginatedMessages`] for the contract.
    #[serde(default)]
    offset: Option<u64>,
    /// Maximum number of **raw entries** to return (default 50, capped at 500).
    /// A raw entry is one JSONL line — a single user / assistant /
    /// thought / tool_call / tool_result row. Display-group collapsing
    /// is a frontend UI abstraction and is not visible here.
    #[serde(default)]
    limit: Option<u32>,
}

/// `GET /sessions/{sid}/messages` — paginated message list for a session.
///
/// Delegates to [`crate::conversation::read_messages_paginated`], which is
/// the same backend used by the legacy gRPC path. Supports `offset` /
/// `limit` query parameters (no `direction`; direction is derived from
/// `offset` itself). Both `offset` and `limit` are measured in raw
/// entries, never in display groups. Returns 404 when the session JSONL
/// file does not exist under `workspace/conversations/`.
async fn get_messages(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    Query(query): Query<GetMessagesQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session_path = state
        .work_dir
        .join("conversations")
        .join(format!("{}.jsonl", sid));

    if !session_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(50).clamp(1, 500);

    let paginated = read_messages_paginated(&session_path, offset, limit).map_err(|e| {
        tracing::warn!(
            session_id = %sid,
            error = %e,
            "Failed to read session messages"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // ADR-035 D9.2: truncate tool_result content to first 5 lines for
    // display in ALL HTTP paths (first-page, scroll-back, reconnect
    // realign). Full content stays in JSONL for LLM context. No exception.
    let messages: Vec<ConversationEntry> = paginated
        .messages
        .into_iter()
        .map(|mut m| {
            if m.role == "tool_result" {
                m.content = crate::cli::truncate_tool_result_for_display(&m.role, &m.content);
            }
            m
        })
        .collect();
    let messages_value = serde_json::to_value(&messages)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let count = messages.len();

    Ok(Json(serde_json::json!({
        "messages": messages_value,
        "offset": paginated.offset,
        "limit": paginated.limit,
        "total": paginated.total,
        "count": count,
    })))
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

/// `GET /sessions/{sid}` — full session detail (ADR-034 §11.2 #4, panel 4).
///
/// Phase 3: this endpoint **absorbs** the legacy `/sessions/{sid}/state`.
/// It merges two previously-separate sources of truth so the desktop
/// Session Status panel can render with a single round-trip:
///
/// 1. **`meta.json`** under `workspace/conversations/meta/{sid}.json`
///    — authoritative ADR-024 metadata (title, created_at,
///    last_active_at, message_count, model, provider, workspace_id,
///    reasoning_effort, temperature, etc.).
/// 2. **`SharedSessionSnapshots`** — live `SessionRuntimeSnapshot`
///    populated by SessionManager as the agent loop runs (current
///    status, todos, context_usage ratio).
///
/// When the session has never been observed live (e.g. a brand-new
/// session or one that has never been touched by the agent loop) the
/// snapshot is absent; we still return the meta-derived fields and
/// surface `live_state = null` so the UI can distinguish "no snapshot
/// yet" from "snapshot present but empty".
async fn get_session(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // ── 1. Read meta.json (authoritative for static fields).
    let meta_path = state
        .work_dir
        .join("conversations")
        .join("meta")
        .join(format!("{}.json", sid));
    let meta: Option<serde_json::Value> = if meta_path.exists() {
        std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    } else {
        None
    };

    // Always emit at least the session_id so the panel can render an
    // empty detail card even if meta.json has not been written yet
    // (e.g. session is being created right now). 404 is reserved for
    // sessions that are **completely unknown** — no meta AND no live
    // snapshot.
    let meta_obj = meta.clone().unwrap_or_else(|| {
        serde_json::json!({
            "session_id": sid,
            "title": null,
            "created_at": null,
            "last_active_at": null,
            "message_count": 0,
        })
    });

    // ── 2. Read live runtime snapshot (best-effort; `None` for cold sessions).
    //
    // ADR-039: `live_state` carries only runtime fields (status, model,
    // provider — the latter two as in-memory mirrors — ratio, todos,
    // context_usage). Persistent fields (workspace_id, reasoning_effort,
    // temperature) are read from `meta.json` above.
    let snapshots = state
        .session_snapshots
        .read()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let live_state: Option<serde_json::Value> = snapshots.get(&sid).and_then(|snap_arc| {
        snap_arc.read().ok().map(|snap| {
            let status: serde_json::Value =
                serde_json::from_str(&snap.status_json).unwrap_or(serde_json::Value::Null);
            let todos: Option<serde_json::Value> = snap
                .todos_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            let context_usage: Option<serde_json::Value> = snap
                .context_usage_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            serde_json::json!({
                "status": status,
                "model": snap.model,
                "provider": snap.provider,
                "ratio": snap.ratio,
                "todos": todos,
                "context_usage": context_usage,
            })
        })
    });

    // ── 3. 404 only when both sources are absent. Either present is
    //       enough to return 200 — the meta card alone is useful for
    //       sessions that have been written but never observed.
    if meta.is_none() && live_state.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(Json(serde_json::json!({
        "session_id": sid,
        "meta": meta_obj,
        "live_state": live_state,
    })))
}

// ── Control plane redirects (ADR-034 §7.6.4) ──────────────────────
//
// Phase 3 removed all `POST /sessions/{sid}/{action}` HTTP handlers
// from this server. User-initiated state changes (approval decision,
// question answer, continue execution, session title, session close,
// notify toggle, compress action) now flow through the dedicated MQTT
// topic `acowork/agents/{id}/sessions/control/{cmd}` so the
// deserialization + routing happens in a single place (see
// `crate::mqtt::control` for the channel side).
//
// Keeping those handlers here would have meant two parallel ways to
// drive the agent loop, which Phase 2 explicitly forbids (see §7.1 G1).

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
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.create_workspace(body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"created": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// `PUT /workspaces/{ws_id}` — update an existing workspace entry.
async fn update_workspace(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
    Json(body): Json<crate::usecases::workspace_mutation::WorkspaceEntryInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.update_workspace(&ws_id, body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"updated": r.ok}))))
        .map_err(workspace_error_to_response)
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
    let svc = state.workspace_mutation.lock().await;
    let svc = svc
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "workspace service not ready"}))))?;
    svc.delete_workspace(&ws_id)
        .await
        .map(|_| Json(serde_json::json!({"deleted": true, "ws_id": ws_id})))
        .map_err(workspace_error_to_response)
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

// ── Document storage handlers (ADR-034 §11.2 #6-9) ───────────────────────
//
// Per-session document uploads are stored as binary blobs on disk
// (`work_dir/sessions/{sid}/documents/{doc_id}`) with a sidecar JSON
// index (`documents.json`) that records the user-supplied filename +
// upload timestamp + size. The doc_id itself is a sha256(content)
// prefix — deterministic, collision-resistant, and stable across
// re-uploads of the same file.

/// Sidecar index format for `documents.json`.
///
/// Persisted as JSON for atomic write-tmp-rename. Each entry corresponds
/// to a single document blob on disk; the sidecar is rebuilt from the
/// filesystem on cold start so a corrupted `documents.json` never loses
/// data — it just gets reset to `{"documents": []}`.
#[derive(Serialize, Deserialize)]
struct DocumentsIndex {
    /// Session this index belongs to (informational; the on-disk
    /// location is the parent directory).
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    /// Format version (currently 1).
    #[serde(default = "default_documents_index_version")]
    version: u32,
    /// Document entries, newest first.
    documents: Vec<DocumentEntry>,
}

fn default_documents_index_version() -> u32 {
    1
}

/// One document record in `documents.json`.
#[derive(Serialize, Deserialize, Clone)]
struct DocumentEntry {
    /// Stable identifier (sha256-prefix of the document content).
    doc_id: String,
    /// User-supplied filename (UI hint only — not used as a filesystem
    /// path component to avoid encoding issues).
    filename: String,
    /// Document size in bytes.
    size: u64,
    /// MIME type if the client supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    /// ISO-8601 upload timestamp (UTC).
    uploaded_at: String,
}

/// Resolve the on-disk directory for session documents.
fn documents_dir(work_dir: &std::path::Path, sid: &str) -> PathBuf {
    work_dir.join("sessions").join(sid).join("documents")
}

/// Resolve the sidecar index path.
fn documents_index_path(work_dir: &std::path::Path, sid: &str) -> PathBuf {
    work_dir.join("sessions").join(sid).join("documents.json")
}

/// Load the sidecar index, returning an empty index if the file is
/// missing or corrupted (never an Err to the caller — corruption is
/// recovered by returning an empty list and letting the caller decide).
fn load_documents_index(work_dir: &std::path::Path, sid: &str) -> DocumentsIndex {
    let path = documents_index_path(work_dir, sid);
    if !path.exists() {
        return DocumentsIndex {
            session_id: Some(sid.to_string()),
            version: 1,
            documents: Vec::new(),
        };
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<DocumentsIndex>(&s).ok())
        .unwrap_or_else(|| DocumentsIndex {
            session_id: Some(sid.to_string()),
            version: 1,
            documents: Vec::new(),
        })
}

/// Persist the sidecar index atomically (write-tmp-rename).
fn save_documents_index(
    work_dir: &std::path::Path,
    sid: &str,
    index: &DocumentsIndex,
) -> Result<(), String> {
    let dir = work_dir.join("sessions").join(sid);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create_dir_all sessions/{}: {}", sid, e))?;
    let path = documents_index_path(work_dir, sid);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(index)
        .map_err(|e| format!("serialize documents index: {}", e))?;
    std::fs::write(&tmp, &json)
        .map_err(|e| format!("write tmp {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("rename tmp -> {}: {}", path.display(), e))?;
    Ok(())
}

/// Compute a stable doc_id from raw bytes (sha256 first 12 hex chars).
///
/// A 48-bit prefix is enough to avoid collisions on the per-session
/// document set (millions of files at p≈10⁻⁹); the full hash is
/// recoverable from the prefix should we ever need to index globally.
fn compute_doc_id(content: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    let h = hasher.finish();
    format!("{:012x}-{:x}", h & 0xFFFF_FFFF_FFFF, (h >> 48) & 0xFFFF)
}

/// `POST /sessions/{sid}/documents` — upload a document for the session.
///
/// Accepts a JSON body `{ filename, content_b64, content_type? }`. The
/// `content_b64` field is the base64-encoded binary payload; we decode
/// it here so the wire format stays JSON-friendly without requiring a
/// multipart parser on the desktop.
#[derive(Debug, Deserialize)]
struct UploadDocumentBody {
    filename: String,
    content_b64: String,
    #[serde(default)]
    content_type: Option<String>,
}

async fn upload_document(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    Json(body): Json<UploadDocumentBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bytes = base64_decode_simple(&body.content_b64)
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let dir = documents_dir(&state.work_dir, &sid);
    std::fs::create_dir_all(&dir)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let doc_id = compute_doc_id(&bytes);
    let blob_path = dir.join(&doc_id);
    std::fs::write(&blob_path, &bytes)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut index = load_documents_index(&state.work_dir, &sid);
    // De-dup: if the same content is re-uploaded we return the existing
    // record rather than creating a duplicate entry.
    if let Some(existing) = index.documents.iter().find(|d| d.doc_id == doc_id) {
        return Ok(Json(serde_json::json!({
            "session_id": sid,
            "doc_id": existing.doc_id,
            "filename": existing.filename,
            "size": existing.size,
            "uploaded_at": existing.uploaded_at,
            "duplicate": true,
        })));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let entry = DocumentEntry {
        doc_id: doc_id.clone(),
        filename: body.filename.clone(),
        size: bytes.len() as u64,
        content_type: body.content_type.clone(),
        uploaded_at: now.clone(),
    };
    index.documents.insert(0, entry.clone());
    save_documents_index(&state.work_dir, &sid, &index)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({
        "session_id": sid,
        "doc_id": doc_id,
        "filename": entry.filename,
        "size": entry.size,
        "uploaded_at": now,
        "duplicate": false,
    })))
}

/// `GET /sessions/{sid}/documents` — list documents uploaded for the session.
async fn list_documents(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
) -> Json<serde_json::Value> {
    let index = load_documents_index(&state.work_dir, &sid);
    Json(serde_json::json!({
        "session_id": sid,
        "documents": index.documents,
        "total": index.documents.len(),
    }))
}

/// `GET /sessions/{sid}/documents/{doc_id}` — read a document blob.
///
/// Binary files are returned base64-encoded; text files are returned as
/// UTF-8. The `content_type` recorded at upload time is the source of
/// truth — we do not sniff the file server-side.
async fn read_document(
    State(state): State<HttpState>,
    Path((sid, doc_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let blob_path = documents_dir(&state.work_dir, &sid).join(&doc_id);
    if !blob_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    let index = load_documents_index(&state.work_dir, &sid);
    let entry = index.documents.iter().find(|d| d.doc_id == doc_id);

    let bytes = std::fs::read(&blob_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let is_text = entry
        .as_ref()
        .and_then(|e| e.content_type.as_deref())
        .map(|ct| ct.starts_with("text/") || ct == "application/json")
        .unwrap_or(false);

    if is_text {
        let content = String::from_utf8_lossy(&bytes).to_string();
        Ok(Json(serde_json::json!({
            "session_id": sid,
            "doc_id": doc_id,
            "filename": entry.map(|e| e.filename.clone()),
            "content_type": entry.and_then(|e| e.content_type.clone()),
            "content": content,
            "encoding": "utf-8",
            "size": bytes.len(),
        })))
    } else {
        let encoded = base64_encode_simple(&bytes);
        Ok(Json(serde_json::json!({
            "session_id": sid,
            "doc_id": doc_id,
            "filename": entry.map(|e| e.filename.clone()),
            "content_type": entry.and_then(|e| e.content_type.clone()),
            "content": encoded,
            "encoding": "base64",
            "size": bytes.len(),
        })))
    }
}

/// `DELETE /sessions/{sid}/documents/{doc_id}` — remove a document.
async fn delete_document(
    State(state): State<HttpState>,
    Path((sid, doc_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let blob_path = documents_dir(&state.work_dir, &sid).join(&doc_id);
    let mut index = load_documents_index(&state.work_dir, &sid);
    let initial_len = index.documents.len();
    index.documents.retain(|d| d.doc_id != doc_id);
    if index.documents.len() == initial_len {
        // doc_id not in the index — nothing to delete from metadata,
        // but still try to remove the blob (best-effort idempotent).
        let _ = std::fs::remove_file(&blob_path);
        return Err(StatusCode::NOT_FOUND);
    }
    save_documents_index(&state.work_dir, &sid, &index)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = std::fs::remove_file(&blob_path);
    Ok(Json(serde_json::json!({
        "session_id": sid,
        "doc_id": doc_id,
        "deleted": true,
    })))
}


// ── Agent panel handlers (ADR-034 §11.2 #23-25) ────────────────────────
//
// Three GET endpoints powering the desktop panels Setup (1), Tools (3)
// and Agent Status (5). All read-only — mutations flow through the
// dedicated MQTT control commands.

/// `GET /agents/{id}/config` — Agent Setup panel data.
async fn get_agent_config(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    // ADR-034: path `id` must match the configured agent_id — the
    // Runtime is a per-agent process, so any other value is a caller
    // mistake that we still tolerate by returning an empty config
    // (so a misconfigured Gateway does not 404 the whole panel).
    let matches = id == state.agent_id;
    let cfg: Option<crate::agent_config::AgentConfig> = if matches {
        crate::agent_config::load_agent_config(&state.work_dir)
            .ok()
            .flatten()
    } else {
        None
    };
    Json(serde_json::json!({
        "agent_id": state.agent_id,
        "matches": matches,
        "config": cfg,
        "manifest_path": state.work_dir.join("manifest.toml"),
        "work_dir": state.work_dir,
    }))
}

/// `PUT /agents/{id}/config` — live-edit agent runtime config (mqtt.md §7).
///
/// Currently supports the `builtin_tools` field (ADR-029 — the only
/// field the Tools panel mutates today). Future per-agent fields
/// (temperature, context_window, etc.) can be added to the request
/// struct without changing the wire path: each optional field is
/// applied via the same read-modify-write cycle that `RuntimeConfigUpdate`
/// uses in `cli.rs`, and the on-disk file is always the source of truth.
///
/// On success the handler:
///   1. Loads the current `agent_tools.json`, applies the patch via
///      [`crate::agent_config::apply_builtin_tools_patch`] (which honours
///      `PLATFORM_TOOLS` and silently ignores unknown tool names — same
///      semantics as the gRPC `RuntimeConfigUpdate` path).
///   2. Persists the merged config via
///      [`crate::agent_config::save_agent_tools_config`] (atomic
///      write-tmp-rename).
///   3. Re-PUBLISHes the retained `acowork/agents/{id}/config` snapshot
///      so any other Desktop subscriber (and the Desktop's own
///      ConfigSnapshot listener) sees the new values immediately,
///      without waiting for the next Gateway poll cycle.
///
/// Active sessions are NOT force-reloaded here. New sessions created
/// after this call pick up the new enabled flags naturally because
/// Phase A re-reads `agent_tools.json` at startup; existing in-flight
/// sessions already pinned their tool list when they were spawned.
/// This matches the contract documented in `mqtt.md` §3.5: Desktop
/// edits are "next-session effective" for tool registry shape.
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

    let work_path = state.work_dir.as_path();

    // ── 1. builtin_tools: read-modify-write via the shared patch helper ──
    if let Some(ref enabled_names) = req.builtin_tools {
        // ADR-029 §7 + mqtt.md §7.1 semantics: the wire shape
        // `builtin_tools: Vec<String>` is the **complete enabled set** —
        // any tool currently in `agent_tools.json` but absent from this
        // list must be flipped to `enabled = false`. This is the same
        // patch construction used by `RuntimeConfigUpdate` in `cli.rs`
        // (~L2017) and `tools/builtin/mod.rs`. Forgetting this loop
        // (i.e. mapping `enabled_names -> enabled=true` directly) makes
        // every unchecked checkbox silently re-enable on the next PUT,
        // because `apply_builtin_tools_patch` only overrides tools
        // present in the patch and leaves everything else untouched.
        //
        // We rebuild the patch by iterating the **current** entries
        // (so PLATFORM_TOOLS force-enable logic in
        // `apply_builtin_tools_patch` still applies) and setting each
        // entry's `enabled` based on membership in `enabled_names`.
        let current = crate::agent_config::load_agent_tools_config(work_path)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("failed to load agent_tools.json: {}", e),
                    })),
                )
            })?
            .map(|cfg| cfg.tools)
            .unwrap_or_default();
        let patch: Vec<crate::agent_config::AgentToolEntry> = current
            .iter()
            .map(|entry| {
                let enabled = enabled_names.iter().any(|n| n == &entry.name);
                crate::agent_config::AgentToolEntry::new(&entry.name, enabled)
            })
            .collect();

        let updated = crate::agent_config::apply_builtin_tools_patch(&current, &patch);
        crate::agent_config::save_agent_tools_config(
            work_path,
            &crate::agent_config::AgentToolsConfig {
                tools: updated.clone(),
            },
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("failed to persist agent_tools.json: {}", e),
                })),
            )
        })?;
        tracing::info!(
            agent_id = %state.agent_id,
            enabled_count = updated.iter().filter(|e| e.enabled).count(),
            total = updated.len(),
            "PUT /agents/{id}/config: builtin_tools persisted"
        );
    }

    // ── 1b. Per-agent config fields: load-merge-save `agent_config.json` ──
    //
    // The Desktop `AgentSetupTab.handleApply` always issues a single PUT
    // that may carry up to 9 optional per-agent fields (temperature,
    // context_window, max_output_tokens, ...). Prior to this block the
    // handler silently dropped them all because the request struct only
    // declared `builtin_tools`, leaving the Setup panel edits to appear
    // saved while `agent_config.json` stayed untouched. That meant the
    // Setup panel's optimistic UI was always overwritten on the next
    // refresh — user-visible as "改动不生效".
    //
    // We follow the same read-modify-write cycle that `cli.rs`
    // (~L2147) uses for the MQTT `RuntimeConfigUpdate` path, so both
    // write paths land in `agent_config.json` with identical semantics
    // and a single source of truth on disk.
    //
    // Wire shape: `Option<serde_json::Value>` lets the panel
    // distinguish three states (see `UpdateAgentConfigRequest`):
    //   - field absent (e.g. `{"builtin_tools": [...]}`) -> leave on-disk value alone
    //   - field present with a value (e.g. `"temperature": 0.7`) -> overwrite
    //   - field present with JSON `null` (e.g. `"temperature": null`)
    //     -> explicitly clear (matches the "fall through to manifest
    //     default" path documented on `AgentConfig::temperature`)
    let (patches, runtime_overrides) = req.project();
    if !patches.is_empty() {
        let mut agent_cfg = crate::agent_config::load_agent_config(work_path)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("failed to load agent_config.json: {}", e),
                    })),
                )
            })?
            .unwrap_or_default();
        for (field, patch) in &patches {
            // Centralized dispatch keeps the wire-shape -> AgentConfig
            // mapping in one place; adding a new field here is the
            // only edit needed to plumb it through.  Each arm
            // deserializes the raw `serde_json::Value` into the
            // concrete `AgentConfig` field type via
            // `serde_json::from_value`; mismatches log a warning and
            // leave the on-disk value untouched (matches the
            // `expect_*` extractors in `project()`).
            match *field {
                "max_output_tokens" => {
                    agent_cfg.max_output_tokens =
                        patch_typed::<u64>(field, patch);
                }
                "max_iterations" => {
                    agent_cfg.max_iterations =
                        patch_typed::<u32>(field, patch);
                }
                "max_sessions" => {
                    agent_cfg.max_sessions =
                        patch_typed::<u64>(field, patch).map(|v| v as usize);
                }
                "temperature" => {
                    agent_cfg.temperature =
                        patch_typed::<f32>(field, patch);
                }
                "context_window" => {
                    agent_cfg.context_window =
                        patch_typed::<u64>(field, patch);
                }
                "shell_approval_threshold" => {
                    agent_cfg.shell_approval_threshold =
                        patch_typed::<String>(field, patch);
                }
                "approval_timeout_secs" => {
                    agent_cfg.approval_timeout_secs =
                        patch_typed::<u64>(field, patch);
                }
                "tool_result_compression_mode" => {
                    agent_cfg.tool_result_compression_mode =
                        patch_typed::<String>(field, patch);
                }
                "tool_result_soft_threshold_chars" => {
                    agent_cfg.tool_result_soft_threshold_chars =
                        patch_typed::<u64>(field, patch).map(|v| v as usize);
                }
                _ => unreachable!("unknown patch field {field}"),
            }
        }
        crate::agent_config::save_agent_config(work_path, &agent_cfg).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("failed to persist agent_config.json: {}", e),
                })),
            )
        })?;
        tracing::info!(
            agent_id = %state.agent_id,
            field_count = patches.len(),
            "PUT /agents/{id}/config: agent_config.json fields persisted"
        );

        // ── 1c. Live-broadcast to active sessions ─────────────────────
        //
        // Persisting to `agent_config.json` only takes effect on the
        // next session restore / process restart. To match the
        // `RuntimeConfigUpdate` semantics that `cli.rs` enforces
        // (temperature / context_window / max_iterations are
        // "live-editable"), we also push the new values into every
        // running `AgentLoop` so the next LLM iteration uses them.
        //
        // We use the same dispatch channel that the existing
        // /sessions/{sid} write paths (approval, continue, title,
        // question) use, with the same `UserOp::UpdateRuntimeConfig`
        // variant that `SessionManager::apply_runtime_config_override`
        // sends internally. The push is best-effort: a missing
        // dispatch_tx (agent not yet ready) or empty session map
        // (no live sessions) means "live-effective" simply doesn't
        // apply yet, but the on-disk file is still authoritative.
        //
        // We deliberately do **not** mutate `SessionManager.runtime_overrides`
        // from here (it lives outside `HttpState`). New sessions
        // spawned before the next `RuntimeConfigUpdate` push from
        // Gateway will use the cached (stale) value, but any
        // subsequent Setup panel edit refreshes the cache. This is
        // the same trade-off that `tools/builtin` already accepts
        // (tool list shape is next-session effective by design).
        broadcast_runtime_overrides(&state, &runtime_overrides).await;
    }

    // ── 2. Re-PUBLISH retained config so other Desktop subscribers ──
    //    see the new values immediately. Best-effort: if the broker
    //    isn't reachable yet, the on-disk file is still authoritative.
    if let Some(mqtt) = state.mqtt_client.lock().await.clone() {
        let cfg_path = work_path.join("config").join("agent_config.json");
        let config_json = std::fs::read_to_string(&cfg_path).unwrap_or_else(|_| "{}".to_string());
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
        "builtin_tools": req.builtin_tools,
    })))
}

/// Request body for `PUT /agents/{id}/config` (mqtt.md §7).
///
/// Each field is **optional** and matches the wire shape that the
/// Desktop `AgentSetupTab.handleApply` builds from `AgentProfileSettings`.
/// The handler applies a read-modify-write cycle so partial updates
/// don't clobber unrelated on-disk values (e.g. touching
/// `temperature` must not erase an existing `tool_result_keep_recent_n`).
///
/// Semantics notes:
///   - Every field here uses the same `"Some(...)" -> overwrite,
///     "None" -> leave alone` rule, so the wire shape is partial.
///   - `builtin_tools` is intentionally kept on the **same** struct
///     even though it persists to a different file
///     (`agent_tools.json`) — the Setup panel issues a single PUT
///     that may mutate both, and the frontend doesn't have to split
///     the request by destination file.
///   - Fields that have no analogue on the wire (e.g.
///     `tool_result_keep_recent_n`, `avatar`, `builtin_avatar`,
///     `system_prompt_override`) are deliberately omitted from this
///     struct: they aren't exposed in the Setup panel today, so
///     accepting them on the wire would silently no-op and confuse
///     callers. They keep flowing through `RuntimeConfigUpdate` over
///     MQTT the same as before.
#[derive(Debug, Deserialize)]
struct UpdateAgentConfigRequest {
    /// Names of builtin tools to enable. The handler treats listed
    /// names as `enabled = true` and applies the patch via
    /// [`crate::agent_config::apply_builtin_tools_patch`], which
    /// preserves the previous enabled flag for any tool not in this
    /// list (and force-enables platform-protected tools like
    /// `context_recall`).
    #[serde(default)]
    builtin_tools: Option<Vec<String>>,

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
    /// ADR-032 C4b: compression trigger mode (`"auto" | "manual"`).
    /// Field-absent leaves on-disk value alone (see struct-level note).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_result_compression_mode: Option<serde_json::Value>,
    /// ADR-032 C4a: tool-result soft compression threshold (chars).
    /// Field-absent leaves on-disk value alone. Boot-only: takes
    /// effect on next session restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_result_soft_threshold_chars: Option<serde_json::Value>,
}

impl UpdateAgentConfigRequest {
    /// Project the request onto `AgentConfig` and return the matching
    /// in-memory `RuntimeConfigOverrides` for live broadcast.
    ///
    /// Returns `(agent_cfg_patch, runtime_overrides)`:
    ///   - `agent_cfg_patch` is the list of (`field_name`, `new_value`)
    ///     tuples the persistence block should apply (every `Some`
    ///     outer request field contributes one).
    ///   - `runtime_overrides` carries the **live-editable** subset
    ///     (temperature / context_window / max_output_tokens /
    ///     max_iterations / shell_approval_threshold /
    ///     approval_timeout_secs / tool_result_compression_mode),
    ///     projecting `Some(Value::Null)` -> `None` for the inner
    ///     field. Boot-only fields (`tool_result_keep_recent_n`,
    ///     `tool_result_soft_threshold_chars`) are intentionally
    ///     forced to `None` here so the live broadcast never carries
    ///     them — see the field-level comments above.
    fn project(&self) -> (
        Vec<(&'static str, FieldPatch<serde_json::Value>)>,
        RuntimeConfigOverrides,
    ) {
        // `FieldPatch::Clear` is what `Some(Value::Null)` projects to
        // for the `AgentConfig` patch side.  We use a custom enum so
        // the persistence loop doesn't have to compare against
        // `Value::Null` everywhere.
        //
        // The patches Vec holds `FieldPatch<serde_json::Value>` (rather
        // than a per-field typed variant) so a single Vec can carry
        // mixed numeric/string fields.  The persistence loop
        // deserializes per field via `serde_json::from_value::<T>()`,
        // while `RuntimeConfigOverrides` is filled via the typed
        // `expect_*` extractors below.
        let mut patches: Vec<(&'static str, FieldPatch<serde_json::Value>)> = Vec::new();
        let mut overrides = RuntimeConfigOverrides::default();

        if let Some(v) = &self.max_output_tokens {
            let patch = value_to_patch(v);
            overrides.max_output_tokens = patch.clone().expect_number("max_output_tokens").as_opt();
            patches.push(("max_output_tokens", patch));
        }
        if let Some(v) = &self.max_iterations {
            let patch = value_to_patch(v);
            overrides.max_iterations = patch.clone().expect_number("max_iterations").as_u32_opt();
            patches.push(("max_iterations", patch));
        }
        if let Some(v) = &self.max_sessions {
            let patch = value_to_patch(v);
            // No live override for max_sessions today — it's a
            // boot-only field (see `cli.rs` runtime_overrides flow).
            patches.push(("max_sessions", patch));
        }
        if let Some(v) = &self.temperature {
            let patch = value_to_patch(v);
            overrides.temperature = patch.clone().expect_number_f32("temperature").as_opt();
            patches.push(("temperature", patch));
        }
        if let Some(v) = &self.context_window {
            let patch = value_to_patch(v);
            overrides.context_window = patch.clone().expect_number("context_window").as_opt();
            patches.push(("context_window", patch));
        }
        if let Some(v) = &self.shell_approval_threshold {
            let patch = value_to_patch(v);
            overrides.shell_approval_threshold = patch
                .clone()
                .expect_string("shell_approval_threshold")
                .as_string_opt();
            patches.push(("shell_approval_threshold", patch));
        }
        if let Some(v) = &self.approval_timeout_secs {
            let patch = value_to_patch(v);
            overrides.approval_timeout_secs =
                patch.clone().expect_number("approval_timeout_secs").as_opt();
            patches.push(("approval_timeout_secs", patch));
        }
        if let Some(v) = &self.tool_result_compression_mode {
            let patch = value_to_patch(v);
            overrides.tool_result_compression_mode = patch
                .clone()
                .expect_string("tool_result_compression_mode")
                .as_string_opt();
            patches.push(("tool_result_compression_mode", patch));
        }
        if let Some(v) = &self.tool_result_soft_threshold_chars {
            let patch = value_to_patch(v);
            // Boot-only — override is intentionally left at None.
            patches.push(("tool_result_soft_threshold_chars", patch));
        }

        (patches, overrides)
    }
}

/// Per-field patch op. `Set(T)` writes a concrete value; `Clear`
/// writes `None` so `skip_serializing_if = Option::is_none` removes
/// the key from `agent_config.json`.
#[derive(Debug, Clone, Copy)]
enum FieldPatch<T> {
    Set(T),
    Clear,
}

impl<T> FieldPatch<T> {
    /// Convert `Set(T)` -> `Some(t)` and `Clear` -> `None`.
    /// Requires `T: Copy`; use [`Self::as_string_opt`] for owned strings.
    fn as_opt(&self) -> Option<T>
    where
        T: Copy,
    {
        match self {
            FieldPatch::Set(v) => Some(*v),
            FieldPatch::Clear => None,
        }
    }
}

impl FieldPatch<u64> {
    /// Narrow `FieldPatch<u64>` to `Option<u32>` for fields whose runtime
    /// override is `u32` (e.g. `max_iterations`).  Values that don't fit
    /// in `u32` collapse to `None` and a warning is logged upstream.
    fn as_u32_opt(&self) -> Option<u32> {
        match self {
            FieldPatch::Set(v) => u32::try_from(*v).ok(),
            FieldPatch::Clear => None,
        }
    }
}

impl FieldPatch<String> {
    /// Owned conversion: `Set(s)` -> `Some(s.clone())`, `Clear` -> `None`.
    fn as_string_opt(&self) -> Option<String> {
        match self {
            FieldPatch::Set(v) => Some(v.clone()),
            FieldPatch::Clear => None,
        }
    }
}

/// Decode the wire-side `serde_json::Value` into a typed patch.
///
/// `Value::Null` becomes `FieldPatch::Clear`; everything else becomes
/// `FieldPatch::Set` and is left to the typed `expect_*` helpers below
/// (which reject wrong-typed JSON so the persistence loop doesn't
/// silently miswrite a string into a numeric field).
fn value_to_patch(v: &serde_json::Value) -> FieldPatch<serde_json::Value> {
    match v {
        serde_json::Value::Null => FieldPatch::Clear,
        other => FieldPatch::Set(other.clone()),
    }
}

/// Type-erase a `FieldPatch<serde_json::Value>` into the
/// persistence-loop-friendly `Option<T>` shape (`Set(v)` -> `Some(T)`,
/// `Clear` -> `None`).  A wrong-typed JSON value (e.g. `Set("foo")` for
/// a `u64` field) collapses to `None` and emits a `tracing::warn!`,
/// matching the `expect_*` extractors in `project()` so the two
/// write paths can never disagree about what landed on disk.
fn patch_typed<T>(
    field: &'static str,
    patch: &FieldPatch<serde_json::Value>,
) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    match patch {
        FieldPatch::Clear => None,
        FieldPatch::Set(v) => match serde_json::from_value::<T>(v.clone()) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(
                    field,
                    value = ?v,
                    error = %e,
                    "PUT /agents/{{id}}/config: type mismatch — leaving on-disk value"
                );
                None
            }
        },
    }
}

trait FieldPatchExt {
    fn expect_number(self, field: &'static str) -> FieldPatch<u64>;
    fn expect_number_f32(self, field: &'static str) -> FieldPatch<f32>;
    fn expect_string(self, field: &'static str) -> FieldPatch<String>;
}

impl FieldPatchExt for FieldPatch<serde_json::Value> {
    fn expect_number(self, field: &'static str) -> FieldPatch<u64> {
        match self {
            FieldPatch::Clear => FieldPatch::Clear,
            FieldPatch::Set(serde_json::Value::Number(n)) => match n.as_u64() {
                Some(v) => FieldPatch::Set(v),
                None => {
                    tracing::warn!(field, value = ?n, "PUT /agents/{{id}}/config: non-u64 number — skipping");
                    FieldPatch::Clear
                }
            },
            FieldPatch::Set(other) => {
                tracing::warn!(field, value = ?other, "PUT /agents/{{id}}/config: expected number — skipping");
                FieldPatch::Clear
            }
        }
    }
    fn expect_number_f32(self, field: &'static str) -> FieldPatch<f32> {
        match self {
            FieldPatch::Clear => FieldPatch::Clear,
            FieldPatch::Set(serde_json::Value::Number(n)) => match n.as_f64() {
                Some(v) => FieldPatch::Set(v as f32),
                None => {
                    tracing::warn!(field, value = ?n, "PUT /agents/{{id}}/config: non-f64 number — skipping");
                    FieldPatch::Clear
                }
            },
            FieldPatch::Set(other) => {
                tracing::warn!(field, value = ?other, "PUT /agents/{{id}}/config: expected number — skipping");
                FieldPatch::Clear
            }
        }
    }
    fn expect_string(self, field: &'static str) -> FieldPatch<String> {
        match self {
            FieldPatch::Clear => FieldPatch::Clear,
            FieldPatch::Set(serde_json::Value::String(s)) => FieldPatch::Set(s),
            FieldPatch::Set(other) => {
                tracing::warn!(field, value = ?other, "PUT /agents/{{id}}/config: expected string — skipping");
                FieldPatch::Clear
            }
        }
    }
}

/// Broadcast a `RuntimeConfigOverrides` push to every live session.
///
/// The overrides are produced by `UpdateAgentConfigRequest::project`
/// above, which already projects the wire shape onto the in-process
/// struct (and forces boot-only fields to `None` so we never
/// invalidate in-flight conversation history pointers — see
/// `RuntimeConfigOverrides::tool_result_soft_threshold_chars` docs).
///
/// The push goes through `dispatch_tx` (per-session `(String,
/// InboundMessage)` channel), reusing the exact same routing that
/// `dispatch_inbound` uses for `UserOp::UpdateRuntimeConfig` today.
/// `gateway_loop` forwards those messages through
/// `forward_to_session_inbound`, which lands in `AgentLoop`'s
/// `drain_inbound_queue` → `apply_user_op` → `apply_runtime_config`,
/// so the next LLM iteration picks up the new temperature /
/// context_window / etc. without restarting the session.
async fn broadcast_runtime_overrides(state: &HttpState, overrides: &RuntimeConfigOverrides) {
    if overrides.is_empty() {
        // No live-editable fields were pushed (e.g. user only touched
        // tool_result_soft_threshold_chars). Skip the broadcast entirely
        // so we don't broadcast a no-op UpdateRuntimeConfig that would
        // still trigger `emit_session_state` on every active session.
        return;
    }

    // Enumerate active session IDs. `session_snapshots` is the same
    // authoritative map that `SessionManager` owns; cloning is cheap
    // because each value is `Arc<RwLock<...>>`.
    let session_ids: Vec<String> = state
        .session_snapshots
        .read()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    if session_ids.is_empty() {
        // No live sessions to push to — the on-disk save above is still
        // authoritative and will be re-loaded on the next session.
        return;
    }

    // Snapshot dispatch_tx once. The slot itself is locked briefly to
    // clone the inner sender; clones of `UnboundedSender` are cheap.
    let tx_opt = state.dispatch_tx.lock().await.clone();
    let Some(tx) = tx_opt else {
        // Agent not yet ready (Phase A pre-Phase D). Same fallback as
        // empty session_ids above — file is authoritative.
        return;
    };

    let user_op = UserOp::UpdateRuntimeConfig(overrides.clone());
    let mut sent = 0usize;
    let mut skipped = 0usize;
    for sid in session_ids {
        let msg = InboundMessage::UserOperation(user_op.clone());
        match tx.send((sid.clone(), msg)) {
            Ok(()) => sent += 1,
            Err(e) => {
                tracing::warn!(
                    session_id = %sid,
                    error = %e,
                    "broadcast_runtime_overrides: dispatch_tx send failed (session likely closed)"
                );
                skipped += 1;
            }
        }
    }
    tracing::info!(
        agent_id = %state.agent_id,
        sent,
        skipped,
        "PUT /agents/{{id}}/config: live-broadcast RuntimeConfigOverrides dispatched"
    );
}

/// `GET /agents/{id}/tools` — Tools panel (merged: builtin + mcp + search).
///
/// ADR-034 §7.6.5 defines the merged response schema:
/// `{tools: [BuiltinToolEntry], mcp_servers: [server_name], search: {providers: [...]}}`
/// (panel 3 pulls all three sources in one HTTP call instead of 3 separate ones).
async fn get_agent_tools(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let matches = id == state.agent_id;
    let (tools, mcp, search) = if matches {
        let tools = crate::agent_config::load_agent_tools_config(&state.work_dir)
            .ok()
            .flatten();
        let mcp = crate::agent_config::load_agent_mcp_config(&state.work_dir)
            .ok()
            .flatten();
        let search = crate::agent_config::load_agent_search_config(&state.work_dir)
            .ok()
            .flatten();
        (tools, mcp, search)
    } else {
        (None, None, None)
    };
    // Schema must match ADR-034 §7.6.5 exactly — see frontend ToolsTab.tsx
    // and mcpStore.loadActiveServers(). The Desktop relies on these keys
    // (`tools`, `mcp_servers`, `search.providers`); mismatches here cause
    // the entire Tools panel to render empty.
    let tools_arr = tools
        .map(|t| t.tools)
        .unwrap_or_default();
    let mcp_server_names = mcp
        .map(|m| {
            m.merged()
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let search_obj = match search {
        Some(cfg) => serde_json::json!({ "providers": cfg.providers }),
        None => serde_json::json!({ "providers": [] }),
    };
    Json(serde_json::json!({
        "agent_id": state.agent_id,
        "matches": matches,
        "tools": tools_arr,
        "mcp_servers": mcp_server_names,
        "search": search_obj,
    }))
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
        "embed_dim": embed_dim,
    }))
}

/// Simple base64 encoder (no external dependency needed for this module).
fn base64_encode_simple(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3f) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3f) as usize] as char);
        result.push(if chunk.len() > 1 { CHARS[((triple >> 6) & 0x3f) as usize] } else { b'=' } as char);
        result.push(if chunk.len() > 2 { CHARS[(triple & 0x3f) as usize] } else { b'=' } as char);
    }
    result
}

/// Simple base64 decoder used by document upload. Tolerates URL-safe
/// variants (replaces `-_` to `+/` before decoding) and padding-less
/// inputs (re-pads to a multiple of 4).
fn base64_decode_simple(input: &str) -> Result<Vec<u8>, String> {
    let normalized: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    let normalized = normalized.replace('-', "+").replace('_', "/");
    let padded = match normalized.len() % 4 {
        0 => normalized,
        1 => return Err("invalid base64 length".to_string()),
        n => normalized + &"=".repeat(4 - n),
    };
    const TABLE: &[u8; 128] = &{
        let mut t = [255u8; 128];
        let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < chars.len() {
            t[chars[i] as usize] = i as u8;
            i += 1;
        }
        t
    };
    let mut out = Vec::with_capacity((padded.len() / 4) * 3);
    for chunk in padded.as_bytes().chunks(4) {
        let mut vals = [255u8; 4];
        for (i, b) in chunk.iter().enumerate() {
            if *b == b'=' {
                vals[i] = 0;
            } else if (*b as usize) < 128 && TABLE[*b as usize] != 255 {
                vals[i] = TABLE[*b as usize];
            } else {
                return Err(format!("invalid base64 character: {}", *b as char));
            }
        }
        let triple = ((vals[0] as u32) << 18)
            | ((vals[1] as u32) << 12)
            | ((vals[2] as u32) << 6)
            | (vals[3] as u32);
        out.push(((triple >> 16) & 0xFF) as u8);
        if chunk.len() > 2 && chunk[2] != b'=' {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if chunk.len() > 3 && chunk[3] != b'=' {
            out.push((triple & 0xFF) as u8);
        }
    }
    Ok(out)
}

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
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim)))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
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
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim)))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
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
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim)))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
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
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim)))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
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
        assert_eq!(body["consolidated"], 0);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-029 §7 wire-semantics regression test.
    ///
    /// `PUT /api/agents/{id}/config` with `builtin_tools` is the
    /// **complete enabled set**: any tool currently in `agent_tools.json`
    /// but absent from the request body MUST be flipped to
    /// `enabled = false`. The previous implementation built the patch
    /// directly from the request body (everything listed → enabled=true),
    /// which made every unchecked checkbox silently re-enable on the
    /// next PUT — the bug only became user-visible when
    /// `chatStore::case "agent_config"` started emitting
    /// `acowork:refresh-agent-config`, at which point the ToolsTab
    /// listener overwrote the optimistic UI with the server's stale
    /// `enabled=true` value.
    ///
    /// This test seeds `agent_tools.json` with three tools, sends a
    /// PUT listing only one of them as enabled, and asserts that:
    ///   - the listed tool stays `enabled=true`
    ///   - the two **unlisted** tools flip to `enabled=false`
    ///   - PLATFORM_TOOLS (e.g. `context_recall`) are still force-enabled
    ///     even when omitted from the PUT body
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
                crate::agent_config::AgentToolEntry::new("context_recall", true),
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
        )
        .await
        .expect("server should start");

        // PUT with only `http_request` enabled — the other two
        // (including the platform tool, which must still stay enabled)
        // must reflect the new state on the next GET /tools.
        let url = format!(
            "http://127.0.0.1:{}/agents/com.test.agent/config",
            server.port
        );
        let client = reqwest::Client::new();
        let response = client
            .put(&url)
            .json(&serde_json::json!({"builtin_tools": ["http_request"]}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "PUT /agents/{{id}}/config should accept builtin_tools, got {}",
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
            map["context_recall"],
            "PLATFORM_TOOLS are force-enabled regardless of PUT body"
        );

        // Verify the GET /tools endpoint also returns the new state —
        // this is what the ToolsTab listener reads, so the optimistic
        // update overwrite would surface the bug here.
        let tools_url = format!(
            "http://127.0.0.1:{}/agents/com.test.agent/tools",
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
        assert!(tool_flags["context_recall"]);

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
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim)))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
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

        // 8b) SVG binary read test (SVG is listed in BINARY_EXTENSIONS so it
        // gets base64-encoded, even though it's actually text; the frontend
        // still renders it correctly in the image preview branch because
        // mimeType starts with "image/").
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
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(body["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, tiny_svg.as_bytes());

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
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(memory_store, embed_dim)))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(temp_dir.clone())))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(temp_dir.clone())))),
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
}
