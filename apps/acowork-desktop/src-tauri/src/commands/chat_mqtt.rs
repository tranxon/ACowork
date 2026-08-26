//! Tauri commands for MQTT operations (ADR-033 Phase 3)
//!
//! These commands are called from the React frontend via `invoke()`:
//! - `connect_mqtt` — connect to the MQTT broker
//! - `disconnect_mqtt` — disconnect and clean up
//! - `mqtt_publish_control` — publish a control command (protobuf-encoded)
//!
//! The `connect_mqtt` message callback also decodes DevMode debug events
//! (ADR-048 D6) published on `acowork/agents/glm-5.3_common/debug/events/#`
//! and re-emits them on the `debug-event` Tauri channel for the frontend
//! `debugStore`.

use std::sync::Arc;

use prost::Message;
use tauri::Emitter;

use acowork_core::mqtt_proto::{
    self, ControlCommand, DataEnvelope,
    control_command, data_envelope, session_message,
};
use crate::mqtt_client::{DesktopMqttClient, MqttMessage, MqttStatus};
use crate::state::AppState;
use acowork_core::defaults;

/// Connect to the MQTT broker and start receiving events.
///
/// Called by the frontend after the Gateway is confirmed healthy.
/// Subscribes to agent lifecycle topics and starts forwarding events
/// to the frontend via `app.emit("mqtt-event", payload)`.
///
/// ADR-036: also wires the broker eventloop's CONNACK / DISCONNECT
/// transitions to a dedicated `mqtt-status` Tauri event so the React
/// `chatStore` can keep `mqttConnected` truthful after a Desktop
/// restart or Runtime process recycling.
#[tauri::command]
pub async fn connect_mqtt(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.mqtt_client.lock().await;
    if guard.is_some() {
        return Ok(()); // Already connected
    }

    let user_id = "default"; // Single-user phase; multi-user will use actual user_id

    // ADR-058 W4 + ADR-055 D3: derive both the MQTT broker host AND port
    // from the Gateway so Remote mode (Gateway behind an SSH tunnel / WSL
    // IP) reaches the broker through the same forwarded host as :19876
    // HTTP. The host is derived from the base URL; the port is fetched
    // dynamically from /api/status (L3-6 residual gap — ADR-058 W4 fixed
    // the host half, ADR-055 Phase 1.3 closes the port half). Local mode
    // derives "127.0.0.1" — identical to the previous hardcode.
    let (mqtt_host, mqtt_port) = {
        let gw = state.gateway.read().await;
        let gateway_base_url = gw.base_url().to_string();
        let mqtt_host = derive_mqtt_broker_host(&gateway_base_url)
            .unwrap_or_else(|| defaults::GATEWAY_MQTT_HOST.to_string());
        // Fetch the broker port dynamically; fall back to the default on
        // any error so the connection still attempts the canonical port.
        let mqtt_port = gw
            .system_status()
            .await
            .map(|s| s.mqtt_port)
            .unwrap_or(defaults::GATEWAY_MQTT_PORT);
        (mqtt_host, mqtt_port)
    };

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

        // ── Plain-text topic: `acowork/agents/+/status` ──
        //
        // The Runtime publishes its lifecycle status as a plain text
        // retained message ("online" / "sleeping" / "offline") — see
        // `acowork-runtime::agent::idle_watcher` (sleeping) and
        // `acowork-runtime::mqtt::client::publish_status` (online/offline).
        // Without this branch the Desktop would silently lose the auto-sleep
        // signal and keep showing the agent as alive long after the
        // Runtime exited.
        if msg.topic.starts_with("acowork/agents/") && msg.topic.ends_with("/status") {
            // Status topic — never fall through to protobuf decode,
            // even if the payload is an unknown status string
            // (parse_plaintext_agent_status already logged the warning).
            if let Some(parsed) = parse_plaintext_agent_status(&msg.topic, &msg.payload) {
                let event = serde_json::json!({
                    "type": "agent_status",
                    "agent_id": parsed.agent_id,
                    "online": parsed.online,
                    "sleeping": parsed.sleeping,
                });
                let _ = app_handle.emit("agent-event", event);
            }
            return;
        }

        // Try to decode as DataEnvelope protobuf
        let envelope = match DataEnvelope::decode(&msg.payload[..]) {
            Ok(e) => e,
            Err(_) => return, // Not protobuf — ignore
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

            // ── ADR-038: explicit lifecycle acks ──
            //
            // The Runtime publishes these after handling an `open_session`
            // MQTT control command (Success) or after rejecting any
            // session-level command for a non-Active session (Error).
            // The Desktop uses them to flip `isSessionReady` and
            // surface a toast with a reopen affordance respectively — see
            // `chatStore.case "session_opened"` and
            // `chatStore.case "session_not_opened"`.
            //
            // SessionOpened / SessionNotOpened proto messages do not
            // carry `agent_id` (it's encoded in the topic path
            // `acowork/agents/{id}/sessions/{sid}/opened` /
            // `…/not_opened`); we parse it out of the topic so the
            // flat-JSON payload stays self-describing for the Desktop,
            // matching the shape of `session_created` / `session_meta`
            // siblings that DO include `agent_id` inline.
            data_envelope::Payload::SessionOpened(opened) => {
                let agent_id = extract_agent_id_from_topic(&msg.topic)
                    .unwrap_or_default();
                let event = serde_json::json!({
                    "type": "session_opened",
                    "agent_id": agent_id,
                    "session_id": opened.session_id,
                    "status": opened.status,
                    "model": opened.model,
                    "provider": opened.provider,
                    "last_active_at": opened.last_active_at,
                });
                let _ = app_handle.emit("agent-event", event);
            }
            data_envelope::Payload::SessionNotOpened(not_opened) => {
                let agent_id = extract_agent_id_from_topic(&msg.topic)
                    .unwrap_or_default();
                let event = serde_json::json!({
                    "type": "session_not_opened",
                    "agent_id": agent_id,
                    "session_id": not_opened.session_id,
                    "attempted_command": not_opened.attempted_command,
                    "reason": not_opened.reason,
                });
                let _ = app_handle.emit("agent-event", event);
            }

            // ── Session config (ADR-043: user-configurable fields only) ──
            data_envelope::Payload::SessionConfig(config) => {
                let event = serde_json::json!({
                    "type": "session_config",
                    "agent_id": config.agent_id,
                    "session_id": config.session_id,
                    "title": config.title,
                    "provider_id": config.provider_id,
                    "model_id": config.model_id,
                    "reasoning_effort": config.reasoning_effort,
                    "temperature": config.temperature,
                    "workspace_id": config.workspace_id,
                });
                tracing::info!(
                    agent_id = %config.agent_id,
                    session_id = %config.session_id,
                    model_id = %config.model_id,
                    provider_id = %config.provider_id,
                    workspace_id = %config.workspace_id,
                    "DESKTOP: emitting session_config agent-event"
                );
                let _ = app_handle.emit("agent-event", event);
            }

            // ── Session state (ADR-043: runtime telemetry only) ──
            data_envelope::Payload::SessionState(state) => {
                let event = serde_json::json!({
                    "type": "session_state",
                    "agent_id": state.agent_id,
                    "session_id": state.session_id,
                    "message_count": state.message_count,
                    "input_tokens": state.input_tokens,
                    "output_tokens": state.output_tokens,
                    "total_input_tokens": state.total_input_tokens,
                    "total_output_tokens": state.total_output_tokens,
                    "ratio": state.ratio,
                    "updated_at": state.updated_at,
                });
                // Parse status and context_usage inline
                let mut m = event.as_object().unwrap().clone();
                if !state.status.is_empty() {
                    match serde_json::from_str::<serde_json::Value>(&state.status) {
                        Ok(val) => { m.insert("status".into(), val); }
                        Err(_) => { m.insert("status".into(), serde_json::Value::String(state.status.clone())); }
                    }
                }
                if !state.context_usage.is_empty() {
                    match serde_json::from_str::<serde_json::Value>(&state.context_usage) {
                        Ok(val) => { m.insert("context_usage".into(), val); }
                        Err(_) => { m.insert("context_usage".into(), serde_json::Value::String(state.context_usage.clone())); }
                    }
                }
                tracing::info!(
                    agent_id = %state.agent_id,
                    session_id = %state.session_id,
                    message_count = state.message_count,
                    "DESKTOP: emitting session_state agent-event"
                );
                let _ = app_handle.emit("agent-event", serde_json::Value::Object(m));
            }

            // ── Agent lifecycle ──
            data_envelope::Payload::AgentStatus(status) => {
                // Same shape as the plain-text branch above — schema
                // must be identical so the React `chatStore.case
                // "agent_status"` reducer can handle either path with
                // one code path.
                let event = serde_json::json!({
                    "type": "agent_status",
                    "agent_id": status.agent_id,
                    "online": status.online,
                    "sleeping": status.sleeping,
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

            // ── Memory node update ──
            data_envelope::Payload::MemoryNodeUpdate(update) => {
                let event = serde_json::json!({
                    "type": "memory_node_update",
                    "agent_id": update.agent_id,
                    "node_id": update.node_id,
                    "node_json": update.node_json,
                });
                let _ = app_handle.emit("agent-event", event);
            }

            // ── Debug protocol events (ADR-048 D6) ──
            //
            // Runtime DevMode events arrive on
            // `acowork/agents/glm-5.3_common/debug/events/{event_type}`.
            // The protobuf payloads carry `session_id` but NOT `agent_id`
            // (it lives in the topic path), so we re-attach it here the
            // same way the SessionOpened / SessionNotOpened arms do.
            //
            // These are emitted on a dedicated `debug-event` Tauri channel
            // (not `agent-event`): the debugStore owns their state and the
            // chatStore's `handleMessageEvent` must not need to know about
            // debug types. Payload `type` mirrors the MQTT topic suffix so
            // the frontend dispatch table stays 1:1 with the wire topics.
            data_envelope::Payload::DebugStepEvent(ev) => {
                let agent_id = extract_agent_id_from_topic(&msg.topic).unwrap_or_default();
                let event = serde_json::json!({
                    "type": "onStep",
                    "agent_id": agent_id,
                    "session_id": ev.session_id,
                    "iteration": ev.iteration,
                    "phase": ev.phase,
                    "prompt_tokens": ev.prompt_tokens,
                    "completion_tokens": ev.completion_tokens,
                    "total_tokens": ev.total_tokens,
                });
                let _ = app_handle.emit("debug-event", event);
            }
            data_envelope::Payload::DebugContextBuiltEvent(ev) => {
                let agent_id = extract_agent_id_from_topic(&msg.topic).unwrap_or_default();
                // sections: proto map<string, SectionMeta> -> flat JSON
                // object keyed by section name (system_prompt, ...). The
                // frontend ContextSnapshotMeta consumes it as a plain
                // Record and iterates the fixed SECTION_ORDER list.
                let sections: serde_json::Map<String, serde_json::Value> = ev
                    .sections
                    .iter()
                    .map(|(name, meta)| {
                        (
                            name.clone(),
                            serde_json::json!({
                                "size_bytes": meta.size_bytes,
                                "token_estimate": meta.token_estimate,
                                "hash": meta.hash,
                            }),
                        )
                    })
                    .collect();
                let event = serde_json::json!({
                    "type": "onContextBuilt",
                    "agent_id": agent_id,
                    "session_id": ev.session_id,
                    "iteration": ev.iteration,
                    "total_token_estimate": ev.total_token_estimate,
                    "sections": serde_json::Value::Object(sections),
                    // ADR-054 step 2: control params carried on the event so
                    // the metadata bar renders without a follow-up RPC.
                    "request_params": ev.request_params.as_ref().map(|rp| serde_json::json!({
                        "model": rp.model,
                        "temperature": rp.temperature,
                        "max_tokens": rp.max_tokens,
                        "reasoning_effort": rp.reasoning_effort,
                        "thinking_mode": rp.thinking_mode,
                    })),
                });
                let _ = app_handle.emit("debug-event", event);
            }
            data_envelope::Payload::DebugStateChangeEvent(ev) => {
                let agent_id = extract_agent_id_from_topic(&msg.topic).unwrap_or_default();
                // `new_state` carries either a DebugState ("Running" /
                // "Paused" / "Stepping" / "Stopped") or a DebugPhase name
                // ("LlmCall", ...) - the Runtime maps both legacy event
                // kinds onto this topic. The frontend discriminates by
                // value (see debugStore `_handleDebugEvent`).
                let event = serde_json::json!({
                    "type": "onStateChange",
                    "agent_id": agent_id,
                    "session_id": ev.session_id,
                    "new_state": ev.new_state,
                    "iteration": ev.iteration,
                });
                let _ = app_handle.emit("debug-event", event);
            }

            // ── Workspace FS change events (ADR-058) ──
            //
            // Runtime's WorkspaceFsWatcher publishes aggregated batches
            // on `acowork/agents/{id}/workspaces/{wid}/fs-changed`
            // (QoS 1, non-retained). Re-emitted on the dedicated
            // `acowork:workspace-fs-changed` Tauri channel — the
            // workspaceStore / fileEditorStore own this state; the
            // chatStore's `handleMessageEvent` must not know about it
            // (same channel-separation rationale as debug-event).
            data_envelope::Payload::WorkspaceFsChangeEvent(ev) => {
                let changes: Vec<serde_json::Value> = ev
                    .changes
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "kind": fs_change_kind_str(c.kind),
                            "path": c.path,
                            "timestamp_ms": c.timestamp_ms,
                        })
                    })
                    .collect();
                let event = serde_json::json!({
                    "agent_id": ev.agent_id,
                    "workspace_id": ev.workspace_id,
                    "changes": changes,
                    "window_end_ms": ev.window_end_ms,
                });
                let _ = app_handle.emit("acowork:workspace-fs-changed", event);
            }

            // ── Global resources & control commands: ignore (Gateway handles these) ──
            // DebugBreakpointEvent / DebugRecordStepEvent are likewise
            // reserved-but-unemitted (see Runtime mqtt/debug_events.rs);
            // they fall through here until handlers exist.
            _ => {}
        }
    };

    let client = DesktopMqttClient::connect(
        &mqtt_host,
        mqtt_port,
        user_id,
        on_message,
        // ADR-036 / ADR-039: bridge `rumqttc` eventloop status → Tauri event.
        //
        // The `mqtt-status` event is BEST-EFFORT real-time notification.
        // The source of truth lives in `DesktopMqttClient::session_state`
        // (a watch channel updated synchronously by the poll task, which
        // `get_mqtt_status` reads).  This callback must stay synchronous
        // and side-effect-free apart from `app.emit` -- any state mutation
        // here would re-introduce the race the architecture was refactored
        // to avoid.
        move |status| {
            let payload = match &status {
                MqttStatus::Connected => serde_json::json!({
                    "connected": true,
                }),
                MqttStatus::Connecting => serde_json::json!({
                    "connected": false,
                    "connecting": true,
                }),
                MqttStatus::Reconnecting { reason } => serde_json::json!({
                    "connected": false,
                    "reconnecting": true,
                    "reason": reason,
                }),
                MqttStatus::Disconnected { reason } => serde_json::json!({
                    "connected": false,
                    "reason": reason,
                }),
            };
            if let Err(e) = app.emit("mqtt-status", payload) {
                tracing::warn!(error = %e, "failed to emit mqtt-status");
            }
        },
    ).await?;

    // Subscribe to ALL agent topics (lifecycle + session messages).
    // `subscribe_agent_lifecycle` now subscribes to ALL_TOPIC_FILTERS
    // which includes `messages/#` – this ensures the initial
    // subscription set matches exactly what `resubscribe_all` restores
    // on reconnect, eliminating the gap that previously caused silent
    // event loss after a reconnect.
    client.subscribe_agent_lifecycle().await?;

    let shared = Arc::new(tokio::sync::Mutex::new(client));
    *guard = Some(shared);

    tracing::info!("Desktop MQTT client connected and subscribed to all agent topics");
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

/// Force a soft-restart of the MQTT client.
///
/// Drops the current `EventLoop` and creates a fresh `AsyncClient` +
/// `EventLoop` pair, then re-subscribes to all topics. Use this when
/// the MQTT connection appears stuck (e.g. status shows "Reconnecting"
/// for an extended period, or messages stop arriving despite the broker
/// being healthy).
///
/// Unlike `disconnect_mqtt` (which tears down the client entirely),
/// this keeps the poll task alive and automatically recovers the
/// connection.
#[tauri::command]
pub async fn force_reconnect_mqtt(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let guard = state.mqtt_client.lock().await;
    let client = guard
        .as_ref()
        .ok_or_else(|| "MQTT client not connected".to_string())?;

    let client = client.lock().await;
    client.force_reconnect();
    tracing::info!("MQTT force-reconnect triggered by user");
    Ok(())
}

/// Snapshot of the current MQTT connection status, returned to the frontend.
///
/// ADR-036 / ADR-039: the source of truth is the `SessionState` watch
/// channel held by `DesktopMqttClient`.  The poll task updates it
/// synchronously inside its `on_status` callback (no `tokio::spawn`
/// indirection), so this read is guaranteed to reflect the latest
/// transition observed by the poll task.
///
/// The frontend's `initMqttListener` calls this AFTER `listen()`
/// resolves, so it never races the `mqtt-status` event either way:
///   - If the event was emitted before `listen()` registered, the
///     snapshot below returns the same value as the event would have.
///   - If `listen()` registered first, future events flow normally.
///
/// `known: false` means the MQTT client is not yet connected (or has
/// been torn down).  The frontend uses this to avoid flashing the
/// disconnected banner on cold start.
#[tauri::command]
pub async fn get_mqtt_status(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let guard = state.mqtt_client.lock().await;
    let Some(client) = guard.as_ref() else {
        eprintln!("[get_mqtt_status] no client exists");
        return Ok(serde_json::json!({
            "known": false,
            "connected": false,
            "reason": null,
        }));
    };
    let client = client.lock().await;
    let state = client.session_state();
    Ok(mqtt_status_to_payload(&state))
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
/// - "open_session": { "session_id" }    // ADR-038
/// - "update_session_title": { "session_id", "title" }
/// - "continue_execution": { "session_id", "reason" }
/// - "enable_notify": { "session_id" }
/// - "disable_notify": { "session_id" }
/// - "approval_decision": { "session_id", "request_id", "approved", "allow_all_session", "reason" }
/// - "cancel_tool": { "session_id", "tool_call_id" } (ADR-045)
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
        // ADR-038: explicit session activation. Runtime transitions
        // Closed/NotFound → Active and acks via `SessionOpened`
        // (or errors via `SessionNotOpened`). Idempotent for
        // already-Active sessions.
        "open_session" => control_command::Command::OpenSession(
            mqtt_proto::OpenSession {
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
        // ADR-045: cancel a single in-flight tool execution by tool_call_id.
        "cancel_tool" => {
            let tool_call_id = json.get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            control_command::Command::CancelTool(
                mqtt_proto::CancelTool {
                    session_id,
                    tool_call_id,
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
            tracing::info!(
                agent_id = %agent_id,
                session_id = %session_id,
                model_id = %model_id,
                provider_id = %provider_id,
                "BUILDING ModelSwitch control command"
            );
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

/// Map a proto `FsChangeKind` (encoded as i32) to the string form the
/// frontend stores dispatch on ("created" / "modified" / "deleted").
fn fs_change_kind_str(kind: i32) -> &'static str {
    match acowork_core::mqtt_proto::FsChangeKind::try_from(kind) {
        Ok(acowork_core::mqtt_proto::FsChangeKind::Created) => "created",
        Ok(acowork_core::mqtt_proto::FsChangeKind::Modified) => "modified",
        Ok(acowork_core::mqtt_proto::FsChangeKind::Deleted) => "deleted",
        _ => "unspecified",
    }
}

/// Derive the MQTT broker host from the Gateway HTTP base URL
/// (ADR-058 §3.5 / W4).
///
/// Remote mode expects the user to forward BOTH ports to the same host
/// (e.g. `ssh -L 19876:localhost:19876 -L 19875:localhost:19875 wsl`),
/// so the Gateway HTTP host is also the broker host. Returns `None` for
/// unparseable URLs — the caller falls back to the localhost default.
fn derive_mqtt_broker_host(gateway_base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(gateway_base_url).ok()?;
    url.host_str().map(|h| h.to_string())
}

#[cfg(test)]
mod adr058_tests {
    use super::*;

    /// ADR-058 review M-1: the i32 → string mapping is the Rust ↔ TS
    /// contract the frontend stores dispatch on ("created" / "modified"
    /// / "deleted"). Any change here must be mirrored in
    /// `workspaceFsEvents.ts` `FsChange.kind`.
    #[test]
    fn fs_change_kind_str_maps_all_variants() {
        use acowork_core::mqtt_proto::FsChangeKind as K;
        assert_eq!(fs_change_kind_str(K::Created as i32), "created");
        assert_eq!(fs_change_kind_str(K::Modified as i32), "modified");
        assert_eq!(fs_change_kind_str(K::Deleted as i32), "deleted");
        // Unspecified + any out-of-range value degrade safely.
        assert_eq!(fs_change_kind_str(K::Unspecified as i32), "unspecified");
        assert_eq!(fs_change_kind_str(99), "unspecified");
        assert_eq!(fs_change_kind_str(-1), "unspecified");
    }

    /// ADR-058 review M-1: broker host derivation from the Gateway base
    /// URL (Remote-mode tunnel support). Local URLs yield "127.0.0.1"
    /// — identical to the previous hardcode; WSL/remote IPs pass through.
    #[test]
    fn derive_mqtt_broker_host_covers_local_and_remote() {
        assert_eq!(
            derive_mqtt_broker_host("http://127.0.0.1:19876"),
            Some("127.0.0.1".to_string())
        );
        assert_eq!(
            derive_mqtt_broker_host("http://localhost:19876"),
            Some("localhost".to_string())
        );
        // Remote / WSL host behind an SSH tunnel.
        assert_eq!(
            derive_mqtt_broker_host("http://192.168.31.10:19876"),
            Some("192.168.31.10".to_string())
        );
        // Path/query are ignored; IPv6 brackets are stripped by host_str.
        assert_eq!(
            derive_mqtt_broker_host("http://10.0.0.5:19876/api"),
            Some("10.0.0.5".to_string())
        );
        // Unparseable input → None (caller falls back to the default).
        assert_eq!(derive_mqtt_broker_host("not a url"), None);
        assert_eq!(derive_mqtt_broker_host(""), None);
    }
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
            // Runtime serializes the whole ChunkEvent::AskQuestion
            // ({request_id, question, options, title, timeout_seconds}) as a
            // single JSON string in `question_json`.  The frontend's
            // `AskQuestionEvent` type expects those fields at the top level
            // (so e.g. `event.options.map(...)` works in AskQuestionCard),
            // so we MUST flatten the parsed object into `m` rather than
            // nesting it under a single key.
            match serde_json::from_str::<serde_json::Value>(&p.question_json) {
                Ok(serde_json::Value::Object(qm)) => {
                    for (k, v) in qm {
                        m.insert(k, v);
                    }
                }
                Ok(other) => {
                    // Unexpected shape (e.g. array) — surface raw so the
                    // frontend can still inspect it under `question_json`.
                    m.insert("question_json".into(), other);
                }
                Err(_) => {
                    m.insert(
                        "question_json".into(),
                        serde_json::Value::String(p.question_json.clone()),
                    );
                }
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
            // Prefer the fully-populated `context_usage` payload when the
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
            if !p.context_usage.is_empty() {
                match serde_json::from_str::<serde_json::Value>(&p.context_usage) {
                    Ok(val) => { m.insert("context_usage".into(), val); }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to parse ContextUsagePayload.context_usage");
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
        // ADR-043: SessionStateChanged payload deleted from SessionMessage.
        // Runtime state (status, ratio, context_usage) now flows through
        // the retained `sessions/{sid}/state` topic (Payload::SessionState).
        session_message::Event::LoopDetectedPaused(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("loop_detected_paused".into()));
            m.insert("session_id".into(), serde_json::Value::String(p.session_id.clone()));
            m.insert("message".into(), serde_json::Value::String(p.message.clone()));
            Some(serde_json::Value::Object(m))
        }
        session_message::Event::IterationLimitPaused(p) => {
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("iteration_limit_paused".into()));
            m.insert("iteration".into(), serde_json::json!(p.iteration));
            m.insert("max_iterations".into(), serde_json::json!(p.max_iterations));
            m.insert("message".into(), serde_json::Value::String(p.message.clone()));
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
        session_message::Event::ToolProgress(p) => {
            // ADR-045: Tool execution progress heartbeat.
            // Frontend uses this to refresh a timer/countdown display.
            // Does NOT carry tool result data — pure control-plane signal.
            let mut m = base.as_object().unwrap().clone();
            m.insert("type".into(), serde_json::Value::String("tool_progress".into()));
            m.insert("tool_call_id".into(), serde_json::Value::String(p.tool_call_id.clone()));
            m.insert("elapsed_ms".into(), serde_json::json!(p.elapsed_ms));
            m.insert("timeout_ms".into(), serde_json::json!(p.timeout_ms));
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

/// Extract the `agent_id` segment from a session-scoped MQTT topic.
///
/// All session topics share the prefix `acowork/agents/{id}/sessions/{sid}/...`.
/// Returns `None` for topics that don't match this shape (which should
/// only happen on malformed/misconfigured topics).
fn extract_agent_id_from_topic(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    // acowork / agents / {id} / sessions / ...  (>=3rd segment, 0-indexed)
    if parts.len() >= 3 && parts[0] == "acowork" && parts[1] == "agents" {
        let agent_id = parts[2];
        if !agent_id.is_empty() {
            return Some(agent_id.to_string());
        }
    }
    None
}

/// Parse the plain-text agent status payload published by the Runtime
/// on `acowork/agents/{id}/status` (retained message).
///
/// Returns:
/// - `Some(...)` when the topic matches the status shape and the
///   payload is one of the known status values.
/// - `None` when the topic is not a status topic (caller should fall
///   through to the protobuf decoder). When the topic *is* a status
///   topic but the payload is unknown, this function logs a warning and
///   still returns `None` — callers should drop the message to avoid
///   spurious protobuf decode attempts.
fn parse_plaintext_agent_status(topic: &str, payload: &[u8]) -> Option<ParsedAgentStatus> {
    if !topic.starts_with("acowork/agents/") || !topic.ends_with("/status") {
        return None;
    }
    let agent_id = extract_agent_id_from_topic(topic)?;
    let payload_str = String::from_utf8_lossy(payload);
    let parsed = match payload_str.trim() {
        "online" => ParsedAgentStatus { agent_id, online: true, sleeping: false },
        "sleeping" => ParsedAgentStatus { agent_id, online: true, sleeping: true },
        "offline" => ParsedAgentStatus { agent_id, online: false, sleeping: false },
        unknown => {
            tracing::warn!(
                topic = %topic,
                payload = %unknown,
                "unknown agent status payload — ignoring"
            );
            return None;
        }
    };
    Some(parsed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedAgentStatus {
    agent_id: String,
    online: bool,
    sleeping: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_agent_id_from_opened_topic() {
        assert_eq!(
            extract_agent_id_from_topic("acowork/agents/com.acowork.pm/sessions/sess-1/opened"),
            Some("com.acowork.pm".to_string())
        );
    }

    #[test]
    fn extract_agent_id_from_not_opened_topic() {
        assert_eq!(
            extract_agent_id_from_topic("acowork/agents/agent_x/sessions/sess-9/not_opened"),
            Some("agent_x".to_string())
        );
    }

    #[test]
    fn extract_agent_id_missing_returns_none() {
        assert_eq!(extract_agent_id_from_topic(""), None);
        assert_eq!(extract_agent_id_from_topic("not/the/expected/topic"), None);
        assert_eq!(extract_agent_id_from_topic("acowork/agents//sessions/x/opened"), None);
    }

    /// Regression test: plain-text "sleeping" payload must surface as
    /// `online=true, sleeping=true` so the React reducer can show the
    /// sleeping UI even before the Gateway republishes the status as a
    /// protobuf DataEnvelope. Before this fix the Desktop would decode
    /// the message as protobuf, fail (not a valid DataEnvelope), and
    /// `return` — the frontend never learned about the sleep.
    #[test]
    fn parse_plaintext_sleeping_payload() {
        let p = parse_plaintext_agent_status(
            "acowork/agents/com.acowork.senior-engineer/status",
            b"sleeping",
        )
        .expect("known status payload must parse");
        assert_eq!(p.agent_id, "com.acowork.senior-engineer");
        assert!(p.online);
        assert!(p.sleeping);
    }

    #[test]
    fn parse_plaintext_online_payload() {
        let p = parse_plaintext_agent_status(
            "acowork/agents/com.example.weather/status",
            b"online",
        )
        .unwrap();
        assert_eq!(p.agent_id, "com.example.weather");
        assert!(p.online);
        assert!(!p.sleeping);
    }

    #[test]
    fn parse_plaintext_offline_payload() {
        let p = parse_plaintext_agent_status(
            "acowork/agents/com.example.weather/status",
            b"offline",
        )
        .unwrap();
        assert_eq!(p.agent_id, "com.example.weather");
        assert!(!p.online);
        assert!(!p.sleeping);
    }

    #[test]
    fn parse_plaintext_unknown_status_returns_none() {
        // Topic is a status topic, payload is garbage → None (caller
        // drops, warning already logged).
        assert!(parse_plaintext_agent_status(
            "acowork/agents/com.x/status",
            b"what is this",
        )
        .is_none());
    }

    #[test]
    fn parse_plaintext_non_status_topic_returns_none() {
        // Not a status topic → fall through to protobuf decoder.
        assert!(parse_plaintext_agent_status(
            "acowork/agents/com.x/sessions/s-1/meta",
            b"online",
        )
        .is_none());
    }
}

/// Convert a `SessionState` into the JSON payload returned by `get_mqtt_status`
/// and emitted as `mqtt-status` events.  Centralised so the snapshot and
/// the event use byte-for-byte identical shapes.
fn mqtt_status_to_payload(state: &acowork_mqtt_session::SessionState) -> serde_json::Value {
    use acowork_mqtt_session::SessionState;
    match state {
        SessionState::Idle => serde_json::json!({
            "known": false,
            "connected": false,
            "reason": null,
        }),
        // `Connecting` means the client exists and is actively trying to
        // connect (initial connect or after force_reconnect).  We return
        // `known: true` so the frontend updates its store and starts the
        // polling fallback, rather than ignoring the snapshot.
        SessionState::Connecting => serde_json::json!({
            "known": true,
            "connected": false,
            "connecting": true,
            "reason": null,
        }),
        SessionState::Reconnecting => serde_json::json!({
            "known": true,
            "connected": false,
            "reconnecting": true,
            "reason": "reconnecting",
        }),
        SessionState::Connected => serde_json::json!({
            "known": true,
            "connected": true,
        }),
        SessionState::Disconnected { reason } => serde_json::json!({
            "known": true,
            "connected": false,
            "reason": reason,
        }),
    }
}
