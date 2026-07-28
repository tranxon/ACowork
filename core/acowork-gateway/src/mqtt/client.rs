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

use rumqttc::{AsyncClient, ConnectionError, ConnectReturnCode, Event, Incoming, MqttOptions, QoS};
use tokio::sync::Mutex;

use acowork_core::defaults;
use acowork_core::mqtt_proto::DataEnvelope;
use acowork_mqtt_session::{
    classify as classify_err, ErrorDescriptor, ErrorKind, RefusedReason, ReconnectPolicy,
    SessionState, SessionStateTx,
};

/// Watchdog timeout for `eventloop.poll()`.
///
/// If poll() doesn't produce any event within this duration, the TCP
/// socket is presumed half-dead (most commonly after OS sleep/wake).
/// The poll task breaks to the soft-restart path, which drops the old
/// EventLoop and creates a fresh TCP connection.
///
/// 20 s = 4 × keepalive interval (5 s). Previously 90 s.
const POLL_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(20);

/// Build an [`ErrorDescriptor`] from rumqttc 0.25's `ConnectionError`.
///
/// This is the same adapter used by the Desktop client, bridging the
/// rumqttc 0.25 error type to the version-agnostic `ErrorDescriptor`
/// used by the shared `acowork-mqtt-session` crate.
fn error_descriptor_from_rumqttc_025(err: &ConnectionError) -> ErrorDescriptor {
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

/// Topic filters that must be re-subscribed on every ConnAck.
///
/// With `clean_session = true` the broker drops all subscriptions on
/// disconnect, so **every** active subscription must be listed here.
/// Without re-subscribing, the Gateway silently loses agent http_port
/// and status updates after any MQTT reconnect.
const PERSISTENT_SUBSCRIPTIONS: &[(&str, QoS)] = &[
    ("acowork/agents/+/http_port", QoS::AtLeastOnce),
    ("acowork/agents/+/status", QoS::AtLeastOnce),
];

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
    /// Shared client handle, swappable during soft-restart.
    shared_client: Arc<Mutex<AsyncClient>>,
    /// The event loop is kept alive by the background poll task.
    _eventloop_guard: Arc<EventLoopGuard>,
}

impl GatewayMqttClient {
    /// Obtain a clone of the current `AsyncClient`.
    async fn client(&self) -> AsyncClient {
        self.shared_client.lock().await.clone()
    }
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
        // Match the broker's `connection_timeout_ms` (5 s). This client
        // connects to the Gateway's own embedded broker on localhost,
        // but TCP half-dead connections can still occur after OS
        // sleep/wake, so we use the same keepalive as the broker timeout.
        options.set_keep_alive(Duration::from_secs(5));
        // Clean start = true (MQTT 3.1.1). No session persistence.
        options.set_clean_session(true);

        let (client, mut eventloop) = AsyncClient::new(options.clone(), 50);
        let shared_client: Arc<Mutex<AsyncClient>> = Arc::new(Mutex::new(client));

        // ADR-039 Phase 2: session state broadcast + reconnect policy.
        let (state_tx, _) = SessionStateTx::new(SessionState::Connecting);
        let poll_state_tx = state_tx.clone();
        let reconnect_policy = ReconnectPolicy::default();

        let task_shared_client = Arc::clone(&shared_client);
        let task_options = options;
        let task_callback = message_callback;

        // Spawn the eventloop poller with the same robust structure as
        // Desktop and Agent Runtime: outer loop (soft-restart) + inner
        // loop (normal polling) + watchdog + error classification.
        let poll_task = tokio::spawn(async move {
            let mut soft_restart_count: u32 = 0;

            loop {
                let mut consecutive_failures: u32 = 0;
                let mut fatal_streak: u32 = 0;

                loop {
                    tokio::select! {
                        event_result = eventloop.poll() => {
                            match event_result {
                                Ok(Event::Incoming(Incoming::Publish(publish))) => {
                                    if let Some(ref cb) = task_callback {
                                        cb(publish.topic, publish.payload.to_vec());
                                    }
                                }
                                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                                    tracing::info!(
                                        "Gateway MQTT broker confirmed (re)connection - re-subscribing persistent topics"
                                    );
                                    poll_state_tx.set(SessionState::Connected);
                                    consecutive_failures = 0;
                                    fatal_streak = 0;

                                    let poll_client = task_shared_client.lock().await.clone();
                                    for (filter, qos) in PERSISTENT_SUBSCRIPTIONS {
                                        if let Err(e) = poll_client.subscribe(*filter, *qos).await {
                                            tracing::warn!(
                                                filter,
                                                error = %e,
                                                "Gateway MQTT resubscribe failed"
                                            );
                                        }
                                    }
                                }
                                Ok(Event::Incoming(Incoming::Disconnect)) => {
                                    poll_state_tx.set(SessionState::Reconnecting);
                                }
                                Ok(_) => continue,
                                Err(e) => {
                                    let desc = error_descriptor_from_rumqttc_025(&e);
                                    let class = classify_err(&desc);

                                    tracing::warn!(
                                        error = %e,
                                        err_class = class.label(),
                                        consecutive_failures,
                                        "Gateway MQTT event loop error"
                                    );

                                    if class.is_fatal() {
                                        fatal_streak += 1;
                                        poll_state_tx.set(SessionState::Reconnecting);

                                        if fatal_streak >= 3 {
                                            soft_restart_count += 1;
                                            tracing::warn!(
                                                soft_restart_count,
                                                "3 consecutive fatal errors - soft-restarting Gateway MQTT client"
                                            );
                                            break;
                                        }
                                        tokio::time::sleep(Duration::from_secs(60)).await;
                                    } else {
                                        poll_state_tx.set(SessionState::Reconnecting);
                                        consecutive_failures += 1;
                                        if let Some(backoff) =
                                            reconnect_policy.backoff(class, consecutive_failures - 1)
                                        {
                                            tokio::time::sleep(backoff.duration).await;
                                        }
                                    }
                                }
                            }
                        }

                        _ = tokio::time::sleep(POLL_WATCHDOG_TIMEOUT) => {
                            tracing::warn!(
                                timeout_s = POLL_WATCHDOG_TIMEOUT.as_secs(),
                                "Gateway MQTT poll() watchdog timeout - forcing soft-restart (possible half-dead socket)"
                            );
                            poll_state_tx.set(SessionState::Reconnecting);
                            break;
                        }
                    }
                }

                // Soft-restart: recreate client + EventLoop
                poll_state_tx.set(SessionState::Connecting);
                let (new_client, new_eventloop) =
                    AsyncClient::new(task_options.clone(), 50);
                *task_shared_client.lock().await = new_client;
                eventloop = new_eventloop;
                tracing::info!(
                    soft_restart_count,
                    "Gateway MQTT client soft-restarted with fresh EventLoop"
                );
            }
        });

        // Wait for the connection to be established.
        let probe_client = shared_client.lock().await.clone();
        let connected = Self::wait_for_connection(&probe_client, 20).await;
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
            shared_client,
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
        self.client().await
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
                // ── Session lifecycle ──
                Some(acowork_core::mqtt_proto::control_command::Command::CreateSession(_)) => "create_session",
                Some(acowork_core::mqtt_proto::control_command::Command::DeleteSession(_)) => "delete_session",
                Some(acowork_core::mqtt_proto::control_command::Command::CloseSession(_)) => "close_session",
                Some(acowork_core::mqtt_proto::control_command::Command::UpdateSessionTitle(_)) => "update_session_title",
                // ── Chat ──
                Some(acowork_core::mqtt_proto::control_command::Command::ChatMessage(_)) => "chat_message",
                Some(acowork_core::mqtt_proto::control_command::Command::Stop(_)) => "stop",
                Some(acowork_core::mqtt_proto::control_command::Command::ContinueExecution(_)) => "continue_execution",
                Some(acowork_core::mqtt_proto::control_command::Command::EnableNotify(_)) => "enable_notify",
                Some(acowork_core::mqtt_proto::control_command::Command::DisableNotify(_)) => "disable_notify",
                // ── User responses ──
                Some(acowork_core::mqtt_proto::control_command::Command::ApprovalDecision(_)) => "approval_decision",
                Some(acowork_core::mqtt_proto::control_command::Command::QuestionAnswer(_)) => "question_answer",
                // ── Per-session config ──
                Some(acowork_core::mqtt_proto::control_command::Command::ModelSwitch(_)) => "model_switch",
                Some(acowork_core::mqtt_proto::control_command::Command::ReasoningEffort(_)) => "reasoning_effort",
                Some(acowork_core::mqtt_proto::control_command::Command::WorkspaceSwitch(_)) => "workspace_switch",
                // ── Context management ──
                Some(acowork_core::mqtt_proto::control_command::Command::CompactContext(_)) => "compact_context",
                Some(acowork_core::mqtt_proto::control_command::Command::CompressAction(_)) => "compress_action",
                // ── System ──
                Some(acowork_core::mqtt_proto::control_command::Command::Intent(_)) => "intent",
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
        self.client().await
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
        self.client().await
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
        self.client().await
            .subscribe(filter, qos.into())
            .await
            .map_err(|e| GatewayMqttClientError::Subscribe(format!("'{}': {}", filter, e)))?;
        Ok(())
    }

    /// Get a clone of the inner AsyncClient for advanced use cases.
    ///
    /// The returned client shares the same event loop, so publishes
    /// through it will be handled by the same connection.
    pub async fn inner(&self) -> AsyncClient {
        self.client().await
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
