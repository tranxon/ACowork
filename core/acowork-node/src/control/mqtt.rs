//! Node Agent MQTT client (ADR-055 §6.2 / ADR-039 reconnect framework).
//!
//! A rumqttc client with:
//! - client_id `node:{node_id}` (protocol §8.5 colon convention);
//! - LastWill `acowork/nodes/{node_id}/status = "offline"` (QoS 1,
//!   retained) — the LWT topic is fixed at CONNECT time, which is why
//!   the node identity must be finalized before connecting (§6.12);
//! - an event-loop poll task with the same soft-restart + error
//!   classification + exponential backoff structure as the Gateway /
//!   Runtime / Desktop clients;
//! - a bootstrap callback re-run on every ConnAck: publish
//!   `status=online` + `info` (retained) and re-subscribe to the
//!   node's control topic filters (clean_session = true means the
//!   broker drops subscriptions on disconnect).

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, ConnectionError, ConnectReturnCode, Event, Incoming, LastWill, MqttOptions, QoS};
use tokio::sync::Mutex;

use acowork_core::mqtt_proto::DataEnvelope;
use acowork_mqtt_session::{
    classify as classify_err, ErrorDescriptor, ErrorKind, ErrClass, RefusedReason, ReconnectPolicy,
    SessionState, SessionStateTx,
};

/// Watchdog timeout for `eventloop.poll()`. 5 s matches the desktop
/// client (28b1aec8): the broker keepalive is 5 s, so a healthy poll
/// returns at least one event per interval; anything silent for 5 s is
/// a half-dead socket (e.g. after OS sleep/wake) and must be cut
/// short immediately. The previous 4 × keepalive (20 s) is why the
/// node took 20 s to recover after every wake.
const POLL_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);

/// Sleep for `dur` unless a force-restart is requested, in which case
/// return `true` so the caller can break to the soft-restart path.
///
/// The force-restart signal must be able to interrupt a backoff
/// sleep, not only `poll()`: after a system wake the poll task often
/// returns a fatal IO error (10053) instead of hanging, and an
/// uninterruptible 60 s fatal backoff leaves the node offline for the
/// whole minute (desktop-app wake incident, 2026-08 — recovery took
/// exactly 60 s because `sleep(60s).await` sat outside any `select!`,
/// so the wake-triggered force-restart could not break it).
async fn interruptible_backoff(
    dur: Duration,
    force_restart: &tokio::sync::Notify,
    kind: &str,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = force_restart.notified() => {
            tracing::info!(kind, "Force-restart requested during backoff");
            true
        }
    }
}

/// Error type for the node MQTT client.
#[derive(Debug, thiserror::Error)]
pub enum NodeMqttClientError {
    #[error("MQTT connection error: {0}")]
    Connection(String),
    #[error("MQTT publish error: {0}")]
    Publish(String),
    #[error("MQTT subscribe error: {0}")]
    Subscribe(String),
}

/// Adapter from rumqttc 0.25 errors to the shared
/// `acowork-mqtt-session` descriptor (same mapping as the Gateway
/// client).
fn error_descriptor_from_rumqttc(err: &ConnectionError) -> ErrorDescriptor {
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

/// Callback invoked for every incoming PUBLISH on the node's
/// subscriptions.
pub type NodeMqttMessageCallback = Arc<dyn Fn(String, Vec<u8>) + Send + Sync>;

/// Callback re-run on every ConnAck (idempotent bootstrap):
/// publish retained status/info + re-subscribe control filters.
pub type NodeMqttBootstrapCallback = Arc<dyn Fn(AsyncClient) + Send + Sync>;

/// The Node Agent's MQTT client. The event loop is polled in a
/// background task that survives reconnects and soft-restarts.
#[derive(Clone)]
pub struct NodeMqttClient {
    shared_client: Arc<Mutex<AsyncClient>>,
    /// Signal to force a soft-restart of the MQTT event loop.
    ///
    /// When [`NodeMqttClient::force_reconnect`] is called, the poll
    /// task receives this notification, breaks out of its inner
    /// polling loop, and recreates the `AsyncClient` + `EventLoop`
    /// from scratch — exactly the same path as the automatic
    /// watchdog-triggered soft-restart (e.g. after a system
    /// sleep/wake the stale TCP connection can stall `poll()`).
    force_restart: Arc<tokio::sync::Notify>,
    _eventloop_guard: Arc<EventLoopGuard>,
}

struct EventLoopGuard {
    _task: tokio::task::JoinHandle<()>,
}

/// Live CONNECT credential slot (ADR-055 Phase 5a).
///
/// The daemon starts with the enrollment token (first boot) and swaps
/// in the Gateway-issued node_token once the `enroll_result` reply
/// arrives. The poll task re-reads the slot on every soft-restart, so
/// a reconnect never re-presents a consumed enrollment token.
pub type SharedNodeMqttCredentials = Arc<Mutex<Option<String>>>;

impl NodeMqttClient {
    /// Connect to the Gateway broker and start the poll task.
    ///
    /// Unlike the Gateway client this does NOT fail after a fixed
    /// connect timeout — a remote node may boot before the Gateway,
    /// so the poll task keeps retrying with backoff forever. The
    /// returned handle is usable immediately; publishes are queued by
    /// rumqttc until the connection comes up.
    ///
    /// ADR-055 Phase 5a: `credentials` is the live CONNECT password
    /// (username is always `node:{node_id}`, protocol §8.5; the
    /// broker's CONNECT auth handler keys on client_id). It starts as
    /// the node_token (reconnect) or a one-time enrollment token
    /// (first boot) and is swapped to the node_token when the
    /// enrollment reply arrives. `None` skips credentials entirely
    /// (compatible with `mqtt.auth_enabled=false` brokers).
    pub async fn connect(
        host: &str,
        port: u16,
        node_id: &str,
        credentials: SharedNodeMqttCredentials,
        ready_topic: Option<String>,
        bootstrap: NodeMqttBootstrapCallback,
        message_callback: Option<NodeMqttMessageCallback>,
    ) -> Result<Self, NodeMqttClientError> {
        let client_id = acowork_core::node::node_client_id(node_id);
        let status_topic = acowork_core::node::node_status_topic(node_id);

        let mut options = MqttOptions::new(client_id.clone(), host, port);
        options.set_keep_alive(Duration::from_secs(5));
        options.set_clean_session(true);
        if let Some(password) = credentials.lock().await.as_deref() {
            options.set_credentials(client_id.clone(), password);
        }
        let pkt_size = acowork_core::defaults::GATEWAY_MQTT_MAX_PACKET_SIZE;
        options.set_max_packet_size(pkt_size, pkt_size);

        // LWT: broker publishes retained "offline" if the node dies
        // ungracefully (§6.2).
        let will = LastWill::new(&status_topic, "offline", QoS::AtLeastOnce, true);
        options.set_last_will(will);

        let (client, mut eventloop) = AsyncClient::new(options.clone(), 50);
        let shared_client: Arc<Mutex<AsyncClient>> = Arc::new(Mutex::new(client));

        let (state_tx, _) = SessionStateTx::new(SessionState::Connecting);
        let poll_state_tx = state_tx.clone();
        let reconnect_policy = ReconnectPolicy::default();

        let task_shared_client = Arc::clone(&shared_client);
        let task_options = options;
        let task_credentials = Arc::clone(&credentials);
        let task_bootstrap = bootstrap;
        let task_callback = message_callback;
        // ADR-059 §7.2: on any observed disconnect the poll task clears
        // the retained NodeReady snapshot (empty payload = deleted) so
        // the Gateway demotes this node back to not-ready even when the
        // LWT misses (e.g. broker restart without the LWT firing).
        // Best-effort: a hard network drop may queue the clear until
        // the reconnect, where the bootstrap's fresh NodeReady publish
        // follows in order — the Gateway converges on the newest state.
        let task_ready_topic = ready_topic;
        let task_force_restart = Arc::new(tokio::sync::Notify::new());
        let force_restart = Arc::clone(&task_force_restart);

        let poll_task = tokio::spawn(async move {
            loop {
                let mut consecutive_failures: u32 = 0;
                let mut fatal_streak: u32 = 0;

                loop {
                    tokio::select! {
                        biased;
                        // Mirror desktop (apps/acowork-desktop/src-tauri/src/
                        // mqtt_client.rs:399): favour the force-restart
                        // signal so a stored notify permit always wins on
                        // the very next poll() return, instead of waiting
                        // for an event-storm iteration order to land.
                        _ = task_force_restart.notified() => {
                            // External recovery trigger (system sleep/wake):
                            // the broker already timed the stale connection
                            // out while the machine slept, so reconnect now
                            // instead of waiting for the poll watchdog.
                            tracing::info!(
                                "Node MQTT force-restart requested (e.g. system wake)"
                            );
                            poll_state_tx.set(SessionState::Reconnecting);
                            break;
                        }
                        event_result = eventloop.poll() => {
                            match event_result {
                                Ok(Event::Incoming(Incoming::Publish(publish))) => {
                                    if let Some(ref cb) = task_callback {
                                        cb(publish.topic, publish.payload.to_vec());
                                    }
                                }
                                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                                    tracing::info!(
                                        node_client_id = %task_options.client_id(),
                                        "Node MQTT (re)connected — running bootstrap"
                                    );
                                    poll_state_tx.set(SessionState::Connected);
                                    consecutive_failures = 0;
                                    fatal_streak = 0;

                                    let poll_client = task_shared_client.lock().await.clone();
                                    task_bootstrap(poll_client);
                                }
                                Ok(Event::Incoming(Incoming::Disconnect)) => {
                                    poll_state_tx.set(SessionState::Reconnecting);
                                    if let Some(ref topic) = task_ready_topic {
                                        let _ = task_shared_client
                                            .lock()
                                            .await
                                            .publish(topic, QoS::AtLeastOnce, true, Vec::new())
                                            .await;
                                    }
                                }
                                Ok(_) => continue,
                                Err(e) => {
                                    let desc = error_descriptor_from_rumqttc(&e);
                                    let class: ErrClass = classify_err(&desc);

                                    tracing::warn!(
                                        error = %e,
                                        err_class = class.label(),
                                        consecutive_failures,
                                        "Node MQTT event loop error"
                                    );

                                    // ADR-059 §7.2: connection-level failure
                                    // also clears the retained NodeReady.
                                    if let Some(ref topic) = task_ready_topic {
                                        let _ = task_shared_client
                                            .lock()
                                            .await
                                            .publish(topic, QoS::AtLeastOnce, true, Vec::new())
                                            .await;
                                    }

                                    if class.is_fatal() {
                                        fatal_streak += 1;
                                        poll_state_tx.set(SessionState::Reconnecting);
                                        if fatal_streak >= 3 {
                                            tracing::warn!(
                                                fatal_streak,
                                                "3 consecutive fatal errors — soft-restarting node MQTT client"
                                            );
                                            break;
                                        }
                                        if interruptible_backoff(
                                            Duration::from_secs(60),
                                            &task_force_restart,
                                            "fatal",
                                        )
                                        .await
                                        {
                                            break;
                                        }
                                    } else {
                                        poll_state_tx.set(SessionState::Reconnecting);
                                        consecutive_failures += 1;
                                        if let Some(backoff) =
                                            reconnect_policy.backoff(class, consecutive_failures - 1)
                                            && interruptible_backoff(
                                                backoff.duration,
                                                &task_force_restart,
                                                "retryable",
                                            )
                                            .await
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                        }

                        _ = tokio::time::sleep(POLL_WATCHDOG_TIMEOUT) => {
                            tracing::warn!(
                                timeout_s = POLL_WATCHDOG_TIMEOUT.as_secs(),
                                "Node MQTT poll() watchdog timeout — forcing soft-restart"
                            );
                            poll_state_tx.set(SessionState::Reconnecting);
                            break;
                        }
                    }
                }

                // Soft-restart: recreate client + EventLoop. The
                // credentials are re-read from the live slot so a
                // reconnect after the enrollment reply presents the
                // node_token, never the (now consumed) enrollment token.
                poll_state_tx.set(SessionState::Connecting);
                let mut fresh_options = task_options.clone();
                if let Some(password) = task_credentials.lock().await.as_deref() {
                    fresh_options
                        .set_credentials(task_options.client_id().to_string(), password);
                }
                let (new_client, new_eventloop) = AsyncClient::new(fresh_options, 50);
                *task_shared_client.lock().await = new_client;
                eventloop = new_eventloop;
                tracing::info!("Node MQTT client soft-restarted with fresh EventLoop");
            }
        });

        Ok(Self {
            shared_client,
            force_restart,
            _eventloop_guard: Arc::new(EventLoopGuard { _task: poll_task }),
        })
    }

    /// Force a soft-restart of the MQTT event loop.
    ///
    /// Signals the background poll task to drop the current `EventLoop`
    /// and create a fresh `AsyncClient` + `EventLoop` pair — the same
    /// recovery path as the automatic watchdog soft-restart, but
    /// triggered externally (system sleep/wake detection, diagnostics).
    /// A pending notification is stored if nobody is waiting yet, so a
    /// call right after a connect is not lost.
    pub fn force_reconnect(&self) {
        self.force_restart.notify_one();
    }

    /// Publish a protobuf `DataEnvelope` payload.
    pub async fn publish_envelope(
        &self,
        topic: &str,
        envelope: &DataEnvelope,
        qos: QoS,
        retain: bool,
    ) -> Result<(), NodeMqttClientError> {
        let payload = prost::Message::encode_to_vec(envelope);
        self.publish_raw(topic, payload, qos, retain).await
    }

    /// Publish a raw binary payload.
    pub async fn publish_raw(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        retain: bool,
    ) -> Result<(), NodeMqttClientError> {
        self.shared_client
            .lock()
            .await
            .publish(topic, qos, retain, payload)
            .await
            .map_err(|e| NodeMqttClientError::Publish(format!("'{topic}': {e}")))?;
        Ok(())
    }

    /// Publish a text payload (e.g. status "online"/"offline").
    pub async fn publish_text(
        &self,
        topic: &str,
        payload: &str,
        qos: QoS,
        retain: bool,
    ) -> Result<(), NodeMqttClientError> {
        self.publish_raw(topic, payload.as_bytes().to_vec(), qos, retain)
            .await
    }

    /// Clone of the shared inner client handle (swapped on
    /// soft-restart). Used by the event dispatcher so replies always
    /// go through the CURRENT connection.
    pub fn shared_handle(&self) -> Arc<Mutex<AsyncClient>> {
        Arc::clone(&self.shared_client)
    }
}

#[cfg(test)]
mod tests {
    use super::interruptible_backoff;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn interruptible_backoff_returns_false_after_duration() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let interrupted =
            interruptible_backoff(Duration::from_millis(20), &notify, "test").await;
        assert!(!interrupted, "must not report interrupted when no signal arrived");
    }

    #[tokio::test]
    async fn interruptible_backoff_returns_true_on_notify() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify2 = Arc::clone(&notify);
        // Wake the helper while it is still inside the sleep future,
        // i.e. NOT during a poll() iteration. Regression for the
        // 2026-08 wake incident where bare `sleep(60s).await` sat
        // outside any `select!` and the notify could not break it.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            notify2.notify_one();
        });
        let interrupted = interruptible_backoff(
            Duration::from_secs(60),
            &notify,
            "test",
        )
        .await;
        assert!(
            interrupted,
            "must return true when notified before duration elapsed"
        );
    }
}
