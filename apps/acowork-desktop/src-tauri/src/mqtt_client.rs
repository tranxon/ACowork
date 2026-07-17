//! Desktop MQTT client (ADR-033 Phase 3).
//!
//! The Tauri Rust backend connects to the Gateway's embedded MQTT broker
//! as a `rumqttc` client. It subscribes to agent lifecycle topics
//! (status, meta, config) and session events, forwarding them to the
//! React frontend via Tauri events. Control commands from the frontend
//! (send message, stop, create session) are published via MQTT.
//!
//! See `docs/zh/protocols/mqtt.md` §5.1.6, §5.2, §9.1.

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, MqttOptions, QoS};
use tokio::sync::Mutex;

use acowork_core::mqtt_proto::{self, ControlCommand, DataEnvelope, data_envelope};

/// MQTT QoS level (mirrors the Gateway's).
#[derive(Debug, Clone, Copy)]
pub enum MqttQoS {
    AtMostOnce,
    AtLeastOnce,
}

impl From<MqttQoS> for QoS {
    fn from(qos: MqttQoS) -> Self {
        match qos {
            MqttQoS::AtMostOnce => QoS::AtMostOnce,
            MqttQoS::AtLeastOnce => QoS::AtLeastOnce,
        }
    }
}

/// The Desktop's MQTT client.
///
/// Wraps `rumqttc::AsyncClient`. Incoming messages are routed to
/// Tauri event emitters via a callback channel.
pub struct DesktopMqttClient {
    client: AsyncClient,
    /// Keep the event loop polling task alive.
    _eventloop_guard: Arc<EventLoopGuard>,
}

struct EventLoopGuard {
    _task: tokio::task::JoinHandle<()>,
}

/// An MQTT message received from the broker.
#[derive(Debug, Clone)]
pub struct MqttMessage {
    pub topic: String,
    pub payload: Vec<u8>,
}

/// MQTT connection status observed from the broker eventloop.
///
/// Per ADR-036 the Rust side is the source-of-truth for connection state:
/// the Desktop `chatStore` only consumes these transitions via the
/// `mqtt-status` Tauri event and never attempts to mutate the underlying
/// `rumqttc::AsyncClient` itself.  Reconnection is owned by `rumqttc`'s
/// built-in retry; this enum only reports the externally-observable state
/// changes (broker confirmed connection / broker sent disconnect / network
/// error).
#[derive(Debug, Clone)]
pub enum MqttStatus {
    /// Broker confirmed the connection (CONNACK received).  Also fired
    /// after a successful automatic reconnect following a disconnect.
    Connected,
    /// Connection is no longer usable.  `reason` is a human-readable
    /// explanation suitable for surfacing in the UI status bar.
    Disconnected { reason: String },
}

impl DesktopMqttClient {
    /// Connect to the MQTT broker and start the event loop.
    ///
    /// Returns IMMEDIATELY after spawning the polling task — does NOT wait
    /// for the initial CONNACK.  Connection status (Connected / Disconnected)
    /// is delivered through the `on_status` callback asynchronously as the
    /// broker eventloop observes CONNACK / DISCONNECT / network errors.
    ///
    /// This is the correct shape for an event-driven MQTT client: the
    /// caller doesn't block, doesn't need to manage a "still connecting"
    /// intermediate state, and the consumer (Tauri layer) is expected to
    /// expose a synchronous `get_mqtt_status` query alongside the
    /// `mqtt-status` event so the frontend can fetch the current state on
    /// startup without racing the listener registration.
    ///
    /// `on_message` — called for every received MQTT Publish.
    /// `on_status`  — ADR-036: called on every state transition
    ///                 (CONNACK → Connected; DISCONNECT / eventloop error →
    ///                 Disconnected).  May fire multiple times over the
    ///                 lifetime of the client (initial connect, reconnects,
    ///                 outages); the consumer treats it as idempotent.
    pub async fn connect<F, G>(
        host: &str,
        port: u16,
        user_id: &str,
        on_message: F,
        on_status: G,
    ) -> Result<Self, String>
    where
        F: Fn(MqttMessage) + Send + Sync + 'static,
        G: Fn(MqttStatus) + Send + Sync + 'static,
    {
        let pid = std::process::id();
        let client_id = format!("user:{}:desktop:{}", user_id, pid);

        let mut options = MqttOptions::new(client_id.clone(), host, port);
        options.set_keep_alive(Duration::from_secs(30));
        options.set_clean_session(true);

        let (client, mut eventloop) = AsyncClient::new(options, 100);

        tracing::info!(
            host,
            port,
            client_id = %client_id,
            "Desktop MQTT client created (poll task spawning)"
        );

        let on_msg = Arc::new(on_message);
        let on_status = Arc::new(on_status);

        // Spawn the eventloop poller.  This task owns `eventloop` for the
        // lifetime of the DesktopMqttClient.  All status transitions
        // (initial CONNACK, automatic reconnects, DISCONNECT, network
        // errors) flow through this single loop and into `on_status`.
        let on_msg_clone = on_msg.clone();
        let on_status_clone = on_status.clone();
        let poll_task = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(rumqttc::Incoming::Publish(publish))) => {
                        on_msg_clone(MqttMessage {
                            topic: publish.topic.clone(),
                            payload: publish.payload.to_vec(),
                        });
                    }
                    // Broker confirmed (re)connection.
                    Ok(Event::Incoming(rumqttc::Incoming::ConnAck(_))) => {
                        on_status_clone(MqttStatus::Connected);
                    }
                    // Broker initiated a disconnect (e.g. admin shutdown,
                    // client_id collision).  `rumqttc` will retry; we just
                    // surface the transition.
                    Ok(Event::Incoming(rumqttc::Incoming::Disconnect)) => {
                        on_status_clone(MqttStatus::Disconnected {
                            reason: "broker sent DISCONNECT".into(),
                        });
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        // Network-level failure (TCP reset, DNS, broker
                        // gone).  Surface as disconnect and let `rumqttc`'s
                        // internal retry recover.  The next successful
                        // `poll()` will deliver ConnAck automatically.
                        tracing::warn!(error = %e, "Desktop MQTT event loop error, retrying");
                        on_status_clone(MqttStatus::Disconnected {
                            reason: format!("eventloop error: {e}"),
                        });
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });

        Ok(Self {
            client,
            _eventloop_guard: Arc::new(EventLoopGuard { _task: poll_task }),
        })
    }

    /// Connect with default localhost and port.
    pub async fn connect_default<F, G>(
        user_id: &str,
        on_message: F,
        on_status: G,
    ) -> Result<Self, String>
    where
        F: Fn(MqttMessage) + Send + Sync + 'static,
        G: Fn(MqttStatus) + Send + Sync + 'static,
    {
        Self::connect("127.0.0.1", 19875, user_id, on_message, on_status).await
    }

    /// Subscribe to a topic filter.
    pub async fn subscribe(&self, filter: &str, qos: MqttQoS) -> Result<(), String> {
        self.client
            .subscribe(filter, qos.into())
            .await
            .map_err(|e| format!("subscribe '{}': {}", filter, e))
    }

    /// Subscribe to all agent lifecycle topics (status, meta, config).
    pub async fn subscribe_agent_lifecycle(&self) -> Result<(), String> {
        self.subscribe("acowork/agents/+/status", MqttQoS::AtLeastOnce).await?;
        self.subscribe("acowork/agents/+/meta", MqttQoS::AtLeastOnce).await?;
        self.subscribe("acowork/agents/+/config", MqttQoS::AtLeastOnce).await?;
        self.subscribe("acowork/agents/+/sessions/created", MqttQoS::AtLeastOnce).await?;
        self.subscribe("acowork/agents/+/sessions/deleted", MqttQoS::AtLeastOnce).await?;
        self.subscribe("acowork/sidecar/+/status", MqttQoS::AtLeastOnce).await?;
        tracing::info!("Subscribed to agent lifecycle topics");
        Ok(())
    }

    /// Subscribe to events for a single session of a specific agent.
    ///
    /// Use this when the Desktop only displays one active session at a time
    /// to avoid receiving events from other sessions and wasting
    /// bandwidth/CPU.
    ///
    /// **QoS 1 is mandatory for the messages/# tree** (stream_delta and
    /// record_complete topics). A subscriber QoS 0 forces the broker to
    /// downgrade delivery of the publishers' QoS 1 frames to QoS 0, opening
    /// the door to reorder and loss — exactly the symptom that motivated
    /// the per-session `seq` counter and end-to-end QoS 1 stream.
    #[allow(dead_code)]
    pub async fn subscribe_agent_session(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<(), String> {
        let filter = format!("acowork/agents/{}/sessions/{}/messages/#", agent_id, session_id);
        self.subscribe(&filter, MqttQoS::AtLeastOnce).await?;

        let filter = format!("acowork/agents/{}/sessions/{}/meta", agent_id, session_id);
        self.subscribe(&filter, MqttQoS::AtLeastOnce).await?;

        let filter = format!("acowork/agents/{}/sessions/{}/config", agent_id, session_id);
        self.subscribe(&filter, MqttQoS::AtLeastOnce).await?;

        tracing::info!(agent_id, session_id, "Subscribed to agent session topics (per-session)");
        Ok(())
    }

    /// Unsubscribe from a single session's events.
    ///
    /// Call this when switching away from a session to avoid receiving
    /// stale events from the old session.
    #[allow(dead_code)]
    pub async fn unsubscribe_agent_session(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<(), String> {
        let filter = format!("acowork/agents/{}/sessions/{}/messages/#", agent_id, session_id);
        self.client
            .unsubscribe(&filter)
            .await
            .map_err(|e| format!("unsubscribe '{}': {}", filter, e))?;

        let filter = format!("acowork/agents/{}/sessions/{}/meta", agent_id, session_id);
        self.client
            .unsubscribe(&filter)
            .await
            .map_err(|e| format!("unsubscribe '{}': {}", filter, e))?;

        let filter = format!("acowork/agents/{}/sessions/{}/config", agent_id, session_id);
        self.client
            .unsubscribe(&filter)
            .await
            .map_err(|e| format!("unsubscribe '{}': {}", filter, e))?;

        tracing::info!(agent_id, session_id, "Unsubscribed from agent session topics");
        Ok(())
    }

    /// Publish a control command to the broker.
    ///
    /// Desktop → Runtime control commands:
    /// - `control/create_session` — create new session
    /// - `control/delete_session` — delete session
    /// - `control/message` — send message to agent
    /// - `control/stop` — stop current generation
    pub async fn publish_control(
        &self,
        agent_id: &str,
        command: &str,
        payload: &[u8],
    ) -> Result<(), String> {
        let topic = format!(
            "acowork/agents/{}/sessions/control/{}",
            agent_id, command
        );
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await
            .map_err(|e| format!("publish control '{}': {}", command, e))
    }

    /// Publish a control command as a `DataEnvelope` protobuf payload.
    ///
    /// This is the canonical way to send control commands via MQTT
    /// per `docs/zh/protocols/mqtt.md` §4 — all messages must use
    /// Protobuf `DataEnvelope` encoding for wire compatibility.
    pub async fn publish_control_protobuf(
        &self,
        agent_id: &str,
        control_command: ControlCommand,
    ) -> Result<(), String> {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::ControlCommand(control_command)),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        // Determine the sub-topic from the command type
        let command = match &envelope.payload {
            Some(data_envelope::Payload::ControlCommand(cmd)) => {
                match &cmd.command {
                    Some(mqtt_proto::control_command::Command::CreateSession(_)) => "create_session",
                    Some(mqtt_proto::control_command::Command::DeleteSession(_)) => "delete_session",
                    Some(mqtt_proto::control_command::Command::CloseSession(_)) => "close_session",
                    // ADR-038: explicit session activation. Runtime ack
                    // (or error) comes via `SessionOpened` /
                    // `SessionNotOpened` events.
                    Some(mqtt_proto::control_command::Command::OpenSession(_)) => "open_session",
                    Some(mqtt_proto::control_command::Command::UpdateSessionTitle(_)) => "update_session_title",
                    Some(mqtt_proto::control_command::Command::ChatMessage(_)) => "chat_message",
                    Some(mqtt_proto::control_command::Command::Stop(_)) => "stop",
                    Some(mqtt_proto::control_command::Command::ContinueExecution(_)) => "continue_execution",
                    Some(mqtt_proto::control_command::Command::EnableNotify(_)) => "enable_notify",
                    Some(mqtt_proto::control_command::Command::DisableNotify(_)) => "disable_notify",
                    Some(mqtt_proto::control_command::Command::ApprovalDecision(_)) => "approval_decision",
                    Some(mqtt_proto::control_command::Command::QuestionAnswer(_)) => "question_answer",
                    Some(mqtt_proto::control_command::Command::ModelSwitch(_)) => "model_switch",
                    Some(mqtt_proto::control_command::Command::ReasoningEffort(_)) => "reasoning_effort",
                    Some(mqtt_proto::control_command::Command::WorkspaceSwitch(_)) => "workspace_switch",
                    Some(mqtt_proto::control_command::Command::CompactContext(_)) => "compact_context",
                    Some(mqtt_proto::control_command::Command::CompressAction(_)) => "compress_action",
                    Some(mqtt_proto::control_command::Command::Intent(_)) => "intent",
                    None => "chat_message",
                }
            }
            _ => "chat_message",
        };

        self.publish_control(agent_id, command, &payload).await
    }

    /// Publish a raw message to any topic.
    #[allow(dead_code)]
    pub async fn publish_raw(
        &self,
        topic: &str,
        payload: &[u8],
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), String> {
        self.client
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| format!("publish '{}': {}", topic, e))
    }

    /// Get the inner AsyncClient.
    #[allow(dead_code)]
    pub fn inner(&self) -> AsyncClient {
        self.client.clone()
    }
}

/// Thread-safe shared DesktopMqttClient.
pub type SharedDesktopMqttClient = Arc<Mutex<DesktopMqttClient>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_desktop_mqtt_client_connects() {
        // This test requires the Gateway broker to be running.
        // In CI, skip if the broker is not available.
        let port = 19875;
        let client = DesktopMqttClient::connect(
            "127.0.0.1",
            port,
            "test-user",
            |_msg| {
                // no-op callback
            },
            |_status| {
                // no-op status callback (ADR-036)
            },
        )
        .await;

        match client {
            Ok(client) => {
                client
                    .subscribe_agent_lifecycle()
                    .await
                    .expect("subscribe should succeed");
                drop(client);
            }
            Err(e) => {
                eprintln!("Skipping test: broker not available ({})", e);
                // Don't fail — the test environment may not have a broker running
            }
        }
    }
}
