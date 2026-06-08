//! LSP relay module
//!
//! LSP protocol relay: WebSocket ↔ stdin/stdout of a language server process.
//!
//! LSP over stdio uses the Base Protocol frame format:
//! ```text
//! Content-Length: <length>\r\n\r\n<JSON-RPC message>
//! ```
//! WebSocket side (vscode-ws-jsonrpc) sends/receives plain JSON-RPC text messages.
//!
//! The relay converts between these two formats:
//! - **WS → stdin**: receive JSON text, prepend `Content-Length` header, write to stdin
//! - **stdout → WS**: parse `Content-Length` header, extract JSON body, send as text
//!
//! Architecture:
//! ```text
//! Monaco (webview) ← WS (JSON text) → Gateway ← stdin/stdout (framed) → LSP Server
//! ```
//!
//! ## Process Pool
//!
//! LSP processes are pooled: their lifetime is bound to the Gateway process,
//! NOT individual WebSocket sessions. This avoids re-indexing (e.g. rust-analyzer)
//! every time the Desktop App reconnects.

pub mod pool;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;

use crate::http::routes::AppState;
pub use pool::LspPool;

// ── LSP server command lookup ──────────────────────────────────────────

/// Resolve the LSP server command for a given language.
///
/// Checks `PATH` first, returns `None` if the command is not found or
/// the language is not supported.
fn resolve_lsp_command(language: &str) -> Option<String> {
    // Priority 1: hard-coded known commands (checked against PATH)
    let candidates: &[&str] = match language.to_lowercase().as_str() {
        "rust" => &["rust-analyzer"],
        "python" => &["pylsp", "pyright-langserver", "python-lsp-server"],
        "typescript" | "javascript" | "ts" | "js" => {
            &["typescript-language-server", "typescript-language-server.cmd"]
        }
        "go" => &["gopls"],
        "c" | "cpp" | "c++" => &["clangd"],
        "json" => &["vscode-json-languageserver", "json-languageserver"],
        "yaml" | "yml" => &["yaml-language-server"],
        "html" => &["vscode-html-languageserver", "html-languageserver"],
        "css" | "scss" | "less" => &["vscode-css-languageserver", "css-languageserver"],
        "markdown" | "md" => &["marksman"],
        _ => return None,
    };

    // Find first candidate that exists on PATH
    for cmd in candidates {
        if find_on_path(cmd) {
            tracing::info!("[LSP] Found LSP command for '{language}': {cmd}");
            return Some(cmd.to_string());
        }
    }

    tracing::warn!("[LSP] No LSP command found for '{language}' (checked: {:?})", candidates);
    None
}

/// Check if a command exists on the system PATH
fn find_on_path(cmd: &str) -> bool {
    // On Windows, also try with .exe / .cmd / .bat extensions
    let candidates: Vec<String> = if cfg!(windows) {
        vec![
            format!("{}.exe", cmd),
            format!("{}.cmd", cmd),
            format!("{}.bat", cmd),
            cmd.to_string(),
        ]
    } else {
        vec![cmd.to_string()]
    };

    // Get PATH from environment
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for name in &candidates {
            let full = dir.join(name);
            if full.is_file() {
                return true;
            }
        }
    }
    false
}

// ── Query parameters ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LspQuery {
    /// Agent ID to resolve workspace directory
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Optional workspace ID for additional workspace directories
    #[serde(default)]
    pub workspace_id: Option<String>,
}

// ── WebSocket handler ──────────────────────────────────────────────────

/// `GET /lsp/{language}` — WebSocket upgrade for LSP relay
///
/// Opens a WebSocket connection, gets/spawns an LSP process from the pool,
/// and relays bytes bidirectionally.
pub async fn lsp_handler(
    ws: WebSocketUpgrade,
    Path(language): Path<String>,
    Query(query): Query<LspQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let lang_lower = language.to_lowercase();

    // Resolve workspace root
    let workspace_root = match resolve_workspace_root_for_lsp(&state, &query).await {
        Ok(root) => root,
        Err((status, msg)) => {
            let err_json = serde_json::json!({ "error": msg, "code": status.as_u16() });
            return (status, axum::Json(err_json)).into_response();
        }
    };

    // Resolve LSP command
    let lsp_cmd = match resolve_lsp_command(&lang_lower) {
        Some(cmd) => cmd,
        None => {
            let msg = format!(
                "No LSP server found for language: {}. \
                 Install one (e.g., 'rust-analyzer', 'pylsp', 'gopls') and ensure it is on PATH.",
                language
            );
            let err_json = serde_json::json!({ "error": msg, "code": 400u16 });
            return (StatusCode::BAD_REQUEST, axum::Json(err_json)).into_response();
        }
    };

    tracing::info!(
        "[LSP] Upgrading WebSocket for language='{}', cmd='{}', workspace='{}'",
        lang_lower, lsp_cmd, workspace_root
    );

    let pool = Arc::clone(&state.lsp_pool);
    ws.on_upgrade(move |socket| lsp_relay(socket, lsp_cmd, workspace_root, pool))
}

/// Bidirectional LSP relay: WebSocket ↔ pooled LSP process
///
/// Uses the process pool to get/spawn an LSP process. When the WebSocket
/// disconnects, the LSP process stays alive for future reconnections.
async fn lsp_relay(
    socket: WebSocket,
    lsp_cmd: String,
    workspace_root: String,
    pool: Arc<LspPool>,
) {
    // Get or spawn from pool
    let entry = match pool.get_or_spawn(&lsp_cmd, &workspace_root).await {
        Ok(e) => e,
        Err(err) => {
            tracing::error!("[LSP] Failed to get/spawn '{}': {}", lsp_cmd, err);
            return;
        }
    };

    let stdin_tx = entry.stdin_tx.clone();
    let mut stdout_rx = entry.stdout_tx.subscribe();

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Task: LSP stdout (broadcast) → WebSocket
    let cmd_for_send = lsp_cmd.clone();
    let send_task = tokio::spawn(async move {
        loop {
            match stdout_rx.recv().await {
                Ok(msg) => {
                    if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        "[LSP] WebSocket client lagged {} messages for '{}'",
                        n, cmd_for_send
                    );
                    // Continue — we lost some messages but can still relay future ones
                }
                Err(_) => {
                    // Channel closed — LSP process died
                    break;
                }
            }
        }
        // Attempt to send close frame
        let _ = ws_tx.send(Message::Close(None)).await;
    });

    // Task: WebSocket → LSP stdin (via mpsc)
    let cmd_for_recv = lsp_cmd.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = ws_rx.next().await {
            let text: String = match msg {
                Ok(Message::Text(t)) => t.to_string(),
                Ok(Message::Binary(data)) => {
                    match String::from_utf8(data.to_vec()) {
                        Ok(s) => s,
                        Err(_) => continue,
                    }
                }
                Ok(Message::Close(_)) => break,
                _ => continue,
            };

            if stdin_tx.send(text).is_err() {
                tracing::warn!("[LSP] stdin channel closed for '{}'", cmd_for_recv);
                break;
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = send_task => {}
        _ = recv_task => {}
    }

    // Client disconnected — mark in pool (process stays alive)
    pool.client_disconnected(&lsp_cmd, &workspace_root).await;
    tracing::info!(
        "[LSP] WebSocket client disconnected for '{}' in '{}'",
        lsp_cmd, workspace_root
    );
}

/// Parse `Content-Length: N` from a header line.
pub fn parse_content_length(line: &str) -> Option<usize> {
    let line = line.trim();
    let prefix = "Content-Length:";
    if let Some(rest) = line.strip_prefix(prefix) {
        rest.trim().parse().ok()
    } else if let Some(rest) = line.strip_prefix("Content-length:") {
        // Some LSP servers use lowercase 'l'
        rest.trim().parse().ok()
    } else {
        None
    }
}

// spawn_lsp removed — spawning is now handled by LspPool::spawn_pooled

// ── Workspace root resolution ─────────────────────────────────────────

/// Resolve workspace root directory for LSP process.
///
/// If `agent_id` is provided, look up the agent's workspace from running agents.
/// Otherwise, the LSP process runs in the current directory (fallback).
async fn resolve_workspace_root_for_lsp(
    state: &AppState,
    query: &LspQuery,
) -> Result<String, (StatusCode, String)> {
    // If no agent_id, use current directory as fallback
    let Some(agent_id) = &query.agent_id else {
        return Ok(".".to_string());
    };

    let gw = state.gateway_state.read().await;
    let info = gw.running_agents.get(agent_id).ok_or_else(|| {
        (StatusCode::NOT_FOUND, "Agent not running — cannot resolve workspace".to_string())
    })?;

    let ws_id = query.workspace_id.as_deref().unwrap_or("");
    if ws_id.is_empty() || ws_id == "__agent_home__" {
        Ok(info.workspace.clone())
    } else {
        // Look up in workspace_config_json
        if let Some(json) = &info.workspace_config_json {
            #[derive(Deserialize)]
            struct AdditionalDir {
                id: String,
                path: String,
            }
            #[derive(Deserialize)]
            struct WsConfig {
                #[serde(default)]
                additional_dirs: Vec<AdditionalDir>,
            }

            if let Ok(cfg) = serde_json::from_str::<WsConfig>(json) {
                for dir in &cfg.additional_dirs {
                    if dir.id == ws_id {
                        return Ok(dir.path.clone());
                    }
                }
            }
        }

        Err((StatusCode::NOT_FOUND, format!("Workspace directory not found: {}", ws_id)))
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content_length_standard() {
        assert_eq!(parse_content_length("Content-Length: 42"), Some(42));
        assert_eq!(parse_content_length("Content-Length:0"), Some(0));
        assert_eq!(parse_content_length("Content-Length: 1234\r\n"), Some(1234));
    }

    #[test]
    fn test_parse_content_length_lowercase() {
        // Some LSP servers use lowercase 'l'
        assert_eq!(parse_content_length("Content-length: 99"), Some(99));
    }

    #[test]
    fn test_parse_content_length_invalid() {
        assert_eq!(parse_content_length("Content-Type: application/json"), None);
        assert_eq!(parse_content_length("X-Custom: 42"), None);
        assert_eq!(parse_content_length(""), None);
    }

    #[test]
    fn test_parse_content_length_not_a_number() {
        assert_eq!(parse_content_length("Content-Length: abc"), None);
    }

    #[test]
    fn test_resolve_lsp_command_known_languages() {
        let _ = resolve_lsp_command("rust");
        let _ = resolve_lsp_command("python");
        let _ = resolve_lsp_command("go");
    }

    #[test]
    fn test_resolve_lsp_command_unknown_language() {
        assert_eq!(resolve_lsp_command("brainfuck"), None);
        assert_eq!(resolve_lsp_command(""), None);
    }

    #[test]
    fn test_resolve_lsp_command_case_insensitive() {
        // Both "Rust" and "rust" should resolve to the same command
        let lower = resolve_lsp_command("rust");
        let upper = resolve_lsp_command("Rust");
        assert_eq!(lower, upper);
    }

    #[test]
    fn test_find_on_path_known_command() {
        // On Windows, `cmd` should always be on PATH
        #[cfg(windows)]
        assert!(find_on_path("cmd"));
        // On Unix, `ls` should always be on PATH
        #[cfg(not(windows))]
        assert!(find_on_path("ls"));
    }

    #[test]
    fn test_find_on_path_nonexistent() {
        assert!(!find_on_path("this_command_definitely_does_not_exist_12345"));
    }
}
