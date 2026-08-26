//! Node control-plane end-to-end test (ADR-055 Phase 2a §7.1).
//!
//! Spawns a real `acowork-node` binary against an in-process broker and
//! verifies the full MQTT contract:
//!
//!   1. `acowork/nodes/{id}/status` → retained "online" on connect;
//!   2. `acowork/nodes/{id}/info` → retained NodeInfo envelope;
//!   3. `acowork/nodes/{id}/control/ping` → NodeEvent "pong" on
//!      `acowork/nodes/{id}/events`;
//!   4. request_id dedup: re-sending the same ping yields a second
//!      pong (cached reply, no re-execution observable at this layer);
//!   5. LWT: SIGKILL-ing the node flips status to retained "offline".
//!
//! The node binary is a sibling of the test harness (built by the
//! workspace). When it is missing (e.g. `cargo test -p acowork-gateway`
//! in isolation), the test is SKIPPED rather than failed — the
//! dependency on the compiled binary is a build-ordering concern, not
//! a code-correctness one.

use std::time::{Duration, Instant};

use prost::Message as _;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use tokio::sync::mpsc;

use acowork_core::mqtt_proto::{
    data_envelope, node_control_command, DataEnvelope, NodeControlCommand, NodeEvent,
};

const NODE_ID: &str = "verify-node";
const TEST_PORT: u16 = 18990;

/// Locate the compiled `acowork-node` binary (workspace target dir).
fn node_binary() -> Option<std::path::PathBuf> {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        // workspace target-dir layout (core/acowork-gateway → ../../target)
        manifest.join("../../target/debug/acowork-node"),
        manifest.join("../../target/debug/acowork-node.exe"),
        // standalone crate layout (core/acowork-node/target/debug)
        manifest.join("../acowork-node/target/debug/acowork-node"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

struct Collector {
    rx: mpsc::Receiver<(String, Vec<u8>)>,
}

impl Collector {
    fn connect(port: u16) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let mut opts = MqttOptions::new("test:node-e2e", "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        opts.set_clean_session(true);
        let (client, mut eventloop) = AsyncClient::new(opts, 64);

        tokio::spawn(async move {
            client
                .subscribe("acowork/nodes/#", QoS::AtLeastOnce)
                .await
                .ok();
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        let _ = tx.send((p.topic, p.payload.to_vec())).await;
                    }
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {}
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Self { rx }
    }

    async fn wait_for_topic(&mut self, suffix: &str, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, self.rx.recv()).await {
                Ok(Some((topic, payload))) => {
                    if topic.ends_with(suffix) {
                        return Some(payload);
                    }
                }
                _ => return None,
            }
        }
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_binary_speaks_the_control_plane_contract() {
    let Some(bin) = node_binary() else {
        eprintln!("SKIP: acowork-node binary not found — run `cargo build --workspace` first");
        return;
    };

    // Broker on an isolated port (the real Gateway owns 19875).
    let broker_handle =
        acowork_gateway::mqtt::start_broker("127.0.0.1", TEST_PORT).expect("broker starts");

    // Fresh node home so identity is minted for this run.
    let home = tempfile::tempdir().unwrap();
    let node_home = home.path().join("node-home");

    let mut child = std::process::Command::new(&bin)
        .args([
            "start",
            "--gateway-host",
            "127.0.0.1",
            "--gateway-mqtt-port",
            &TEST_PORT.to_string(),
            "--name",
            NODE_ID,
            "--home",
        ])
        .arg(&node_home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn node");

    let mut collector = Collector::connect(TEST_PORT);

    // 1) status = online (retained).
    let status = collector
        .wait_for_topic(&format!("/nodes/{NODE_ID}/status"), Duration::from_secs(10))
        .await
        .expect("status topic");
    assert_eq!(std::str::from_utf8(&status).unwrap().trim(), "online");

    // 2) info = NodeInfo envelope (retained).
    let info_bytes = collector
        .wait_for_topic(&format!("/nodes/{NODE_ID}/info"), Duration::from_secs(10))
        .await
        .expect("info topic");
    let info = DataEnvelope::decode(&info_bytes[..])
        .expect("info decodes")
        .payload
        .expect("info payload");
    let data_envelope::Payload::NodeInfo(info) = info else {
        panic!("expected NodeInfo");
    };
    assert_eq!(info.node_id, NODE_ID);
    assert_eq!(info.protocol_version, acowork_core::node::NODE_PROTOCOL_VERSION);

    // 3) ping → pong.
    send_ping(NODE_ID, "req-e2e-1").await;
    let pong = collector
        .wait_for_topic(&format!("/nodes/{NODE_ID}/events"), Duration::from_secs(5))
        .await
        .expect("pong event");
    let event = decode_event(&pong);
    assert_eq!(event.request_id, "req-e2e-1");
    assert_eq!(event.status, "ok");
    assert_eq!(event.message, "pong");

    // 4) request_id dedup: same id → a second (cached) reply.
    send_ping(NODE_ID, "req-e2e-1").await;
    let dup_pong = collector
        .wait_for_topic(&format!("/nodes/{NODE_ID}/events"), Duration::from_secs(5))
        .await
        .expect("duplicate pong");
    assert_eq!(decode_event(&dup_pong).request_id, "req-e2e-1");

    // 5) LWT: hard-kill → broker publishes retained "offline".
    let _ = child.kill();
    let offline = collector
        .wait_for_topic(&format!("/nodes/{NODE_ID}/status"), Duration::from_secs(10))
        .await
        .expect("offline status");
    assert_eq!(std::str::from_utf8(&offline).unwrap().trim(), "offline");

    // Cleanup.
    let _ = child.wait();
    drop(broker_handle);
}

async fn send_ping(node_id: &str, request_id: &str) {
    let cmd = NodeControlCommand {
        node_id: node_id.to_string(),
        request_id: request_id.to_string(),
        command: Some(node_control_command::Command::Ping(Default::default())),
    };
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeControlCommand(cmd)),
    };
    let mut opts = MqttOptions::new("test:node-e2e-pinger", "127.0.0.1", TEST_PORT);
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = AsyncClient::new(opts, 16);
    client
        .publish(
            format!("acowork/nodes/{node_id}/control/ping"),
            QoS::AtLeastOnce,
            false,
            envelope.encode_to_vec(),
        )
        .await
        .expect("publish ping");
    // rumqttc queues requests; the event loop must run to flush the
    // CONNECT + PUBLISH out the socket. Poll for a short window.
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(100), eventloop.poll()).await;
    }
    drop(eventloop);
}

fn decode_event(bytes: &[u8]) -> NodeEvent {
    let envelope = DataEnvelope::decode(bytes).expect("event decodes");
    match envelope.payload.expect("event payload") {
        data_envelope::Payload::NodeEvent(event) => event,
        _ => panic!("expected NodeEvent"),
    }
}
