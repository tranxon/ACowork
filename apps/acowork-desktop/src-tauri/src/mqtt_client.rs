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

use acowork_core::defaults;
use acowork_core::mqtt_proto::{self, ControlCommand, DataEnvelope, data_envelope};
use acowork_mqtt_session::{
    classify as classify_err, ErrorDescriptor, ErrorKind, RefusedReason, ReconnectPolicy,
    SessionState, SessionStateTx,
};

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
    /// ADR-039 Phase 2: session state broadcast.
    ///
    /// Held in the struct purely to keep the original `watch::Sender` alive
    /// for the lifetime of the client. All status transitions are driven
    /// through the `poll_state_tx` clone captured by the eventloop task;
    /// `self.state_tx` is read by the public `session_state()` accessor so
    /// external consumers can poll the current state.
    #[allow(dead_code)]
    state_tx: SessionStateTx,
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

/// Build an [`ErrorDescriptor`] from rumqttc 0.25's `ConnectionError`.
///
/// This is the Desktop-side adapter that bridges the rumqttc 0.25
/// error type to the version-agnostic `ErrorDescriptor` used by
/// the shared `acowork-mqtt-session` crate.
///
/// ADR-039 Phase 2.
fn error_descriptor_from_rumqttc_025(err: &rumqttc::ConnectionError) -> ErrorDescriptor {
    use rumqttc::{ConnectionError, ConnectReturnCode};

    match err {
        ConnectionError::NetworkTimeout | ConnectionError::FlushTimeout => ErrorDescriptor {
            kind: ErrorKind::Timeout,
            io_kind: None,
        },
        ConnectionError::Io(io_err) => ErrorDescriptor {
            kind: ErrorKind::Io,
            io_kind: Some(io_err.kind()),
        },
        ConnectionError::ConnectionRefused(code) => ErrorDescriptor {
            kind: ErrorKind::ConnectionRefused(match code {
                ConnectReturnCode::BadUserNamePassword => RefusedReason::BadUserNamePassword,
                ConnectReturnCode::NotAuthorized => RefusedReason::NotAuthorized,
                ConnectReturnCode::RefusedProtocolVersion => RefusedReason::RefusedProtocolVersion,
                ConnectReturnCode::BadClientId => RefusedReason::BadClientId,
                ConnectReturnCode::ServiceUnavailable => RefusedReason::ServiceUnavailable,
                _ => RefusedReason::Unknown,
            }),
            io_kind: None,
        },
        ConnectionError::MqttState(_) => ErrorDescriptor {
            kind: ErrorKind::MqttState,
            io_kind: None,
        },
        ConnectionError::NotConnAck(_) => ErrorDescriptor {
            kind: ErrorKind::NotConnAck,
            io_kind: None,
        },
        ConnectionError::RequestsDone => ErrorDescriptor {
            kind: ErrorKind::RequestsDone,
            io_kind: None,
        },
        _ => ErrorDescriptor {
            kind: ErrorKind::Other,
            io_kind: None,
        },
    }
}

/// Lifecycle topic filters that must be re-subscribed on every
/// ConnAck (initial connect + automatic reconnects). With
/// `clean_session = true` the broker drops all subscriptions on
/// disconnect, so omitting this step makes the Desktop silently
/// lose agent status/meta/config events after a broker restart.
///
/// ADR-039 P2: mirrors the Runtime's `run_bootstrap` subscribe steps.
const LIFECYCLE_TOPIC_FILTERS: &[(&str, MqttQoS)] = &[
    ("acowork/agents/+/status", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/meta", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/config", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/sessions/created", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/sessions/deleted", MqttQoS::AtLeastOnce),
    ("acowork/sidecar/+/status", MqttQoS::AtLeastOnce),
];

/// Re-subscribe to all lifecycle topics after a (re)connect.
///
/// Called from the event loop's `ConnAck` handler. With
/// `clean_session = true` the broker drops subscriptions on
/// disconnect, so this is essential to avoid silently losing
/// agent lifecycle events after a broker restart.
///
/// ADR-039 P2.
async fn resubscribe_lifecycle(client: &AsyncClient) {
    for (filter, qos) in LIFECYCLE_TOPIC_FILTERS {
        if let Err(e) = client.subscribe(*filter, (*qos).into()).await {
            tracing::warn!(filter, error = %e, "Desktop MQTT resubscribe failed");
        }
    }
    tracing::info!("Desktop MQTT lifecycle topics re-subscribed");
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

        // ADR-039: align outgoing packet size with the broker's
        // `max_payload_size` (`GATEWAY_MQTT_MAX_PACKET_SIZE`). Without
        // this, large Desktop publish packets (e.g. protobuf-wrapped
        // ControlCommand payloads with embedded `config_json`) would hit
        // rumqttc's default 10 KB outgoing limit and trigger
        // `OutgoingPacketTooLarge`, which the broker translates into
        // `connection closed by peer`.
        let pkt_size = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE;
        options.set_max_packet_size(pkt_size, pkt_size);

        let (client, mut eventloop) = AsyncClient::new(options, 100);

        tracing::info!(
            host,
            port,
            client_id = %client_id,
            "Desktop MQTT client created (poll task spawning)"
        );

        let on_msg = Arc::new(on_message);
        let on_status = Arc::new(on_status);

        // ADR-039 Phase 2: session state broadcast + reconnect policy.
        let (state_tx, _) = SessionStateTx::new(SessionState::Connecting);
        let poll_state_tx = state_tx.clone();
        let reconnect_policy = ReconnectPolicy::default();

        // Spawn the eventloop poller.  This task owns `eventloop` for the
        // lifetime of the DesktopMqttClient.  All status transitions
        // (initial CONNACK, automatic reconnects, DISCONNECT, network
        // errors) flow through this single loop and into `on_status`.
        let on_msg_clone = on_msg.clone();
        let on_status_clone = on_status.clone();
        let poll_client = client.clone();
        let poll_task = tokio::spawn(async move {
            let mut consecutive_failures: u32 = 0;
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
                        poll_state_tx.set(SessionState::Connected);
                        consecutive_failures = 0;
                        // ADR-039 P2: re-subscribe lifecycle topics on
                        // every (re)connect. With clean_session = true
                        // the broker drops all subscriptions on
                        // disconnect, so without this the Desktop would
                        // silently lose agent status/meta/config events
                        // after a broker restart.
                        resubscribe_lifecycle(&poll_client).await;
                    }
                    // Broker initiated a disconnect (e.g. admin shutdown,
                    // client_id collision).  `rumqttc` will retry; we just
                    // surface the transition.
                    Ok(Event::Incoming(rumqttc::Incoming::Disconnect)) => {
                        on_status_clone(MqttStatus::Disconnected {
                            reason: "broker sent DISCONNECT".into(),
                        });
                        poll_state_tx.set(SessionState::Reconnecting);
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        // ADR-039 Phase 2: classify the error and apply
                        // the appropriate recovery strategy. Build
                        // ErrorDescriptor from rumqttc 0.25's
                        // ConnectionError manually (version-agnostic).
                        let desc = error_descriptor_from_rumqttc_025(&e);
                        let class = classify_err(&desc);
                        tracing::warn!(
                            error = %e,
                            err_class = class.label(),
                            consecutive_failures,
                            "Desktop MQTT event loop error"
                        );

                        on_status_clone(MqttStatus::Disconnected {
                            reason: format!("eventloop error: {e}"),
                        });

                        if class.is_fatal() {
                            // E2/E3/E4/E6: do not retry.
                            poll_state_tx.set(SessionState::Disconnected {
                                reason: format!("{}: {}", class.label(), e),
                            });
                            break;
                        }

                        // E1/E5: retryable. Apply exponential backoff.
                        poll_state_tx.set(SessionState::Reconnecting);
                        consecutive_failures += 1;
                        if let Some(backoff) =
                            reconnect_policy.backoff(class, consecutive_failures - 1)
                        {
                            tracing::info!(
                                attempt = backoff.attempt,
                                sleep_ms = backoff.duration.as_millis(),
                                "Desktop backing off before reconnect attempt"
                            );
                            tokio::time::sleep(backoff.duration).await;
                        }
                    }
                }
            }
        });

        Ok(Self {
            client,
            _eventloop_guard: Arc::new(EventLoopGuard { _task: poll_task }),
            state_tx,
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

    /// ADR-039 Phase 2: returns the current MQTT session state.
    ///
    /// Reserved public accessor intended for Tauri commands / DevMode that
    /// need to read the current session state on demand. Currently no
    /// caller exists; suppress the dead-code warning until the integration
    /// lands (the underlying `state_tx` field is still required to keep the
    /// broadcast channel alive for the lifetime of the client).
    #[allow(dead_code)]
    pub fn session_state(&self) -> SessionState {
        self.state_tx.current()
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
        for (filter, qos) in LIFECYCLE_TOPIC_FILTERS {
            self.subscribe(filter, *qos).await?;
        }
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
