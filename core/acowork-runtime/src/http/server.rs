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

use crate::agent::inbound::InboundMessage;
use crate::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use crate::conversation::{
    read_messages_paginated, scan_sessions_from_meta, ConversationEntry,
};
use crate::http::memory_query;

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

/// Shared embedding-provider dimension (0 = no provider).
///
/// Surfaced by the memory-stats endpoint as `model_dim` so the desktop
/// can detect dimension mismatches with the persisted HNSW index.
pub type SharedEmbedDimension = Arc<std::sync::RwLock<u64>>;

/// State shared with HTTP handlers.
#[derive(Clone)]
struct HttpState {
    work_dir: PathBuf,
    agent_id: String,
    /// Shared map of per-session state snapshots, keyed by session_id.
    /// Each value is the same `Arc<RwLock<SessionStateSnapshot>>` held
    /// by `SessionHandle`, so reads are always up-to-date.
    session_snapshots: SharedSessionSnapshots,
    /// Shared latest session info, updated by SessionManager on every
    /// session creation and startup scan.  Read by `get_latest_session`.
    latest_session: SharedLatestSession,
    /// Dispatch sender for write operations (approval, question, continue, title).
    /// Set after the session manager starts. None = agent not ready yet.
    #[allow(dead_code)]
    dispatch_tx: SharedDispatchSender,
    /// Late-bound memory store. Populated by Phase B after the HTTP
    /// server is already listening. Memory handlers return a
    /// stable empty response when this is still `None`.
    memory_store: SharedMemoryStore,
    /// Active embedding provider dimension. Set once at Phase A
    /// (from AgentHelloConfig) and read by the stats endpoint.
    embed_provider_dim: SharedEmbedDimension,
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
    pub async fn start(
        work_dir: PathBuf,
        agent_id: String,
        session_snapshots: SharedSessionSnapshots,
        latest_session: SharedLatestSession,
        dispatch_tx: SharedDispatchSender,
        memory_store: SharedMemoryStore,
        embed_provider_dim: SharedEmbedDimension,
    ) -> Result<Self, RuntimeHttpServerError> {
        let state = HttpState {
            work_dir,
            agent_id,
            session_snapshots,
            latest_session,
            dispatch_tx,
            memory_store,
            embed_provider_dim,
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
            .route("/memory/nodes", get(get_memory_nodes))
            // New GET on the same path that already has DELETE.
            .route(
                "/memory/nodes/{nid}",
                get(get_memory_node).delete(delete_memory_node),
            )
            .route("/memory/stats", get(get_memory_stats))
            .route("/memory/consolidate", post(trigger_consolidate))
            .route("/files/{id}", get(get_file))
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
            // 3 NEW agent panel endpoints (panels 1/3/5).
            .route("/agents/{id}/config", get(get_agent_config))
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
}

async fn health(State(state): State<HttpState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        agent_id: state.agent_id,
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
/// This is the backend for `GET /api/agents/{id}/sessions` via the
/// Gateway reverse proxy.
async fn list_sessions(
    State(state): State<HttpState>,
    Query(query): Query<ListSessionsQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conversations_dir = state.work_dir.join("conversations");
    let page = query.page.unwrap_or(1).max(1);
    let size = query.size.unwrap_or(20).clamp(1, 200);

    let scanned = scan_sessions_from_meta(&conversations_dir);
    let total_count = scanned.len() as u32;
    let total_pages = if total_count == 0 {
        0
    } else {
        total_count.div_ceil(size)
    };

    let page_start = ((page - 1) as usize) * (size as usize);
    let page_end = (page_start + size as usize).min(scanned.len());

    let page_sessions: Vec<serde_json::Value> = scanned
        .into_iter()
        .skip(page_start)
        .take(page_end.saturating_sub(page_start))
        .map(|(session_id, meta)| {
            serde_json::json!({
                "session_id": session_id,
                "title": meta.title,
                "created_at": meta.created_at,
                "last_active_at": meta.last_active_at,
                "message_count": meta.message_count,
                "workspace_id": meta.workspace_id,
                "model": meta.model,
                "provider": meta.provider,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "sessions": page_sessions,
        "total_count": total_count,
        "total_pages": total_pages,
        "page": page,
        "size": size,
    })))
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
async fn get_memory_graph(State(state): State<HttpState>) -> Json<serde_json::Value> {
    let store = state.memory_store.read().ok().and_then(|g| g.clone());

    // size=100000 is the legacy panel's cap and matches
    // MAX_UNFILTERED_MEMORY_SCAN semantics; we don't need to honour the
    // ADR-033 unfiltered-scan guard here because the Memory panel is
    // operator-facing and load-bearing failures are surfaced through
    // the index_health field of /memory/stats instead.
    let params = memory_query::ListNodesParams {
        page: 1,
        size: 100_000,
        node_type: String::new(),
        keyword: String::new(),
        time_range: "all".to_string(),
    };
    let out = memory_query::list_nodes(store.as_ref(), params);

    let mut nodes: Vec<serde_json::Value> = Vec::with_capacity(out.nodes.len());
    for node in &out.nodes {
        // Promote the lightweight list view into a node-shaped JSON
        // entry that the desktop can drop straight onto the canvas.
        // The `properties` field is omitted here to keep the payload
        // small — the Memory panel fetches detail on demand via
        // `GET /memory/nodes/{nid}` (added in Phase 3).
        nodes.push(serde_json::json!({
            "node_id": node.node_id,
            "node_type": node.node_type,
            "content": node.content,
            "confidence": node.confidence,
            "decay_score": node.decay_score,
            "created_at": node.created_at,
            "last_accessed_at": node.last_accessed_at,
            "access_count": node.access_count,
            "status": node.status,
        }));
    }

    Json(serde_json::json!({
        "agent_id": state.agent_id,
        "node_count": nodes.len(),
        "nodes": nodes,
        "edges": [],
    }))
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
    let params = memory_query::ListNodesParams {
        page: params.page.unwrap_or(1),
        size: params.size.unwrap_or(20),
        node_type: params.node_type.unwrap_or_default(),
        keyword: params.keyword.unwrap_or_default(),
        time_range: params.time_range.unwrap_or_default(),
    };
    let store = state.memory_store.read().ok().and_then(|g| g.clone());
    let out = memory_query::list_nodes(store.as_ref(), params);
    Ok(Json(memory_query::list_output_to_json(&out)))
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
    let store = state.memory_store.read().ok().and_then(|g| g.clone());
    let out = memory_query::get_node(store.as_ref(), node_id);
    // Translate "Node not found" into 404; a "no store" response is
    // still 200 with `found = false` so the desktop can distinguish
    // "cold start" from "gone".
    if !out.found && out.message == "Node not found" {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(memory_query::get_output_to_json(&out)))
}

/// `GET /memory/stats` — memory statistics.
async fn get_memory_stats(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let store = state.memory_store.read().ok().and_then(|g| g.clone());
    let dim = state
        .embed_provider_dim
        .read()
        .map(|d| *d)
        .unwrap_or(0);
    let out = memory_query::get_stats(store.as_ref(), dim);
    Ok(Json(memory_query::stats_output_to_json(&out)))
}

/// `DELETE /memory/nodes/{nid}` — delete a memory node.
async fn delete_memory_node(
    State(state): State<HttpState>,
    Path(nid): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let node_id: u64 = nid.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let store = state.memory_store.read().ok().and_then(|g| g.clone());
    let out = memory_query::delete_node(store.as_ref(), node_id);
    Ok(Json(memory_query::delete_output_to_json(&out)))
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
    let store = state.memory_store.read().ok().and_then(|g| g.clone());
    let out = memory_query::trigger_consolidate(store.as_ref(), body.force, body.retention_days);
    Ok(Json(memory_query::consolidate_output_to_json(&out)))
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
/// 2. **`SharedSessionSnapshots`** — live `SessionStateSnapshot`
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

    // ── 2. Read live snapshot (best-effort; `None` for cold sessions).
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
                "workspace_id": snap.workspace_id,
                "ratio": snap.ratio,
                "reasoning_effort": snap.reasoning_effort,
                "temperature": snap.temperature,
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

/// `GET /files/{id}` — file content.
///
/// Serves files from the Runtime's workspace. The `id` is a relative
/// path within the workspace (sanitized to prevent path traversal).
/// Returns text content for text files, base64-encoded content for binary
/// files (images, PDFs, etc.).
async fn get_file(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Sanitize: only allow alphanumeric + / + . + _ + - in the file id
    if !id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '/' || c == '.' || c == '_' || c == '-')
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Prevent path traversal
    if id.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }

    let file_path = state.work_dir.join(&id);

    if !file_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let _metadata = std::fs::metadata(&file_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let extension = file_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Determine content type and encoding based on file extension.
    let is_text = matches!(
        extension.as_str(),
        "txt" | "md" | "json" | "yaml" | "yml" | "toml" | "xml" | "html"
            | "css" | "js" | "ts" | "rs" | "py" | "go" | "java" | "c" | "cpp"
            | "h" | "sh" | "bat" | "ps1" | "log" | "csv" | "env" | "cfg"
            | "ini" | "sql" | "r" | "rb" | "lua" | "swift" | "kt" | "scala"
            | "vue" | "svelte" | "tsx" | "jsx" | "proto" | "graphql"
    );

    if is_text {
        let content = std::fs::read_to_string(&file_path)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        Ok(Json(serde_json::json!({
            "path": id,
            "content": content,
            "content_type": format!("text/{}; charset=utf-8", extension),
            "size": content.len(),
            "encoding": "utf-8",
        })))
    } else {
        let content = std::fs::read(&file_path)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let content_type = match extension.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "webp" => "image/webp",
            "ico" => "image/x-icon",
            "bmp" => "image/bmp",
            "pdf" => "application/pdf",
            "zip" => "application/zip",
            "gz" | "gzip" => "application/gzip",
            "tar" => "application/x-tar",
            "mp4" => "video/mp4",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            "ttf" => "font/ttf",
            "otf" => "font/otf",
            _ => "application/octet-stream",
        };

        // Use simple base64 encoding for binary files.
        let encoded = base64_encode_simple(&content);

        Ok(Json(serde_json::json!({
            "path": id,
            "content": encoded,
            "content_type": content_type,
            "size": content.len(),
            "encoding": "base64",
        })))
    }
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

// ── Workspace Handlers ─────────────────────────────────────────────────────

/// `GET /workspaces` — list workspace directories from agent_workspaces.json.
async fn list_workspaces(
    State(state): State<HttpState>,
) -> Json<serde_json::Value> {
    let config_path = state.work_dir.join("config").join("agent_workspaces.json");

    let workspaces = if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(val) => val
                    .get("additional_dirs")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        }
    } else {
        vec![]
    };

    Json(serde_json::json!({
        "agent_id": state.agent_id,
        "workspaces": workspaces,
    }))
}

// ── Workspace Tree Types ─────────────────────────────────────────────

/// Query parameters for `GET /workspaces/tree`.
#[derive(Deserialize)]
struct TreeQuery {
    workspace_id: Option<String>,
    path: Option<String>,
}

/// Response for `GET /workspaces/tree`.
#[derive(Serialize)]
struct TreeResponse {
    root: String,
    path: String,
    entries: Vec<TreeEntry>,
}

/// A single directory entry in the tree response.
#[derive(Serialize)]
struct TreeEntry {
    name: String,
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    children_count: Option<usize>,
}

/// Resolve a workspace root path from agent_workspaces.json.
/// Falls back to agent_home if workspace_id not found or config missing.
fn resolve_workspace_root(work_dir: &std::path::Path, workspace_id: Option<&str>) -> PathBuf {
    let ws_id = match workspace_id {
        Some(id) if !id.is_empty() && id != "__agent_home__" => id,
        _ => return work_dir.to_path_buf(),
    };

    let config_path = work_dir.join("config").join("agent_workspaces.json");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return work_dir.to_path_buf(),
    };

    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return work_dir.to_path_buf(),
    };

    if let Some(dirs) = val.get("additional_dirs").and_then(|v| v.as_array()) {
        for dir in dirs {
            if let Some(path) = dir.get("id").and_then(|v| v.as_str()).filter(|id| *id == ws_id)
                .and_then(|_| dir.get("path").and_then(|v| v.as_str()))
            {
                return PathBuf::from(path);
            }
        }
    }

    work_dir.to_path_buf()
}

/// `GET /workspaces/tree` — list directory contents for a workspace.
async fn list_tree(
    State(state): State<HttpState>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<TreeResponse>, (StatusCode, Json<serde_json::Value>)> {
    let workspace_root = resolve_workspace_root(&state.work_dir, query.workspace_id.as_deref());

    let requested_path = query.path.as_deref().unwrap_or("");

    // Build absolute path and canonicalize for security
    let abs_path = if requested_path.is_empty() {
        workspace_root.clone()
    } else {
        workspace_root.join(requested_path)
    };

    let canonical_root = std::fs::canonicalize(&workspace_root)
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let canonical_abs = std::fs::canonicalize(&abs_path)
        .unwrap_or_else(|_| abs_path.to_path_buf());

    // Prevent path traversal
    if !canonical_abs.starts_with(&canonical_root) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Path traversal detected"})),
        ));
    }

    // Compute relative path
    let rel_path = canonical_abs
        .strip_prefix(&canonical_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // Read directory
    let read_dir = match std::fs::read_dir(&canonical_abs) {
        Ok(rd) => rd,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to read directory: {}", e)})),
            ));
        }
    };

    let root_str = canonical_root.to_string_lossy().replace('\\', "/");
    let mut dirs: Vec<TreeEntry> = Vec::new();
    let mut files: Vec<TreeEntry> = Vec::new();

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }

        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());

        if is_dir {
            let children_count = std::fs::read_dir(entry.path())
                .ok()
                .map(|rd| {
                    rd.filter(|e| {
                        e.as_ref()
                            .map(|e| !e.file_name().to_string_lossy().starts_with('.'))
                            .unwrap_or(false)
                    })
                    .count()
                })
                .unwrap_or(0);

            dirs.push(TreeEntry {
                name,
                entry_type: "directory".to_string(),
                size: None,
                modified: metadata.and_then(|m| {
                    m.modified().ok().and_then(|t| {
                        t.duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .ok()
                            .map(|d| {
                                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                                    .map(|dt| dt.to_rfc3339())
                                    .unwrap_or_default()
                            })
                    })
                }),
                children_count: Some(children_count),
            });
        } else {
            files.push(TreeEntry {
                name,
                entry_type: "file".to_string(),
                size: metadata.as_ref().map(|m| m.len()),
                modified: metadata.and_then(|m| {
                    m.modified().ok().and_then(|t| {
                        t.duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .ok()
                            .map(|d| {
                                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                                    .map(|dt| dt.to_rfc3339())
                                    .unwrap_or_default()
                            })
                    })
                }),
                children_count: None,
            });
        }
    }

    dirs.sort_by_key(|a| a.name.to_lowercase());
    files.sort_by_key(|a| a.name.to_lowercase());
    let mut entries = dirs;
    entries.append(&mut files);

    Ok(Json(TreeResponse {
        root: root_str,
        path: rel_path,
        entries,
    }))
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

// ── Workspace mutation handlers (ADR-034 §11.2 #18-21) ───────────────
//
// All four routes read-modify-write `agent_workspaces.json` atomically.
// The schema is `{ version, additional_dirs: [{id, path, access, ...}] }`
// — Phase 2 (delete/§5.4.4) established this shape; mutation handlers
// preserve forward compatibility by keeping unknown fields untouched.

/// On-disk schema for `agent_workspaces.json`. The free-form
/// `additional_dirs` entries are kept as raw JSON so newly-added
/// fields (e.g. `display_name`, `tags`) survive round trips without
/// requiring a crate bump.
#[derive(Serialize, Deserialize)]
struct WorkspacesConfig {
    #[serde(default = "default_workspaces_version")]
    version: u32,
    #[serde(default)]
    additional_dirs: Vec<serde_json::Value>,
}

fn default_workspaces_version() -> u32 {
    1
}

fn workspaces_config_path(work_dir: &std::path::Path) -> PathBuf {
    work_dir.join("config").join("agent_workspaces.json")
}

fn load_workspaces_config(work_dir: &std::path::Path) -> WorkspacesConfig {
    let path = workspaces_config_path(work_dir);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<WorkspacesConfig>(&s).ok())
        .unwrap_or_else(|| WorkspacesConfig {
            version: 1,
            additional_dirs: Vec::new(),
        })
}

fn save_workspaces_config(
    work_dir: &std::path::Path,
    cfg: &WorkspacesConfig,
) -> Result<(), String> {
    let dir = work_dir.join("config");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create config dir: {}", e))?;
    let path = workspaces_config_path(work_dir);
    let tmp = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("serialize workspaces config: {}", e))?;
    std::fs::write(&tmp, &json)
        .map_err(|e| format!("write tmp: {}", e))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("rename tmp: {}", e))?;
    Ok(())
}

/// One entry submitted by a workspace mutation request.
#[derive(Deserialize)]
struct WorkspaceEntryInput {
    id: String,
    path: String,
    access: String,
    #[serde(default)]
    prompt_file: Option<String>,
    #[serde(default)]
    last_active: Option<bool>,
}

/// `POST /workspaces` — add a new workspace entry.
async fn create_workspace(
    State(state): State<HttpState>,
    Json(body): Json<WorkspaceEntryInput>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut cfg = load_workspaces_config(&state.work_dir);
    if cfg.additional_dirs.iter().any(|d| {
        d.get("id").and_then(|v| v.as_str()) == Some(&body.id)
    }) {
        return Err(StatusCode::CONFLICT);
    }
    let entry = serde_json::json!({
        "id": body.id,
        "path": body.path,
        "access": body.access,
        "prompt_file": body.prompt_file,
        "last_active": body.last_active.unwrap_or(false),
    });
    cfg.additional_dirs.push(entry.clone());
    save_workspaces_config(&state.work_dir, &cfg)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "created": true,
        "entry": entry,
    })))
}

/// `PUT /workspaces/{ws_id}` — update an existing workspace entry.
async fn update_workspace(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
    Json(body): Json<WorkspaceEntryInput>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut cfg = load_workspaces_config(&state.work_dir);
    let entry = cfg
        .additional_dirs
        .iter_mut()
        .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(&ws_id))
        .ok_or(StatusCode::NOT_FOUND)?;
    entry["id"] = serde_json::json!(body.id);
    entry["path"] = serde_json::json!(body.path);
    entry["access"] = serde_json::json!(body.access);
    if let Some(pf) = &body.prompt_file {
        entry["prompt_file"] = serde_json::json!(pf);
    }
    if let Some(la) = body.last_active {
        entry["last_active"] = serde_json::json!(la);
    }
    let updated = entry.clone();
    save_workspaces_config(&state.work_dir, &cfg)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "updated": true,
        "entry": updated,
    })))
}

/// `PUT /workspaces/{ws_id}/prompt-file` — set the prompt file path on an entry.
async fn set_workspace_prompt_file(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
    Json(body): Json<PromptFileBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut cfg = load_workspaces_config(&state.work_dir);
    let entry = cfg
        .additional_dirs
        .iter_mut()
        .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(&ws_id))
        .ok_or(StatusCode::NOT_FOUND)?;
    entry["prompt_file"] = serde_json::json!(body.prompt_file);
    save_workspaces_config(&state.work_dir, &cfg)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "updated": true,
        "ws_id": ws_id,
        "prompt_file": body.prompt_file,
    })))
}

/// `DELETE /workspaces/{ws_id}` — remove a workspace entry.
async fn delete_workspace(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut cfg = load_workspaces_config(&state.work_dir);
    let initial_len = cfg.additional_dirs.len();
    cfg.additional_dirs
        .retain(|d| d.get("id").and_then(|v| v.as_str()) != Some(&ws_id));
    if cfg.additional_dirs.len() == initial_len {
        return Err(StatusCode::NOT_FOUND);
    }
    save_workspaces_config(&state.work_dir, &cfg)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "deleted": true,
        "ws_id": ws_id,
    })))
}

#[derive(Deserialize)]
struct PromptFileBody {
    prompt_file: Option<String>,
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

        let memory_store: SharedMemoryStore =
            std::sync::Arc::new(std::sync::RwLock::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            memory_store,
            embed_dim,
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

        let memory_store: SharedMemoryStore =
            std::sync::Arc::new(std::sync::RwLock::new(None));
        let embed_dim: SharedEmbedDimension = std::sync::Arc::new(std::sync::RwLock::new(0));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            memory_store,
            embed_dim,
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

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            "com.test.agent".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            memory_store,
            embed_dim,
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

        // /memory/stats should report no_store but surface model_dim.
        let url = format!("http://127.0.0.1:{}/memory/stats", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["index_health"], "no_store");
        assert_eq!(body["model_dim"], 512);

        // DELETE /memory/nodes/{nid} should report graceful error.
        let url = format!("http://127.0.0.1:{}/memory/nodes/12345", server.port);
        let response = reqwest::Client::new()
            .delete(&url)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["deleted"], false);
        assert_eq!(body["message"], "Memory store not available");

        // POST /memory/consolidate should report graceful error.
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
        assert_eq!(body["message"], "Memory store not available");

        std::fs::remove_dir_all(&temp_dir).ok();
    }
}
