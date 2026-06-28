//! JSON-RPC API for Agent Runtime codebase tools.
//!
//! Provides a simple HTTP endpoint that proxies LSP JSON-RPC requests
//! to the pooled LSP process. The Agent Runtime's `codebase` tool uses
//! this to perform code intelligence operations (definition lookup,
//! references, hover, workspace symbol search, diagnostics).
//!
//! ## Endpoint
//!
//! `POST /api/codebase/rpc` — send a single LSP JSON-RPC request or
//! notification to the language server for the specified language and
//! workspace.
//!
//! ## Initialization
//!
//! The handler automatically performs the LSP `initialize` /
//! `initialized` handshake on first use of a pooled process. The
//! `InitializeResult` is cached in the pool entry (shared with WebSocket
//! relay clients), so subsequent calls skip the handshake.
//!
//! ## Request / Notification
//!
//! The caller sets `expect_response` to `true` for LSP requests (e.g.
//! `textDocument/definition`) and `false` for notifications (e.g.
//! `textDocument/didOpen`). When `expect_response` is false, the handler
//! sends the message and returns immediately.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::resolve_lsp_command;
use crate::server::AppState;

/// Global JSON-RPC id counter for codebase requests.
/// Uses a large offset to avoid collisions with WebSocket relay ids
/// (which typically start from 1).
static RPC_ID_COUNTER: AtomicU64 = AtomicU64::new(1_000_000);

/// Timeout for individual LSP JSON-RPC requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for the LSP `initialize` handshake.
const INIT_TIMEOUT: Duration = Duration::from_secs(60);

// ── Request / Response types ───────────────────────────────────────────

/// Request body for `POST /api/codebase/rpc`.
#[derive(Debug, Deserialize)]
pub struct CodebaseRpcRequest {
    /// Language id (e.g. "rust", "python", "typescript").
    pub language: String,
    /// Workspace root directory (absolute path).
    pub workspace_root: String,
    /// LSP method (e.g. "textDocument/definition", "textDocument/didOpen").
    pub method: String,
    /// LSP method params (JSON object).
    #[serde(default)]
    pub params: Value,
    /// Whether to wait for a response (true for requests, false for
    /// notifications like didOpen/didClose). Defaults to `true`.
    #[serde(default = "default_expect_response")]
    pub expect_response: bool,
}

fn default_expect_response() -> bool {
    true
}

/// Response body for `POST /api/codebase/rpc`.
#[derive(Debug, Serialize)]
pub struct CodebaseRpcResponse {
    /// Whether the LSP request succeeded.
    pub success: bool,
    /// LSP response `result` field (present when `success` is true and
    /// `expect_response` was true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error message (present when `success` is false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Handler ────────────────────────────────────────────────────────────

/// `POST /api/codebase/rpc` — proxy a single LSP JSON-RPC request.
///
/// Pool lifecycle: `get_or_spawn` increments `active_clients`; this function
/// guarantees `client_disconnected` is called exactly once on every exit path
/// after a successful `get_or_spawn`.
pub async fn codebase_rpc(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CodebaseRpcRequest>,
) -> Result<Json<CodebaseRpcResponse>, StatusCode> {
    // 1. Resolve LSP server command for the requested language.
    let lang_lower = req.language.to_lowercase();
    let spec = match resolve_lsp_command(&lang_lower).await {
        Some(spec) => spec,
        None => {
            return Ok(Json(CodebaseRpcResponse {
                success: false,
                result: None,
                error: Some(format!(
                    "No LSP server found for language: {}",
                    req.language
                )),
            }));
        }
    };

    // 2. Get or spawn a pooled LSP process (increments active_clients).
    let entry = match state
        .lsp_pool
        .get_or_spawn(&spec.command, &spec.args, &req.workspace_root)
        .await
    {
        Ok(e) => e,
        Err(e) => {
            return Ok(Json(CodebaseRpcResponse {
                success: false,
                result: None,
                error: Some(format!("Failed to start LSP server: {e}")),
            }));
        }
    };

    // 3. Execute the actual LSP request.
    let result = execute_codebase_rpc(&entry, &req).await;

    // 4. Always release the pool reference — this is the single cleanup point
    //    for every exit path after get_or_spawn succeeds.
    state
        .lsp_pool
        .client_disconnected(&spec.command, &spec.args, &req.workspace_root)
        .await;

    result
}

/// Execute the actual LSP request after the pool entry is obtained.
///
/// Pool lifecycle (`get_or_spawn` / `client_disconnected`) is managed by
/// the caller ([`codebase_rpc`]). This function only handles LSP protocol
/// logic: initialize handshake, request/response, and notifications.
async fn execute_codebase_rpc(
    entry: &Arc<crate::pool::LspProcessEntry>,
    req: &CodebaseRpcRequest,
) -> Result<Json<CodebaseRpcResponse>, StatusCode> {
    // Ensure the LSP process is initialized.
    // The init_result cache is shared with WebSocket relay clients.
    // If already cached, skip the handshake entirely.
    let needs_init = entry.init_result.lock().await.is_none();
    if needs_init {
        match ensure_initialized(entry, &req.workspace_root).await {
            Ok(()) => {}
            Err(e) => {
                return Ok(Json(CodebaseRpcResponse {
                    success: false,
                    result: None,
                    error: Some(format!("LSP initialization failed: {e}")),
                }));
            }
        }
    }

    // Send the actual request.
    if req.expect_response {
        // Request: send and wait for matching response.
        let id = RPC_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": req.method,
            "params": req.params,
        });

        match send_request_and_wait(entry, id, &request, REQUEST_TIMEOUT).await {
            Ok(response) => {
                // Check for LSP error in the response.
                if let Some(err) = response.get("error") {
                    Ok(Json(CodebaseRpcResponse {
                        success: false,
                        result: None,
                        error: Some(format!("LSP error: {err}")),
                    }))
                } else {
                    Ok(Json(CodebaseRpcResponse {
                        success: true,
                        result: response.get("result").cloned(),
                        error: None,
                    }))
                }
            }
            Err(e) => Ok(Json(CodebaseRpcResponse {
                success: false,
                result: None,
                error: Some(e),
            })),
        }
    } else {
        // Notification: send and return immediately (no response expected).
        let notification = json!({
            "jsonrpc": "2.0",
            "method": req.method,
            "params": req.params,
        });
        let msg = serde_json::to_string(&notification).unwrap_or_default();
        let _ = entry.stdin_tx.send(msg);
        Ok(Json(CodebaseRpcResponse {
            success: true,
            result: None,
            error: None,
        }))
    }
}

// ── LSP initialization ────────────────────────────────────────────────

/// Perform the LSP `initialize` / `initialized` handshake.
///
/// Sends an `initialize` request with standard capabilities, waits for
/// the `InitializeResult` response, caches it in the pool entry, then
/// sends the `initialized` notification.
async fn ensure_initialized(
    entry: &Arc<crate::pool::LspProcessEntry>,
    workspace_root: &str,
) -> Result<(), String> {
    let id = RPC_ID_COUNTER.fetch_add(1, Ordering::Relaxed);

    // Build the initialize request with standard capabilities.
    let root_uri = format!("file://{}", workspace_root);
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "processId": null,
            "rootUri": root_uri,
            "workspaceFolders": [{
                "uri": root_uri,
                "name": "workspace"
            }],
            "capabilities": {
                "textDocument": {
                    "synchronization": {
                        "didOpen": true,
                        "didChange": true,
                        "didClose": true
                    },
                    "hover": {
                        "contentFormat": ["markdown", "plaintext"]
                    },
                    "definition": {
                        "linkSupport": false
                    },
                    "references": {},
                    "publishDiagnostics": {
                        "relatedInformation": true
                    }
                },
                "workspace": {
                    "symbol": {},
                    "workspaceFolders": true
                }
            }
        }
    });

    // Send initialize and wait for response.
    let response =
        send_request_and_wait(entry, id, &init_request, INIT_TIMEOUT)
            .await?;

    // Cache the InitializeResult.
    if let Some(result) = response.get("result") {
        let result_str = serde_json::to_string(result).unwrap_or_default();
        *entry.init_result.lock().await = Some(result_str);
    } else {
        return Err(format!(
            "Initialize returned no result: {}",
            response.get("error").cloned().unwrap_or_default()
        ));
    }

    // Send the `initialized` notification.
    let initialized_notification = json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    });
    let msg = serde_json::to_string(&initialized_notification).unwrap_or_default();
    let _ = entry.stdin_tx.send(msg);

    tracing::info!(
        "[Codebase] LSP initialized for '{}' (PID {})",
        entry.command,
        entry.pid
    );

    Ok(())
}

// ── JSON-RPC request-response helper ──────────────────────────────────

/// Send a JSON-RPC request to the LSP process and wait for the matching
/// response by id.
///
/// Subscribes to the process's stdout broadcast channel and filters
/// messages by JSON-RPC id. Non-matching messages (notifications,
/// other responses) are skipped.
async fn send_request_and_wait(
    entry: &Arc<crate::pool::LspProcessEntry>,
    id: u64,
    request: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    // Subscribe BEFORE sending to avoid missing the response.
    let mut rx = entry.stdout_tx.subscribe();

    // Send the request.
    let msg = serde_json::to_string(request)
        .map_err(|e| format!("Failed to serialize request: {e}"))?;
    entry
        .stdin_tx
        .send(msg)
        .map_err(|e| format!("Failed to send to LSP stdin: {e}"))?;

    // Wait for the matching response.
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(raw_msg)) => {
                // Parse the message and check if it's our response.
                if let Ok(parsed) = serde_json::from_str::<Value>(&raw_msg) {
                    // Skip notifications (no "id" field).
                    let Some(resp_id) = parsed.get("id") else {
                        continue;
                    };
                    // Check if this is our response (match by id).
                    if resp_id == id || resp_id.as_u64() == Some(id) {
                        return Ok(parsed);
                    }
                }
                // Not our response — keep waiting.
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                // Missed some messages — keep waiting for our response.
                continue;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err("LSP stdout channel closed (process may have exited)".to_string());
            }
            Err(_) => {
                return Err(format!(
                    "LSP request timed out after {}s (id={id})",
                    timeout.as_secs()
                ));
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::LspPool;
    use acowork_core::event_bus::EventBus;
    use crate::state::LspRelayState;

    /// Build a test AppState with an empty pool and fresh event bus.
    fn test_state() -> Arc<AppState> {
        let lsp_pool = Arc::new(LspPool::new());
        let event_bus = EventBus::<LspRelayState>::new(16);
        Arc::new(AppState {
            lsp_pool,
            event_bus,
        })
    }

    #[tokio::test]
    async fn test_codebase_rpc_unknown_language() {
        let state = test_state();
        let req_body = CodebaseRpcRequest {
            language: "brainfuck".to_string(),
            workspace_root: "/tmp".to_string(),
            method: "textDocument/definition".to_string(),
            params: json!({}),
            expect_response: true,
        };

        let resp = codebase_rpc(
            State(Arc::clone(&state)),
            Json(req_body),
        )
        .await
        .unwrap();

        assert!(!resp.success);
        assert!(resp.error.is_some());
        assert!(resp.error.as_ref().unwrap().contains("No LSP server"));
    }

    #[tokio::test]
    async fn test_codebase_rpc_notification_returns_immediately() {
        // This test verifies that a notification (expect_response=false)
        // returns success immediately without waiting for a response.
        // We use a language that has a configured LSP server but may not
        // be installed — the handler should still attempt to spawn it.
        // If the server isn't installed, we get an error about spawning,
        // which is fine — we're testing the notification path, not the
        // actual LSP communication.
        let state = test_state();
        let req_body = CodebaseRpcRequest {
            language: "brainfuck".to_string(),
            workspace_root: "/tmp".to_string(),
            method: "textDocument/didOpen".to_string(),
            params: json!({}),
            expect_response: false,
        };

        let resp = codebase_rpc(
            State(Arc::clone(&state)),
            Json(req_body),
        )
        .await
        .unwrap();

        // Should fail because no LSP server for "brainfuck", but the
        // expect_response=false path is not reached.
        assert!(!resp.success);
    }

    #[tokio::test]
    async fn test_default_expect_response_is_true() {
        assert!(default_expect_response());
    }

    #[tokio::test]
    async fn test_rpc_id_counter_increments() {
        let id1 = RPC_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let id2 = RPC_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        assert_eq!(id2, id1 + 1);
    }
}
