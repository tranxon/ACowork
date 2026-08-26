//! Node control-plane end-to-end test (ADR-055 Phase 2a §7.1 + Phase 5a).
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
//!   5. LWT: SIGKILL-ing the node flips status to retained "offline";
//!   6. **Phase 5a auth**: with an auth-enabled broker, a node booting
//!      with an enrollment token enrolls (enroll → enroll_result),
//!      persists the minted node_token, and reconnects with it after a
//!      restart; anonymous / wrong-token node connections are rejected
//!      (CONNACK error).
//!
//! The node binary is a sibling of the test harness (built by the
//! workspace). When it is missing (e.g. `cargo test -p acowork-gateway`
//! in isolation), the test is SKIPPED rather than failed — the
//! dependency on the compiled binary is a build-ordering concern, not
//! a code-correctness one.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prost::Message as _;
use rumqttc::{AsyncClient, ConnectReturnCode, Event, Incoming, MqttOptions, QoS};
use tokio::sync::mpsc;

use acowork_core::mqtt_proto::{
    data_envelope, node_control_command, DataEnvelope, NodeControlCommand, NodeEnroll,
    NodeEnrollResult, NodeEvent,
};
use acowork_core::node::node_enroll_result_topic;
use acowork_gateway::mqtt::enrollment::{
    EnrollmentTokenStore, NodeTokenStore, SharedEnrollmentTokenStore, SharedNodeTokenStore,
    TokenValidation,
};
use acowork_gateway::mqtt::{start_broker_with_auth, BrokerAuth};

const NODE_ID: &str = "verify-node";
const TEST_PORT: u16 = 18990;
/// Phase 5a auth scenarios run on isolated ports so the anonymous
/// control-plane test and the auth tests can run in parallel.
const AUTH_TEST_PORT: u16 = 18991;
const AUTH_REJECT_PORT: u16 = 18992;
/// Reverse-proxy port for the auth happy-path node — distinct from the
/// default 19900 so it does not collide with the control-plane test's
/// node when both tests run in parallel.
const AUTH_PROXY_PORT: u16 = 19901;
const AUTH_NODE_ID: &str = "auth-node";
const PUBLISHER_TOKEN: &str = "e2e-publisher-token";

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
    client: AsyncClient,
    rx: mpsc::Receiver<(String, Vec<u8>)>,
}

impl Collector {
    /// Connect and subscribe to `acowork/nodes/#`.
    ///
    /// `client_id` / `credentials` are for Phase 5a auth-enabled
    /// brokers — the collector impersonates the Gateway's internal
    /// publisher (`gateway:publisher` + its token) so it can observe
    /// node traffic; the anonymous `test:node-e2e` client is used for
    /// the auth-disabled control-plane test.
    fn connect(port: u16, client_id: &str, credentials: Option<(&str, &str)>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        opts.set_clean_session(true);
        if let Some((user, pass)) = credentials {
            opts.set_credentials(user.to_string(), pass.to_string());
        }
        let (client, mut eventloop) = AsyncClient::new(opts, 64);
        let poll_client = client.clone();

        tokio::spawn(async move {
            poll_client
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
        Self { client, rx }
    }

    /// Publish through the collector's own connection (Phase 5a: the
    /// collector impersonates the Gateway publisher, so it is the only
    /// client that can answer the node's enroll request without a
    /// second `gateway:publisher` connection bumping it off the broker).
    async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<(), String> {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await
            .map_err(|e| e.to_string())
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

    let mut collector = Collector::connect(TEST_PORT, "test:node-e2e", None);

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

/// Decode a `DataEnvelope<NodeEnroll>` payload.
fn decode_enroll(bytes: &[u8]) -> NodeEnroll {
    let envelope = DataEnvelope::decode(bytes).expect("enroll decodes");
    match envelope.payload.expect("enroll payload") {
        data_envelope::Payload::NodeEnroll(enroll) => enroll,
        _ => panic!("expected NodeEnroll"),
    }
}

/// Poll `identity.json` until the Gateway-issued `node_token` appears
/// (written by the node's enroll_result handler).
async fn wait_for_identity_token(path: &std::path::Path, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(path)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(token) = json.get("node_token").and_then(|v| v.as_str())
        {
            return Some(token.to_string());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// Whether the broker rejects a CONNECT with the given client_id /
/// password (CONNACK error or immediate disconnect). `false` when the
/// connection is accepted.
async fn expect_connect_rejected(port: u16, client_id: &str, password: Option<&str>) -> bool {
    let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    if let Some(p) = password {
        opts.set_credentials(client_id.to_string(), p.to_string());
    }
    let (_client, mut eventloop) = AsyncClient::new(opts, 16);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, eventloop.poll()).await {
            // Poll error = broker refused / dropped the connection.
            Ok(Err(_)) => return true,
            Ok(Ok(Event::Incoming(Incoming::ConnAck(ack)))) => {
                return ack.code != ConnectReturnCode::Success;
            }
            Ok(Ok(_)) => continue,
            Err(_) => return false, // timeout without error — still alive
        }
    }
    false
}

/// Phase 5a happy path: enroll → node_token → credential reconnect.
///
/// The test plays the Gateway's enrollment half (validate → consume →
/// mint → enroll_result) against the real node binary; the broker runs
/// with `auth_enabled`, so every CONNECT must present a valid token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_enrolls_and_reconnects_with_node_token_under_auth() {
    let Some(bin) = node_binary() else {
        eprintln!("SKIP: acowork-node binary not found — run `cargo build --workspace` first");
        return;
    };

    // ── Auth-enabled broker + fresh token stores ───────────────────────
    let gw_home = tempfile::tempdir().unwrap();
    let mut enrollment_store = EnrollmentTokenStore::load(gw_home.path());
    let enrollment_token = enrollment_store.create_token(Duration::from_secs(3600));
    let enrollment_tokens: SharedEnrollmentTokenStore = Arc::new(Mutex::new(enrollment_store));
    let node_tokens: SharedNodeTokenStore = Arc::new(Mutex::new(NodeTokenStore::load(gw_home.path())));

    let auth = BrokerAuth {
        auth_enabled: true,
        enrollment_tokens: enrollment_tokens.clone(),
        node_tokens: node_tokens.clone(),
        publisher_token: PUBLISHER_TOKEN.to_string(),
        http_token: Some("e2e-http-token".to_string()),
    };
    let broker_handle = start_broker_with_auth("127.0.0.1", AUTH_TEST_PORT, Some(auth))
        .expect("auth broker starts");

    // The collector impersonates the Gateway publisher — the only
    // credential an observer can use on an auth-enabled broker.
    let mut collector = Collector::connect(
        AUTH_TEST_PORT,
        "gateway:publisher",
        Some(("gateway:publisher", PUBLISHER_TOKEN)),
    );

    // ── 1) Node boots with the enrollment token ────────────────────────
    let home = tempfile::tempdir().unwrap();
    let node_home = home.path().join("node-home");
    let mut child = std::process::Command::new(&bin)
        .args([
            "start",
            "--gateway-host",
            "127.0.0.1",
            "--gateway-mqtt-port",
            &AUTH_TEST_PORT.to_string(),
            "--name",
            AUTH_NODE_ID,
            "--proxy-port",
            &AUTH_PROXY_PORT.to_string(),
            "--token",
            &enrollment_token,
            "--home",
        ])
        .arg(&node_home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn node");

    // ── 2) Enroll request arrives; act as the Gateway ──────────────────
    let enroll_bytes = collector
        .wait_for_topic(&format!("/nodes/{AUTH_NODE_ID}/enroll"), Duration::from_secs(10))
        .await
        .expect("enroll request");
    let enroll = decode_enroll(&enroll_bytes);
    assert_eq!(enroll.node_id, AUTH_NODE_ID);
    assert!(!enroll.machine_uid.is_empty());
    assert_eq!(
        enroll.enrollment_token, enrollment_token,
        "node presents the --token in the enroll payload"
    );

    // Gateway-side handshake: validate → consume → mint node token.
    {
        let mut store = enrollment_tokens.lock().unwrap();
        assert_eq!(
            store.validate_token(&enrollment_token),
            TokenValidation::Valid,
            "one-time enrollment token is still valid"
        );
        assert!(store.consume_token(&enrollment_token, AUTH_NODE_ID));
    }
    let node_token = node_tokens
        .lock()
        .unwrap()
        .upsert(AUTH_NODE_ID, &enroll.machine_uid);
    assert!(!node_token.is_empty());

    // Answer with `enroll_result` (QoS 1, non-retained).
    let result = NodeEnrollResult {
        node_id: AUTH_NODE_ID.to_string(),
        machine_uid: enroll.machine_uid.clone(),
        node_token: node_token.clone(),
        status: "ok".to_string(),
        message: String::new(),
    };
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeEnrollResult(result)),
    };
    collector
        .publish(
            &node_enroll_result_topic(AUTH_NODE_ID),
            envelope.encode_to_vec(),
        )
        .await
        .expect("publish enroll_result");
    // ── 3) Node persists the token and reconnects with it ──────────────
    let identity_path = node_home.join("identity.json");
    let persisted = wait_for_identity_token(&identity_path, Duration::from_secs(10))
        .await
        .expect("node_token persisted to identity.json");
    assert_eq!(persisted, node_token, "identity.json carries the minted token");

    // Hard-kill, then restart WITHOUT the enrollment token — the node
    // must reconnect using the persisted node_token (broker-side
    // `node:{id}` rule). Success is proven by the retained status=online
    // replay, which only happens after a successful ConnAck.
    let _ = child.kill();
    let _ = child.wait();
    let mut child2 = std::process::Command::new(&bin)
        .args([
            "start",
            "--gateway-host",
            "127.0.0.1",
            "--gateway-mqtt-port",
            &AUTH_TEST_PORT.to_string(),
            "--name",
            AUTH_NODE_ID,
            "--proxy-port",
            &AUTH_PROXY_PORT.to_string(),
            "--home",
        ])
        .arg(&node_home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("re-spawn node");

    // The hard-kill disconnects the first process, so the broker first
    // replays its retained LWT "offline"; keep reading until the
    // restarted node (reconnected with the persisted node_token)
    // publishes its own retained "online".
    let status = loop {
        let msg = collector
            .wait_for_topic(&format!("/nodes/{AUTH_NODE_ID}/status"), Duration::from_secs(10))
            .await
            .expect("status after node_token reconnect");
        if std::str::from_utf8(&msg).unwrap_or("").trim() == "online" {
            break msg;
        }
    };
    assert_eq!(std::str::from_utf8(&status).unwrap().trim(), "online");

    // Cleanup.
    let _ = child2.kill();
    let _ = child2.wait();
    drop(broker_handle);
}

/// Phase 5a negative path: the auth-enabled broker rejects node
/// CONNECTs without a credential, with a wrong token, and for
/// unclassified client_ids — and accepts a valid enrollment token
/// (positive control).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_broker_rejects_uncredentialed_node_connects() {
    let gw_home = tempfile::tempdir().unwrap();
    let mut enrollment_store = EnrollmentTokenStore::load(gw_home.path());
    let enrollment_token = enrollment_store.create_token(Duration::from_secs(3600));
    let enrollment_tokens: SharedEnrollmentTokenStore = Arc::new(Mutex::new(enrollment_store));
    let node_tokens: SharedNodeTokenStore = Arc::new(Mutex::new(NodeTokenStore::load(gw_home.path())));

    let auth = BrokerAuth {
        auth_enabled: true,
        enrollment_tokens: enrollment_tokens.clone(),
        node_tokens: node_tokens.clone(),
        publisher_token: PUBLISHER_TOKEN.to_string(),
        http_token: Some("e2e-http-token".to_string()),
    };
    let broker_handle = start_broker_with_auth("127.0.0.1", AUTH_REJECT_PORT, Some(auth))
        .expect("auth broker starts");

    // Missing credential.
    assert!(
        expect_connect_rejected(AUTH_REJECT_PORT, "node:evil-node", None).await,
        "anonymous node CONNECT must be rejected"
    );
    // Wrong token.
    assert!(
        expect_connect_rejected(AUTH_REJECT_PORT, "node:evil-node", Some("wrong-token")).await,
        "wrong-token node CONNECT must be rejected"
    );
    // Unclassified client_id (no matching CONNECT rule).
    assert!(
        expect_connect_rejected(AUTH_REJECT_PORT, "random-client", None).await,
        "unclassified client_id must be rejected"
    );
    // Positive control: a valid, unconsumed enrollment token passes.
    assert!(
        !expect_connect_rejected(
            AUTH_REJECT_PORT,
            "node:good-node",
            Some(&enrollment_token),
        )
        .await,
        "valid enrollment token must be accepted"
    );

    drop(broker_handle);
}
