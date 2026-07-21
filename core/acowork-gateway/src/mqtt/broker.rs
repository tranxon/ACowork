//! Embedded rumqttd MQTT broker (ADR-033 Phase 1).
//!
//! The Gateway embeds a rumqttd broker in-process, listening on
//! `127.0.0.1:19875` (MQTT 3.1.1 / TCP). All clients (Runtime, Desktop,
//! and the Gateway's own publisher) connect to this broker.
//!
//! See `docs/zh/protocols/mqtt.md` §1–§2 for the protocol conventions
//! and architecture overview.

use std::net::SocketAddr;

use rumqttd::{Broker, Config};

use acowork_core::defaults;

/// Error type for MQTT broker operations.
#[derive(Debug, thiserror::Error)]
pub enum MqttBrokerError {
    #[error("MQTT broker failed to start: {0}")]
    Start(String),
    #[error("MQTT broker config error: {0}")]
    Config(String),
}

/// Handle to the running MQTT broker.
///
/// When created via `start_broker()`, holds the `Broker` directly so
/// dropping the handle shuts down the broker. When created via
/// `start_broker_in_thread()`, the broker lives in a parked OS thread.
///
/// Production callers never need to shut the broker down — it lives
/// for the lifetime of the Gateway process. The optional `shutdown_tx`
/// exists only so the debug HTTP endpoints (`POST /api/debug/mqtt/*`)
/// can request a graceful exit without restarting the whole process.
pub struct MqttBrokerHandle {
    /// Direct ownership (used by `start_broker`).
    #[allow(dead_code)]
    _broker: Option<Broker>,
    /// Channel to signal the broker thread to exit.
    /// `None` for handles created via `start_broker()` (direct
    /// ownership — `Drop` of `Broker` shuts down the listener).
    /// `Some` for handles from `start_broker_in_thread()` so debug
    /// endpoints can request a graceful exit.
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

/// Start the embedded MQTT broker.
///
/// Creates a `Broker` from the config and calls `start()`, which spawns
/// the router thread and server threads internally (OS threads, not
/// tokio tasks). The returned `MqttBrokerHandle` keeps the broker alive.
///
/// The caller should hold the handle for the lifetime of the Gateway
/// process. Dropping it shuts down the broker.
pub fn start_broker(host: &str, port: u16) -> Result<MqttBrokerHandle, MqttBrokerError> {
    let listen_addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| MqttBrokerError::Config(format!("Invalid listen address '{}:{}': {}", host, port, e)))?;

    let config = build_broker_config(host, port);

    tracing::info!(
        addr = %listen_addr,
        port,
        max_connections = defaults::GATEWAY_MQTT_MAX_CONNECTIONS,
        max_packet_size = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE,
        "Starting embedded MQTT broker (rumqttd)"
    );

    let mut broker = Broker::new(config);
    broker
        .start()
        .map_err(|e| MqttBrokerError::Start(format!("rumqttd broker start failed: {}", e)))?;

    tracing::info!(addr = %listen_addr, "MQTT broker started and accepting connections");

    // Return the Broker in the handle so the caller can keep it alive.
    Ok(MqttBrokerHandle {
        _broker: Some(broker),
        shutdown_tx: None,
        listen_addr,
    })
}

/// Start the broker in a separate OS thread (non-blocking).
///
/// Required when calling from inside a tokio runtime, because rumqttd
/// creates its own tokio runtime internally. The broker thread parks
/// after startup to keep the `Broker` alive for the process lifetime.
///
/// Uses a short timeout (500 ms) to confirm startup, but does NOT block
/// indefinitely — if the broker doesn't respond in time, the function
/// still returns `Ok` so the Gateway can continue starting.
/// Start the broker in a separate OS thread (non-blocking).
///
/// Required when calling from inside a tokio runtime, because rumqttd
/// creates its own tokio runtime internally. The broker thread parks
/// after startup to keep the `Broker` alive for the process lifetime.
///
/// Uses a short timeout (500 ms) to confirm startup, but does NOT block
/// indefinitely — if the broker doesn't respond in time, the function
/// still returns `Ok` so the Gateway can continue starting.
pub fn start_broker_in_thread(host: &str, port: u16) -> Result<MqttBrokerHandle, MqttBrokerError> {
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
            // Park until shutdown signal arrives. If `start_broker`
            // fails, signal the parent and exit immediately.
            match start_broker(&h, port) {
                Ok(broker) => {
                    // Send confirmation — the Broker is alive on this
                    // thread's stack, and will stay alive until the
                    // thread exits (either via the shutdown signal or
                    // process exit).
                    let _ = tx.send(Ok(()));
                    // Park until shutdown signal arrives.
                    //
                    // `park_timeout` (vs. plain `park`) is used so the
                    // thread can react to a shutdown request from the
                    // debug HTTP endpoints without an explicit
                    // `Thread::unpark` — which would require exposing
                    // the parked Thread handle in `MqttBrokerHandle`,
                    // a strictly debug-only concern. The 200 ms
                    // timeout is a deliberate trade-off: it costs ~5
                    // wakeups/sec/idle thread, but lets the debug
                    // endpoint shut the broker down cleanly.
                    loop {
                        std::thread::park_timeout(std::time::Duration::from_millis(200));
                        if shutdown_rx.try_recv().is_ok() {
                            break;
                        }
                    }
                    // Broker drops here, closing all TCP connections.
                    drop(broker);
                    tracing::info!("MQTT broker thread exiting (broker dropped)");
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        })
        .map_err(|e| MqttBrokerError::Start(format!("spawn thread: {}", e)))?;

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
        _broker: None,
        shutdown_tx: Some(shutdown_tx),
        listen_addr,
    })
}

/// Start the MQTT broker with default localhost settings.
pub fn start_default_broker() -> Result<MqttBrokerHandle, MqttBrokerError> {
    start_broker(
        defaults::GATEWAY_MQTT_HOST,
        defaults::GATEWAY_MQTT_PORT,
    )
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
/// This **does not wait** for the broker thread to actually exit (and
/// for `Broker::drop()` to close the TCP listener). Callers that need
/// to rebind the same port should sleep briefly after calling this to
/// let the OS TIME_WAIT state drain.
///
/// Only handles created via [`start_broker_in_thread`] have a
/// shutdown channel; handles from [`start_broker`] return an error.
pub fn signal_shutdown(&mut self) -> Result<(), String> {
    let tx = self
        .shutdown_tx
        .take()
        .ok_or_else(|| "broker not running in threaded mode".to_string())?;
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

    #[tokio::test]
    async fn test_broker_starts_and_accepts_connections() {
        // Use a non-default port to avoid conflicts with a running Gateway.
        let port = 18975; // different from default 19875
        let host = "127.0.0.1";

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

        // The handle is dropped here, shutting down the broker.
        drop(handle);
        drop(client);
    }
}
