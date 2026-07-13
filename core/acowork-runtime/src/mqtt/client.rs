//! Runtime MQTT client (ADR-033 Phase 2).
//!
//! Connects to the Gateway's embedded broker with:
//! - `client_id: "agent:{agent_id}"`
//! - Last Will: `acowork/agents/{id}/status = "offline"` (Retained, QoS 1)
//!
//! On connect, publishes:
//! - `acowork/agents/{id}/status = "online"` (Retained)
//! - `acowork/agents/{id}/meta` (Retained) — AgentMeta protobuf
//! - `acowork/agents/{id}/config` (Retained) — AgentConfig protobuf
//!
//! Subscribes to:
//! - `acowork/global/#` — global resource available state
//! - `acowork/agents/{id}/sessions/control/#` — control commands from Desktop
//!
//! See `docs/zh/protocols/mqtt.md` §5.1 (startup sequence) and §8.1 (Will Message).

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, QoS};
use tokio::sync::Mutex;

use acowork_core::mqtt_proto::{
    AgentConfig, AgentMeta, DataEnvelope,
};

use crate::mqtt::available_cache::SharedAvailableCache;

/// Error type for Runtime MQTT client operations.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeMqttClientError {
    #[error("MQTT connection error: {0}")]
    Connection(String),
    #[error("MQTT publish error: {0}")]
    Publish(String),
    #[error("MQTT subscribe error: {0}")]
    Subscribe(String),
}

/// QoS level wrapper (mirrors the Gateway's MqttQoS).
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

/// The Runtime's MQTT client.
///
/// Wraps `rumqttc::AsyncClient` with:
/// - Last Will for automatic offline detection
/// - Agent lifecycle publishing (status/meta/config)
/// - Global resource subscription → `AvailableResourceCache`
/// - Control command subscription → caller-provided channel
pub struct RuntimeMqttClient {
    client: AsyncClient,
    /// The agent_id this client represents.
    agent_id: String,
    /// Keep the event loop polling task alive.
    _eventloop_guard: Arc<EventLoopGuard>,
}

struct EventLoopGuard {
    _task: tokio::task::JoinHandle<()>,
}

impl RuntimeMqttClient {
    /// Connect to the MQTT broker and perform the Phase 2 startup sequence.
    ///
    /// 1. Connect with Last Will (`agents/{id}/status = "offline"`, Retained)
    /// 2. PUBLISH `agents/{id}/status = "online"` (Retained)
    /// 3. PUBLISH `agents/{id}/meta` (Retained) — AgentMeta
    /// 4. PUBLISH `agents/{id}/config` (Retained) — AgentConfig
    /// 5. SUBSCRIBE `acowork/global/#`
    /// 6. SUBSCRIBE `acowork/agents/{id}/sessions/control/#`
    ///
    /// The `available_cache` is updated from `acowork/global/#` messages.
    /// Control commands are forwarded to the provided `control_tx` channel.
    pub async fn connect(
        host: &str,
        port: u16,
        agent_id: &str,
        agent_name: &str,
        agent_version: &str,
        avatar: Option<&str>,
        builtin_avatar: Option<&str>,
        config_json: &str,
        available_cache: SharedAvailableCache,
        control_tx: tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
    ) -> Result<Self, RuntimeMqttClientError> {
        let client_id = format!("agent:{}", agent_id);
        let status_topic = format!("acowork/agents/{}/status", agent_id);
        let meta_topic = format!("acowork/agents/{}/meta", agent_id);
        let config_topic = format!("acowork/agents/{}/config", agent_id);
        let control_filter = format!("acowork/agents/{}/sessions/control/#", agent_id);

        // Configure MQTT options with Last Will
        let mut options = MqttOptions::new(client_id.clone(), host, port);
        options.set_keep_alive(Duration::from_secs(30));
        options.set_clean_session(true);

        // Last Will: if Runtime crashes/disconnects, broker publishes "offline" retained
        let will = LastWill::new(&status_topic, "offline", QoS::AtLeastOnce, true);
        options.set_last_will(will);

        let (client, mut eventloop) = AsyncClient::new(options, 100);

        // Spawn event loop poller that:
        // - Updates available_cache from acowork/global/# messages
        // - Forwards control commands to control_tx
        let poll_agent_id = agent_id.to_string();
        let poll_cache = available_cache.clone();
        let poll_control_tx = control_tx.clone();
        let poll_task = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(rumqttc::Incoming::Publish(publish))) => {
                        let topic = &publish.topic;

                        // Route global resource updates to the available cache
                        if topic.starts_with("acowork/global/") {
                            let mut cache_write = poll_cache.write().await;
                            cache_write.update_from_mqtt(topic, &publish.payload);
                        }

                        // Route control commands to the control channel
                        if topic.starts_with(&format!(
                            "acowork/agents/{}/sessions/control/",
                            poll_agent_id
                        )) {
                            let _ = poll_control_tx.send((topic.clone(), publish.payload.to_vec()));
                        }
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::warn!(error = %e, "Runtime MQTT event loop error, will retry");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });

        // Wait for connection to be established
        Self::wait_for_connection(&client, 20).await;

        tracing::info!(
            host,
            port,
            client_id = %client_id,
            agent_id = %agent_id,
            "Runtime MQTT client connected to broker"
        );

        let mqtt_client = Self {
            client: client.clone(),
            agent_id: agent_id.to_string(),
            _eventloop_guard: Arc::new(EventLoopGuard { _task: poll_task }),
        };

        // ── Step 2: PUBLISH status = "online" (Retained) ──
        mqtt_client
            .publish_status(true)
            .await?;

        // ── Step 3: PUBLISH meta (Retained) ──
        let meta = AgentMeta {
            agent_id: agent_id.to_string(),
            name: agent_name.to_string(),
            version: agent_version.to_string(),
            avatar: avatar.unwrap_or("").to_string(),
            builtin_avatar: builtin_avatar.unwrap_or("").to_string(),
        };
        mqtt_client
            .publish_envelope(&meta_topic, &DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AgentMeta(meta)),
            }, MqttQoS::AtLeastOnce, true)
            .await?;

        // ── Step 4: PUBLISH config (Retained) ──
        let config = AgentConfig {
            agent_id: agent_id.to_string(),
            config_json: config_json.to_string(),
        };
        mqtt_client
            .publish_envelope(&config_topic, &DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AgentConfig(config)),
            }, MqttQoS::AtLeastOnce, true)
            .await?;

        // ── Step 5: SUBSCRIBE acowork/global/# ──
        mqtt_client
            .subscribe("acowork/global/#", MqttQoS::AtLeastOnce)
            .await?;

        // ── Step 6: SUBSCRIBE agents/{id}/sessions/control/# ──
        mqtt_client
            .subscribe(&control_filter, MqttQoS::AtLeastOnce)
            .await?;

        tracing::info!(agent_id = %agent_id, "Runtime MQTT client: published status/meta/config + subscribed to global + control");

        Ok(mqtt_client)
    }

    /// Wait for the MQTT connection by attempting a lightweight subscribe.
    async fn wait_for_connection(client: &AsyncClient, max_attempts: usize) {
        for _ in 0..max_attempts {
            match client
                .subscribe("_acowork/health_check", QoS::AtMostOnce)
                .await
            {
                Ok(_) => {
                    let _ = client.unsubscribe("_acowork/health_check").await;
                    return;
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Publish a `DataEnvelope` payload to a topic.
    pub async fn publish_envelope(
        &self,
        topic: &str,
        envelope: &DataEnvelope,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), RuntimeMqttClientError> {
        let payload = prost::Message::encode_to_vec(envelope);
        self.client
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Publish agent status (online/offline) as a plain text Retained message.
    pub async fn publish_status(&self, online: bool) -> Result<(), RuntimeMqttClientError> {
        let topic = format!("acowork/agents/{}/status", self.agent_id);
        let payload = if online { "online" } else { "offline" };
        self.client
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("status: {}", e)))?;
        Ok(())
    }

    /// Publish a raw payload to a topic (non-protobuf).
    pub async fn publish_raw(
        &self,
        topic: &str,
        payload: &[u8],
        qos: MqttQoS,
    ) -> Result<(), RuntimeMqttClientError> {
        self.client
            .publish(topic, qos.into(), false, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Subscribe to a topic filter.
    pub async fn subscribe(
        &self,
        filter: &str,
        qos: MqttQoS,
    ) -> Result<(), RuntimeMqttClientError> {
        self.client
            .subscribe(filter, qos.into())
            .await
            .map_err(|e| RuntimeMqttClientError::Subscribe(format!("'{}': {}", filter, e)))?;
        Ok(())
    }

    /// Publish a session event (chunk, done, error, etc.) to the messages topic.
    #[allow(dead_code)]
    pub async fn publish_session_event(
        &self,
        session_id: &str,
        event_type: &str,
        envelope: &DataEnvelope,
    ) -> Result<(), RuntimeMqttClientError> {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/messages/{}",
            self.agent_id, session_id, event_type
        );
        // Session messages are QoS 0 (fire-and-forget for streaming events)
        self.publish_envelope(&topic, envelope, MqttQoS::AtMostOnce, false)
            .await
    }

    /// Publish a session lifecycle event (created/deleted).
    #[allow(dead_code)]
    pub async fn publish_session_lifecycle(
        &self,
        event_type: &str,
        envelope: &DataEnvelope,
    ) -> Result<(), RuntimeMqttClientError> {
        let topic = format!("acowork/agents/{}/sessions/{}", self.agent_id, event_type);
        self.publish_envelope(&topic, envelope, MqttQoS::AtLeastOnce, false)
            .await
    }

    /// Get the agent_id this client represents.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Get a clone of the inner AsyncClient.
    pub fn inner(&self) -> AsyncClient {
        self.client.clone()
    }
}

impl Drop for RuntimeMqttClient {
    fn drop(&mut self) {
        // Best-effort: publish "offline" before the connection drops.
        // The Last Will ensures this happens even on crash, but a clean
        // disconnect publishes immediately rather than waiting for keep-alive timeout.
        let client = self.client.clone();
        let status_topic = format!("acowork/agents/{}/status", self.agent_id);
        tokio::spawn(async move {
            let _ = client
                .publish(status_topic, QoS::AtLeastOnce, true, "offline")
                .await;
        });
    }
}

/// Shared, thread-safe RuntimeMqttClient.
pub type SharedRuntimeMqttClient = Arc<Mutex<RuntimeMqttClient>>;

// ── MQTT Chunk Publisher (ADR-033 Phase 4: dual-channel session events) ──

/// Lightweight, clonable publisher for MQTT session events.
///
/// Created from a `RuntimeMqttClient` and passed to `SessionCore` so that
/// chunk/tool_call/done events can be published via MQTT alongside the
/// existing gRPC channel.
#[derive(Clone)]
pub struct MqttChunkPublisher {
    agent_id: String,
    client: AsyncClient,
}

impl MqttChunkPublisher {
    /// Create from a RuntimeMqttClient.
    pub fn from_runtime_client(client: &RuntimeMqttClient) -> Self {
        Self {
            agent_id: client.agent_id().to_string(),
            client: client.inner(),
        }
    }

    /// Publish a session event envelope to the broker.
    async fn publish(&self, session_id: &str, event_type: &str, payload: &[u8]) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/messages/{}",
            self.agent_id, session_id, event_type
        );
        if let Err(e) = self
            .client
            .publish(topic, QoS::AtMostOnce, false, payload)
            .await
        {
            tracing::warn!(error = %e, session_id, event_type, "Failed to publish MQTT session event");
        }
    }

    /// Publish a chunk event via MQTT.
    #[allow(dead_code)]
    pub(crate) fn publish_chunk(&self, session_id: &str, message_id: &str, delta: &str) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let d = delta.to_string();
        tokio::spawn(async move {
            let payload = serde_json::json!({
                "message_id": mid,
                "delta": d,
            });
            if let Ok(bytes) = serde_json::to_vec(&payload) {
                publisher.publish(&sid, "chunk", &bytes).await;
            }
        });
    }

    /// Publish a done event via MQTT.
    #[allow(dead_code)]
    pub(crate) fn publish_done(&self, session_id: &str, message_id: &str) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        tokio::spawn(async move {
            let payload = serde_json::json!({"message_id": mid});
            if let Ok(bytes) = serde_json::to_vec(&payload) {
                publisher.publish(&sid, "done", &bytes).await;
            }
        });
    }

    /// Publish a tool_call event via MQTT.
    #[allow(dead_code)]
    pub(crate) fn publish_tool_call(&self, session_id: &str, tool_name: &str, tool_input: &str) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let tn = tool_name.to_string();
        let ti = tool_input.to_string();
        tokio::spawn(async move {
            let payload = serde_json::json!({
                "tool_name": tn,
                "tool_input": ti,
            });
            if let Ok(bytes) = serde_json::to_vec(&payload) {
                publisher.publish(&sid, "tool_call", &bytes).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_runtime_mqtt_client_connects_and_publishes() {
        // Start a broker (using the Gateway's broker module)
        let port = 18980;
        let broker = acowork_gateway::mqtt::start_broker("127.0.0.1", port)
            .expect("broker should start");

        let cache = crate::mqtt::available_cache::new_shared_cache();
        let (control_tx, _control_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();

        let client = RuntimeMqttClient::connect(
            "127.0.0.1",
            port,
            "com.test.agent",
            "Test Agent",
            "1.0.0",
            None,
            None,
            "{}",
            cache,
            control_tx,
        )
        .await
        .expect("Runtime MQTT client should connect");

        // Verify status was published as retained by subscribing and receiving it
        use rumqttc::{AsyncClient as SubClient, MqttOptions as SubOpts};
        let mut sub_opts = SubOpts::new("test:subscriber", "127.0.0.1", port);
        sub_opts.set_keep_alive(Duration::from_secs(5));
        let (sub_client, mut sub_eventloop) = SubClient::new(sub_opts, 10);
        sub_client
            .subscribe("acowork/agents/com.test.agent/#", QoS::AtLeastOnce)
            .await
            .unwrap();

        let mut received_topics = Vec::new();
        for _ in 0..100 {
            match sub_eventloop.poll().await {
                Ok(Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                    received_topics.push(p.topic);
                    if received_topics.len() >= 3 {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }

        assert!(
            received_topics
                .contains(&"acowork/agents/com.test.agent/status".to_string()),
            "should receive status: {:?}",
            received_topics
        );
        assert!(
            received_topics
                .contains(&"acowork/agents/com.test.agent/meta".to_string()),
            "should receive meta: {:?}",
            received_topics
        );
        assert!(
            received_topics
                .contains(&"acowork/agents/com.test.agent/config".to_string()),
            "should receive config: {:?}",
            received_topics
        );

        drop(sub_client);
        drop(client);
        drop(broker);
    }
}
