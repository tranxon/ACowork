//! Tauri commands for MQTT operations (ADR-033 Phase 3)
//!
//! These commands are called from the React frontend via `invoke()`:
//! - `connect_mqtt` — connect to the MQTT broker
//! - `disconnect_mqtt` — disconnect and clean up
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
                if let Some(event) = &sm.event
                    && let Some(flat) = session_message_to_flat(sm.agent_id.as_str(), sm.session_id.as_str(), event)
                {
                    let _ = app_handle.emit("agent-event", flat);
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
/// Use this to avoid bandwidth waste from other sessions.
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

/// Publish a control command via MQTT (protobuf-encoded DataEnvelope).
///
/// ADR-034 Phase 5: All 17 control commands supported via MQTT.
/// No HTTP fallback for any control command.
///
/// The payload_json is a JSON object whose shape depends on the command:
/// - "chat_message": { "session_id", "message_id", "content", "command", "params_json" }
/// - "stop": { "session_id", "reason" }
/// - "create_session": {}
/// - "delete_session": { "session_id" }
/// - "close_session": { "session_id" }
/// - "update_session_title": { "session_id", "title" }
/// - "continue_execution": { "session_id", "reason" }
/// - "enable_notify": { "session_id" }
/// - "disable_notify": { "session_id" }
/// - "approval_decision": { "session_id", "request_id", "approved", "allow_all_session", "reason" }
/// - "question_answer": { "session_id", "request_id", "answer" }
/// - "model_switch": { "session_id", "model_id", "provider_id" }
/// - "reasoning_effort": { "session_id", "effort" }
/// - "workspace_switch": { "session_id", "workspace_id" }
/// - "compact_context": { "session_id" }
/// - "compress_action": { "session_id", "compress_type" }
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
/// Maps frontend JSON → protobuf per ADR-034 §3.2 / `docs/zh/protocols/mqtt.md` §9.1.
/// ADR-034 Phase 5: all sub-command `agent_id` fields removed (now only in ControlCommand top level);
/// 8 new commands added; "message" renamed to "chat_message" with params_json.
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
        // ── Session lifecycle ──
        "create_session" => control_command::Command::CreateSession(
            mqtt_proto::CreateSession {},
        ),
        "delete_session" => control_command::Command::DeleteSession(
            mqtt_proto::DeleteSession {
                session_id,
            },
        ),
        "close_session" => control_command::Command::CloseSession(
            mqtt_proto::CloseSession {
                session_id,
            },
        ),
        "update_session_title" => {
            let title = json.get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::UpdateSessionTitle(
                mqtt_proto::UpdateSessionTitle {
                    session_id,
                    title,
                },
            )
        }

        // ── Chat ──
        "chat_message" => {
            let message_id = json.get("message_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = json.get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cmd_text = json.get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let params_json = json.get("params_json")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::ChatMessage(
                mqtt_proto::ChatMessage {
                    session_id,
                    message_id,
                    content,
                    command: cmd_text,
                    params_json,
                },
            )
        }
        "stop" => {
            let reason = json.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("user_requested")
                .to_string();
            control_command::Command::Stop(
                mqtt_proto::Stop {
                    session_id,
                    reason,
                },
            )
        }
        "continue_execution" => {
            let reason = json.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("user_requested")
                .to_string();
            control_command::Command::ContinueExecution(
                mqtt_proto::ContinueExecution {
                    session_id,
                    reason,
                },
            )
        }
        "enable_notify" => control_command::Command::EnableNotify(
            mqtt_proto::EnableNotify {
                session_id,
            },
        ),
        "disable_notify" => control_command::Command::DisableNotify(
            mqtt_proto::DisableNotify {
                session_id,
            },
        ),

        // ── User responses to runtime prompts ──
        "approval_decision" => {
            let request_id = json.get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let approved = json.get("approved")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let allow_all = json.get("allow_all_session")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let reason = json.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::ApprovalDecision(
                mqtt_proto::ApprovalDecision {
                    session_id,
                    request_id,
                    approved,
                    allow_all_session: allow_all,
                    reason,
                },
            )
        }
        "question_answer" => {
            let request_id = json.get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let answer = json.get("answer")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::QuestionAnswer(
                mqtt_proto::QuestionAnswer {
                    session_id,
                    request_id,
                    answer,
                },
            )
        }

        // ── Per-session config ──
        "model_switch" => {
            let model_id = json.get("model_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let provider_id = json.get("provider_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::ModelSwitch(
                mqtt_proto::ModelSwitch {
                    session_id,
                    model_id,
                    provider_id,
                },
            )
        }
        "reasoning_effort" => {
            let effort = json.get("effort")
                .and_then(|v| v.as_str())
                .unwrap_or("medium")
                .to_string();
            control_command::Command::ReasoningEffort(
                mqtt_proto::ReasoningEffort {
                    session_id,
                    effort,
                },
            )
        }
        "workspace_switch" => {
            let workspace_id = json.get("workspace_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::WorkspaceSwitch(
                mqtt_proto::WorkspaceSwitch {
                    session_id,
                    workspace_id,
                },
            )
        }

        // ── Context management ──
        "compact_context" => control_command::Command::CompactContext(
            mqtt_proto::CompactContext {
                session_id,
            },
        ),
        "compress_action" => {
            let compress_type = json.get("compress_type")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            control_command::Command::CompressAction(
                mqtt_proto::CompressAction {
                    session_id,
                    compress_type,
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
            // Prefer the fully-populated `context_usage_json` payload when the
            // Runtime publishes it: it carries `context_window`, `total_tokens`,
            // `usage_percent` and `usable_context` that the StatusBar needs.
            // Falling back to the legacy 4 token-count fields would render the
            // StatusBar with `undefined` and crash `formatTokenCount`.
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("context_usage".into()));
            // Legacy per-field tokens (kept for any older subscriber).
            m.insert("input_tokens".into(), serde_json::json!(p.input_tokens));
            m.insert("output_tokens".into(), serde_json::json!(p.output_tokens));
            m.insert("total_input_tokens".into(), serde_json::json!(p.total_input_tokens));
            m.insert("total_output_tokens".into(), serde_json::json!(p.total_output_tokens));
            if !p.context_usage_json.is_empty() {
                match serde_json::from_str::<serde_json::Value>(&p.context_usage_json) {
                    Ok(val) => { m.insert("context_usage".into(), val); }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to parse ContextUsagePayload.context_usage_json");
                    }
                }
            }
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
        session_message::Event::StreamDelta(p) => {
            // ADR-035: incremental streaming delta carrying whole new lines.
            // Each line is ALWAYS a complete line (never a partial/token), so
            // the frontend appends without re-splitting.
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("stream_delta".into()));
            let lines: Vec<serde_json::Value> = p
                .lines
                .iter()
                .map(|l| {
                    serde_json::json!({
                        "role": l.role,
                        "message_id": l.message_id,
                        "line_no": l.line_no,
                        "content": l.content,
                    })
                })
                .collect();
            m.insert("lines".into(), serde_json::Value::Array(lines));
            // Per-session monotonic seq (ADR-035). The frontend's
            // `insertBySeq` needs it to place the streaming placeholder at
            // the correct position in messages[] under broker reorder.
            // Omitted (None) only for pre-seq Runtimes — frontend then
            // falls back to append-to-end.
            if let Some(seq) = p.seq {
                m.insert("seq".into(), serde_json::json!(seq));
            }
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::RecordComplete(p) => {
            // ADR-035: a record finalized (committed to JSONL), carrying the
            // COMPLETE content. Frontend freezes the active stream into
            // messages[] on receipt.
            //
            // For tool_call / tool_result records the backend now forwards
            // tool_name / tool_call_id / is_error so the frontend can
            // reconstruct the pairing without an HTTP round-trip. For
            // assistant / thought these fields are empty / false; we still
            // emit them so the frontend's switch statement stays uniform.
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("record_complete".into()));
            m.insert("role".into(), serde_json::Value::String(p.role.clone()));
            m.insert("message_id".into(), serde_json::Value::String(p.message_id.clone()));
            m.insert("content".into(), serde_json::Value::String(p.content.clone()));
            m.insert("tool_name".into(), serde_json::Value::String(p.tool_name.clone()));
            m.insert("tool_call_id".into(), serde_json::Value::String(p.tool_call_id.clone()));
            m.insert("is_error".into(), serde_json::Value::Bool(p.is_error));
            // Per-session monotonic seq — MUST match the seq of the matching
            // stream_delta placeholder so the frontend freeze lands at the
            // same slot; also orders direct tool_call / tool_result records.
            if let Some(seq) = p.seq {
                m.insert("seq".into(), serde_json::json!(seq));
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
