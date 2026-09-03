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

use rumqttc::{AsyncClient, LastWill, QoS};
use tokio::sync::Mutex;

use acowork_core::mqtt_proto::DataEnvelope;
use acowork_mqtt_session::{
    ErrClass, MqttClient, MqttClientConfig, MqttClientError, MqttClientHandler,
};

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

impl From<MqttClientError> for NodeMqttClientError {
    fn from(e: MqttClientError) -> Self {
        match e {
            MqttClientError::Connection(s) => NodeMqttClientError::Connection(s),
            MqttClientError::Publish(s) => NodeMqttClientError::Publish(s),
            MqttClientError::Subscribe(s) => NodeMqttClientError::Subscribe(s),
        }
    }
}

/// Callback invoked for every incoming PUBLISH on the node's
/// subscriptions.
pub type NodeMqttMessageCallback = Arc<dyn Fn(String, Vec<u8>) + Send + Sync>;

/// Callback re-run on every ConnAck (idempotent bootstrap):
/// publish retained status/info + re-subscribe control filters.
pub type NodeMqttBootstrapCallback = Arc<dyn Fn(AsyncClient) + Send + Sync>;

/// The Node Agent's MQTT client.
///
/// Wraps the shared [`MqttClient`] (ADR-065): the entire poll loop —
/// error classification, backoff, soft-restart, watchdog and wake
/// recovery — lives in `acowork-mqtt-session`. This struct only adds
/// the Node's entity differences via [`NodeHandler`].
#[derive(Clone)]
pub struct NodeMqttClient {
    inner: MqttClient<NodeHandler>,
}

/// Node entity handler for the shared [`MqttClient`] (ADR-065 Step 4).
///
/// Implements the Node's per-process differences: message routing,
/// bootstrap on every ConnAck, retained NodeReady cleanup on
/// disconnect/error, and dynamic CONNECT credentials re-read on every
/// soft-restart (enrollment token → node_token swap, ADR-055 Phase 5a).
struct NodeHandler {
    /// CONNECT username (protocol §8.5: `node:{node_id}`).
    client_id: String,
    /// Live CONNECT password slot, re-read on every soft-restart.
    credentials: SharedNodeMqttCredentials,
    /// ADR-059 §7.2: retained NodeReady topic to clear on
    /// disconnect/error (empty payload = deleted). `None` for the
    /// one-shot enrollment/rename/leave paths that never announced it.
    ready_topic: Option<String>,
    /// Bootstrap callback re-run on every ConnAck.
    bootstrap: NodeMqttBootstrapCallback,
    /// Message callback for incoming publishes.
    message_callback: Option<NodeMqttMessageCallback>,
}

#[async_trait::async_trait]
impl MqttClientHandler for NodeHandler {
    async fn on_publish(&self, topic: &str, payload: &[u8]) {
        if let Some(ref cb) = self.message_callback {
            cb(topic.to_string(), payload.to_vec());
        }
    }

    async fn on_connack(&self, client: &AsyncClient) -> Result<(), String> {
        tracing::info!(
            node_client_id = %self.client_id,
            "Node MQTT (re)connected — running bootstrap"
        );
        (self.bootstrap)(client.clone());
        Ok(())
    }

    async fn on_disconnect(&self, client: &AsyncClient) {
        // ADR-059 §7.2: on any observed disconnect the poll task clears
        // the retained NodeReady snapshot (empty payload = deleted) so
        // the Gateway demotes this node back to not-ready even when the
        // LWT misses (e.g. broker restart without the LWT firing).
        if let Some(ref topic) = self.ready_topic {
            let _ = client
                .publish(topic, QoS::AtLeastOnce, true, Vec::new())
                .await;
        }
    }

    async fn on_error(&self, client: &AsyncClient, _class: ErrClass, _error: &str) {
        // ADR-059 §7.2: connection-level failure also clears the
        // retained NodeReady.
        if let Some(ref topic) = self.ready_topic {
            let _ = client
                .publish(topic, QoS::AtLeastOnce, true, Vec::new())
                .await;
        }
    }

    async fn on_soft_restart(&self) -> Option<(String, String)> {
        // Re-read the live credential slot so a reconnect after the
        // enrollment reply presents the node_token, never the (now
        // consumed) enrollment token.
        self.credentials
            .lock()
            .await
            .as_deref()
            .map(|p| (self.client_id.clone(), p.to_string()))
    }
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

        // LWT: broker publishes retained "offline" if the node dies
        // ungracefully (§6.2).
        let will = LastWill::new(&status_topic, "offline", QoS::AtLeastOnce, true);

        // Initial CONNECT credentials (username `node:{node_id}` +
        // current password from the live slot). The slot is re-read on
        // every soft-restart via `NodeHandler::on_soft_restart`.
        let initial_credentials = credentials
            .lock()
            .await
            .as_deref()
            .map(|p| (client_id.clone(), p.to_string()));

        let config = MqttClientConfig {
            client_id: client_id.clone(),
            host: host.to_string(),
            port,
            credentials: initial_credentials,
            last_will: Some(will),
            max_packet_size: acowork_core::defaults::GATEWAY_MQTT_MAX_PACKET_SIZE,
            queue_capacity: 50,
        };

        let handler = NodeHandler {
            client_id,
            credentials,
            ready_topic,
            bootstrap,
            message_callback,
        };

        let inner = MqttClient::connect(config, handler, None).await?;

        Ok(Self { inner })
    }

    /// Force a soft-restart of the MQTT event loop.
    ///
    /// Force a soft-restart of the MQTT event loop.
    ///
    /// Signals the background poll task to drop the current `EventLoop`
    /// and create a fresh `AsyncClient` + `EventLoop` pair — the same
    /// recovery path as the automatic watchdog soft-restart, but
    /// triggered externally (system sleep/wake detection, diagnostics).
    /// Uses the shared `ForceRestart` (AtomicBool + Notify): a pending
    /// request is stored if nobody is waiting yet, so a call right after
    /// a connect is not lost.
    pub fn force_reconnect(&self) {
        self.inner.force_reconnect();
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
        self.inner.publish_raw(topic, payload, qos, retain).await?;
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
        self.inner.shared_handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_mqtt_session::ForceRestart;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn interruptible_backoff_returns_false_after_duration() {
        let fr = Arc::new(ForceRestart::new());
        let interrupted = fr.interruptible_backoff(Duration::from_millis(20), "test").await;
        assert!(!interrupted, "must not report interrupted when no signal arrived");
    }

    #[tokio::test]
    async fn interruptible_backoff_returns_true_on_request() {
        let fr = Arc::new(ForceRestart::new());
        let fr2 = Arc::clone(&fr);
        // Wake the helper while it is still inside the sleep future,
        // i.e. NOT during a poll() iteration. Regression for the
        // 2026-08 wake incident where bare `sleep(60s).await` sat
        // outside any `select!` and the notify could not break it.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            fr2.request();
        });
        let interrupted = fr.interruptible_backoff(Duration::from_secs(60), "test").await;
        assert!(
            interrupted,
            "must return true when requested before duration elapsed"
        );
    }

    /// ADR-065 §7 acceptance #3 (Node path): the shared
    /// `From<&ConnectionError>` adapter must classify the wake-time
    /// `MqttState(StateError::Io(ECONNRESET))` shape as Transient, NOT
    /// fatal E4 ConfigError. This is the exact bug that made the Node
    /// wait out the 60 s fatal backoff after every OS wake (2026-09-03
    /// incident). The Node previously carried a private adapter that
    /// missed the `MqttState::Io` unwrap; Step 3 makes the shared
    /// adapter unconditionally available to the Node.
    #[test]
    fn shared_adapter_classifies_wake_reset_as_transient() {
        use acowork_mqtt_session::{classify, ErrorDescriptor, ErrClass};
        use std::io;

        let err = rumqttc::ConnectionError::MqttState(rumqttc::StateError::Io(
            io::Error::new(io::ErrorKind::ConnectionReset, "reset"),
        ));
        let desc = ErrorDescriptor::from(&err);
        assert_eq!(classify(&desc), ErrClass::Transient);
    }
}
