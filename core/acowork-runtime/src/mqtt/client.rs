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
    AgentConfig, AgentMeta, AskQuestionPayload, ChunkPayload, CompactingPayload,
    ContextUsagePayload, DataEnvelope, DonePayload, ErrorPayload,
    IterationLimitPausedPayload, NewDataAvailablePayload, RecordCompletePayload,
    SessionMessage, SessionStateChangedPayload, StoppedPayload, StreamDeltaPayload,
    StreamLine, TodoUpdatedPayload, ToolApprovalNeededPayload,
    data_envelope, session_message,
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

/// Configuration for `RuntimeMqttClient::connect`.
///
/// ADR-034 Phase 8: replaces 11 individual parameters.
pub struct MqttConnectConfig<'a> {
    pub host: &'a str,
    pub port: u16,
    pub agent_id: &'a str,
    pub agent_name: &'a str,
    pub agent_version: &'a str,
    pub avatar: Option<&'a str>,
    pub builtin_avatar: Option<&'a str>,
    pub config_json: &'a str,
    pub available_cache: SharedAvailableCache,
    pub control_tx: tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
}

/// Event payload for `publish_session_state_changed`.
///
/// ADR-034 Phase 8: replaces 8 individual `Option<&str>` arguments.
pub struct SessionStateChangeEvent<'a> {
    pub session_id: &'a str,
    pub status_json: &'a str,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub workspace_id: Option<&'a str>,
    pub ratio: Option<f64>,
    pub reasoning_effort: Option<&'a str>,
    pub temperature: Option<f32>,
    pub context_usage_json: Option<&'a str>,
}

/// Event payload for `publish_tool_approval_needed`.
///
/// ADR-034 Phase 8: replaces 8 individual arguments.
pub struct ToolApprovalNeededEvent<'a> {
    pub session_id: &'a str,
    pub request_id: &'a str,
    pub tool_name: &'a str,
    pub action: &'a str,
    pub risk_level: &'a str,
    pub reason: &'a str,
    pub tool_call_id: &'a str,
    pub approval_timeout_secs: u64,
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
    /// ADR-034 Phase 8: takes a single `MqttConnectConfig` struct.
    pub async fn connect(
        cfg: MqttConnectConfig<'_>,
    ) -> Result<Self, RuntimeMqttClientError> {
        let client_id = format!("agent:{}", cfg.agent_id);
        let status_topic = format!("acowork/agents/{}/status", cfg.agent_id);
        let meta_topic = format!("acowork/agents/{}/meta", cfg.agent_id);
        let config_topic = format!("acowork/agents/{}/config", cfg.agent_id);
        let control_filter = format!("acowork/agents/{}/sessions/control/#", cfg.agent_id);

        // Configure MQTT options with Last Will
        let mut options = MqttOptions::new(client_id.clone(), cfg.host, cfg.port);
        options.set_keep_alive(Duration::from_secs(30));
        options.set_clean_session(true);

        // Last Will: if Runtime crashes/disconnects, broker publishes "offline" retained
        let will = LastWill::new(&status_topic, "offline", QoS::AtLeastOnce, true);
        options.set_last_will(will);

        let (client, mut eventloop) = AsyncClient::new(options, 100);

        // Spawn event loop poller that:
        // - Updates available_cache from acowork/global/# messages
        // - Forwards control commands to control_tx
        let poll_agent_id = cfg.agent_id.to_string();
        let poll_cache = cfg.available_cache.clone();
        let poll_control_tx = cfg.control_tx.clone();
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
            host = %cfg.host,
            port = %cfg.port,
            client_id = %client_id,
            agent_id = %cfg.agent_id,
            "Runtime MQTT client connected to broker"
        );

        let mqtt_client = Self {
            client: client.clone(),
            agent_id: cfg.agent_id.to_string(),
            _eventloop_guard: Arc::new(EventLoopGuard { _task: poll_task }),
        };

        // ── Step 2: PUBLISH status = "online" (Retained) ──
        mqtt_client
            .publish_status(true)
            .await?;

        // ── Step 3: PUBLISH meta (Retained) ──
        let meta = AgentMeta {
            agent_id: cfg.agent_id.to_string(),
            name: cfg.agent_name.to_string(),
            version: cfg.agent_version.to_string(),
            avatar: cfg.avatar.unwrap_or("").to_string(),
            builtin_avatar: cfg.builtin_avatar.unwrap_or("").to_string(),
        };
        mqtt_client
            .publish_envelope(&meta_topic, &DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AgentMeta(meta)),
            }, MqttQoS::AtLeastOnce, true)
            .await?;

        // ── Step 4: PUBLISH config (Retained) ──
        let config = AgentConfig {
            agent_id: cfg.agent_id.to_string(),
            config_json: cfg.config_json.to_string(),
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

        tracing::info!(agent_id = %cfg.agent_id, "Runtime MQTT client: published status/meta/config + subscribed to global + control");

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
        retain: bool,
    ) -> Result<(), RuntimeMqttClientError> {
        self.client
            .publish(topic, qos.into(), retain, payload)
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
/// existing gRPC channel. All payloads use `DataEnvelope` protobuf encoding
/// per `docs/zh/protocols/mqtt.md` §4.
///
/// NOTE: Wired into the session loop via MQTT chunk relay task
/// in `subsystems.rs`. See P0-2 in `docs/review/zh/28-adr-033-mqtt-refactor-code-review.md`.
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

    /// Return the agent_id this publisher is bound to.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Publish a session lifecycle envelope (created/deleted) to the
    /// `acowork/agents/{id}/sessions/{event_type}` topic with QoS 1.
    pub async fn publish_lifecycle(
        &self,
        event_type: &str,
        envelope: &DataEnvelope,
    ) -> Result<(), RuntimeMqttClientError> {
        let topic = format!("acowork/agents/{}/sessions/{}", self.agent_id, event_type);
        let bytes = prost::Message::encode_to_vec(envelope);
        self.client
            .publish(topic, QoS::AtLeastOnce, false, bytes)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("lifecycle: {}", e)))
    }

    /// Publish a session event envelope to the broker at QoS 0 (default for
    /// `messages/*` streaming events per ADR-035 D1 / `mqtt.md` §8.3).
    async fn publish(&self, session_id: &str, event_type: &str, payload: &[u8]) {
        self.publish_with_qos(session_id, event_type, payload, QoS::AtMostOnce).await;
    }

    /// Publish a session event envelope at the given QoS.
    ///
    /// ADR-035 O2: `record_complete` is published at QoS 1 (AtLeastOnce)
    /// because it is the authoritative terminal event — losing it leaves
    /// the message stuck in the streaming state with no fallback. All other
    /// `messages/*` events stay at QoS 0 (a lost `stream_delta` frame is
    /// covered by the next frame or the final `record_complete`).
    async fn publish_with_qos(
        &self,
        session_id: &str,
        event_type: &str,
        payload: &[u8],
        qos: QoS,
    ) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/messages/{}",
            self.agent_id, session_id, event_type
        );
        if let Err(e) = self
            .client
            .publish(topic, qos, false, payload)
            .await
        {
            tracing::warn!(error = %e, session_id, event_type, "Failed to publish MQTT session event");
        }
    }

    /// Publish a chunk event via MQTT (QoS 0, protobuf DataEnvelope).
    #[allow(dead_code)]
    pub(crate) fn publish_chunk(&self, session_id: &str, message_id: &str, delta: &str) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let d = delta.to_string();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::Chunk(ChunkPayload {
                    message_id: mid,
                    delta: d,
                })),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "chunk", &bytes).await;
        });
    }

    /// Publish a done event via MQTT (QoS 0, protobuf DataEnvelope).
    pub(crate) fn publish_done(&self, session_id: &str, message_id: &str) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::Done(DonePayload {
                    message_id: mid,
                })),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "done", &bytes).await;
        });
    }

    // ADR-035 C1: publish_tool_call / publish_tool_result removed — tool_call
    // and tool_result records are now delivered via the unified
    // `record_complete` event (see publish_record_complete below), which
    // carries role / message_id / content and applies D9.2 truncation for
    // tool_result at the publish layer.

    /// Publish an error event via MQTT (QoS 1, protobuf DataEnvelope).
    pub(crate) fn publish_error(&self, session_id: &str, message_id: &str, error_msg: &str) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let err = error_msg.to_string();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::Error(ErrorPayload {
                    message_id: mid,
                    error: err,
                })),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "error", &bytes).await;
        });
    }

    /// Publish a stopped event via MQTT (QoS 1, protobuf DataEnvelope).
    pub(crate) fn publish_stopped(&self, session_id: &str, message_id: &str) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::Stopped(StoppedPayload {
                    message_id: mid,
                })),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "stopped", &bytes).await;
        });
    }

    /// Publish a session_state_changed event via MQTT (QoS 1).
    ///
    /// ADR-034 Phase 8: takes a single `SessionStateChangeEvent` struct.
    pub(crate) fn publish_session_state_changed(
        &self,
        ev: SessionStateChangeEvent<'_>,
    ) {
        let publisher = self.clone();
        let sid = ev.session_id.to_string();
        let sjson = ev.status_json.to_string();
        let m = ev.model.map(|s| s.to_string()).unwrap_or_default();
        let p = ev.provider.map(|s| s.to_string()).unwrap_or_default();
        let w = ev.workspace_id.map(|s| s.to_string()).unwrap_or_default();
        let r = ev.ratio.unwrap_or(0.0);
        let re = ev.reasoning_effort.map(|s| s.to_string()).unwrap_or_default();
        let t = ev.temperature.unwrap_or(0.0);
        let cu = ev.context_usage_json.map(|s| s.to_string()).unwrap_or_default();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::SessionStateChanged(
                    SessionStateChangedPayload {
                        session_id: sid.clone(),
                        status_json: sjson,
                        model: m,
                        provider: p,
                        workspace_id: w,
                        ratio: r,
                        reasoning_effort: re,
                        temperature: t,
                        context_usage_json: cu,
                    },
                )),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "session_state_changed", &bytes).await;
        });
    }

    /// Publish a context_usage event via MQTT (QoS 0).
    ///
    /// Carries the full [`acowork_core::protocol::ContextUsageInfo`] serialised
    /// as JSON in `context_usage_json`. The Desktop Rust subscriber expands it
    /// into the same shape it emits for `SessionStateChanged`, so the frontend
    /// can render the StatusBar from either source without special-casing.
    pub(crate) fn publish_context_usage(
        &self,
        session_id: &str,
        ctx_info: &acowork_core::protocol::ContextUsageInfo,
    ) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        // Backwards-compat: keep the legacy individual token fields populated
        // for any in-flight Desktop subscriber that hasn't switched to
        // `context_usage_json` yet.
        let input_tokens = ctx_info.input_tokens;
        let output_tokens = ctx_info.output_tokens;
        let total_input_tokens = ctx_info.total_input_tokens.unwrap_or(0);
        let total_output_tokens = ctx_info.total_output_tokens.unwrap_or(0);
        let cu_json = serde_json::to_string(ctx_info).unwrap_or_default();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::ContextUsage(ContextUsagePayload {
                    session_id: sid.clone(),
                    input_tokens,
                    output_tokens,
                    total_input_tokens,
                    total_output_tokens,
                    context_usage_json: cu_json,
                })),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "context_usage", &bytes).await;
        });
    }

    /// Publish a compacting_started/compacting_ended event via MQTT (QoS 1).
    pub(crate) fn publish_compacting(&self, session_id: &str, started: bool) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let event_type = if started { "compacting_started" } else { "compacting_ended" };
        tokio::spawn(async move {
            let payload = CompactingPayload {
                session_id: sid.clone(),
            };
            let event = if started {
                session_message::Event::CompactingStarted(payload)
            } else {
                session_message::Event::CompactingEnded(payload)
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(SessionMessage {
                    agent_id,
                    session_id: sid.clone(),
                    event: Some(event),
                })),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, event_type, &bytes).await;
        });
    }

    /// Publish an ask_question event via MQTT (QoS 1).
    pub(crate) fn publish_ask_question(&self, session_id: &str, message_id: &str, question_json: &str) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let qj = question_json.to_string();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::AskQuestion(AskQuestionPayload {
                    message_id: mid,
                    question_json: qj,
                })),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "ask_question", &bytes).await;
        });
    }

    /// Publish a todo_updated event via MQTT (QoS 0).
    pub(crate) fn publish_todo_updated(&self, session_id: &str, todos_json: &str) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let tj = todos_json.to_string();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::TodoUpdated(TodoUpdatedPayload {
                    todos_json: tj,
                })),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "todo_updated", &bytes).await;
        });
    }

    /// Publish an iteration_limit_paused event via MQTT (QoS 1).
    pub(crate) fn publish_iteration_limit_paused(
        &self,
        session_id: &str,
        iteration: u32,
        max_iterations: u32,
    ) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::IterationLimitPaused(
                    IterationLimitPausedPayload {
                        session_id: sid.clone(),
                        iteration,
                        max_iterations,
                    },
                )),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "iteration_limit_paused", &bytes).await;
        });
    }

    /// Publish a tool_approval_needed event via MQTT (QoS 1).
    ///
    /// ADR-034 Phase 8: takes a single `ToolApprovalNeededEvent` struct.
    pub(crate) fn publish_tool_approval_needed(
        &self,
        ev: ToolApprovalNeededEvent<'_>,
    ) {
        let publisher = self.clone();
        let sid = ev.session_id.to_string();
        let rid = ev.request_id.to_string();
        let tn = ev.tool_name.to_string();
        let act = ev.action.to_string();
        let rl = ev.risk_level.to_string();
        let rsn = ev.reason.to_string();
        let tcid = ev.tool_call_id.to_string();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::ToolApprovalNeeded(
                    ToolApprovalNeededPayload {
                        session_id: sid.clone(),
                        request_id: rid,
                        tool_name: tn,
                        action: act,
                        risk_level: rl,
                        reason: rsn,
                        tool_call_id: tcid,
                        approval_timeout_secs: ev.approval_timeout_secs,
                    },
                )),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "tool_approval_needed", &bytes).await;
        });
    }

    /// Publish a new_data_available event via MQTT (QoS 0).
    pub(crate) fn publish_new_data_available(
        &self,
        session_id: &str,
        interval_ms: u32,
        title: Option<&str>,
    ) {
        let publisher = self.clone();
        let sid = session_id.to_string();
        let t = title.map(|s| s.to_string()).unwrap_or_default();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::NewDataAvailable(
                    NewDataAvailablePayload {
                        session_id: sid.clone(),
                        interval_ms,
                        title: t,
                    },
                )),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "new_data_available", &bytes).await;
        });
    }

    /// Publish a `stream_delta` event via MQTT (QoS 0, ADR-035).
    ///
    /// Carries the new COMPLETE streaming lines since the last push. Each
    /// `StreamLine.content` is a whole line — never a partial line or token.
    pub(crate) fn publish_stream_delta(
        &self,
        session_id: &str,
        lines: &[StreamLine],
    ) {
        if lines.is_empty() {
            return;
        }
        let publisher = self.clone();
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let lines = lines.to_vec();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::StreamDelta(StreamDeltaPayload {
                    session_id: sid.clone(),
                    lines,
                })),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            publisher.publish(&sid, "stream_delta", &bytes).await;
        });
    }

    /// Publish a `record_complete` event via MQTT (QoS 1, ADR-035 C1/O2).
    ///
    /// Carries the COMPLETE finalized record. The frontend freezes the
    /// active stream buffer into `messages[]` on receipt and clears
    /// `activeStream`. Published at QoS 1 (AtLeastOnce) because this is
    /// the authoritative terminal event — losing it leaves the message
    /// stuck in the streaming state (ADR-035 O2).
    ///
    /// ADR-035 D9.2: `tool_result` content is truncated to the first 5
    /// lines before publishing. The full content stays in JSONL for LLM
    /// context. No exception — the frontend never receives full tool_result.
    pub(crate) fn publish_record_complete(
        &self,
        session_id: &str,
        role: &str,
        message_id: &str,
        content: &str,
    ) {
        // ADR-035 D9.2: truncate tool_result to first 5 lines for display.
        // Full content stays in JSONL for LLM context. No exception.
        let final_content = if role == "tool_result" {
            truncate_tool_result_lines(content)
        } else {
            content.to_string()
        };
        let publisher = self.clone();
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let role = role.to_string();
        let mid = message_id.to_string();
        tokio::spawn(async move {
            let event = SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(session_message::Event::RecordComplete(RecordCompletePayload {
                    session_id: sid.clone(),
                    role,
                    message_id: mid,
                    content: final_content,
                })),
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::SessionMessage(event)),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            // ADR-035 O2: QoS 1 — record_complete is the authoritative
            // terminal event; losing it leaves the message stuck.
            publisher.publish_with_qos(&sid, "record_complete", &bytes, QoS::AtLeastOnce).await;
        });
    }
}

/// ADR-035 D9.2: truncate tool_result to first 5 lines for frontend display.
/// Full content stays in JSONL. No exception.
fn truncate_tool_result_lines(result_json: &str) -> String {
    let lines: Vec<&str> = result_json.lines().collect();
    if lines.len() <= 5 {
        return result_json.to_string();
    }
    let mut truncated = lines.into_iter().take(5).collect::<Vec<_>>().join("\n");
    truncated.push_str("\n...(truncated)");
    truncated
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
            MqttConnectConfig {
                host: "127.0.0.1",
                port,
                agent_id: "com.test.agent",
                agent_name: "Test Agent",
                agent_version: "1.0.0",
                avatar: None,
                builtin_avatar: None,
                config_json: "{}",
                available_cache: cache,
                control_tx,
            },
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
