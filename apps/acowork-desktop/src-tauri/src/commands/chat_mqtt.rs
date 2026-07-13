//! Tauri commands for MQTT operations (ADR-033 Phase 3)
//!
//! These commands are called from the React frontend via `invoke()`:
//! - `connect_mqtt` — connect to the MQTT broker
//! - `disconnect_mqtt` — disconnect and clean up
//! - `mqtt_subscribe_agent` — subscribe to session events for an agent
//! - `mqtt_publish_control` — publish a control command (protobuf-encoded)

use std::sync::Arc;

use prost::Message;
use tauri::Emitter;

use acowork_core::mqtt_proto::{
    self, ControlCommand, DataEnvelope,
    control_command, data_envelope, session_message,
};
use crate::mqtt_client::{DesktopMqttClient, MqttMessage};
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

    // Create callback that decodes MQTT protobuf messages and emits
    // structured flat-JSON events to the React frontend.
    // Also emits raw "mqtt-event" for debugging.
    let app_handle = app.clone();
    let on_message = move |msg: MqttMessage| {
        // Always emit raw event for debugging
        let raw_payload = serde_json::json!({
            "topic": msg.topic,
            "payload_base64": base64_encode(&msg.payload),
        });
        let _ = app_handle.emit("mqtt-event", raw_payload);

        // Try to decode as DataEnvelope protobuf
        let envelope = match DataEnvelope::decode(&msg.payload[..]) {
            Ok(e) => e,
            Err(_) => return, // Not protobuf — ignore (plain text topics handled elsewhere)
        };

        let Some(payload) = &envelope.payload else { return };

        match payload {
            // ── Session message events (streaming) ──
            data_envelope::Payload::SessionMessage(sm) => {
                if let Some(event) = &sm.event {
                    if let Some(flat) = session_message_to_flat(sm.agent_id.as_str(), sm.session_id.as_str(), event) {
                        let _ = app_handle.emit("agent-event", flat);
                    }
                }
            }

            // ── Session lifecycle ──
            data_envelope::Payload::SessionCreated(created) => {
                let event = serde_json::json!({
                    "type": "session_created",
                    "agent_id": created.agent_id,
                    "session_id": created.session_id,
                    "title": created.title,
                    "created_at": created.created_at,
                });
                let _ = app_handle.emit("agent-event", event);
            }
            data_envelope::Payload::SessionDeleted(deleted) => {
                let event = serde_json::json!({
                    "type": "session_deleted",
                    "agent_id": deleted.agent_id,
                    "session_id": deleted.session_id,
                    "deleted_at": deleted.deleted_at,
                });
                let _ = app_handle.emit("agent-event", event);
            }

            // ── Session meta (model_confirmed usage etc.) ──
            data_envelope::Payload::SessionMeta(meta) => {
                let event = serde_json::json!({
                    "type": "session_meta",
                    "agent_id": meta.agent_id,
                    "session_id": meta.session_id,
                    "title": meta.title,
                    "state": meta.state,
                    "message_count": meta.message_count,
                    "provider_id": meta.provider_id,
                    "model_id": meta.model_id,
                    "input_tokens": meta.input_tokens,
                    "output_tokens": meta.output_tokens,
                    "total_input_tokens": meta.total_input_tokens,
                    "total_output_tokens": meta.total_output_tokens,
                });
                let _ = app_handle.emit("agent-event", event);
            }

            // ── Agent lifecycle ──
            data_envelope::Payload::AgentStatus(status) => {
                let event = serde_json::json!({
                    "type": "agent_status",
                    "agent_id": status.agent_id,
                    "online": status.online,
                });
                let _ = app_handle.emit("agent-event", event);
            }
            data_envelope::Payload::AgentMeta(meta) => {
                let event = serde_json::json!({
                    "type": "agent_meta",
                    "agent_id": meta.agent_id,
                    "name": meta.name,
                    "version": meta.version,
                    "avatar": meta.avatar,
                    "builtin_avatar": meta.builtin_avatar,
                });
                let _ = app_handle.emit("agent-event", event);
            }
            data_envelope::Payload::AgentConfig(config) => {
                let event = serde_json::json!({
                    "type": "agent_config",
                    "agent_id": config.agent_id,
                    "config_json": config.config_json,
                });
                let _ = app_handle.emit("agent-event", event);
            }

            // ── Sidecar ──
            data_envelope::Payload::SidecarStatus(sc) => {
                let event = serde_json::json!({
                    "type": "sidecar_status",
                    "kind": sc.kind,
                    "endpoint": sc.endpoint,
                    "ready": sc.ready,
                });
                let _ = app_handle.emit("agent-event", event);
            }

            // ── Global resources & control commands: ignore (Gateway handles these) ──
            _ => {}
        }
    };

    let client = DesktopMqttClient::connect_default(user_id, on_message).await?;

    // Subscribe to agent lifecycle topics
    client.subscribe_agent_lifecycle().await?;

    // ADR-033: Subscribe to all session message topics (streaming chunks,
    // context_usage, session_state_changed, etc.) so the frontend receives
    // real-time session events across all sessions.
    client.subscribe(
        "acowork/agents/+/sessions/+/messages/#",
        crate::mqtt_client::MqttQoS::AtMostOnce,
    ).await?;

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

/// Subscribe to session events for a specific agent session.
///
/// Called when the user enters the chat view for a specific session.
/// Use this instead of `mqtt_subscribe_agent_sessions` when only one
/// session is active to avoid bandwidth waste from other sessions.
#[allow(dead_code)]
#[tauri::command]
pub async fn mqtt_subscribe_agent_session(
    agent_id: String,
    session_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let guard = state.mqtt_client.lock().await;
    let client = guard
        .as_ref()
        .ok_or_else(|| "MQTT client not connected".to_string())?;

    let client = client.lock().await;
    client.subscribe_agent_session(&agent_id, &session_id).await
}

/// Unsubscribe from a specific agent session's events.
///
/// Called when switching away from a session to stop receiving stale events.
#[allow(dead_code)]
#[tauri::command]
pub async fn mqtt_unsubscribe_agent_session(
    agent_id: String,
    session_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let guard = state.mqtt_client.lock().await;
    let client = guard
        .as_ref()
        .ok_or_else(|| "MQTT client not connected".to_string())?;

    let client = client.lock().await;
    client.unsubscribe_agent_session(&agent_id, &session_id).await
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
    // ADR-033: subscribe_agent_sessions is deprecated but this is the
    // all-sessions subscription — per-session requires a session_id
    #[allow(deprecated)]
    client.subscribe_agent_sessions(&agent_id).await
}

/// Publish a control command via MQTT (protobuf-encoded DataEnvelope).
///
/// Used for fire-and-forget commands: send_message, stop, create_session,
/// delete_session, model_switch, reasoning_effort, compact_context, workspace_switch.
/// For commands requiring acknowledgment, use HTTP instead.
///
/// The payload_json is a JSON object whose shape depends on the command:
/// - "message": { "session_id", "message_id", "content" }
/// - "stop": { "session_id" }
/// - "create_session": {}
/// - "delete_session": { "session_id" }
/// - "model_switch": { "session_id", "model_id" }
/// - "reasoning_effort": { "session_id", "effort" }
/// - "compact_context": { "session_id" }
/// - "workspace_switch": { "session_id", "workspace_id" }
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

    tracing::info!(
        agent_id = %agent_id,
        command = %command,
        "mqtt_publish_control: publishing control command"
    );

    // Build ControlCommand protobuf from the JSON payload.
    let control = build_control_command(&agent_id, &command, &payload_json)?;

    client.publish_control_protobuf(&agent_id, control).await
}

/// Build a `ControlCommand` protobuf from a JSON payload and command type.
///
/// Maps frontend JSON → protobuf per `docs/zh/protocols/mqtt.md` §9.1.
fn build_control_command(
    agent_id: &str,
    command: &str,
    json: &serde_json::Value,
) -> Result<ControlCommand, String> {
    let session_id = json.get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let cmd = match command {
        "create_session" => control_command::Command::CreateSession(
            mqtt_proto::CreateSessionCommand {
                agent_id: agent_id.to_string(),
            },
        ),
        "delete_session" => control_command::Command::DeleteSession(
            mqtt_proto::DeleteSessionCommand {
                agent_id: agent_id.to_string(),
                session_id,
            },
        ),
        "message" => {
            let message_id = json.get("message_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = json.get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::Message(
                mqtt_proto::MessageCommand {
                    agent_id: agent_id.to_string(),
                    session_id,
                    message_id,
                    content,
                },
            )
        }
        "stop" => control_command::Command::Stop(
            mqtt_proto::StopCommand {
                agent_id: agent_id.to_string(),
                session_id,
            },
        ),
        "model_switch" => {
            let model_id = json.get("model_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::ModelSwitch(
                mqtt_proto::ModelSwitchCommand {
                    agent_id: agent_id.to_string(),
                    session_id,
                    model_id,
                },
            )
        }
        "reasoning_effort" => {
            let effort = json.get("effort")
                .and_then(|v| v.as_str())
                .unwrap_or("medium")
                .to_string();
            control_command::Command::ReasoningEffort(
                mqtt_proto::ReasoningEffortCommand {
                    agent_id: agent_id.to_string(),
                    session_id,
                    effort,
                },
            )
        }
        "compact_context" => control_command::Command::CompactContext(
            mqtt_proto::CompactContextCommand {
                agent_id: agent_id.to_string(),
                session_id,
            },
        ),
        "workspace_switch" => {
            let workspace_id = json.get("workspace_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::WorkspaceSwitch(
                mqtt_proto::WorkspaceSwitchCommand {
                    agent_id: agent_id.to_string(),
                    session_id,
                    workspace_id,
                },
            )
        }
        other => return Err(format!("Unknown control command: {}", other)),
    };

    Ok(ControlCommand {
        agent_id: agent_id.to_string(),
        command: Some(cmd),
    })
}

/// Convert a `session_message::Event` protobuf oneof to flat JSON
/// matching the old WebSocket event format that `handleMessageEvent` expects.
fn session_message_to_flat(
    agent_id: &str,
    session_id: &str,
    event: &session_message::Event,
) -> Option<serde_json::Value> {
    let base = serde_json::json!({
        "agent_id": agent_id,
        "session_id": session_id,
    });

    match event {
        session_message::Event::Chunk(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("chunk".into()));
            m.insert("message_id".into(), serde_json::Value::String(p.message_id.clone()));
            m.insert("delta".into(), serde_json::Value::String(p.delta.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::ToolCall(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("tool_call".into()));
            m.insert("message_id".into(), serde_json::Value::String(p.message_id.clone()));
            m.insert("tool_name".into(), serde_json::Value::String(p.tool_name.clone()));
            m.insert("arguments_json".into(), serde_json::Value::String(p.arguments_json.clone()));
            m.insert("call_id".into(), serde_json::Value::String(p.call_id.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::ToolResult(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("tool_result".into()));
            m.insert("call_id".into(), serde_json::Value::String(p.call_id.clone()));
            m.insert("result_json".into(), serde_json::Value::String(p.result_json.clone()));
            m.insert("is_error".into(), serde_json::Value::Bool(p.is_error));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::Done(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("done".into()));
            m.insert("message_id".into(), serde_json::Value::String(p.message_id.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::Error(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("error".into()));
            m.insert("message_id".into(), serde_json::Value::String(p.message_id.clone()));
            m.insert("content".into(), serde_json::Value::String(p.error.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::Stopped(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("stopped".into()));
            m.insert("message_id".into(), serde_json::Value::String(p.message_id.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::AskQuestion(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("ask_question".into()));
            m.insert("message_id".into(), serde_json::Value::String(p.message_id.clone()));
            // Parse question_json into object if possible
            match serde_json::from_str::<serde_json::Value>(&p.question_json) {
                Ok(val) => { m.insert("question".into(), val); }
                Err(_) => { m.insert("question_json".into(), serde_json::Value::String(p.question_json.clone())); }
            }
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::TodoUpdated(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("todo_list_updated".into()));
            match serde_json::from_str::<serde_json::Value>(&p.todos_json) {
                Ok(val) => { m.insert("todos".into(), val); }
                Err(_) => { m.insert("todos_json".into(), serde_json::Value::String(p.todos_json.clone())); }
            }
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::ReasoningStarted(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("reasoning_started".into()));
            m.insert("message_id".into(), serde_json::Value::String(p.message_id.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::ReasoningEnded(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("reasoning_ended".into()));
            m.insert("message_id".into(), serde_json::Value::String(p.message_id.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::CompactingStarted(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("compacting_started".into()));
            m.insert("session_id".into(), serde_json::Value::String(p.session_id.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::CompactingEnded(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("compacting_ended".into()));
            m.insert("session_id".into(), serde_json::Value::String(p.session_id.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::ContextUsage(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("context_usage".into()));
            m.insert("input_tokens".into(), serde_json::json!(p.input_tokens));
            m.insert("output_tokens".into(), serde_json::json!(p.output_tokens));
            m.insert("total_input_tokens".into(), serde_json::json!(p.total_input_tokens));
            m.insert("total_output_tokens".into(), serde_json::json!(p.total_output_tokens));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::MemoryUpdated(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("memory_updated".into()));
            m.insert("node_id".into(), serde_json::Value::String(p.node_id.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::SkillExecuted(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("skill_executed".into()));
            m.insert("skill_name".into(), serde_json::Value::String(p.skill_name.clone()));
            m.insert("success".into(), serde_json::Value::Bool(p.success));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::SessionStateChanged(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("session_state_changed".into()));
            m.insert("model".into(), serde_json::Value::String(p.model.clone()));
            m.insert("provider".into(), serde_json::Value::String(p.provider.clone()));
            m.insert("workspace_id".into(), serde_json::Value::String(p.workspace_id.clone()));
            m.insert("ratio".into(), serde_json::json!(p.ratio));
            m.insert("reasoning_effort".into(), serde_json::Value::String(p.reasoning_effort.clone()));
            m.insert("temperature".into(), serde_json::json!(p.temperature));
            if !p.status_json.is_empty() {
                match serde_json::from_str::<serde_json::Value>(&p.status_json) {
                    Ok(val) => { m.insert("status".into(), val); }
                    Err(_) => { m.insert("status_json".into(), serde_json::Value::String(p.status_json.clone())); }
                }
            }
            if !p.context_usage_json.is_empty() {
                match serde_json::from_str::<serde_json::Value>(&p.context_usage_json) {
                    Ok(val) => { m.insert("context_usage".into(), val); }
                    Err(_) => { m.insert("context_usage_json".into(), serde_json::Value::String(p.context_usage_json.clone())); }
                }
            }
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::IterationLimitPaused(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("iteration_limit_paused".into()));
            m.insert("iteration".into(), serde_json::json!(p.iteration));
            m.insert("max_iterations".into(), serde_json::json!(p.max_iterations));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::ToolApprovalNeeded(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("tool_approval_needed".into()));
            m.insert("request_id".into(), serde_json::Value::String(p.request_id.clone()));
            m.insert("tool_name".into(), serde_json::Value::String(p.tool_name.clone()));
            m.insert("action".into(), serde_json::Value::String(p.action.clone()));
            m.insert("risk_level".into(), serde_json::Value::String(p.risk_level.clone()));
            m.insert("reason".into(), serde_json::Value::String(p.reason.clone()));
            m.insert("tool_call_id".into(), serde_json::Value::String(p.tool_call_id.clone()));
            m.insert("approval_timeout_secs".into(), serde_json::json!(p.approval_timeout_secs));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::NewDataAvailable(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("new_data_available".into()));
            m.insert("interval_ms".into(), serde_json::json!(p.interval_ms));
            if !p.title.is_empty() {
                m.insert("title".into(), serde_json::Value::String(p.title.clone()));
            }
            Some(serde_json::Value::Object(m))
        }
    }
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
