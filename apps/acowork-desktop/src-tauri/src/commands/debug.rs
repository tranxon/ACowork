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
//!
//! ADR-048 follow-up: a dedicated `enable_agent_debug` command exists
//! alongside the generic `debug_rpc` for the runtime DevMode flip path
//! (see `ResultsPanel.tsx` "Enable Debug" button). It exists separately
//! because the generic `debug_rpc` is gated on `debugStore.debugAgentId`
//! (set by `connect()`) — but at the moment the user clicks "Enable
//! Debug", that field is necessarily `null` because the agent isn't in
//! DevMode yet. Routing through a dedicated command avoids the
//! chicken-and-egg.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::state::AppState;

/// Response shape for `enable_agent_debug` — mirrors the Runtime's
/// `EnableDebugResult` (see `acowork-runtime/src/http/debug.rs`).
///
/// `already_enabled: true` means the runtime reported a no-op
/// confirmation (DevMode was already active); the Desktop still needs
/// to refresh the agent list to pick up the Gateway-side state
/// transition, but the user-facing copy can be "DevMode is on" rather
/// than "DevMode just turned on".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnableDebugResult {
    pub enabled: bool,
    pub already_enabled: bool,
    pub debug_port: u32,
}

/// Flip DevMode on at runtime for `agent_id`. Idempotent — calling
/// twice is safe; the second call returns `already_enabled: true`.
///
/// Routes through the same Gateway wildcard proxy as [`debug_rpc`]:
/// `POST /api/agents/{agent_id}/debug/enable` -> Runtime
/// `/api/debug/enable`. The Gateway's `proxy_debug_rpc` hook updates
/// `running_agents[id].debug_state = Enabled` on a 2xx response, so
/// after this command returns the Desktop's next `fetchAgents` call
/// will report `debug_state = "enabled"` and the Debug Panel +
/// step/pause/resume controls become active.
#[tauri::command]
pub async fn enable_agent_debug(
    state: State<'_, AppState>,
    agent_id: String,
    debug_port: Option<u32>,
) -> Result<EnableDebugResult, String> {
    // Wrap the Runtime's `{ ok, data, error }` envelope — `debug_rpc`
    // already unwraps `data`, so we receive either the
    // `EnableDebugResult` payload directly or a transport-level error.
    let body = serde_json::json!({
        "debug_port": debug_port.unwrap_or(0),
    });
    let data = {
        let client = state.gateway.read().await;
        client
            .debug_rpc(&agent_id, "POST", "enable", None, Some(&body))
            .await
            .map_err(|e| e.to_string())?
    };
    serde_json::from_value::<EnableDebugResult>(data)
        .map_err(|e| format!("malformed /api/debug/enable response: {}", e))
}

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
