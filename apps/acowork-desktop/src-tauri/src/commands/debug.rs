//! Debug Protocol RPC commands (ADR-048 D6).
//!
//! The Desktop talks to the Runtime's DevMode debug surface through two
//! channels, mirroring the production IPC split (ADR-033 / ADR-034):
//!
//! - **RPC** (this module): `invoke("debug_rpc", …)` -> Gateway HTTP
//!   reverse proxy (`/api/agents/glm-5.3_common/debug/{path}`, one
//!   wildcard route added in ADR-048 D5) -> Runtime
//!   `/api/debug/{path}`. Replaces the legacy direct WebSocket
//!   JSON-RPC connection to `127.0.0.1:19878`.
//! - **Events**: MQTT `acowork/agents/glm-5.3_common/debug/events/{type}`
//!   (QoS 0), decoded in `commands/chat_mqtt.rs` and re-emitted on the
//!   `debug-event` Tauri channel for `stores/debugStore.ts`.
//!
//! One generic command serves every debug endpoint so that endpoints
//! added on the Runtime (D2 route table in
//! `acowork-runtime/src/http/debug.rs`) need no Desktop Rust change -
//! only a new call site in the frontend store.

use std::collections::HashMap;

use tauri::State;

use crate::state::AppState;

/// Relay a Debug Protocol HTTP RPC to the Runtime via the Gateway.
///
/// - `agent_id`: which agent's Runtime to address (path parameter of
///   the Gateway wildcard proxy route).
/// - `method`: `"GET"` or `"POST"`.
/// - `path`: `/api/debug/*` suffix, e.g. `"state"`,
///   `"context/3/sections/system_prompt"`, `"context/rewind"`.
/// - `query`: optional query parameters (e.g. `session_id` for GETs).
/// - `body`: optional JSON body (POSTs).
///
/// Returns the `data` field of the Runtime's `{ ok, data?, error? }`
/// envelope (`null` for endpoints that return no payload), or an error
/// message string on failure (Runtime debug error, Gateway proxy error,
/// or transport failure).
#[tauri::command]
pub async fn debug_rpc(
    state: State<'_, AppState>,
    agent_id: String,
    method: String,
    path: String,
    query: Option<HashMap<String, String>>,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let client = state.gateway.read().await;
    client
        .debug_rpc(&agent_id, &method, &path, query.as_ref(), body.as_ref())
        .await
        .map_err(|e| e.to_string())
}
