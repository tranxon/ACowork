//! Embedded rumqttd MQTT broker (ADR-033 Phase 1).
//!
//! The Gateway embeds a rumqttd broker in-process, listening on
//! `127.0.0.1:19875` (MQTT 3.1.1 / TCP). All clients (Runtime, Desktop,
//! and the Gateway's own publisher) connect to this broker.
//!
//! See `docs/zh/protocols/mqtt.md` §1–§2 for the protocol conventions
//! and architecture overview.
//!
//! ADR-055 Phase 5a adds CONNECT-layer authentication via rumqttd's
//! `ConnectionSettings::set_auth_handler`. The policy is a pure
//! function ([`check_connect_auth`]) so it can be unit-tested without
//! a running broker; the broker thread only adapts it to rumqttd's
//! async handler shape.

use std::net::SocketAddr;

use rumqttd::{Broker, Config};

use acowork_core::defaults;
use acowork_core::node::NODE_CLIENT_ID_PREFIX;

use super::enrollment::{
    constant_time_eq, EnrollmentTokenStore, NodeTokenStore, SharedEnrollmentTokenStore,
    SharedNodeTokenStore, TokenValidation,
};

/// Error type for MQTT broker operations.
#[derive(Debug, thiserror::Error)]
pub enum MqttBrokerError {
    #[error("MQTT broker failed to start: {0}")]
    Start(String),
    #[error("MQTT broker config error: {0}")]
    Config(String),
}

/// ADR-055 Phase 5a: broker CONNECT authentication inputs.
///
/// Cloned into the broker thread; every credential check is delegated
/// to the pure decision function [`check_connect_auth`]. The stores
/// are shared (std Mutex) with the MQTT dispatch and the HTTP
/// handlers, so tokens issued at runtime are immediately honored.
#[derive(Clone)]
pub struct BrokerAuth {
    /// Master switch — `mqtt.auth_enabled` config.
    pub auth_enabled: bool,
    /// One-time enrollment tokens (first node connect).
    pub enrollment_tokens: SharedEnrollmentTokenStore,
    /// Long-lived per-node tokens (node reconnect + `agent:{id}`).
    pub node_tokens: SharedNodeTokenStore,
    /// Internal publisher credential (generated at Gateway startup).
    pub publisher_token: String,
    /// HTTP bearer token (HttpAuth) — Desktop MQTT credential.
    pub http_token: Option<String>,
}

/// Reference snapshot of the auth decision inputs — lets
/// [`check_connect_auth`] stay a pure function over locked store
/// guards.
pub struct ConnectAuthContext<'a> {
    pub auth_enabled: bool,
    pub enrollment_tokens: &'a EnrollmentTokenStore,
    pub node_tokens: &'a NodeTokenStore,
    pub publisher_token: Option<&'a str>,
    pub http_token: Option<&'a str>,
}

/// Pure CONNECT authentication decision (ADR-055 §6.8, Phase 5a).
///
/// client_id conventions (protocol docs §8.5):
/// - `node:{node_id}` — Node Agent. Password = the node's long-lived
///   token, or a valid unconsumed enrollment token (first connect).
/// - `agent:{agent_id}` — Runtime. Password = ANY registered node
///   token (Phase 5a simplification: agent→node ownership is NOT
///   verified — the Node only injects its own token when spawning
///   Runtimes; strict per-agent ACLs are deferred to Phase 5b).
/// - `gateway:publisher` — Gateway's internal publisher, password =
///   the startup-generated publisher token.
/// - `user:{name}:desktop:{id}` — Desktop, password = the HTTP bearer
///   token (HttpAuth; available when `http.auth_enabled` is on).
///
/// The `username` field is informational at this tier (identity is
/// keyed by client_id); it is not yet cross-checked against the
/// client_id. Everything else is rejected when auth is enabled; when
/// `auth_enabled` is false every connection passes (default).
pub fn check_connect_auth(
    client_id: &str,
    _username: &str,
    password: &str,
    ctx: &ConnectAuthContext<'_>,
) -> bool {
    if !ctx.auth_enabled {
        return true;
    }
    if let Some(node_id) = client_id.strip_prefix(NODE_CLIENT_ID_PREFIX) {
        if ctx.node_tokens.node_token_matches(node_id, password) {
            return true;
        }
        // First-connect path: a valid, unconsumed enrollment token
        // (auth_enabled implies the enrollment store was loaded).
        return ctx.enrollment_tokens.validate_token(password) == TokenValidation::Valid;
    }
    if client_id.starts_with("agent:") {
        return ctx.node_tokens.any_token_matches(password);
    }
    if client_id == "gateway:publisher" {
        return match ctx.publisher_token {
            Some(expected) => constant_time_eq(expected.as_bytes(), password.as_bytes()),
            None => false,
        };
    }
    if let Some(rest) = client_id.strip_prefix("user:")
        && rest.contains(":desktop:")
    {
        return match ctx.http_token {
            Some(expected) => constant_time_eq(expected.as_bytes(), password.as_bytes()),
            None => false,
        };
    }
    false
}

/// Handle to the running MQTT broker.
///
/// The broker lives on a dedicated OS thread (see [`start_broker`]);
/// this handle only carries the shutdown channel and the listen
/// address.
///
/// Production callers never need to shut the broker down — it lives
/// for the lifetime of the Gateway process. The `shutdown_tx` exists
/// only so the debug HTTP endpoints (`POST /api/debug/mqtt/*`) can
/// request a graceful exit without restarting the whole process.
pub struct MqttBrokerHandle {
    /// Channel to signal the broker thread to exit.
    shutdown_tx: Option<std::sync::mpsc::Sender<()>>,
    /// The address the broker is listening on.
    pub listen_addr: SocketAddr,
}

/// Build the rumqttd `Config` from a TOML template (the library's intended API).
///
/// Programmatic struct construction is fragile — rumqttd expects config via
/// deserialization and fields like `ConsoleSettings` have non-trivial defaults.
pub fn build_broker_config(host: &str, port: u16) -> Config {
    let config_toml = format!(
        r#"
id = 0

[router]
max_connections = {max_conn}
max_segment_size = {max_pkt}
max_segment_count = 10
max_read_len = 1048576
max_outgoing_packet_count = 1000
instant_ack = true

[v4.acowork]
name = "acowork"
listen = "{host}:{port}"
next_connection_delay_ms = 1

[v4.acowork.connections]
connection_timeout_ms = 5000
max_payload_size = {max_pkt}
max_inflight_count = 100
max_inflight_size = 1048576
throttle_delay_ms = 0
dynamic_filters = false

[console]
listen = "127.0.0.1:0"
"#,
        host = host,
        port = port,
        max_conn = defaults::GATEWAY_MQTT_MAX_CONNECTIONS,
        max_pkt = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE,
    );

    toml::from_str(&config_toml)
        .unwrap_or_else(|e| panic!("BUG: invalid MQTT broker config template: {}", e))
}

/// Start the embedded MQTT broker (non-blocking, single entry point).
///
/// The broker runs on a dedicated OS thread named `mqtt-broker`.
///
/// # Why a background thread?
///
/// rumqttd 0.20's `Broker::start()` never returns — it joins the
/// server threads, whose accept loops run forever. Calling it directly
/// would block the calling thread indefinitely, so this function runs
/// the broker on a dedicated OS thread and returns after a bounded
/// startup confirmation. The permanent block inside `Broker::start()`
/// is what keeps the broker alive on that thread's stack.
///
/// Uses a short timeout (500 ms) to confirm startup, but does NOT block
/// indefinitely — if the broker doesn't respond in time, the function
/// still returns `Ok` so the Gateway can continue starting.
///
/// This is the ONLY public entry point for starting the broker; there
/// is intentionally no "direct" (blocking) variant.
pub fn start_broker(host: &str, port: u16) -> Result<MqttBrokerHandle, MqttBrokerError> {
    start_broker_with_auth(host, port, None)
}

/// Start the embedded MQTT broker with an optional CONNECT auth
/// handler (ADR-055 Phase 5a). `Some(auth)` wires
/// [`check_connect_auth`] into rumqttd's `set_auth_handler`;
/// `None` keeps the historical permissive behavior (every connection
/// passes).
pub fn start_broker_with_auth(
    host: &str,
    port: u16,
    auth: Option<BrokerAuth>,
) -> Result<MqttBrokerHandle, MqttBrokerError> {
    let listen_addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| MqttBrokerError::Config(format!(
            "Invalid listen address '{}:{}': {}",
            host, port, e
        )))?;

    let (tx, rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    let h = host.to_string();

    std::thread::Builder::new()
        .name("mqtt-broker".into())
        .spawn(move || {
            let mut config = build_broker_config(&h, port);
            if let Some(auth) = auth {
                let v4 = config.v4.as_mut().expect("v4 servers configured");
                let server = v4.get_mut("acowork").expect("server 'acowork' configured");
                tracing::info!(
                    auth_enabled = auth.auth_enabled,
                    "MQTT broker CONNECT auth handler installed (ADR-055 Phase 5a)"
                );
                server.connections.set_auth_handler(
                    move |client_id, username, password| {
                        // std::sync::Mutex poisoning is unrecoverable
                        // here — any poisoned lock means a panicked
                        // holder, so fall back to the inner guard and
                        // log via the normal decision path.
                        std::future::ready({
                            let enrollment = auth
                                .enrollment_tokens
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let node_tokens = auth
                                .node_tokens
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let ctx = ConnectAuthContext {
                                auth_enabled: auth.auth_enabled,
                                enrollment_tokens: &enrollment,
                                node_tokens: &node_tokens,
                                publisher_token: Some(auth.publisher_token.as_str()),
                                http_token: auth.http_token.as_deref(),
                            };
                            check_connect_auth(&client_id, &username, &password, &ctx)
                        })
                    },
                );
            }
            let mut broker = Broker::new(config);

            tracing::info!(
                addr = %listen_addr,
                port,
                max_connections = defaults::GATEWAY_MQTT_MAX_CONNECTIONS,
                max_packet_size = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE,
                "Starting embedded MQTT broker (rumqttd)"
            );

            // If startup fails (e.g. port already taken), signal the
            // parent and exit immediately.
            if let Err(e) = broker.start() {
                let _ = tx.send(Err(MqttBrokerError::Start(format!(
                    "rumqttd broker start failed: {e}"
                ))));
                return;
            }

            // NOTE: `Broker::start()` normally blocks here forever (it
            // joins the server threads) — that is what keeps the broker
            // alive on this thread's stack. The `Ok(())` confirmation
            // is only reachable in the abnormal case where the server
            // threads exited (e.g. a bind failure raced with connect).
            let _ = tx.send(Ok(()));

            // Park until a shutdown signal arrives.
            //
            // `park_timeout` (vs. plain `park`) is used so the thread
            // can react to a shutdown request from the debug HTTP
            // endpoints without an explicit `Thread::unpark` — which
            // would require exposing the parked Thread handle in
            // `MqttBrokerHandle`, a strictly debug-only concern. The
            // 200 ms timeout is a deliberate trade-off: it costs ~5
            // wakeups/sec/idle thread, but lets the debug endpoint shut
            // the broker down cleanly.
            loop {
                std::thread::park_timeout(std::time::Duration::from_millis(200));
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
            }
            // Broker drops here, closing all TCP connections.
            drop(broker);
            tracing::info!("MQTT broker thread exiting (broker dropped)");
        })
        .map_err(|e| MqttBrokerError::Start(format!("spawn thread: {e}")))?;

    // Don't block indefinitely. Give the broker 500 ms to start, then proceed.
    match rx.recv_timeout(std::time::Duration::from_millis(500)) {
        Ok(Ok(())) => {
            tracing::info!(addr = %listen_addr, "MQTT broker confirmed started");
        }
        Ok(Err(e)) => {
            return Err(e);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // Broker thread is still starting up — this is fine.
            // The Gateway continues booting; MQTT may not be available
            // for the first few moments but will be soon.
            tracing::warn!(
                addr = %listen_addr,
                "MQTT broker startup not confirmed within 500 ms; proceeding (broker may still be initializing)"
            );
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            // Thread panicked before sending
            return Err(MqttBrokerError::Start(
                "broker thread panicked during startup".to_string(),
            ));
        }
    }

    Ok(MqttBrokerHandle {
        shutdown_tx: Some(shutdown_tx),
        listen_addr,
    })
}

/// Gracefully shut down the broker.
///
/// Sends a signal to the broker thread (if running in-thread mode) to
/// unpark and exit, which drops the `Broker` and closes all TCP
/// connections. After shutdown, the broker is permanently stopped —
/// restarting requires creating a new handle via `start_broker`.
impl MqttBrokerHandle {
    /// Signal the broker thread to exit, then return immediately.
    ///
    /// This **does not wait** for the broker thread to actually exit.
    /// Note that rumqttd's `Broker::start()` blocks the broker thread
    /// forever (it joins the server threads), so in practice the TCP
    /// listener stays open until process exit; the signal only ends
    /// the broker thread's park loop. Production callers should treat
    /// the broker as process-lifetime state and never shut it down.
    pub fn signal_shutdown(&mut self) -> Result<(), String> {
        let tx = self
            .shutdown_tx
            .take()
            .ok_or_else(|| "broker already shut down".to_string())?;
        tx.send(()).map_err(|_| "broker thread already exited".to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_broker_config_defaults() {
        let config = build_broker_config(
            defaults::GATEWAY_MQTT_HOST,
            defaults::GATEWAY_MQTT_PORT,
        );

        assert_eq!(config.router.max_connections, 100);
        assert_eq!(
            config.router.max_segment_size,
            10 * 1024 * 1024,
            "segment size should cover 10 MB packets"
        );

        let v4 = config.v4.as_ref().expect("v4 servers must be configured");
        let server = v4.get("acowork").expect("server 'acowork' must exist");
        assert_eq!(server.listen.port(), defaults::GATEWAY_MQTT_PORT);
        assert_eq!(
            server.connections.max_payload_size,
            10 * 1024 * 1024
        );
        assert!(server.tls.is_none(), "TLS should be disabled for localhost");
    }

    #[test]
    fn test_build_broker_config_custom_host_port() {
        let config = build_broker_config("127.0.0.1", 32100);
        let v4 = config.v4.as_ref().expect("v4 servers must be configured");
        let server = v4.get("acowork").unwrap();
        assert_eq!(server.listen.port(), 32100);
    }

    use std::path::PathBuf;

    /// Build an auth context with in-memory (unpersisted) stores.
    fn test_ctx<'a>(
        auth_enabled: bool,
        enrollment: &'a EnrollmentTokenStore,
        node_tokens: &'a NodeTokenStore,
        publisher_token: Option<&'a str>,
        http_token: Option<&'a str>,
    ) -> ConnectAuthContext<'a> {
        ConnectAuthContext {
            auth_enabled,
            enrollment_tokens: enrollment,
            node_tokens,
            publisher_token,
            http_token,
        }
    }

    fn empty_enrollment() -> EnrollmentTokenStore {
        EnrollmentTokenStore::load(&PathBuf::from("/nonexistent"))
    }

    fn empty_node_tokens() -> NodeTokenStore {
        NodeTokenStore::load(&PathBuf::from("/nonexistent"))
    }

    #[test]
    fn auth_disabled_allows_everything() {
        let enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let ctx = test_ctx(false, &enrollment, &node_tokens, Some("p"), Some("h"));
        // Unknown client ids, empty passwords — all pass when disabled.
        assert!(check_connect_auth("node:local", "", "", &ctx));
        assert!(check_connect_auth("anything:else", "", "", &ctx));
        assert!(check_connect_auth("", "", "", &ctx));
    }

    #[test]
    fn node_accepts_node_token() {
        let enrollment = empty_enrollment();
        let mut node_tokens = empty_node_tokens();
        let token = node_tokens.upsert("gpu-1", "uid-1");
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        assert!(check_connect_auth("node:gpu-1", "", &token, &ctx));
        assert!(!check_connect_auth("node:gpu-1", "", "wrong", &ctx));
        // Unenrolled node — no node token, no enrollment token.
        assert!(!check_connect_auth("node:other", "", "wrong", &ctx));
    }

    #[test]
    fn node_first_connect_accepts_enrollment_token() {
        let mut enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let tok = enrollment.create_token(std::time::Duration::from_secs(3600));
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        assert!(check_connect_auth("node:gpu-1", "", &tok, &ctx));

        // Consumed token is rejected on a later connect.
        assert!(enrollment.consume_token(&tok, "gpu-1"));
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        assert!(!check_connect_auth("node:gpu-1", "", &tok, &ctx));
    }

    #[test]
    fn agent_accepts_any_registered_node_token() {
        let enrollment = empty_enrollment();
        let mut node_tokens = empty_node_tokens();
        let token = node_tokens.upsert("gpu-1", "uid-1");
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        // Phase 5a simplification: ownership is not verified.
        assert!(check_connect_auth("agent:com.example", "", &token, &ctx));
        assert!(!check_connect_auth("agent:com.example", "", "wrong", &ctx));
    }

    #[test]
    fn publisher_accepts_internal_token() {
        let enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("pub-tok"), Some("h"));
        assert!(check_connect_auth("gateway:publisher", "", "pub-tok", &ctx));
        assert!(!check_connect_auth("gateway:publisher", "", "wrong", &ctx));
        // No publisher token configured → reject.
        let ctx = test_ctx(true, &enrollment, &node_tokens, None, Some("h"));
        assert!(!check_connect_auth("gateway:publisher", "", "pub-tok", &ctx));
    }

    #[test]
    fn desktop_accepts_http_token() {
        let enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("http-tok"));
        assert!(check_connect_auth("user:nicholas:desktop:mac-1", "", "http-tok", &ctx));
        assert!(!check_connect_auth("user:nicholas:desktop:mac-1", "", "wrong", &ctx));
        // No http token → reject desktop client ids.
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), None);
        assert!(!check_connect_auth("user:nicholas:desktop:mac-1", "", "http-tok", &ctx));
    }

    #[test]
    fn unknown_client_ids_are_rejected() {
        let enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        assert!(!check_connect_auth("user:nicholas:web:mac-1", "", "h", &ctx));
        assert!(!check_connect_auth("random", "", "", &ctx));
        assert!(!check_connect_auth("node:", "", "", &ctx), "empty node id");
    }

    #[tokio::test]
    async fn test_broker_starts_and_accepts_connections() {
        // Use a non-default port to avoid conflicts with a running Gateway.
        let port = 18975; // different from default 19875
        let host = "127.0.0.1";

        // Threaded mode: `start_broker` blocks forever on rumqttd's
        // `Broker::start()` (it joins the server threads, whose accept
        // loops never exit), so calling it from the test thread would
        // hang whenever the port is free. `start_broker`
        // parks the broker on a background OS thread and returns after
        // a bounded startup confirmation.
        let handle = start_broker(host, port).expect("broker should start");
        assert_eq!(handle.listen_addr.port(), port);

        // Verify the broker is listening by connecting a rumqttc client.
        use rumqttc::{AsyncClient, MqttOptions, QoS};
        use std::time::Duration;

        let mut mqttoptions = MqttOptions::new("test:broker_smoke", host, port);
        mqttoptions.set_keep_alive(Duration::from_secs(5));

        let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

        // Poll the event loop a few times to establish the connection.
        let mut connected = false;
        for _ in 0..20 {
            match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Incoming::ConnAck(_))) => {
                    connected = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        assert!(connected, "rumqttc client should connect to the broker");

        // Publish and subscribe smoke test
        client
            .subscribe("acowork/test/#", QoS::AtLeastOnce)
            .await
            .expect("subscribe should succeed");

        // Give the broker a moment to process the subscription
        tokio::time::sleep(Duration::from_millis(50)).await;

        client
            .publish(
                "acowork/test/hello",
                QoS::AtLeastOnce,
                false,
                b"smoke test",
            )
            .await
            .expect("publish should succeed");

        // Dropping the handle releases the shutdown channel; the broker
        // thread parks until process exit, when the OS reclaims the
        // listener. The test never joins rumqttd's threads, so it cannot
        // hang on shutdown.
        drop(handle);
        drop(client);
    }
}
