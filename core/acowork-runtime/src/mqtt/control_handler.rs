//! MQTT control handler (ADR-033 Phase 3).
//!
//! Receives ControlCommand protobuf messages from the MQTT `control_rx`
//! channel and dispatches to the Runtime agent loop, following the same
//! business logic as the gRPC `process_gateway_recv()` in cli.rs.
//!
//! Protocol: `docs/zh/protocols/mqtt.md` §3.2, §5.2
//!
//! ## Message flow
//!
//! ```text
//! MQTT topic: acowork/agents/{id}/sessions/control/{cmd}
//!   ↓
//! RuntimeMqttClient (subscription)
//!   ↓
//! control_rx: UnboundedReceiver<(topic: String, payload: Vec<u8>)>
//!   ↓
//! ControlHandler::dispatch(topic, payload)
//!   ↓
//! parse DataEnvelope → ControlCommand
//!   ↓
//! match command:
//!   Message → push to agent loop session
//!   Stop → send stop signal
//!   CreateSession → allocate sid, publish created event
//!   DeleteSession → cleanup session
//! ```
//!
//! ## Performance
//!
//! - Control commands (QoS 1): handled inline, not spawned
//! - Session events (QoS 0): fire-and-forget via `publish_session_event`
//! - `control_rx` is Unbounded → backpressure-safe

use acowork_core::mqtt_proto::{self, data_envelope::Payload};
use prost::Message as ProstMessage;
use tokio::sync::mpsc;

/// Parsed MQTT control command with routing metadata.
#[derive(Debug)]
pub enum ControlAction {
    /// User wants to send a chat message.
    SendMessage {
        session_id: String,
        message_id: String,
        content: String,
    },
    /// User wants to stop generation.
    StopGeneration {
        session_id: String,
    },
    /// User wants to create a new session.
    CreateSession,
    /// User wants to delete a session.
    DeleteSession {
        session_id: String,
    },
    /// User wants to switch model.
    ModelSwitch {
        session_id: String,
        model_id: String,
    },
    /// User wants to change reasoning effort level.
    ReasoningEffort {
        session_id: String,
        effort: String,
    },
    /// User wants to trigger context compaction.
    CompactContext {
        session_id: String,
    },
    /// Gateway pushes an IntentReceived (cron trigger, cross-agent messaging).
    IntentReceived {
        from: String,
        action: String,
        params_json: String,
    },
    /// User wants to switch workspace for a session.
    WorkspaceSwitch {
        session_id: String,
        workspace_id: String,
    },
    /// Unknown or unimplemented command.
    Unsupported {
        command_type: String,
    },
}

/// Parse a raw MQTT payload (protobuf DataEnvelope bytes) into a ControlAction.
pub fn parse_control_payload(topic: &str, payload: &[u8]) -> Option<ControlAction> {
    let envelope = mqtt_proto::DataEnvelope::decode(payload).ok()?;

    let command = match envelope.payload? {
        Payload::ControlCommand(cmd) => cmd,
        _ => {
            tracing::debug!(topic, "MQTT control message is not a ControlCommand");
            return None;
        }
    };

    let action = match command.command? {
        mqtt_proto::control_command::Command::Message(msg) => ControlAction::SendMessage {
            session_id: msg.session_id,
            message_id: msg.message_id,
            content: msg.content,
        },
        mqtt_proto::control_command::Command::Stop(stop) => ControlAction::StopGeneration {
            session_id: stop.session_id,
        },
        mqtt_proto::control_command::Command::CreateSession(_) => ControlAction::CreateSession,
        mqtt_proto::control_command::Command::DeleteSession(del) => ControlAction::DeleteSession {
            session_id: del.session_id,
        },
        mqtt_proto::control_command::Command::ModelSwitch(sw) => ControlAction::ModelSwitch {
            session_id: sw.session_id,
            model_id: sw.model_id,
        },
        mqtt_proto::control_command::Command::ReasoningEffort(re) => ControlAction::ReasoningEffort {
            session_id: re.session_id,
            effort: re.effort,
        },
        mqtt_proto::control_command::Command::CompactContext(cc) => ControlAction::CompactContext {
            session_id: cc.session_id,
        },
        mqtt_proto::control_command::Command::WorkspaceSwitch(ws) => ControlAction::WorkspaceSwitch {
            session_id: ws.session_id,
            workspace_id: ws.workspace_id,
        },
        mqtt_proto::control_command::Command::Intent(intent) => ControlAction::IntentReceived {
            from: intent.from,
            action: intent.action,
            params_json: intent.params_json,
        },
    };

    Some(action)
}

/// Spawn a background task that reads `control_rx` and dispatches actions.
///
/// Messages are dispatched to the provided `inbound_tx` channel, which feeds
/// into the agent loop's InboundQueue. This mirrors gRPC's dispatch_client_message().
pub fn spawn_control_handler(
    mut control_rx: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    agent_id: String,
    inbound_tx: mpsc::UnboundedSender<crate::agent::inbound::InboundMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some((topic, payload)) = control_rx.recv().await {
            let action = match parse_control_payload(&topic, &payload) {
                Some(a) => a,
                None => continue,
            };

            use ControlAction::*;
            match &action {
                SendMessage { session_id: _, message_id: _, content } => {
                    let msg = crate::agent::inbound::InboundMessage::UserMessage(content.clone());
                    if inbound_tx.send(msg).is_err() {
                        tracing::warn!("control handler: inbound_tx closed");
                        break;
                    }
                }
                StopGeneration { session_id: _ } => {
                    let msg = crate::agent::inbound::InboundMessage::Stop {
                        reason: "MQTT stop".to_string(),
                    };
                    let _ = inbound_tx.send(msg);
                }
                CreateSession => {
                    tracing::info!(agent_id = %agent_id, "MQTT: create_session requested");
                    // Session creation is handled by SessionManager via session_task.
                    // Publish a SystemNotification to trigger creation.
                    let msg = crate::agent::inbound::InboundMessage::SystemNotification {
                        notification_type: "create_session".to_string(),
                        data: serde_json::json!({}),
                    };
                    let _ = inbound_tx.send(msg);
                }
                DeleteSession { session_id } => {
                    let msg = crate::agent::inbound::InboundMessage::SystemNotification {
                        notification_type: "delete_session".to_string(),
                        data: serde_json::json!({ "session_id": session_id }),
                    };
                    let _ = inbound_tx.send(msg);
                }
                ModelSwitch { session_id: _, model_id } => {
                    let msg = crate::agent::inbound::InboundMessage::SystemNotification {
                        notification_type: "model_switch".to_string(),
                        data: serde_json::json!({ "model_id": model_id }),
                    };
                    let _ = inbound_tx.send(msg);
                }
                ReasoningEffort { session_id: _, effort } => {
                    let msg = crate::agent::inbound::InboundMessage::SystemNotification {
                        notification_type: "reasoning_effort".to_string(),
                        data: serde_json::json!({ "effort": effort }),
                    };
                    let _ = inbound_tx.send(msg);
                }
                CompactContext { session_id: _ } => {
                    let msg = crate::agent::inbound::InboundMessage::SystemNotification {
                        notification_type: "compact_context".to_string(),
                        data: serde_json::json!({}),
                    };
                    let _ = inbound_tx.send(msg);
                }
                WorkspaceSwitch { session_id: _, workspace_id } => {
                    let msg = crate::agent::inbound::InboundMessage::SystemNotification {
                        notification_type: "workspace_switch".to_string(),
                        data: serde_json::json!({ "workspace_id": workspace_id }),
                    };
                    let _ = inbound_tx.send(msg);
                }
                IntentReceived { from, action, params_json } => {
                    let params: serde_json::Value = serde_json::from_str(params_json)
                        .unwrap_or(serde_json::json!({}));
                    let msg = crate::agent::inbound::InboundMessage::IntentMessage {
                        from: from.clone(),
                        action: action.clone(),
                        params,
                    };
                    let _ = inbound_tx.send(msg);
                }
                Unsupported { command_type } => {
                    tracing::warn!(command_type, topic, "Unsupported MQTT control command");
                }
            }
        }
        tracing::info!("MQTT control handler: control_rx closed");
    })
}
