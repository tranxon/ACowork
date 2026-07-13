//! Tauri commands for MQTT operations (ADR-033 Phase 3)
//!
//! These commands are called from the React frontend via `invoke()`:
//! - `connect_mqtt` — connect to the MQTT broker
//! - `disconnect_mqtt` — disconnect and clean up
//! - `mqtt_subscribe_agent` — subscribe to session events for an agent
//! - `mqtt_publish_control` — publish a control command

use std::sync::Arc;

use tauri::Emitter;

use crate::mqtt_client::{DesktopMqttClient, MqttMessage, SharedDesktopMqttClient};
use crate::state::AppState;

/// Connect to the MQTT broker and start receiving events.
///
/// Called by the frontend after the Gateway is confirmed healthy.
/// Subscribes to agent lifecycle topics and starts forwarding events
/// to the frontend via `app.emit("mqtt-event", payload)`.
#[tauri::command]
pub async fn connect_mqtt(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.mqtt_client.lock().await;
    if guard.is_some() {
        return Ok(()); // Already connected
    }

    let user_id = "default"; // Single-user phase; multi-user will use actual user_id

    // Create callback that forwards MQTT messages to the React frontend
    let app_handle = app.clone();
    let on_message = move |msg: MqttMessage| {
        let payload = serde_json::json!({
            "topic": msg.topic,
            "payload_base64": base64_encode(&msg.payload),
        });
        let _ = app_handle.emit("mqtt-event", payload);
    };

    let client = DesktopMqttClient::connect_default(user_id, on_message).await?;

    // Subscribe to agent lifecycle topics
    client.subscribe_agent_lifecycle().await?;

    let shared = Arc::new(tokio::sync::Mutex::new(client));
    *guard = Some(shared);

    tracing::info!("Desktop MQTT client connected and subscribed to agent lifecycle");
    Ok(())
}

/// Disconnect the MQTT client.
#[tauri::command]
pub async fn disconnect_mqtt(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.mqtt_client.lock().await;
    *guard = None;
    tracing::info!("Desktop MQTT client disconnected");
    Ok(())
}

/// Subscribe to session events for a specific agent.
///
/// Called when the user enters the chat view for an agent.
#[tauri::command]
pub async fn mqtt_subscribe_agent_sessions(
    agent_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let guard = state.mqtt_client.lock().await;
    let client = guard
        .as_ref()
        .ok_or_else(|| "MQTT client not connected".to_string())?;

    let client = client.lock().await;
    client.subscribe_agent_sessions(&agent_id).await
}

/// Publish a control command via MQTT.
///
/// Used for fire-and-forget commands: send_message, stop, create_session,
/// delete_session. For commands requiring acknowledgment, use HTTP instead.
#[tauri::command]
pub async fn mqtt_publish_control(
    agent_id: String,
    command: String,
    payload_json: serde_json::Value,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let guard = state.mqtt_client.lock().await;
    let client = guard
        .as_ref()
        .ok_or_else(|| "MQTT client not connected".to_string())?;

    let client = client.lock().await;
    client.publish_control_json(&agent_id, &command, &payload_json).await
}

/// Simple base64 encoder (no external dependency needed for tests).
fn base64_encode(data: &[u8]) -> String {
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
