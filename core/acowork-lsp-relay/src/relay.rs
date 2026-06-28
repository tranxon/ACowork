//! Bidirectional LSP relay: WebSocket ↔ pooled LSP process.
//!
//! Uses the process pool to get/spawn an LSP process. When the WebSocket
//! disconnects, the LSP process stays alive for future reconnections.
//!
//! ## Initialize handshake caching
//!
//! LSP protocol only allows `initialize` once per server lifetime. When
//! a subsequent WebSocket client reconnects to an already-initialized
//! pooled process, the relay intercepts `initialize`/`initialized` messages:
//! - `initialize`: synthesises a response from the cached `InitializeResult`
//! - `initialized`: suppressed (not forwarded to the already-initialized LSP).

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};

use crate::config::LspServerSpec;
use crate::pool::LspPool;
use crate::protocol::{
    extract_jsonrpc_id, extract_method_hint, is_initialize_request, is_initialize_result,
    is_initialized_notification, substitute_jsonrpc_id,
};

/// Run the bidirectional LSP relay for a single WebSocket connection.
pub async fn lsp_relay(
    socket: WebSocket,
    spec: LspServerSpec,
    workspace_root: String,
    pool: Arc<LspPool>,
) {
    tracing::info!(
        "[LSP] relay — entering lsp_relay for cmd='{}' args={:?}, workspace='{}'",
        spec.command,
        spec.args,
        workspace_root
    );

    let entry = match pool
        .get_or_spawn(&spec.command, &spec.args, &workspace_root)
        .await
    {
        Ok(e) => {
            tracing::info!(
                "[LSP] relay — pool entry obtained for '{}', PID={}, active_clients={}",
                spec.command,
                e.pid,
                e.active_clients.load(std::sync::atomic::Ordering::Relaxed)
            );
            e
        }
        Err(err) => {
            tracing::error!(
                "[LSP] relay — Failed to get/spawn '{}': {}",
                spec.command,
                err
            );
            return;
        }
    };

    let stdin_tx = entry.stdin_tx.clone();
    let mut stdout_rx = entry.stdout_tx.subscribe();
    let entry_for_send = Arc::clone(&entry);
    let entry_for_recv = Arc::clone(&entry);

    let (synth_tx, mut synth_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Task: LSP stdout (broadcast) + synthesised messages → WebSocket
    let cmd_for_send = spec.command.clone();
    let mut send_task = tokio::spawn(async move {
        tracing::info!(
            "[LSP] relay — stdout→WS task started for '{}'",
            cmd_for_send
        );
        loop {
            tokio::select! {
                result = stdout_rx.recv() => {
                    match result {
                        Ok(msg) => {
                            if is_initialize_result(&msg) {
                                let mut cached = entry_for_send.init_result.lock().await;
                                if cached.is_none() {
                                    *cached = Some(msg.clone());
                                    tracing::info!(
                                        "[LSP] relay — cached InitializeResult for '{}' ({} bytes)",
                                        cmd_for_send,
                                        msg.len()
                                    );
                                }
                            }
                            let method_hint = extract_method_hint(&msg);
                            if method_hint.starts_with("workspace/") || method_hint == "client/registerFeature" || method_hint == "client/unregisterFeature" || method_hint == "(response)" {
                                tracing::info!(
                                    "[LSP] relay → WS: '{}' method='{}' len={}",
                                    cmd_for_send,
                                    method_hint,
                                    msg.len()
                                );
                            } else {
                                tracing::debug!(
                                    "[LSP] relay → WS: '{}' method='{}' len={}",
                                    cmd_for_send,
                                    method_hint,
                                    msg.len()
                                );
                            }
                            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                                tracing::warn!("[LSP] relay → WS: send failed for '{}', breaking", cmd_for_send);
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                "[LSP] WebSocket client lagged {} messages for '{}'",
                                n, cmd_for_send
                            );
                        }
                        Err(_) => {
                            tracing::warn!("[LSP] relay — stdout channel closed for '{}', breaking", cmd_for_send);
                            break;
                        }
                    }
                }
                Some(synth_msg) = synth_rx.recv() => {
                    tracing::debug!(
                        "[LSP] relay → WS (synth): '{}' len={}",
                        cmd_for_send,
                        synth_msg.len()
                    );
                    if ws_tx.send(Message::Text(synth_msg.into())).await.is_err() {
                        tracing::warn!("[LSP] relay → WS (synth): send failed for '{}', breaking", cmd_for_send);
                        break;
                    }
                }
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
        tracing::info!("[LSP] relay — stdout→WS task ended for '{}'", cmd_for_send);
    });

    // Task: WebSocket → LSP stdin (via mpsc)
    let cmd_for_recv = spec.command.clone();
    let mut recv_task = tokio::spawn(async move {
        tracing::info!("[LSP] relay — WS→stdin task started for '{}'", cmd_for_recv);
        let mut synthesized_init = false;
        while let Some(msg) = ws_rx.next().await {
            let text: String = match msg {
                Ok(Message::Text(t)) => {
                    let method_hint = extract_method_hint(t.as_str());
                    if method_hint.starts_with("workspace/")
                        || method_hint == "client/registerFeature"
                        || method_hint == "client/unregisterFeature"
                    {
                        tracing::info!(
                            "[LSP] relay WS → stdin: '{}' method='{}' len={}",
                            cmd_for_recv,
                            method_hint,
                            t.len()
                        );
                    } else {
                        tracing::debug!(
                            "[LSP] relay WS → stdin: '{}' method='{}' len={}",
                            cmd_for_recv,
                            method_hint,
                            t.len()
                        );
                    }
                    t.to_string()
                }
                Ok(Message::Binary(data)) => match String::from_utf8(data.to_vec()) {
                    Ok(s) => s,
                    Err(_) => continue,
                },
                Ok(Message::Close(_)) => break,
                _ => continue,
            };

            if is_initialize_request(&text) {
                let cached = entry_for_recv.init_result.lock().await;
                if let Some(ref cached_result) = *cached {
                    let req_id = extract_jsonrpc_id(&text);
                    let response = substitute_jsonrpc_id(cached_result, &req_id);
                    tracing::info!(
                        "[LSP] relay — synthesised init response for '{}' (reusing cached)",
                        cmd_for_recv
                    );
                    drop(cached);
                    synthesized_init = true;
                    let _ = synth_tx.send(response);
                    continue;
                }
                drop(cached);
            }

            if is_initialized_notification(&text) && synthesized_init {
                tracing::debug!(
                    "[LSP] relay — suppressed 'initialized' for '{}' (init was synthesised)",
                    cmd_for_recv
                );
                continue;
            }

            if stdin_tx.send(text).is_err() {
                tracing::warn!("[LSP] stdin channel closed for '{}'", cmd_for_recv);
                break;
            }
        }
    });

    let cmd_for_log = spec.command.clone();
    tokio::select! {
        r = &mut send_task => tracing::info!("[LSP] relay — send_task completed first for '{}' (result: {:?})", cmd_for_log, r),
        r = &mut recv_task => tracing::info!("[LSP] relay — recv_task completed first for '{}' (result: {:?})", cmd_for_log, r),
    }

    pool.client_disconnected(&spec.command, &spec.args, &workspace_root)
        .await;
    tracing::info!(
        "[LSP] relay — WebSocket client disconnected for '{}' in '{}'",
        spec.command,
        workspace_root
    );
}
