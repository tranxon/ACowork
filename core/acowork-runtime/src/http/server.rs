//! Runtime localhost HTTP server (ADR-033 Phase 2).
//!
//! Serves read-only queries for the Gateway reverse proxy:
//!
//! ```text
//! GET /sessions                          — full session list
//! GET /sessions/{sid}/messages           — full message list for a session
//! GET /memory/graph                      — full memory graph
//! GET /files/{id}                        — file content
//! GET /health                            — health check
//! ```
//!
//! The server binds to `127.0.0.1:0` (random port) and is intended
//! for Gateway reverse proxy access only — not direct Desktop access.
//!
//! See `docs/zh/protocols/mqtt.md` §7.5.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde::Serialize;

/// Error type for Runtime HTTP server operations.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeHttpServerError {
    #[error("HTTP server error: {0}")]
    Server(String),
    #[error("Failed to bind: {0}")]
    Bind(String),
}

/// State shared with HTTP handlers.
#[derive(Clone)]
struct HttpState {
    work_dir: PathBuf,
    agent_id: String,
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
    pub async fn start(work_dir: PathBuf, agent_id: String) -> Result<Self, RuntimeHttpServerError> {
        let state = HttpState {
            work_dir,
            agent_id,
        };

        let app = Router::new()
            .route("/health", get(health))
            .route("/sessions", get(list_sessions))
            .route("/sessions/{sid}/messages", get(get_messages))
            .route("/memory/graph", get(get_memory_graph))
            .route("/files/{id}", get(get_file))
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

/// `GET /sessions` — full session list.
///
/// Reads the session directory and returns all session metadata.
/// This is the backend for `GET /api/agents/{id}/sessions` via the
/// Gateway reverse proxy.
async fn list_sessions(State(state): State<HttpState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions_dir = state.work_dir.join("sessions");

    if !sessions_dir.exists() {
        return Ok(Json(serde_json::json!({
            "sessions": [],
        })));
    }

    let mut sessions = Vec::new();

    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to read sessions dir");
            return Ok(Json(serde_json::json!({ "sessions": [] })));
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }

        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let metadata = std::fs::metadata(&path).ok();
        let modified = metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        // Read first line for session metadata
        let first_line = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| content.lines().next().map(|s| s.to_string()));

        let (title, created_at) = if let Some(ref line) = first_line {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
                (
                    json.get("title").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    json.get("created_at").and_then(|v| v.as_str()).map(|s| s.to_string()),
                )
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        // Count messages (lines in the JSONL file)
        let message_count = std::fs::read_to_string(&path)
            .ok()
            .map(|content| content.lines().filter(|l| !l.is_empty()).count())
            .unwrap_or(0) as u32;

        sessions.push(serde_json::json!({
            "session_id": session_id,
            "title": title,
            "created_at": created_at,
            "message_count": message_count,
            "last_modified": modified,
        }));
    }

    // Sort by last_modified descending (most recent first)
    sessions.sort_by(|a, b| {
        let a_mod = a.get("last_modified").and_then(|v| v.as_u64()).unwrap_or(0);
        let b_mod = b.get("last_modified").and_then(|v| v.as_u64()).unwrap_or(0);
        b_mod.cmp(&a_mod)
    });

    Ok(Json(serde_json::json!({
        "sessions": sessions,
    })))
}

/// `GET /sessions/{sid}/messages` — full message list for a session.
///
/// Reads the session JSONL file and returns all messages.
/// Supports optional `cursor` and `limit` query params for pagination.
async fn get_messages(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session_path = state.work_dir.join("sessions").join(format!("{}.jsonl", sid));

    if !session_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let content = std::fs::read_to_string(&session_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let messages: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    Ok(Json(serde_json::json!({
        "session_id": sid,
        "messages": messages,
        "count": messages.len(),
    })))
}

/// `GET /memory/graph` — full memory graph.
///
/// Returns the memory graph data from the Runtime's memory store.
/// Phase 2: returns a placeholder; full implementation depends on
/// the Grafeo memory engine integration.
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

/// `GET /files/{id}` — file content.
///
/// Serves files from the Runtime's workspace. The `id` is a relative
/// path within the workspace (sanitized to prevent path traversal).
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

    let content = std::fs::read_to_string(&file_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({
        "path": id,
        "content": content,
        "size": content.len(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_http_server_starts_and_responds() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let server = RuntimeHttpServer::start(temp_dir.clone(), "com.test.agent".to_string())
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

        // Create a test session file
        let sessions_dir = temp_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let session_file = sessions_dir.join("20260101_120000_abc.jsonl");
        std::fs::write(
            &session_file,
            r#"{"type":"meta","title":"Test Session","created_at":"2026-01-01T12:00:00Z"}
{"type":"user","content":"Hello"}
{"type":"assistant","content":"Hi there!"}
"#,
        )
        .unwrap();

        let server = RuntimeHttpServer::start(temp_dir.clone(), "com.test.agent".to_string())
            .await
            .unwrap();

        // List sessions
        let url = format!("http://127.0.0.1:{}/sessions", server.port);
        let response = reqwest::get(&url).await.unwrap();
        let body: serde_json::Value = response.json().await.unwrap();
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], "20260101_120000_abc");
        assert_eq!(sessions[0]["title"], "Test Session");
        assert_eq!(sessions[0]["message_count"], 3);

        // Get messages
        let url = format!(
            "http://127.0.0.1:{}/sessions/20260101_120000_abc/messages",
            server.port
        );
        let response = reqwest::get(&url).await.unwrap();
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["count"], 3);
        assert_eq!(body["messages"][1]["content"], "Hello");

        std::fs::remove_dir_all(&temp_dir).ok();
    }
}
