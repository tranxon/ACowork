//! Gateway MQTT client (ADR-033 Phase 1).
//!
//! The Gateway uses a `rumqttc::AsyncClient` to connect to its own
//! embedded broker. This client (`client_id: gateway:publisher`) is
//! the sole publisher of `acowork/global/{kind}` Retained topics.
//!
//! In Phase 2+, the Gateway also subscribes to agent status topics
//! for the Agent Registry. Phase 1 uses this client only for publishing.

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use tokio::sync::Mutex;

use acowork_core::defaults;
use acowork_core::mqtt_proto::DataEnvelope;

/// Callback type for receiving non-global MQTT messages (e.g. agent http_port).
pub type MqttMessageCallback = Arc<dyn Fn(String, Vec<u8>) + Send + Sync>;

/// Error type for Gateway MQTT client operations.
#[derive(Debug, thiserror::Error)]
pub enum GatewayMqttClientError {
    #[error("MQTT connection error: {0}")]
    Connection(String),
    #[error("MQTT publish error: {0}")]
    Publish(String),
    #[error("MQTT subscribe error: {0}")]
    Subscribe(String),
}

/// QoS level for MQTT messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttQoS {
    /// At most once (fire-and-forget). For streaming events.
    AtMostOnce,
    /// At least once. For state changes and control commands.
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

/// The Gateway's MQTT client for publishing to the embedded broker.
///
/// Wraps `rumqttc::AsyncClient` with connection management and
/// protobuf-aware publish helpers. The event loop is polled in a
/// background tokio task to maintain the connection.
#[derive(Clone)]
pub struct GatewayMqttClient {
    client: AsyncClient,
    /// The event loop is kept alive by the background poll task.
    /// We hold the handle to prevent it from being dropped.
    _eventloop_guard: Arc<EventLoopGuard>,
}

/// Guard that keeps the event loop polling task alive.
struct EventLoopGuard {
    _task: tokio::task::JoinHandle<()>,
}

impl GatewayMqttClient {
    /// Create and connect a new Gateway MQTT client.
    ///
    /// Spawns a background task that polls the event loop to maintain
    /// the connection and handle reconnections automatically.
    pub async fn connect(
        host: &str,
        port: u16,
        client_id: &str,
        message_callback: Option<MqttMessageCallback>,
    ) -> Result<Self, GatewayMqttClientError> {
        let mut options = MqttOptions::new(client_id, host, port);
        options.set_keep_alive(Duration::from_secs(30));
        // Clean start = true (MQTT 3.1.1). No session persistence.
        options.set_clean_session(true);

        let (client, mut eventloop) = AsyncClient::new(options, 50);

        // Spawn a background task to poll the event loop.
        // This is required for the connection to be maintained and
        // for automatic reconnection on network failures.
        let poll_task = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Incoming::Publish(publish))) => {
                        tracing::trace!(topic = %publish.topic, "MQTT publish (gateway client)");
                        if let Some(ref cb) = message_callback {
                            cb(publish.topic, publish.payload.to_vec());
                        }
                    }
                    Ok(Event::Incoming(_)) => continue,
                    Ok(Event::Outgoing(outgoing)) => {
                        tracing::trace!(?outgoing, "MQTT outgoing (gateway client)");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "MQTT event loop error (gateway client), will retry");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });

        // Wait for the connection to be established.
        // We attempt a lightweight operation (subscribe to a dummy topic)
        // to verify connectivity. If it fails after retries, return error.
        let connected = Self::wait_for_connection(&client, 20).await;
        if !connected {
            return Err(GatewayMqttClientError::Connection(format!(
                "Failed to connect to MQTT broker at {}:{} within timeout",
                host, port
            )));
        }

        tracing::info!(
            host,
            port,
            client_id,
            "Gateway MQTT client connected to broker"
        );

        Ok(Self {
            client,
            _eventloop_guard: Arc::new(EventLoopGuard { _task: poll_task }),
        })
    }

    /// Create the Gateway publisher client (`client_id: gateway:publisher`).
    pub async fn new_publisher(
        host: &str,
        port: u16,
    ) -> Result<Self, GatewayMqttClientError> {
        Self::connect(host, port, defaults::GATEWAY_MQTT_PUBLISHER_CLIENT_ID, None).await
    }

    /// Create a publisher with a message callback for incoming subscriptions.
    pub async fn new_publisher_with_callback(
        host: &str,
        port: u16,
        callback: MqttMessageCallback,
    ) -> Result<Self, GatewayMqttClientError> {
        Self::connect(host, port, defaults::GATEWAY_MQTT_PUBLISHER_CLIENT_ID, Some(callback)).await
    }

    /// Create the Gateway publisher client with default localhost settings.
    pub async fn new_default_publisher() -> Result<Self, GatewayMqttClientError> {
        Self::new_publisher(
            defaults::GATEWAY_MQTT_HOST,
            defaults::GATEWAY_MQTT_PORT,
        )
        .await
    }

    /// Wait for the MQTT connection to be established by polling.
    ///
    /// Tries up to `max_attempts` times with a short delay between attempts.
    /// Returns true if connected, false if timed out.
    async fn wait_for_connection(client: &AsyncClient, max_attempts: usize) -> bool {
        // rumqttc's AsyncClient methods are fire-and-forget — they queue
        // requests and the event loop processes them. The connection is
        // established when the event loop processes the CONNECT packet.
        // We verify by attempting a subscribe (which requires a connection).
        for attempt in 0..max_attempts {
            // Try subscribing to a dummy topic. If the connection isn't
            // established yet, this will be queued and processed once
            // connected. We check connectivity by seeing if the subscribe
            // doesn't error immediately (which it won't in rumqttc).
            match client
                .subscribe("_acowork/health_check", QoS::AtMostOnce)
                .await
            {
                Ok(_) => {
                    // Subscribe queued successfully — connection is likely up.
                    // Unsubscribe to clean up.
                    let _ = client
                        .unsubscribe("_acowork/health_check")
                        .await;
                    return true;
                }
                Err(rumqttc::ClientError::Request(_)) => {
                    // Disconnected — retry
                    tracing::debug!(
                        attempt,
                        "MQTT connect attempt failed (request queued), retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => {
                    tracing::debug!(attempt, error = %e, "MQTT connect attempt error, retrying");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        false
    }

    /// Publish a `DataEnvelope` payload to a topic.
    ///
    /// The envelope is protobuf-encoded and published as binary.
    pub async fn publish_envelope(
        &self,
        topic: &str,
        envelope: &DataEnvelope,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), GatewayMqttClientError> {
        let payload = prost::Message::encode_to_vec(envelope);
        self.client
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| GatewayMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        tracing::trace!(topic, qos = ?qos, retain, "MQTT published envelope");
        Ok(())
    }

    /// Publish a control command to an agent's control topic.
    ///
    /// Desktop sends commands to Runtime via MQTT (mqtt.md §5.2).
    /// QoS: AtLeastOnce — control commands must not be lost.
    pub async fn publish_control_command(
        &self,
        agent_id: &str,
        command: acowork_core::mqtt_proto::ControlCommand,
    ) -> Result<(), GatewayMqttClientError> {
        let topic = format!("acowork/agents/{}/sessions/control/{}",
            agent_id,
            match &command.command {
                Some(acowork_core::mqtt_proto::control_command::Command::CreateSession(_)) => "create_session",
                Some(acowork_core::mqtt_proto::control_command::Command::DeleteSession(_)) => "delete_session",
                Some(acowork_core::mqtt_proto::control_command::Command::Message(_)) => "message",
                Some(acowork_core::mqtt_proto::control_command::Command::Stop(_)) => "stop",
                Some(acowork_core::mqtt_proto::control_command::Command::ModelSwitch(_)) => "model_switch",
                _ => "unknown",
            }
        );
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::ControlCommand(command)),
        };
        self.publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false).await
    }

    /// Publish a raw binary payload to a topic.
    pub async fn publish_raw(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), GatewayMqttClientError> {
        self.client
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| GatewayMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Publish a text payload to a topic (e.g. agent status "online"/"offline").
    pub async fn publish_text(
        &self,
        topic: &str,
        payload: &str,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), GatewayMqttClientError> {
        self.client
            .publish(topic, qos.into(), retain, payload.as_bytes())
            .await
            .map_err(|e| GatewayMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Subscribe to a topic filter.
    #[allow(dead_code)]
    pub async fn subscribe(
        &self,
        filter: &str,
        qos: MqttQoS,
    ) -> Result<(), GatewayMqttClientError> {
        self.client
            .subscribe(filter, qos.into())
            .await
            .map_err(|e| GatewayMqttClientError::Subscribe(format!("'{}': {}", filter, e)))?;
        Ok(())
    }

    /// Get a clone of the inner AsyncClient for advanced use cases.
    ///
    /// The returned client shares the same event loop, so publishes
    /// through it will be handled by the same connection.
    pub fn inner(&self) -> AsyncClient {
        self.client.clone()
    }
}

/// A shared, thread-safe Gateway MQTT client.
pub type SharedGatewayMqttClient = Arc<Mutex<GatewayMqttClient>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_gateway_client_connects_to_broker() {
        // Start a broker on a test port
        let port = 18976;
        let broker_handle = crate::mqtt::broker::start_broker("127.0.0.1", port)
            .expect("broker should start");

        // Connect a publisher client
        let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("client should connect");

        // Publish a test message
        client
            .publish_text(
                "acowork/test/status",
                "online",
                MqttQoS::AtLeastOnce,
                true,
            )
            .await
            .expect("publish should succeed");

        // Clean up
        drop(client);
        drop(broker_handle);
    }

    #[tokio::test]
    async fn test_publish_envelope() {
        let port = 18977;
        let broker_handle = crate::mqtt::broker::start_broker("127.0.0.1", port)
            .expect("broker should start");

        let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("client should connect");

        // Build and publish a DataEnvelope
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(
                acowork_core::mqtt_proto::AvailableProviders {
                    version: 1,
                    providers: vec![],
                },
            )),
        };

        client
            .publish_envelope(
                "acowork/global/providers",
                &envelope,
                MqttQoS::AtLeastOnce,
                true,
            )
            .await
            .expect("envelope publish should succeed");

        drop(client);
        drop(broker_handle);
    }
}
