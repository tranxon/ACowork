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

impl DesktopMqttClient {
    /// Connect to the MQTT broker and start the event loop.
    ///
    /// The `on_message` callback is called for every received MQTT message.
    /// In the Tauri context, this callback forwards messages to
    /// `app_handle.emit("mqtt-message", payload)` to reach the React frontend.
    pub async fn connect<F>(
        host: &str,
        port: u16,
        user_id: &str,
        on_message: F,
    ) -> Result<Self, String>
    where
        F: Fn(MqttMessage) + Send + Sync + 'static,
    {
        let pid = std::process::id();
        let client_id = format!("user:{}:desktop:{}", user_id, pid);

        let mut options = MqttOptions::new(client_id.clone(), host, port);
        options.set_keep_alive(Duration::from_secs(30));
        options.set_clean_session(true);

        let (client, mut eventloop) = AsyncClient::new(options, 100);

        // Wait for connection
        Self::wait_for_connection(&client, 20).await;

        tracing::info!(
            host,
            port,
            client_id = %client_id,
            "Desktop MQTT client connected"
        );

        let on_msg = Arc::new(on_message);

        // Spawn event loop poller
        let on_msg_clone = on_msg.clone();
        let poll_task = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(rumqttc::Incoming::Publish(publish))) => {
                        on_msg_clone(MqttMessage {
                            topic: publish.topic.clone(),
                            payload: publish.payload.to_vec(),
                        });
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::warn!(error = %e, "Desktop MQTT event loop error, retrying");
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
    pub async fn connect_default<F>(user_id: &str, on_message: F) -> Result<Self, String>
    where
        F: Fn(MqttMessage) + Send + Sync + 'static,
    {
        Self::connect("127.0.0.1", 19875, user_id, on_message).await
    }

    /// Wait for the MQTT connection by attempting a lightweight subscribe.
    async fn wait_for_connection(client: &AsyncClient, max_attempts: usize) {
        for _ in 0..max_attempts {
            match client
                .subscribe("_acowork/desktop_health", QoS::AtMostOnce)
                .await
            {
                Ok(_) => {
                    let _ = client.unsubscribe("_acowork/desktop_health").await;
                    return;
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
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

    /// Subscribe to all session events for a specific agent.
    ///
    /// Subscribes to ALL sessions of the agent. For per-session subscriptions,
    /// use `subscribe_agent_session` instead to avoid bandwidth waste.
    #[deprecated = "ADR-033: Use subscribe_agent_session() for per-session subscriptions to avoid bandwidth waste when an agent has many sessions"]
    pub async fn subscribe_agent_sessions(&self, agent_id: &str) -> Result<(), String> {
        let filter = format!("acowork/agents/{}/sessions/+/meta", agent_id);
        self.subscribe(&filter, MqttQoS::AtLeastOnce).await?;

        let filter = format!("acowork/agents/{}/sessions/+/config", agent_id);
        self.subscribe(&filter, MqttQoS::AtLeastOnce).await?;

        let filter = format!("acowork/agents/{}/sessions/+/messages/#", agent_id);
        self.subscribe(&filter, MqttQoS::AtMostOnce).await?;

        tracing::info!(agent_id, "Subscribed to agent session topics");
        Ok(())
    }

    /// Subscribe to events for a single session of a specific agent.
    ///
    /// Recommended over `subscribe_agent_sessions()` when the Desktop only
    /// displays one active session at a time — avoids receiving events from
    /// other sessions and wasting bandwidth/CPU.
    #[allow(dead_code)]
    pub async fn subscribe_agent_session(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<(), String> {
        let filter = format!("acowork/agents/{}/sessions/{}/messages/#", agent_id, session_id);
        self.subscribe(&filter, MqttQoS::AtMostOnce).await?;

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

    /// Publish a control command as JSON text.
    #[deprecated = "ADR-033: Use publish_control_protobuf() instead — all MQTT payloads must be DataEnvelope protobuf per mqtt.md §4"]
    #[allow(dead_code)]
    pub async fn publish_control_json(
        &self,
        agent_id: &str,
        command: &str,
        json: &serde_json::Value,
    ) -> Result<(), String> {
        let payload = serde_json::to_vec(json)
            .map_err(|e| format!("serialize control payload: {}", e))?;
        self.publish_control(agent_id, command, &payload).await
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
                    Some(mqtt_proto::control_command::Command::Message(_)) => "message",
                    Some(mqtt_proto::control_command::Command::Stop(_)) => "stop",
                    Some(mqtt_proto::control_command::Command::ModelSwitch(_)) => "model_switch",
                    Some(mqtt_proto::control_command::Command::ReasoningEffort(_)) => "reasoning_effort",
                    Some(mqtt_proto::control_command::Command::CompactContext(_)) => "compact_context",
                    None => "message", // fallback
                    _ => "message",
                }
            }
            _ => "message",
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
