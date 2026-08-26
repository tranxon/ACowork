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

/// Watchdog timeout for `eventloop.poll()`.
///
/// If poll() doesn't produce any event within this duration, the TCP
/// socket is presumed half-dead (most commonly after OS sleep/wake where
/// the kernel hasn't yet detected the broken connection). The poll task
/// breaks to the soft-restart path, which drops the old EventLoop and
/// creates a fresh TCP connection.
///
/// 5 s = 1 × keepalive interval. Normal connections produce at least
/// one PINGRESP within every keepalive interval (now 5 s — see
/// `set_keep_alive` below; rumqttc paces PINGREQ at keepalive/2 so a
/// healthy connection actually emits a PINGRESP every ~2.5 s), so 5 s
/// without any event strongly indicates a stuck socket.
///
/// History:
/// - Originally 90 s — left users staring at "Reconnecting..." for up
///   to 90 s after OS wake-from-sleep.
/// - Lowered to 20 s (4 × keepalive) for better wake-recovery UX.
/// - Lowered to 5 s (1 × keepalive) to further cut wake-recovery
///   latency. The cost is a higher chance of spurious soft-restarts on
///   genuinely busy event loops — acceptable because soft-restart on a
///   healthy connection only causes a ~100 ms input-disabled flash
///   (replacing one EventLoop with another on localhost is sub-10 ms;
///   the 14 SUBSCRIBE frames add another ~50 ms).
const POLL_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);

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
///
/// The inner `AsyncClient` is shared via `Arc<tokio::sync::Mutex<...>>`
/// so the event-loop task can swap it during a **soft-restart**
/// (drop old EventLoop + create fresh client/connection) without
/// invalidating the handle held by Tauri command callers.
pub struct DesktopMqttClient {
    shared_client: Arc<tokio::sync::Mutex<AsyncClient>>,
    /// Keep the event loop polling task alive.
    _eventloop_guard: Arc<EventLoopGuard>,
    /// Signal to force a soft-restart of the MQTT event loop.
    ///
    /// When `force_reconnect()` is called, the poll task receives this
    /// notification, breaks out of its inner polling loop, and recreates
    /// the `AsyncClient` + `EventLoop` from scratch – exactly the same
    /// path as the automatic 3-fatal-error recovery.
    force_restart: Arc<tokio::sync::Notify>,
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
    /// The client is attempting to establish a connection (initial
    /// connect or after a soft-restart / force-reconnect).  The
    /// frontend uses this to avoid flashing the disconnected banner
    /// while the client is actively trying to connect.
    Connecting,
    /// Broker confirmed the connection (CONNACK received).  Also fired
    /// after a successful automatic reconnect following a disconnect.
    Connected,
    /// Connection was lost and the client is retrying with backoff.
    /// `reason` explains why the connection was lost.
    Reconnecting { reason: String },
    /// Connection is no longer usable.  `reason` is a human-readable
    /// explanation suitable for surfacing in the UI status bar.
    /// Currently unused by the poll task (which emits `Reconnecting`
    /// instead), but retained for explicit disconnect scenarios.
    #[allow(dead_code)]
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

/// All topic filters that must be re-subscribed on every ConnAck
/// (initial connect + automatic reconnects). With
/// `clean_session = true` the broker drops all subscriptions on
/// disconnect, so **every** active subscription must be listed here.
///
/// ADR-039 P2: mirrors the Runtime's `run_bootstrap` subscribe steps.
///
/// CRITICAL: `messages/#` MUST be in this list. Without it, any
/// reconnect causes the Desktop to silently lose all session message
/// events (stream_delta, record_complete, session_state_changed,
/// done, stopped, context_usage, todo_updated) – the agent keeps
/// running but the frontend appears frozen / "connecting".
const ALL_TOPIC_FILTERS: &[(&str, MqttQoS)] = &[
    // ── Lifecycle topics ──
    ("acowork/agents/+/status", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/meta", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/config", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/sessions/created", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/sessions/deleted", MqttQoS::AtLeastOnce),
    // ADR-043: Retained per-session config + state. Runtime publishes
    // config (title/model/provider/workspace/reasoning_effort/temperature)
    // and state (status/message_count/tokens/ratio/context_usage) on
    // separate retained topics. Broker stores the last value, so
    // (re)connect immediately receives the current state.
    ("acowork/agents/+/sessions/+/config", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/sessions/+/state", MqttQoS::AtLeastOnce),
    ("acowork/sidecar/+/status", MqttQoS::AtLeastOnce),
    // ── Session message events ──
    // ADR-033: Subscribe to all session message topics (streaming chunks,
    // context_usage, session_state_changed, etc.) so the frontend receives
    // real-time session events across all sessions.
    // QoS 1 is mandatory: the Runtime publishes stream_delta and
    // record_complete at QoS 1; subscribing at QoS 0 would force the
    // broker to downgrade delivery, losing end-to-end ordering.
    ("acowork/agents/+/sessions/+/messages/#", MqttQoS::AtLeastOnce),
    // ── Debug protocol events (ADR-048 D6) ──
    // Runtime publishes DevMode debug events (onStep / onContextBuilt /
    // onStateChange) on `acowork/agents/glm-5.3_common/debug/events/{type}`.
    // Zero traffic outside DevMode, so subscribing unconditionally is
    // free. QoS 0 matches the publisher: events are fire-and-forget and
    // the DevMode panel re-syncs via `GET /api/debug/state` after a
    // reconnect.
    ("acowork/agents/+/debug/events/#", MqttQoS::AtMostOnce),
    // ── Workspace FS change events (ADR-058) ──
    // Runtime publishes aggregated workspace file changes on
    // `acowork/agents/{id}/workspaces/{wid}/fs-changed`. QoS 1 is
    // mandatory (same reason as messages/#: a lost event desyncs the
    // Desktop FileTree until the reconnect full-sync fallback fires).
    ("acowork/agents/+/workspaces/+/fs-changed", MqttQoS::AtLeastOnce),
];

/// Re-subscribe to ALL topics after a (re)connect.
///
/// Called from the event loop's `ConnAck` handler. With
/// `clean_session = true` the broker drops all subscriptions on
/// disconnect, so this is essential to avoid silently losing events
/// after a reconnect.
///
/// ADR-039 P2.
async fn resubscribe_all(client: &AsyncClient) {
    for (filter, qos) in ALL_TOPIC_FILTERS {
        if let Err(e) = client.subscribe(*filter, (*qos).into()).await {
            tracing::warn!(filter, error = %e, "Desktop MQTT resubscribe failed");
        }
    }
    tracing::info!("Desktop MQTT topics re-subscribed ({} filters)", ALL_TOPIC_FILTERS.len());
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
        credentials: Option<(&str, &str)>,
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
        // ADR-055 Phase 5a: authenticate against an auth-enabled broker
        // (`user:{id}:desktop:{pid}` CONNECT username + http_token).
        if let Some((username, password)) = credentials {
            options.set_credentials(username.to_string(), password.to_string());
        }
        // ADR-039: match the broker's `connection_timeout_ms` (5 s, see
        // `core/acowork-gateway/src/mqtt/broker.rs`). Setting the client
        // keepalive to 5 s means PINGREQs are emitted well inside the
        // broker's idle window. The previous 30 s value caused the broker
        // to disconnect the client after every OS sleep/wake (broker
        // timed out at 5 s while client still thought itself connected
        // until the next PINGREQ 30 s later).
        options.set_keep_alive(Duration::from_secs(5));
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

        let (client, eventloop) = AsyncClient::new(options.clone(), 100);
        let shared_client = Arc::new(tokio::sync::Mutex::new(client));

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

        // Clone shared state for the poll task.
        let task_shared_client = shared_client.clone();
        let task_options = options;
        let force_restart = Arc::new(tokio::sync::Notify::new());
        let task_force_restart = force_restart.clone();

        // Spawn the eventloop poller.
        //
        // Structure: outer loop (soft-restart) + inner loop (normal polling).
        //
        // - Inner loop polls the current `EventLoop`. On retryable errors
        //   (E1/E5) it applies exponential backoff and continues. On
        //   "fatal" errors (E2/E3/E4/E6) it uses a fixed 60s backoff.
        // - After 3 consecutive fatal errors the inner loop breaks to the
        //   outer loop, which drops the old `EventLoop` + `AsyncClient`
        //   and creates a fresh pair – a **soft-restart** that recovers
        //   from any internal state corruption without restarting the
        //   process.
        // - The new `AsyncClient` is swapped into `shared_client` so
        //   callers (Tauri commands) automatically use the new handle.
        let poll_task = tokio::spawn(async move {
            let mut eventloop = eventloop; // moved into task

            // Outer loop: each iteration is a fresh client + eventloop.
            let mut soft_restart_count: u32 = 0;
            loop {
                let mut consecutive_failures: u32 = 0;
                let mut fatal_streak: u32 = 0;

                // Inner loop: poll the current eventloop.
                //
                // We use `select!` to allow external force-restart
                // signals to interrupt polling at any time.
                loop {
                    tokio::select! {
                        biased; // favour the force-restart signal
                        _ = task_force_restart.notified() => {
                            tracing::info!(
                                "MQTT force-restart requested by user – \
                                 breaking to soft-restart path"
                            );
                            on_status(MqttStatus::Connecting);
                            poll_state_tx.set(SessionState::Connecting);
                            break; // break inner loop -> outer loop recreates
                        }
                        event_result = eventloop.poll() => {
                            match event_result {
                        Ok(Event::Incoming(rumqttc::Incoming::Publish(publish))) => {
                            on_msg(MqttMessage {
                                topic: publish.topic.clone(),
                                payload: publish.payload.to_vec(),
                            });
                        }
                        // Broker confirmed (re)connection.
                        Ok(Event::Incoming(rumqttc::Incoming::ConnAck(_))) => {
                            on_status(MqttStatus::Connected);
                            poll_state_tx.set(SessionState::Connected);
                            consecutive_failures = 0;
                            fatal_streak = 0;
                            // ADR-039 P2: re-subscribe ALL topics on every
                            // (re)connect. With clean_session = true the broker
                            // drops all subscriptions on disconnect, so without
                            // this the Desktop would silently lose agent
                            // status/meta/config AND session message events
                            // (stream_delta, record_complete, etc.) after a
                            // reconnect.
                            let poll_client = task_shared_client.lock().await.clone();
                            resubscribe_all(&poll_client).await;
                        }
                        // Broker initiated a disconnect (e.g. admin shutdown,
                        // client_id collision).  `rumqttc` will retry; we just
                        // surface the transition.
                        Ok(Event::Incoming(rumqttc::Incoming::Disconnect)) => {
                            on_status(MqttStatus::Reconnecting {
                                reason: "broker sent DISCONNECT".into(),
                            });
                            poll_state_tx.set(SessionState::Reconnecting);
                        }
                        Ok(_) => continue,
                        Err(e) => {
                            let desc = error_descriptor_from_rumqttc_025(&e);
                            let class = classify_err(&desc);

                            on_status(MqttStatus::Reconnecting {
                                reason: format!("eventloop error: {e}"),
                            });

                            if class.is_fatal() {
                                // E2/E3/E4/E6: in our local-no-auth-no-TLS
                                // architecture these are extremely unlikely,
                                // but if they do occur the EventLoop's
                                // internal state may be corrupt. Use a long
                                // 60s backoff; after 3 consecutive fatal
                                // errors, soft-restart (recreate client +
                                // EventLoop from scratch).
                                fatal_streak += 1;
                                tracing::error!(
                                    error = %e,
                                    err_class = class.label(),
                                    fatal_streak,
                                    "Desktop MQTT fatal error"
                                );
                                poll_state_tx.set(SessionState::Reconnecting);

                                if fatal_streak >= 3 {
                                    soft_restart_count += 1;
                                    tracing::warn!(
                                        soft_restart_count,
                                        "3 consecutive fatal errors – soft-restarting MQTT client"
                                    );
                                    break; // break inner loop -> outer loop recreates
                                }

                                tokio::time::sleep(Duration::from_secs(60)).await;
                            } else {
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
                        } // close match
                        } // close event_result arm

                        // Watchdog: if poll() hasn't produced any event in
                        // POLL_WATCHDOG_TIMEOUT, the TCP socket is likely
                        // half-dead (e.g. after OS sleep/wake where the
                        // kernel hasn't detected the broken connection).
                        // Break to soft-restart path to create a fresh
                        // connection.  The sleep future is dropped (reset)
                        // every time poll() returns, so this only fires
                        // when the connection is truly silent.
                        _ = tokio::time::sleep(POLL_WATCHDOG_TIMEOUT) => {
                            tracing::warn!(
                                timeout_s = POLL_WATCHDOG_TIMEOUT.as_secs(),
                                "MQTT poll() watchdog timeout - \
                                 forcing soft-restart (possible half-dead socket)"
                            );
                            on_status(MqttStatus::Reconnecting {
                                reason: "poll watchdog timeout (possible half-dead socket)".into(),
                            });
                            poll_state_tx.set(SessionState::Reconnecting);
                            break;
                        }
                    } // close select!
                } // close inner loop

                // ── Soft-restart: recreate client + EventLoop ──
                //
                // The old EventLoop is dropped (its TCP connection closes).
                // A fresh AsyncClient + EventLoop pair is created with the
                // same MqttOptions. The new AsyncClient is swapped into the
                // shared slot so Tauri command callers automatically use it.
                on_status(MqttStatus::Connecting);
                poll_state_tx.set(SessionState::Connecting);
                let (new_client, new_eventloop) =
                    AsyncClient::new(task_options.clone(), 100);
                *task_shared_client.lock().await = new_client;
                eventloop = new_eventloop;
                tracing::info!(
                    soft_restart_count,
                    "MQTT client soft-restarted with fresh EventLoop"
                );
                // Loop back to the inner loop with the fresh eventloop.
            }
        });

        Ok(Self {
            shared_client,
            _eventloop_guard: Arc::new(EventLoopGuard { _task: poll_task }),
            force_restart,
            state_tx,
        })
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
        let client = self.shared_client.lock().await.clone();
        client
            .subscribe(filter, qos.into())
            .await
            .map_err(|e| format!("subscribe '{}': {}", filter, e))
    }

    /// Subscribe to all agent lifecycle + session message topics.
    ///
    /// Called once during `connect_mqtt` to establish the initial
    /// subscriptions. On subsequent reconnects, the event loop's
    /// ConnAck handler calls `resubscribe_all` instead (same filter
    /// list, different call path).
    pub async fn subscribe_agent_lifecycle(&self) -> Result<(), String> {
        for (filter, qos) in ALL_TOPIC_FILTERS {
            self.subscribe(filter, *qos).await?;
        }
        tracing::info!("Subscribed to all agent topics ({} filters)", ALL_TOPIC_FILTERS.len());
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
        let client = self.shared_client.lock().await.clone();

        let filter = format!("acowork/agents/{}/sessions/{}/messages/#", agent_id, session_id);
        client
            .unsubscribe(&filter)
            .await
            .map_err(|e| format!("unsubscribe '{}': {}", filter, e))?;

        let filter = format!("acowork/agents/{}/sessions/{}/meta", agent_id, session_id);
        client
            .unsubscribe(&filter)
            .await
            .map_err(|e| format!("unsubscribe '{}': {}", filter, e))?;

        let filter = format!("acowork/agents/{}/sessions/{}/config", agent_id, session_id);
        client
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
        let client = self.shared_client.lock().await.clone();
        client
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
                    Some(mqtt_proto::control_command::Command::CancelTool(_)) => "cancel_tool",
                    Some(mqtt_proto::control_command::Command::ModelSwitch(_)) => "model_switch",
                    Some(mqtt_proto::control_command::Command::ReasoningEffort(_)) => "reasoning_effort",
                    Some(mqtt_proto::control_command::Command::WorkspaceSwitch(_)) => "workspace_switch",
                    Some(mqtt_proto::control_command::Command::CompactContext(_)) => "compact_context",
                    Some(mqtt_proto::control_command::Command::CompressAction(_)) => "compress_action",
                    Some(mqtt_proto::control_command::Command::Intent(_)) => "intent",
                    Some(mqtt_proto::control_command::Command::ActiveHeartbeat(_)) => "active_heartbeat",
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
        let client = self.shared_client.lock().await.clone();
        client
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| format!("publish '{}': {}", topic, e))
    }

    /// Get a clone of the inner AsyncClient.
    #[allow(dead_code)]
    pub async fn inner(&self) -> AsyncClient {
        self.shared_client.lock().await.clone()
    }

    /// Force a soft-restart of the MQTT event loop.
    ///
    /// Signals the background poll task to drop the current `EventLoop`
    /// and create a fresh `AsyncClient` + `EventLoop` pair. This is the
    /// same recovery path as the automatic 3-fatal-error soft-restart,
    /// but triggered externally by the user via the `force_reconnect_mqtt`
    /// Tauri command.
    ///
    /// Use this when the MQTT connection appears stuck (e.g. status shows
    /// "Reconnecting" for an extended period, or messages stop arriving
    /// despite the broker being healthy).
    pub fn force_reconnect(&self) {
        self.force_restart.notify_one();
    }

    /// Wait for the MQTT client to reach `Connected` state.
    ///
    /// Returns `true` if connected within the timeout, `false` otherwise.
    /// Call this after `force_reconnect()` to ensure the frontend loads
    /// with a live connection rather than racing against the reconnection.
    ///
    /// Uses a simple polling loop instead of `subscribe()` + `changed()`
    /// to avoid the race where `Connected` is set between the subscribe
    /// and the `changed()` wait, causing the latter to hang indefinitely.
    pub async fn wait_for_connected(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        // Fast path: already connected.
        if self.state_tx.current().is_connected() {
            return true;
        }
        // Poll every 100 ms until connected or timeout.
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.state_tx.current().is_connected() {
                return true;
            }
        }
        false
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
            None,
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
