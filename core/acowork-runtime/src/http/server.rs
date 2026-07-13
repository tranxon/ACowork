//! Runtime localhost HTTP server (ADR-033 Phase 2).
//!
//! Serves queries and write operations for the Gateway reverse proxy:
//!
//! ```text
//! GET  /sessions                          — full session list
//! GET  /sessions/latest                   — latest session info
//! GET  /sessions/{sid}/messages           — full message list for a session
//! GET  /sessions/{sid}/state              — session state snapshot
//! POST /sessions/{sid}/approval           — relay approval decision to agent loop
//! POST /sessions/{sid}/question           — relay question answer to agent loop
//! POST /sessions/{sid}/continue           — relay continue execution to agent loop
//! PUT  /sessions/{sid}/title              — update session title
//! GET  /memory/graph                      — full memory graph
//! GET  /memory/nodes                      — paginated memory node list (Grafeo)
//! GET  /memory/stats                      — memory statistics + index diagnostics (Grafeo)
//! DELETE /memory/nodes/{nid}              — delete a memory node (Grafeo)
//! POST /memory/consolidate                — trigger offline consolidation (Grafeo)
//! GET  /files/{id}                        — file content
//! GET  /health                            — health check
//! ```
//!
//! The four `/memory/*` endpoints (excluding `/memory/graph`, which
//! is a placeholder reading JSONL files) share their business logic
//! with the legacy gRPC path through
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
    routing::{delete, get, post, put},
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

        let app = Router::new()
            .route("/health", get(health))
            .route("/sessions", get(list_sessions))
            .route("/sessions/latest", get(get_latest_session))
            .route("/sessions/{sid}/messages", get(get_messages))
            .route("/sessions/{sid}/state", get(get_session_state))
            .route("/sessions/{sid}/approval", post(handle_approval))
            .route("/sessions/{sid}/question", post(handle_question))
            .route("/sessions/{sid}/continue", post(handle_continue))
            .route("/sessions/{sid}/title", put(handle_update_title))
            .route("/memory/graph", get(get_memory_graph))
            .route("/memory/nodes", get(get_memory_nodes))
            .route("/memory/stats", get(get_memory_stats))
            .route("/memory/nodes/{nid}", delete(delete_memory_node))
            .route("/memory/consolidate", post(trigger_consolidate))
            .route("/files/{id}", get(get_file))
            .route("/workspaces", get(list_workspaces))
            .route("/workspaces/tree", get(list_tree))
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
    /// Pagination cursor (opaque — produced by previous response).
    #[serde(default)]
    cursor: Option<String>,
    /// Maximum number of entries to return (default 50, capped at 500).
    #[serde(default)]
    limit: Option<u32>,
    /// `"backward"` (older, default) or `"forward"` (newer).
    #[serde(default)]
    direction: Option<String>,
}

/// `GET /sessions/{sid}/messages` — paginated message list for a session.
///
/// Delegates to [`crate::conversation::read_messages_paginated`], which is
/// the same backend used by the legacy gRPC path. Supports `cursor` /
/// `limit` / `direction` query parameters; returns 404 when the session
/// JSONL file does not exist under `workspace/conversations/`.
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

    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let direction = query.direction.as_deref().unwrap_or("backward");

    let paginated = read_messages_paginated(
        &session_path,
        query.cursor.clone(),
        limit,
        direction,
    )
    .map_err(|e| {
        tracing::warn!(
            session_id = %sid,
            error = %e,
            "Failed to read session messages"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // `ConversationEntry` already implements Serialize with the exact
    // shape the desktop expects (`id`, `ts`, `role`, `content`,
    // `metadata?`, `kind?`). Use the typed struct so a future field
    // added to the JSONL format surfaces as a clear compile error here
    // rather than a silent JSON-shape drift.
    let messages: Vec<ConversationEntry> = paginated.messages;
    let messages_value = serde_json::to_value(&messages)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let count = messages.len();

    // ADR-021 hint for the frontend PollingManager: total committed lines
    // (approximate — counts non-empty lines in the JSONL file). The gRPC
    // path uses the writer-thread atomic counter, but the HTTP state
    // currently does not have access to it; an accurate count is not
    // required for cursor-based pagination to work correctly.
    let total_lines = std::fs::read_to_string(&session_path)
        .map(|content| content.lines().filter(|l| !l.is_empty()).count() as u64)
        .unwrap_or(0);

    Ok(Json(serde_json::json!({
        "session_id": sid,
        "messages": messages_value,
        "count": count,
        "has_more": paginated.has_more,
        "cursor": paginated.cursor,
        "total_lines": total_lines,
    })))
}

/// `GET /memory/graph` — full memory graph.
///
/// Returns the memory graph data from the Runtime's memory store.
///
/// TODO(ADR-033): Phase 2 — replace JSONL file reading with Grafeo query API.
/// Currently reads `.jsonl` files in the `memory/` directory as a fallback.
/// Once Grafeo engine is integrated, this should call `grafeo::query_graph()`
/// to get the real-time memory graph structure including nodes and edges.
async fn get_memory_graph(State(state): State<HttpState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let memory_path = state.work_dir.join("memory");
    
    let mut nodes: Vec<serde_json::Value> = Vec::new();
    if memory_path.exists() {
        // Read all .jsonl files in the memory directory
        if let Ok(entries) = std::fs::read_dir(&memory_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    for line in content.lines().filter(|l| !l.is_empty()) {
                        if let Ok(node) = serde_json::from_str::<serde_json::Value>(line) {
                            nodes.push(node);
                        }
                    }
                }
            }
        }
    }

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

/// `GET /sessions/{sid}/state` — session state snapshot.
///
/// Reads the session state snapshot from the shared map populated by
/// SessionManager. The snapshot is always up-to-date because it shares
/// the same `Arc<RwLock<SessionStateSnapshot>>` with `SessionHandle`.
async fn get_session_state(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let snapshots = state.session_snapshots.read().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let snapshot_arc = snapshots.get(&sid).ok_or(StatusCode::NOT_FOUND)?;
    let snapshot = snapshot_arc.read().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Parse JSON-string fields into JSON values.
    let status: serde_json::Value =
        serde_json::from_str(&snapshot.status_json).unwrap_or(serde_json::Value::Null);
    let todos: Option<serde_json::Value> = snapshot
        .todos_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let context_usage: Option<serde_json::Value> = snapshot
        .context_usage_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());

    Ok(Json(serde_json::json!({
        "session_id": snapshot.session_id,
        "status": status,
        "model": snapshot.model,
        "provider": snapshot.provider,
        "workspace_id": snapshot.workspace_id,
        "ratio": snapshot.ratio,
        "reasoning_effort": snapshot.reasoning_effort,
        "temperature": snapshot.temperature,
        "todos": todos,
        "context_usage": context_usage,
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

// ── Write operation handlers (ADR-033 Phase 3) ────────────────────────

/// Request body for approval decision.
#[derive(Debug, Deserialize)]
struct ApprovalBody {
    request_id: String,
    action: String,
    /// Optional session_id for context (not always required in approval flow).
    #[allow(dead_code)]
    #[serde(default)]
    session_id: Option<String>,
}

/// Request body for question answer.
#[derive(Debug, Deserialize)]
struct QuestionBody {
    request_id: String,
    answer: String,
    /// Optional session_id for context.
    #[allow(dead_code)]
    session_id: Option<String>,
}

/// Request body for continue execution.
#[derive(Debug, Deserialize)]
struct ContinueBody {
    #[serde(default)]
    #[allow(dead_code)]
    session_id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Request body for update session title.
#[derive(Debug, Deserialize)]
struct UpdateTitleBody {
    title: String,
}

/// Helper: send an InboundMessage through the dispatch channel.
fn try_dispatch(
    dispatch_tx: &SharedDispatchSender,
    session_id: &str,
    msg: InboundMessage,
) -> Result<(), StatusCode> {
    let tx_guard = dispatch_tx
        .try_lock()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    match tx_guard.as_ref() {
        Some(tx) => {
            tx.send((session_id.to_string(), msg))
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            Ok(())
        }
        None => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

/// `POST /sessions/{sid}/approval` — relay approval decision to agent loop.
async fn handle_approval(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    Json(body): Json<ApprovalBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    tracing::info!(
        session_id = %sid,
        request_id = %body.request_id,
        action = %body.action,
        "Runtime HTTP: approval decision received"
    );

    let approved = matches!(body.action.as_str(), "allow" | "allow_all_session");
    let allow_all_session = body.action == "allow_all_session";

    let msg = InboundMessage::ApprovalDecision {
        request_id: body.request_id.clone(),
        approved,
        allow_all_session,
        reason: if approved { None } else { Some(format!("User denied: {}", body.action)) },
    };

    try_dispatch(&state.dispatch_tx, &sid, msg)?;

    Ok(Json(serde_json::json!({
        "request_id": body.request_id,
        "action": body.action,
        "status": "resolved",
    })))
}

/// `POST /sessions/{sid}/question` — relay question answer to agent loop.
async fn handle_question(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    Json(body): Json<QuestionBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    tracing::info!(
        session_id = %sid,
        request_id = %body.request_id,
        answer_preview = %body.answer.chars().take(80).collect::<String>(),
        "Runtime HTTP: question answer received"
    );

    let msg = InboundMessage::QuestionAnswer {
        request_id: body.request_id.clone(),
        answer: body.answer.clone(),
    };

    try_dispatch(&state.dispatch_tx, &sid, msg)?;

    Ok(Json(serde_json::json!({
        "request_id": body.request_id,
        "status": "resolved",
    })))
}

/// `POST /sessions/{sid}/continue` — relay continue execution to agent loop.
async fn handle_continue(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    Json(body): Json<ContinueBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    tracing::info!(
        session_id = %sid,
        reason = %body.reason.as_deref().unwrap_or("user_requested"),
        "Runtime HTTP: continue execution received"
    );

    let msg = InboundMessage::ContinueExecution {
        reason: body.reason.unwrap_or_else(|| "user_requested".to_string()),
    };

    try_dispatch(&state.dispatch_tx, &sid, msg)?;

    Ok(Json(serde_json::json!({
        "status": "continued",
        "session_id": sid,
    })))
}

/// `PUT /sessions/{sid}/title` — update session title.
async fn handle_update_title(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    Json(body): Json<UpdateTitleBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    tracing::info!(
        session_id = %sid,
        title = %body.title,
        "Runtime HTTP: update session title"
    );

    // Update title via SystemNotification to the session's agent loop.
    let msg = InboundMessage::SystemNotification {
        notification_type: "update_session_title".to_string(),
        data: serde_json::json!({
            "session_id": sid,
            "title": body.title,
        }),
    };

    try_dispatch(&state.dispatch_tx, &sid, msg)?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "session_id": sid,
    })))
}

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
fn resolve_workspace_root(work_dir: &PathBuf, workspace_id: Option<&str>) -> PathBuf {
    let ws_id = match workspace_id {
        Some(id) if !id.is_empty() && id != "__agent_home__" => id,
        _ => return work_dir.clone(),
    };

    let config_path = work_dir.join("config").join("agent_workspaces.json");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return work_dir.clone(),
    };

    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return work_dir.clone(),
    };

    if let Some(dirs) = val.get("additional_dirs").and_then(|v| v.as_array()) {
        for dir in dirs {
            if dir.get("id").and_then(|v| v.as_str()) == Some(ws_id) {
                if let Some(path) = dir.get("path").and_then(|v| v.as_str()) {
                    return PathBuf::from(path);
                }
            }
        }
    }

    work_dir.clone()
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
        .unwrap_or_else(|_| workspace_root.clone());
    let canonical_abs = std::fs::canonicalize(&abs_path)
        .unwrap_or_else(|_| abs_path);

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
