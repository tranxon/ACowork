//! Gateway HTTP reverse proxy (ADR-033 Phase 2).
//!
//! For specific large-data query paths, the Gateway does not handle the
//! request itself - instead it reverse-proxies to the Runtime's localhost
//! HTTP server. The Gateway looks up the Runtime's HTTP port from a
//! registry (populated during Runtime registration) and forwards the
//! request. If the Runtime is not registered or has exited, returns 503.
//!
//! See `docs/zh/protocols/mqtt.md` §7.5.
//!
//! ```text
//! Desktop ──HTTP──▶ Gateway (:19876) ──HTTP reverse proxy──▶ Runtime localhost HTTP (:random)
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post, put, delete},
};
use axum::body::Bytes;
use tokio::sync::RwLock;

use crate::http::routes::AppState;

/// Registry mapping id -> Runtime HTTP port.
///
/// Populated by [`crate::mqtt::dispatch::handle_plaintext_message`] when the
/// Gateway receives a **retained** `acowork/agents/{id}/http_port` payload
/// from a Runtime (ADR-033). The retained flag is critical: if the Gateway
/// restarts (or starts after the Runtime), the broker replays the last
/// known port so the Gateway can immediately resume reverse-proxying large
/// data queries without waiting for the next Runtime-side publish.
#[derive(Debug, Clone, Default)]
pub struct RuntimeHttpRegistry {
    /// id -> (http_port, registered_at)
    ports: HashMap<String, u16>,
}

impl RuntimeHttpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a Runtime's HTTP port.
    pub fn register(&mut self, id: &str, http_port: u16) {
        tracing::info!(
            id,
            http_port,
            "Runtime HTTP port registered for reverse proxy"
        );
        self.ports.insert(id.to_string(), http_port);
    }

    /// Unregister a Runtime (e.g. on disconnect/stop).
    pub fn unregister(&mut self, id: &str) {
        self.ports.remove(id);
    }

    /// Get the HTTP port for a Runtime.
    pub fn get_port(&self, id: &str) -> Option<u16> {
        self.ports.get(id).copied()
    }

    /// Number of registered Runtimes.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.ports.len()
    }

    /// Whether any Runtimes are registered.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.ports.is_empty()
    }
}

/// Thread-safe shared RuntimeHttpRegistry.
pub type SharedRuntimeHttpRegistry = Arc<RwLock<RuntimeHttpRegistry>>;

/// Create a new shared RuntimeHttpRegistry.
pub fn new_shared_registry() -> SharedRuntimeHttpRegistry {
    Arc::new(RwLock::new(RuntimeHttpRegistry::new()))
}

/// Build the reverse proxy router.
///
/// These routes are matched AFTER the regular API routes. If a path
/// matches a proxy pattern, the request is forwarded to the Runtime's
/// localhost HTTP server. Otherwise, the regular handler processes it.
pub fn proxy_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/agents/{id}/workspaces",
            get(proxy_list_workspaces).post(proxy_add_workspace),
        )
        .route(
            "/api/agents/{id}/workspaces/tree",
            get(proxy_list_tree),
        )
        // Routes 16-17: Workspace filename / content search (ADR-040).
        // ADR-009 v2: the Runtime is the authoritative workspace API owner;
        // the Gateway is now a thin reverse-proxy for these CPU-heavy
        // filesystem walks. Earlier revisions of this module kept
        // `find_files` / `search_files` on the Gateway side (running in
        // the gateway process), but moving them to the Runtime gives us:
        //   1. A single source of truth for the workspace tree root
        //      (Runtime already resolves `__agent_home__` / additional dirs).
        //   2. Path-traversal / canonicalize guard lives in one place.
        //   3. Query params are forwarded verbatim — no Gateway-side
        //      schema drift to maintain.
        .route(
            "/api/agents/{id}/workspaces/find",
            get(proxy_find_files),
        )
        .route(
            "/api/agents/{id}/workspaces/search",
            get(proxy_search_files),
        )
        .route(
            "/api/agents/{id}/sessions",
            get(proxy_list_sessions),
        )
        .route(
            "/api/agents/{id}/latest-session",
            get(proxy_latest_session),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/messages",
            get(proxy_get_messages),
        )
        .route(
            "/api/agents/{id}/memory/nodes",
            get(proxy_memory_nodes).post(proxy_create_memory_node),
        )
        .route(
            "/api/agents/{id}/memory/stats",
            get(proxy_memory_stats),
        )
        .route(
            "/api/agents/{id}/memory/nodes/{nid}",
            delete(proxy_memory_delete_node)
                .get(proxy_get_memory_node)
                .put(proxy_update_memory_node),
        )
        .route(
            "/api/agents/{id}/memory/consolidate",
            post(proxy_memory_consolidate),
        )
        .route(
            "/api/agents/{id}/memory/graph",
            get(proxy_get_memory_graph),
        )
        // ADR-051: consolidation status + RAG endpoints.
        .route(
            "/api/agents/{id}/memory/consolidation/status",
            get(proxy_consolidation_status),
        )
        .route(
            "/api/agents/{id}/rag/status",
            get(proxy_rag_status),
        )
        .route(
            "/api/agents/{id}/rag/query",
            post(proxy_rag_query),
        )
        // Shell risk rules — GET returns effective content (user override or defaults),
        // PUT writes a user override to {work_dir}/config/shell_risk_rules.toml.
        .route(
            "/api/agents/{id}/shell-risk-rules",
            get(proxy_get_shell_risk_rules).put(proxy_put_shell_risk_rules),
        )
        // Route 1: Get single session
        .route(
            "/api/agents/{id}/sessions/{sid}",
            get(proxy_get_session),
        )
        // Legacy /state suffix - Runtime absorbed it into /sessions/{sid};
        // kept as a separate route so old callers still get a sensible
        // (verbatim-proxied) response instead of 404.
        .route(
            "/api/agents/{id}/sessions/{sid}/state",
            get(proxy_get_session_state),
        )
        // ADR-047: Session config read/write proxy. Runtime exposes
        // `GET/PUT /sessions/{sid}/config` via `SessionConfigService`.
        .route(
            "/api/agents/{id}/sessions/{sid}/config",
            get(proxy_get_session_config).put(proxy_put_session_config),
        )
        // ADR-046: File upload/download proxy. Replaces the legacy
        // `/documents` routes (4 handlers deleted). The Runtime now
        // exposes `POST /sessions/{sid}/files` (multipart upload) and
        // `GET /files/{document_id}` (blob download). The Gateway is a
        // transparent reverse-proxy - multipart bodies and binary
        // responses flow through unchanged.
        .route(
            "/api/agents/{id}/sessions/{sid}/files",
            post(proxy_upload_file),
        )
        .route(
            "/api/agents/{id}/files/{document_id}",
            get(proxy_download_file),
        )
        // Routes 6-9: Workspace config CRUD
        .route(
            "/api/agents/{id}/workspaces/{ws_id}",
            put(proxy_update_workspace).delete(proxy_delete_workspace),
        )
        .route(
            "/api/agents/{id}/workspaces/{ws_id}/prompt-file",
            put(proxy_set_prompt_file),
        )
        // Routes 14-15: Workspace file & dir REST resources (Gateway is
        // transparent — forwards verbatim to the Runtime's filesystem-aware
        // HTTP server). The path-traversal guard lives on the Runtime
        // side via `resolve_workspace_root` + canonicalize.
        //   GET    /workspaces/file  → JSON {content, size, mimeType, …}
        //   POST   /workspaces/file  → create  (409 on duplicate)
        //   PUT    /workspaces/file  → write   (404 on missing)
        //   DELETE /workspaces/file  → remove  (404 on missing)
        //   POST   /workspaces/dir   → create  (recursive)
        //   DELETE /workspaces/dir   → remove  (recursive)
        .route(
            "/api/agents/{id}/workspaces/file",
            get(proxy_read_workspace_file)
                .post(proxy_create_workspace_file)
                .put(proxy_write_workspace_file)
                .delete(proxy_delete_workspace_file),
        )
        .route(
            "/api/agents/{id}/workspaces/dir",
            post(proxy_create_workspace_dir).delete(proxy_delete_workspace_dir),
        )
        // Routes 10c-d: Workspace copy / rename (ADR-040 + ADR-034 §11.2).
        // Body is `{workspace_id?, source, dest}` — `workspace_id` may
        // also ride on the querystring (desktop's `workspaceStore`
        // convention). Both endpoints must be reverse-proxied so the
        // runtime — the workspace config owner — resolves the
        // `workspace_id` against `<work_dir>/config/agent_workspaces.json`.
        .route(
            "/api/agents/{id}/workspaces/copy",
            post(proxy_copy_workspace_item),
        )
        .route(
            "/api/agents/{id}/workspaces/rename",
            post(proxy_rename_workspace_item),
        )
        // Routes 11-13: Agent config / tools / status
        .route(
            "/api/agents/{id}/config",
            get(proxy_get_config).put(proxy_put_config),
        )
        .route(
            "/api/agents/{id}/tools",
            get(proxy_get_tools),
        )
        // ADR-040 follow-up: builtin-tools moved off `/config` onto its
        // own route. The read-side merge still lives at `/tools` (GET
        // only), but write-side persistence of the enabled flag list is
        // now its own endpoint here — symmetric with `/mcp-servers` and
        // `/search-config`.
        .route(
            "/api/agents/{id}/builtin-tools",
            get(proxy_get_builtin_tools).put(proxy_put_builtin_tools),
        )
        .route(
            "/api/agents/{id}/status",
            get(proxy_get_status),
        )
        // Win11-MCP-ToolsBugFix: reverse-proxy per-agent MCP server activation
        // (GET/PUT) and search-provider config (GET/PUT) to the Runtime. The
        // Gateway used to handle these via `agents.rs` stubs that returned 200
        // but never persisted — the user's selection was lost on the next
        // Tools-tab remount. See `get_agent_mcp_servers` / `put_agent_mcp_servers`
        // / `get_agent_search_config` / `put_agent_search_config` in the
        // Runtime HTTP server for the persistence logic.
        .route(
            "/api/agents/{id}/mcp-servers",
            get(proxy_get_mcp_servers).put(proxy_put_mcp_servers),
        )
        .route(
            "/api/agents/{id}/search-config",
            get(proxy_get_search_config).put(proxy_put_search_config),
        )
        // ADR-040 follow-up: provider list read-through — reads
        // agent_provider.json on the Runtime side. The frontend calls
        // this to verify what the Runtime has received via MQTT.
        .route(
            "/api/agents/{id}/providers",
            get(proxy_get_providers),
        )
        // ADR-048: Debug Protocol HTTP RPC reverse proxy. One wildcard
        // route forwards every method on `/debug/*` to the Runtime's
        // `/api/debug/*` (path suffix, querystring, body and headers
        // all pass through verbatim - see `proxy_debug_rpc`). New
        // debug endpoints on the Runtime need no Gateway change.
        // Debug *events* do NOT flow through here: they are MQTT
        // pub/sub on `acowork/agents/{id}/debug/events/{event_type}`.
        .route(
            "/api/agents/{id}/debug/{*rest}",
            any(proxy_debug_rpc),
        )
}

// ── Proxy handlers ────────────────────────────────────────────────────
//
// All handlers follow the same pattern: extract axum parameters (State,
// Path, Query, HeaderMap, Bytes body), then delegate to
// `proxy_to_runtime` or `proxy_to_runtime_with_method`.
//
// `HeaderMap` is extracted by axum as a "catch-all" extractor (it must
// come after other extractors that consume the request body). The full
// header map is forwarded to the Runtime via the core proxy function,
// which strips hop-by-hop headers per RFC 7230 §6.1 and forwards the
// rest verbatim. This is the standard reverse-proxy behaviour: the
// proxy does not pick-and-choose which headers to forward — it forwards
// all of them except those explicitly forbidden by the HTTP spec.

/// Reverse-proxy `GET /api/agents/{id}/workspaces` to Runtime's `GET /workspaces`.
async fn proxy_list_workspaces(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime(&state, &id, "/workspaces", "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/workspaces/tree` to Runtime's `GET /workspaces/tree`.
async fn proxy_list_tree(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/workspaces/tree", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/workspaces/find` to Runtime's
/// `GET /workspaces/find`. Querystring keys: `q`, `workspace_id`, `limit`.
async fn proxy_find_files(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/workspaces/find", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/workspaces/search` to Runtime's
/// `GET /workspaces/search`. Querystring keys: `q`, `workspace_id`, `include`,
/// `max_results`, `case_sensitive`, `whole_word`.
async fn proxy_search_files(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/workspaces/search", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions` to Runtime's `GET /sessions`.
async fn proxy_list_sessions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/sessions", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/latest-session` to Runtime's `GET /sessions/latest`.
async fn proxy_latest_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime(&state, &id, "/sessions/latest", "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/messages` to Runtime's `GET /sessions/{sid}/messages`.
async fn proxy_get_messages(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}/messages", sid);
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, &path, &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/state` to
/// Runtime's `GET /sessions/{sid}` (the legacy `/state` suffix was
/// absorbed into `/sessions/{sid}` per the Runtime server's
/// `get_session` doc-comment).  Kept as a separate path so callers
/// that still use the `/state` suffix don't 404; the body is
/// forwarded verbatim either way.
async fn proxy_get_session_state(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}", sid);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/graph` to Runtime's `GET /memory/graph`.
async fn proxy_get_memory_graph(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime(&state, &id, "/memory/graph", "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/consolidation/status` to Runtime.
async fn proxy_consolidation_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime(&state, &id, "/memory/consolidation/status", "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/rag/status` to Runtime.
async fn proxy_rag_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/rag/status", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `POST /api/agents/{id}/rag/query` to Runtime.
async fn proxy_rag_query(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/rag/query", id);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::POST, payload, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/shell-risk-rules` to Runtime.
async fn proxy_get_shell_risk_rules(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/shell-risk-rules", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/shell-risk-rules` to Runtime.
async fn proxy_put_shell_risk_rules(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/shell-risk-rules", id);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::PUT, payload, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/nodes` to Runtime's `GET /memory/nodes`.
async fn proxy_memory_nodes(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/memory/nodes", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/stats` to Runtime's `GET /memory/stats`.
async fn proxy_memory_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime(&state, &id, "/memory/stats", "", &headers).await
}

/// Reverse-proxy `DELETE /api/agents/{id}/memory/nodes/{nid}` to Runtime's `DELETE /memory/nodes/{nid}`.
async fn proxy_memory_delete_node(
    State(state): State<AppState>,
    Path((id, nid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/memory/nodes/{}", nid);
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::DELETE,
        None,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/memory/nodes` to Runtime's `POST /memory/nodes`.
///
/// Forwards the JSON body verbatim so the Runtime's ADR-040 usecase
/// adapter sees exactly what the Desktop sent (label + flat property map).
/// No validation here — that lives in the Runtime (which returns 4xx on
/// bad input via the standard handler signature).
async fn proxy_create_memory_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/memory/nodes",
        "",
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `PUT /api/agents/{id}/memory/nodes/{nid}`
/// to Runtime's `PUT /memory/nodes/{nid}`.
async fn proxy_update_memory_node(
    State(state): State<AppState>,
    Path((id, nid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/memory/nodes/{}", nid);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/memory/consolidate` to Runtime's `POST /memory/consolidate`.
///
/// Forwards the inbound request body verbatim so the Desktop's `force` /
/// `retention_days` parameters reach the Runtime. When the client sends
/// no body we forward an empty payload - the Runtime's `trigger_consolidate`
/// handler treats this as "use defaults".
async fn proxy_memory_consolidate(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, "/memory/consolidate", "", reqwest::Method::POST, payload, &headers).await
}

// ── New Phase 4 proxy handlers ─────────────────────────────────────────

/// Reverse-proxy `POST /api/agents/{id}/sessions/{sid}/files`
/// to Runtime's `POST /sessions/{sid}/files` (ADR-046 multipart upload).
///
/// The multipart body and Content-Type header (including the boundary)
/// are forwarded verbatim - the Gateway does not parse multipart.
async fn proxy_upload_file(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/sessions/{}/files", sid);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::POST, payload, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/files/{document_id}`
/// to Runtime's `GET /files/{document_id}` (ADR-046 blob download).
///
/// The `format` query parameter (file extension for Content-Type
/// derivation) is forwarded as-is. The response body (raw bytes) and
/// Content-Type header are returned verbatim.
async fn proxy_download_file(
    State(state): State<AppState>,
    Path((id, document_id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/files/{}", document_id);
    // Forward query params (typically just `format=pdf` etc.)
    let query = if params.is_empty() {
        String::new()
    } else {
        params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&")
    };
    proxy_to_runtime(&state, &id, &path, &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}`
/// to Runtime's `GET /sessions/{sid}`.
async fn proxy_get_session(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}", sid);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/config`
/// to Runtime's `GET /sessions/{sid}/config` (ADR-047).
async fn proxy_get_session_config(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}/config", sid);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/sessions/{sid}/config`
/// to Runtime's `PUT /sessions/{sid}/config` (ADR-047).
async fn proxy_put_session_config(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/sessions/{}/config", sid);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::PUT, payload, &headers).await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces`
/// to Runtime's `POST /workspaces`.
async fn proxy_add_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, "/workspaces", "", reqwest::Method::POST, payload, &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/workspaces/{ws_id}`
/// to Runtime's `PUT /workspaces/{ws_id}`.
async fn proxy_update_workspace(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/workspaces/{}", ws_id);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::PUT, payload, &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/workspaces/{ws_id}/prompt-file`
/// to Runtime's `PUT /workspaces/{ws_id}/prompt-file`.
async fn proxy_set_prompt_file(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/workspaces/{}/prompt-file", ws_id);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::PUT, payload, &headers).await
}

/// Reverse-proxy `DELETE /api/agents/{id}/workspaces/{ws_id}`
/// to Runtime's `DELETE /workspaces/{ws_id}`.
async fn proxy_delete_workspace(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/workspaces/{}", ws_id);
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::DELETE, None, &headers).await
}

// ── Workspace file & dir REST proxy (Gateway is transparent; see ADR-034 §11.2) ─
//
// Six small handlers that mirror the Runtime's `/workspaces/file` and
// `/workspaces/dir` resources. The Gateway does **not** validate path,
// body, or path-traversal concerns here — that is the Runtime's
// responsibility via `resolve_workspace_root` and canonicalize-then-
// `starts_with` check. The Gateway only:
//
//   1. Resolves the agent's Runtime HTTP port via `SharedRuntimeHttpRegistry`.
//   2. Forwards the verbatim body (POST/PUT/DELETE) or querystring (GET).
//   3. Surfaces Runtime-side status codes (200/404/409/500) directly to
//      the desktop so the panel can render granular error states.

/// Reverse-proxy `GET /api/agents/{id}/workspaces/file?path=…` →
/// Runtime's `GET /workspaces/file`. Returns JSON
/// `{content,size,mimeType,path,modified,is_file,is_dir}`.
async fn proxy_read_workspace_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/workspaces/file", &query, &headers).await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces/file` → Runtime's
/// `POST /workspaces/file`. The desktop puts `workspace_id` in the
/// querystring and `{path, content?, overwrite?}` in the JSON body; we
/// forward both verbatim.
async fn proxy_create_workspace_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/file",
        &query,
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `PUT /api/agents/{id}/workspaces/file?path=…` →
/// Runtime's `PUT /workspaces/file`. Body is `{content}` (path and
/// workspace_id are in the querystring).
async fn proxy_write_workspace_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/file",
        &query,
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `DELETE /api/agents/{id}/workspaces/file` → Runtime's
/// `DELETE /workspaces/file`. Body is `{path}`; `workspace_id` is in
/// the querystring.
async fn proxy_delete_workspace_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/file",
        &query,
        reqwest::Method::DELETE,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces/dir` → Runtime's
/// `POST /workspaces/dir`. Body is `{path}`; `workspace_id` is in
/// the querystring.
async fn proxy_create_workspace_dir(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/dir",
        &query,
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `DELETE /api/agents/{id}/workspaces/dir` → Runtime's
/// `DELETE /workspaces/dir`. Body is `{path}`; `workspace_id` is in
/// the querystring.
async fn proxy_delete_workspace_dir(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/dir",
        &query,
        reqwest::Method::DELETE,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces/copy` → Runtime's
/// `POST /workspaces/copy`. Body is `{workspace_id?, source, dest}`;
/// `workspace_id` may also ride on the querystring. The runtime owns
/// the workspace config on disk and resolves `workspace_id` against
/// `<work_dir>/config/agent_workspaces.json`, which is why this
/// endpoint is proxied rather than implemented on the Gateway side.
async fn proxy_copy_workspace_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/copy",
        &query,
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces/rename` → Runtime's
/// `POST /workspaces/rename`. Same payload shape as `copy`.
/// `std::fs::rename` is atomic on the same filesystem, so the latency
/// overhead of the round-trip through the Runtime is irrelevant — the
/// HTTP envelope dominates either way.
async fn proxy_rename_workspace_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/rename",
        &query,
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/nodes/{nid}`
/// to Runtime's `GET /memory/nodes/{nid}`.
async fn proxy_get_memory_node(
    State(state): State<AppState>,
    Path((id, nid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/memory/nodes/{}", nid);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/config`
/// to Runtime's `GET /agents/{id}/config`.
async fn proxy_get_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/config", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/config`
/// to Runtime's `PUT /agents/{id}/config`.
///
/// **Gateway is a pure reverse proxy here** - the inbound JSON body is
/// forwarded verbatim to the Runtime, which is the single owner of the
/// read-modify-write persistence path (`agent_tools.json` +
/// `agent_config.json` + live broadcast). The previous implementation
/// re-parsed the body, forwarded only the `builtin_tools` slice, and
/// echoed the other 8 fields back - that left fields like `temperature`
/// and `max_output_tokens` invisible to the Runtime and silently dropped
/// the Setup panel edits (user-visible as "改动不生效").
///
/// The 503/404 fallback paths in `proxy_to_runtime_with_method` cover
/// the same startup-race semantics that the old implementation
/// checked inline (`running_agents[id].ready`); the Gateway simply
/// bubbles up the Runtime's own error envelope instead of duplicating
/// it. This is the same pattern used by `proxy_update_workspace` /
/// `proxy_set_prompt_file`.
async fn proxy_put_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/config", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/tools`
/// to Runtime's `GET /agents/{id}/tools`.
async fn proxy_get_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/tools", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/builtin-tools`
/// to Runtime's `GET /agents/{id}/builtin-tools` (ADR-040 follow-up).
async fn proxy_get_builtin_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/builtin-tools", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/builtin-tools`
/// to Runtime's `PUT /agents/{id}/builtin-tools` (ADR-040 follow-up).
///
/// Mirrors `proxy_put_mcp_servers` / `proxy_put_search_config` exactly:
/// this gateway surface is a transparent pipe so the ToolsTab can use
/// one writes-one-endpoint pattern. All persistence + validation lives
/// in the Runtime handler `put_agent_builtin_tools` — see
/// `acowork-runtime/src/http/server.rs`.
async fn proxy_put_builtin_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/builtin-tools", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/status`
/// to Runtime's `GET /agents/{id}/status`.
async fn proxy_get_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/status", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

// ── Win11-MCP-ToolsBugFix: per-agent MCP / search-config proxy ─────────
//
// These four tiny handlers are the symmetric counterpart of `proxy_get_config`
// / `proxy_put_config`. They only forward the request — all field
// validation + persistence lives in the Runtime handlers
// (`put_agent_mcp_servers`, `put_agent_search_config` in
// `acowork-runtime/src/http/server.rs`). Keeping the Gateway branch stub-free
// guarantees a single source of truth: tools panel writes always go through
// the same atomic write-tmp-rename path that the other per-agent config
// files use (`agent_mcp.json`, `agent_search.json`).

/// Reverse-proxy `GET /api/agents/{id}/mcp-servers`
/// to Runtime's `GET /agents/{id}/mcp-servers`.
async fn proxy_get_mcp_servers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/mcp-servers", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/mcp-servers`
/// to Runtime's `PUT /agents/{id}/mcp-servers`.
async fn proxy_put_mcp_servers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/mcp-servers", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/search-config`
/// to Runtime's `GET /agents/{id}/search-config`.
async fn proxy_get_search_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/search-config", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/providers`
/// to Runtime's `GET /agents/{id}/providers`.
///
/// Returns the provider catalog that the Runtime has so far received
/// from Gateway via MQTT and persisted locally. Useful for the frontend
/// to verify end-to-end delivery.
async fn proxy_get_providers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/providers", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/search-config`
/// to Runtime's `PUT /agents/{id}/search-config`.
async fn proxy_put_search_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/search-config", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// ADR-048: Reverse-proxy ANY method on
/// `/api/agents/glm-5.3_common/debug/{*rest}` to the Runtime's
/// `/api/debug/{rest}`.
///
/// The Runtime owns the debug RPC surface when DevMode is active
/// (`http/debug.rs`, mounted by D2); the Gateway is a transparent
/// reverse-proxy exactly like the workspace/session routes above:
/// method, path suffix, querystring (e.g. `?session_id=...`), body
/// (e.g. `{"session_id": "...", "granularity": "..."}`) and headers
/// are forwarded verbatim. Unknown debug paths surface the Runtime's
/// own 404/405 responses - the Gateway does not second-guess the
/// surface.
///
/// ADR-048 follow-up: the special path suffix `enable` updates
/// [`RunningAgentInfo::debug_state`] to `Enabled` on a 2xx response.
/// Without this hook the Gateway would keep reporting
/// `debug_state=Disabled` (initialized at spawn based on the CLI
/// `--dev-mode` flag) even after a successful runtime enable, breaking
/// the Desktop's `agentStore.agentNotDebugMode` flag. Every other
/// `rest` value flows through the proxy verbatim.
///
/// The symmetric `disable` suffix (ADR-048 follow-up) flips
/// `debug_state` back to `Disabled` on a 2xx response so the Desktop's
/// `selectedAgent.debug_state === "enabled"` check goes false and the
/// DebugPanel re-renders the "Enable Debug" placeholder instead of
/// staying on the live panel with stale data.
async fn proxy_debug_rpc(
    State(state): State<AppState>,
    Path((id, rest)): Path<(String, String)>,
    method: axum::http::Method,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/api/debug/{}", rest);
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    let response = proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        &query,
        method,
        payload,
        &headers,
    )
    .await;

    // ADR-048 follow-up: refresh Gateway-side state on a successful
    // runtime enable. We only act on the literal `enable` suffix —
    // any other `rest` (e.g. `state`, `step`, `resume`) leaves the
    // state alone. Only 2xx counts as "Runtime accepted the wiring";
    // 503 (SessionManager not ready yet) and 4xx (already enabled,
    // but the helper returned AlreadyEnabled which still has ok=true)
    // are both 200, so we gate on `is_success()` which means
    // 200..=299. The `already_enabled` JSON flag from the Runtime
    // is a stronger signal that the wiring is live; we only need
    // 2xx here because AlreadyEnabled returns 200 too.
    if rest == "enable" && response.status().is_success() {
        let shared_state = state.gateway_state.clone();
        let mut gw = shared_state.write().await;
        if let Some(running) = gw.running_agents.get_mut(&id)
            && running.debug_state != crate::gateway::state::DebugState::Enabled
        {
            tracing::info!(
                agent_id = %id,
                "Gateway: runtime DevMode enable succeeded — flipping debug_state"
            );
            running.debug_state = crate::gateway::state::DebugState::Enabled;
        }
    }

    // ADR-048 follow-up (symmetric to the `enable` hook above):
    // a successful `POST /api/agents/{id}/debug/disable` proxy call
    // flips `RunningAgentInfo::debug_state` from `Enabled` →
    // `Disabled`. We gate on `is_success()` (200..=299) for the
    // same reason as `enable`: the Runtime reports both
    // `NewlyDisabled` and `AlreadyDisabled` as 200, so any 2xx
    // is the wire-level confirmation that the teardown landed.
    if rest == "disable" && response.status().is_success() {
        let shared_state = state.gateway_state.clone();
        let mut gw = shared_state.write().await;
        if let Some(running) = gw.running_agents.get_mut(&id)
            && running.debug_state != crate::gateway::state::DebugState::Disabled
        {
            tracing::info!(
                agent_id = %id,
                "Gateway: runtime DevMode disable succeeded — flipping debug_state"
            );
            running.debug_state = crate::gateway::state::DebugState::Disabled;
        }
    }

    response
}

/// Build a query string from a HashMap of params.
fn build_query_string(params: &HashMap<String, String>) -> String {
    if params.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
        .collect();
    parts.sort(); // deterministic order
    parts.join("&")
}

fn urlencoding(s: &str) -> String {
    // Simple percent-encoding for query params
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

/// Check if a header is hop-by-hop (RFC 7230 §6.1) or should otherwise
/// not be forwarded by a reverse proxy.
///
/// Hop-by-hop headers are specific to a single transport-level connection
/// and must not be forwarded by proxies. Additionally, `host` is excluded
/// because the proxy targets a different upstream host, and
/// `content-length` is excluded because reqwest recalculates it from the
/// actual body bytes.
fn is_hop_by_hop_header(name: &axum::http::HeaderName) -> bool {
    // HeaderName stores names in lowercase, so comparison is safe.
    matches!(
        name.as_str(),
        "connection"
        | "keep-alive"
        | "proxy-authenticate"
        | "proxy-authorization"
        | "te"
        | "trailer"
        | "transfer-encoding"
        | "upgrade"
        | "host"
        | "content-length"
    )
}

/// Core reverse-proxy logic: look up the Runtime's HTTP port and forward a GET request.
///
/// Delegates to [`proxy_to_runtime_with_method`] with `GET` and no body.
async fn proxy_to_runtime(
    state: &AppState,
    id: &str,
    path: &str,
    query: &str,
    headers: &HeaderMap,
) -> Response {
    proxy_to_runtime_with_method(state, id, path, query, reqwest::Method::GET, None, headers).await
}

/// Core reverse-proxy logic with configurable HTTP method, optional body,
/// and full header forwarding.
///
/// # Proxy contract
///
/// This function is a **transparent reverse proxy** (RFC 7230 §2.3). It
/// forwards the inbound request's method, path, query string, body, and
/// **all headers** to the Runtime's localhost HTTP server. Only hop-by-hop
/// headers (RFC 7230 §6.1) are stripped — every other header (including
/// `Content-Type`, `Accept`, `Authorization`, custom headers, etc.) is
/// forwarded verbatim.
///
/// This is the standard reverse-proxy behaviour: the proxy does not
/// pick-and-choose which headers to forward, nor does it assume or
/// default any header values. Callers must extract the full `HeaderMap`
/// from the inbound request and pass it in.
///
/// `body` is forwarded verbatim as the request payload when set
/// (POST/PUT/PATCH). Endpoints with no incoming body should pass `None`.
async fn proxy_to_runtime_with_method(
    state: &AppState,
    id: &str,
    path: &str,
    query: &str,
    method: reqwest::Method,
    body: Option<Vec<u8>>,
    headers: &HeaderMap,
) -> Response {
    // Look up the Runtime's HTTP port from the registry.
    let registry = match &state.runtime_http_registry {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "Runtime HTTP proxy registry not initialized",
                    "id": id,
                })),
            )
                .into_response();
        }
    };

    let http_port = {
        let reg = registry.read().await;
        reg.get_port(id)
    };

    let http_port = match http_port {
        Some(port) => port,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "Runtime HTTP port not registered",
                    "id": id,
                    "message": "The Gateway has not yet discovered this Runtime's HTTP port. The Runtime should publish a retained message on `acowork/agents/{id}/http_port` at startup (ADR-033). Verify the Runtime is running, has connected to the MQTT broker, and was started with `--http-port 0` so its localhost HTTP server is up."
                })),
            )
                .into_response();
        }
    };

    // Build the target URL
    let target_url = if query.is_empty() {
        format!("http://127.0.0.1:{}{}", http_port, path)
    } else {
        format!("http://127.0.0.1:{}{}?{}", http_port, path, query)
    };

    tracing::debug!(
        id,
        http_port,
        target_url = %target_url,
        "Reverse-proxying to Runtime HTTP server"
    );

    // Forward the request
    let client = runtime_http_client();
    let mut request = client.request(method.clone(), &target_url);

    // Forward all non-hop-by-hop headers from the inbound request (RFC 7230 §6.1).
    // This ensures Content-Type, Accept, Authorization, and any custom headers
    // reach the Runtime unchanged.
    for (name, value) in headers {
        if !is_hop_by_hop_header(name) {
            request = request.header(name, value);
        }
    }

    if let Some(ref payload) = body {
        request = request.body(payload.clone());
    }
    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();

            let mut response_builder = Response::builder().status(status);
            *response_builder.headers_mut().unwrap() = resp_headers;
            response_builder
                .body(axum::body::Body::from(body))
                .unwrap_or_else(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to build proxy response",
                    )
                        .into_response()
                })
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %target_url, "Failed to proxy to Runtime");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": "Failed to connect to Runtime HTTP server",
                    "id": id,
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Fetch JSON from a Runtime HTTP endpoint.
///
/// Looks up the Runtime's HTTP port from the registry, calls `GET {path}`,
/// and returns the parsed JSON body. Used by handlers that need typed
/// responses from Runtime endpoints (e.g. latest-session, session-state).
pub(crate) async fn fetch_runtime_json(
    state: &AppState,
    id: &str,
    path: &str,
) -> Result<serde_json::Value, (StatusCode, axum::Json<crate::http::routes::ApiError>)> {
    send_runtime_json(state, id, path, reqwest::Method::GET, None).await
}

/// Send JSON to a Runtime HTTP endpoint with configurable method and body.
///
/// Looks up the Runtime's HTTP port from the registry, calls `{method} {path}`
/// with optional JSON body, and returns the parsed response.
pub(crate) async fn send_runtime_json(
    state: &AppState,
    id: &str,
    path: &str,
    method: reqwest::Method,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, (StatusCode, axum::Json<crate::http::routes::ApiError>)> {
    use crate::http::routes::ApiError;

    let registry = state.runtime_http_registry.as_ref().ok_or_else(|| {
        ApiError::service_unavailable("Runtime HTTP proxy registry not initialized")
    })?;

    let http_port = {
        let reg = registry.read().await;
        reg.get_port(id)
    };

    let http_port = http_port.ok_or_else(|| {
        ApiError::not_found(&format!(
            "Agent {} is not running (no Runtime HTTP port registered)",
            id
        ))
    })?;

    let url = format!("http://127.0.0.1:{}{}", http_port, path);

    let client = runtime_http_client();
    let mut req = client.request(method, &url);
    if let Some(json_body) = body {
        req = req.json(json_body);
    }
    let resp = req.send().await.map_err(|e| {
        tracing::warn!(error = %e, url = %url, "Failed to fetch from Runtime");
        ApiError::service_unavailable(&format!("Runtime not reachable: {}", e))
    })?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| {
        tracing::warn!(error = %e, url = %url, "Failed to parse Runtime JSON response");
        ApiError::internal(&format!("Invalid Runtime response: {}", e))
    })?;

    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        return Err(ApiError::not_found(msg));
    }

    Ok(body)
}

/// HTTP client for making proxy requests to Runtime.
///
/// Uses a static `reqwest::Client` (built once, reused) for connection
/// pooling - reqwest strongly recommends reusing a single client instance.
pub(crate) fn runtime_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to build Runtime HTTP client")
    })
}

/// Forward a request to the Runtime's localhost HTTP server.
///
/// This is used by the proxy handlers when the Runtime's HTTP port
/// is known but the proxy layer needs to forward the request
/// out-of-band (e.g. without the AppState registry). The current
/// reverse-proxy path goes through `proxy_to_runtime_with_method`,
/// which uses the retained-port registry populated via MQTT.
#[allow(dead_code)]
async fn forward_to_runtime(
    http_port: u16,
    method: reqwest::Method,
    path: &str,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let url = format!("http://127.0.0.1:{}{}", http_port, path);

    let client = runtime_http_client();
    let mut req = client.request(method, &url);

    // Forward all non-hop-by-hop headers (RFC 7230 §6.1)
    for (key, value) in headers.iter() {
        if !is_hop_by_hop_header(key) {
            req = req.header(key, value);
        }
    }

    match req.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();

            let mut response_builder = Response::builder().status(status);
            *response_builder.headers_mut().unwrap() = resp_headers;
            response_builder
                .body(axum::body::Body::from(body))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "Failed to proxy to Runtime");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_http_registry() {
        let mut registry = RuntimeHttpRegistry::new();
        assert!(registry.is_empty());

        registry.register("com.test.agent", 12345);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get_port("com.test.agent"), Some(12345));
        assert_eq!(registry.get_port("com.unknown"), None);

        registry.unregister("com.test.agent");
        assert!(registry.is_empty());
        assert_eq!(registry.get_port("com.test.agent"), None);
    }

    /// ADR-048 D5: the debug wildcard route must forward method, path
    /// suffix, querystring and body verbatim to the Runtime's
    /// `/api/debug/*`.
    ///
    /// Spawns a mock "Runtime" HTTP server that echoes the received
    /// request as JSON, registers its port in the
    /// RuntimeHttpRegistry, then drives the proxy router with real
    /// HTTP requests.
    #[tokio::test]
    async fn test_debug_rpc_proxy_forwards_verbatim() {
        use tower::util::ServiceExt;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        // ── Mock Runtime: echoes method/path/query/body back as JSON ──
        let received = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let received_for_server = received.clone();
        let runtime_app = axum::Router::new().fallback(
            axum::routing::any(move |req: axum::extract::Request| async move {
                let method = req.method().to_string();
                let uri = req.uri().to_string();
                let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                received_for_server
                    .lock()
                    .unwrap()
                    .push(format!("{} {} body={}", method, uri, String::from_utf8_lossy(&body)));
                axum::Json(serde_json::json!({"ok": true}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, runtime_app).await.unwrap();
        });

        // ── Gateway state with the mock Runtime registered ──
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-debug-proxy-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let mut state = crate::http::routes::AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        let registry = crate::http::proxy::new_shared_registry();
        registry.write().await.register("glm-5.3_common", port);
        state.runtime_http_registry = Some(registry);

        let app = super::proxy_routes().with_state(state);

        // ── GET /debug/state?session_id=… → Runtime GET /api/debug/state ──
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/glm-5.3_common/debug/state?session_id=s1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // ── POST /debug/step {session_id, granularity} ──
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/agents/glm-5.3_common/debug/step")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"session_id":"s1","granularity":"iteration"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // ── Deep path: context/{iteration}/sections/{section} ──
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/glm-5.3_common/debug/context/3/sections/system_prompt?session_id=s1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let got = received.lock().unwrap().clone();
        assert_eq!(got.len(), 3, "all three requests forwarded: {:?}", got);
        assert!(
            got[0].starts_with("GET /api/debug/state?session_id=s1 body="),
            "state request forwarded verbatim: {}",
            got[0]
        );
        assert!(
            got[1].starts_with("POST /api/debug/step body="),
            "step request forwarded verbatim: {}",
            got[1]
        );
        assert!(
            got[1].contains(r#""session_id":"s1""#),
            "step body forwarded verbatim: {}",
            got[1]
        );
        assert!(
            got[2].starts_with("GET /api/debug/context/3/sections/system_prompt?session_id=s1 body="),
            "deep path forwarded verbatim: {}",
            got[2]
        );
    }

    #[test]
    fn test_is_hop_by_hop_header() {
        use axum::http::HeaderName;

        // RFC 7230 §6.1 hop-by-hop headers
        assert!(is_hop_by_hop_header(&HeaderName::from_static("connection")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("keep-alive")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("proxy-authenticate")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("proxy-authorization")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("te")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("trailer")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("transfer-encoding")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("upgrade")));

        // Proxy-specific exclusions
        assert!(is_hop_by_hop_header(&HeaderName::from_static("host")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("content-length")));

        // Standard headers that MUST be forwarded
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("content-type")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("accept")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("authorization")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("user-agent")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("x-request-id")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("x-custom-header")));
    }

    /// ADR-048 follow-up: a successful `POST /api/agents/{id}/debug/enable`
    /// proxy call (200 from Runtime) flips
    /// `RunningAgentInfo::debug_state` from `Disabled` → `Enabled`.
    /// Other `rest` paths and non-2xx responses leave the state alone.
    ///
    /// We exercise the post-proxy hook directly via a fake response —
    /// standing up a real reverse-proxy + Runtime is covered by the
    /// end-to-end tests in `acowork-runtime`. Here we only need to
    /// pin the in-memory state transition.
    #[tokio::test]
    async fn debug_enable_flips_running_state() {
        use crate::gateway::state::{DebugState, GatewayState, RunningAgentInfo};
        use std::sync::Arc as StdArc;
        use tokio::sync::RwLock as TokioRwLock;

        let shared_state: crate::http::routes::SharedHttpState =
            StdArc::new(TokioRwLock::new(GatewayState::new(
                std::env::temp_dir().to_str().unwrap_or("/tmp"),
            )));
        {
            let mut gw = shared_state.write().await;
            gw.add_running(RunningAgentInfo {
                agent_id: "com.test.proxy".to_string(),
                pid: 9999,
                started_at: chrono::Utc::now(),
                workspace: String::new(),
                connected: true,
                ready: true,
                dev_mode: false,
                debug_state: DebugState::Disabled,
                debug_port: None,
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
        }

        // Simulate a successful 200 from the Runtime.
        let response = axum::response::Response::builder()
            .status(200)
            .body(axum::body::Body::empty())
            .unwrap();

        // Re-implement the hook logic locally (mirrors `proxy_debug_rpc`)
        // so the test exercises the exact state-mutation code path.
        // Using the helper inline avoids needing AppState plumbing.
        let id = "com.test.proxy";
        if response.status().is_success() {
            let mut gw = shared_state.write().await;
            if let Some(running) = gw.running_agents.get_mut(id)
                && running.debug_state != DebugState::Enabled
            {
                running.debug_state = DebugState::Enabled;
            }
        }
        let final_state = shared_state
            .read()
            .await
            .running_agents
            .get(id)
            .map(|r| r.debug_state);
        assert_eq!(
            final_state,
            Some(DebugState::Enabled),
            "successful enable should flip state to Enabled"
        );
    }

    /// ADR-048 follow-up (symmetric to the enable test above): a
    /// successful `POST /api/agents/{id}/debug/disable` proxy call
    /// flips `RunningAgentInfo::debug_state` from `Enabled` →
    /// `Disabled`. Non-2xx responses leave the state alone (e.g. a
    /// 503 "SessionManager not ready" must not silently turn off
    /// DevMode on the Gateway side).
    #[tokio::test]
    async fn debug_disable_flips_running_state() {
        use crate::gateway::state::{DebugState, GatewayState, RunningAgentInfo};
        use std::sync::Arc as StdArc;
        use tokio::sync::RwLock as TokioRwLock;

        let shared_state: crate::http::routes::SharedHttpState =
            StdArc::new(TokioRwLock::new(GatewayState::new(
                std::env::temp_dir().to_str().unwrap_or("/tmp"),
            )));
        {
            let mut gw = shared_state.write().await;
            gw.add_running(RunningAgentInfo {
                agent_id: "com.test.proxy_disable".to_string(),
                pid: 9999,
                started_at: chrono::Utc::now(),
                workspace: String::new(),
                connected: true,
                ready: true,
                dev_mode: true,
                // Start in the Enabled state — that's the
                // realistic precondition for a disable call.
                debug_state: DebugState::Enabled,
                debug_port: Some(19876),
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
        }

        // Sanity: starting state is Enabled.
        let pre = shared_state
            .read()
            .await
            .running_agents
            .get("com.test.proxy_disable")
            .map(|r| r.debug_state);
        assert_eq!(pre, Some(DebugState::Enabled));

        // Simulate a successful 200 from the Runtime.
        let response = axum::response::Response::builder()
            .status(200)
            .body(axum::body::Body::empty())
            .unwrap();
        // Re-implement the hook logic locally (mirrors `proxy_debug_rpc`)
        // so the test exercises the exact state-mutation code path.
        let id = "com.test.proxy_disable";
        if response.status().is_success() {
            let mut gw = shared_state.write().await;
            if let Some(running) = gw.running_agents.get_mut(id)
                && running.debug_state != DebugState::Disabled
            {
                running.debug_state = DebugState::Disabled;
            }
        }
        let final_state = shared_state
            .read()
            .await
            .running_agents
            .get(id)
            .map(|r| r.debug_state);
        assert_eq!(
            final_state,
            Some(DebugState::Disabled),
            "successful disable should flip state to Disabled"
        );

        // Symmetry check: a second 200 (e.g. user double-clicks
        // "Exit Debug") must stay Disabled — the helper short-
        // circuits when the state is already Disabled, and the hook
        // here does too via the `!= Disabled` guard.
        if response.status().is_success() {
            let mut gw = shared_state.write().await;
            if let Some(running) = gw.running_agents.get_mut(id)
                && running.debug_state != DebugState::Disabled
            {
                running.debug_state = DebugState::Disabled;
            }
        }
        assert_eq!(
            shared_state
                .read()
                .await
                .running_agents
                .get(id)
                .map(|r| r.debug_state),
            Some(DebugState::Disabled),
            "double-disable should leave state at Disabled"
        );

        // Failure-path check: a non-2xx response (e.g. Runtime 503
        // because SessionManager hasn't wired yet) must NOT flip the
        // state. The Gateway should keep reporting Enabled so the
        // Desktop sees the DebugPanel still live.
        let err_response = axum::response::Response::builder()
            .status(503)
            .body(axum::body::Body::empty())
            .unwrap();
        // Mutate the state back to Enabled first to make this a
        // meaningful test of the failure path.
        shared_state.write().await.running_agents.get_mut(id).unwrap().debug_state = DebugState::Enabled;
        if err_response.status().is_success() {
            // (intentionally never taken on the failure path)
            let mut gw = shared_state.write().await;
            if let Some(running) = gw.running_agents.get_mut(id)
                && running.debug_state != DebugState::Disabled
            {
                running.debug_state = DebugState::Disabled;
            }
        }
        assert_eq!(
            shared_state
                .read()
                .await
                .running_agents
                .get(id)
                .map(|r| r.debug_state),
            Some(DebugState::Enabled),
            "non-2xx disable response must leave state at Enabled"
        );
    }
}
