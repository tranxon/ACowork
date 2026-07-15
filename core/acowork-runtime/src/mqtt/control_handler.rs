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

/// Parsed MQTT control command with routing metadata.
#[derive(Debug)]
pub enum ControlAction {
    /// User wants to send a chat message.
    SendMessage {
        session_id: String,
        message_id: String,
        content: String,
        /// Optional slash command prefix (e.g. "/commit")
        command: String,
        /// Rich payload as JSON (document_ids / content_parts / attached_context).
        /// Empty string = plain text only.
        params_json: String,
    },
    /// User wants to stop generation.
    StopGeneration {
        session_id: String,
        /// Stop reason for logging. Free-form but conventionally:
        /// "user_requested" | "iteration_limit" | "budget_exceeded" | "error"
        reason: String,
    },
    /// User wants to create a new session.
    CreateSession,
    /// User wants to delete a session.
    DeleteSession {
        session_id: String,
    },
    /// User wants to gracefully close a session (triggers distillation, preserves JSONL).
    CloseSession {
        session_id: String,
    },
    /// User wants to update the session title.
    UpdateSessionTitle {
        session_id: String,
        title: String,
    },
    /// User wants to continue a paused session (e.g. after iteration_limit).
    ContinueExecution {
        session_id: String,
        reason: String,
    },
    /// User wants the Desktop to start receiving events for this session.
    EnableNotify {
        session_id: String,
    },
    /// User wants the Desktop to stop receiving events for this session.
    DisableNotify {
        session_id: String,
    },
    /// User wants to switch model.
    ///
    /// `provider_id` is `Some(non_empty_string)` when the frontend wants the
    /// Runtime to rebuild the per-session Provider instance for a model
    /// hosted by a different provider (ADR-012). This mirrors the gRPC/
    /// WebSocket-era `params["provider"]` field extracted by
    /// `cli.rs::process_gateway_recv` and forwarded as
    /// `SessionManager::route_model_switch(_, _, Some(provider))`. Empty
    /// strings (and missing fields) are normalized to `None` to preserve
    /// the "model-only switch" semantics of the legacy code path.
    ModelSwitch {
        session_id: String,
        model_id: String,
        provider_id: Option<String>,
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
    /// User wants to trigger a typed compression (SUMMARY / TOOL_RESULTS).
    /// Distinct from `CompactContext` which is context-window driven.
    CompressAction {
        session_id: String,
        /// 1 = SUMMARY, 2 = TOOL_RESULTS. See `mqtt_proto::CompressType`.
        compress_type: i32,
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
    /// User approved/denied a tool approval request.
    ApprovalDecision {
        session_id: String,
        request_id: String,
        approved: bool,
        allow_all_session: bool,
        reason: String,
    },
    /// User answered a question prompt from the Runtime.
    QuestionAnswer {
        session_id: String,
        request_id: String,
        answer: String,
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
        mqtt_proto::control_command::Command::ChatMessage(msg) => ControlAction::SendMessage {
            session_id: msg.session_id,
            message_id: msg.message_id,
            content: msg.content,
            command: msg.command,
            params_json: msg.params_json,
        },
        mqtt_proto::control_command::Command::Stop(stop) => ControlAction::StopGeneration {
            session_id: stop.session_id,
            reason: stop.reason,
        },
        mqtt_proto::control_command::Command::CreateSession(_) => ControlAction::CreateSession,
        mqtt_proto::control_command::Command::DeleteSession(del) => ControlAction::DeleteSession {
            session_id: del.session_id,
        },
        mqtt_proto::control_command::Command::CloseSession(cs) => ControlAction::CloseSession {
            session_id: cs.session_id,
        },
        mqtt_proto::control_command::Command::UpdateSessionTitle(ust) => ControlAction::UpdateSessionTitle {
            session_id: ust.session_id,
            title: ust.title,
        },
        mqtt_proto::control_command::Command::ContinueExecution(ce) => ControlAction::ContinueExecution {
            session_id: ce.session_id,
            reason: ce.reason,
        },
        mqtt_proto::control_command::Command::EnableNotify(en) => ControlAction::EnableNotify {
            session_id: en.session_id,
        },
        mqtt_proto::control_command::Command::DisableNotify(dn) => ControlAction::DisableNotify {
            session_id: dn.session_id,
        },
        mqtt_proto::control_command::Command::ModelSwitch(sw) => {
            // ADR-012: provider_id is optional. Empty/missing means "keep the
            // current Provider instance, only update the model name" — the
            // legacy behaviour from the gRPC/WebSocket transport.
            let provider_id = if sw.provider_id.is_empty() {
                None
            } else {
                Some(sw.provider_id)
            };
            ControlAction::ModelSwitch {
                session_id: sw.session_id,
                model_id: sw.model_id,
                provider_id,
            }
        },
        mqtt_proto::control_command::Command::ReasoningEffort(re) => ControlAction::ReasoningEffort {
            session_id: re.session_id,
            effort: re.effort,
        },
        mqtt_proto::control_command::Command::CompactContext(cc) => ControlAction::CompactContext {
            session_id: cc.session_id,
        },
        mqtt_proto::control_command::Command::CompressAction(ca) => ControlAction::CompressAction {
            session_id: ca.session_id,
            // CompressType enum is generated as i32 in prost. Values:
            // 0 = UNSPECIFIED, 1 = SUMMARY, 2 = TOOL_RESULTS.
            compress_type: ca.compress_type,
        },
        mqtt_proto::control_command::Command::WorkspaceSwitch(ws) => ControlAction::WorkspaceSwitch {
            session_id: ws.session_id,
            workspace_id: ws.workspace_id,
        },
        mqtt_proto::control_command::Command::ApprovalDecision(ad) => ControlAction::ApprovalDecision {
            session_id: ad.session_id,
            request_id: ad.request_id,
            approved: ad.approved,
            allow_all_session: ad.allow_all_session,
            reason: ad.reason,
        },
        mqtt_proto::control_command::Command::QuestionAnswer(qa) => ControlAction::QuestionAnswer {
            session_id: qa.session_id,
            request_id: qa.request_id,
            answer: qa.answer,
        },
        mqtt_proto::control_command::Command::Intent(intent) => ControlAction::IntentReceived {
            from: intent.from,
            action: intent.action,
            params_json: intent.params_json,
        },
    };

    Some(action)
}


