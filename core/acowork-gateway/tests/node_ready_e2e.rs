//! ADR-059 Phase 2 acceptance — NodeReady drives subsystem readiness.
//!
//! Spawns a real broker plus an in-process dispatch surface and verifies
//! the Gateway's `node.{id}` readiness lifecycle end-to-end:
//!
//!   1. A Node that publishes a retained `DataEnvelope<NodeReady>`
//!      (after CONNECT + control subscriptions) marks `node.{id}` ready,
//!      and the orchestrator's aggregated phase reaches `READY`;
//!   2. Node restart: clearing the retained snapshot (shutdown) demotes
//!      the phase back to `BOOTING`; the re-announced `NodeReady` from
//!      the new process restores `READY` (ADR-059 §7.2 re-delivery);
//!   3. LWT `status=offline` while ready demotes the phase back to
//!      `BOOTING` until the Node re-announces.
//!
//! The dispatch surface mirrors `gateway/mod.rs` — a Gateway MQTT client
//! whose callback routes every message through
//! `dispatch::handle_plaintext_message` with the shared registry.

use std::sync::Arc;
use std::time::Duration;

use prost::Message as _;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use tokio::sync::RwLock;

use acowork_core::mqtt_proto::{data_envelope, DataEnvelope};
use acowork_core::node::node_ready_topic;
use acowork_gateway::bootstrap::{BootstrapOrchestrator, ReadinessKind, SubsystemReadinessRegistry};
use acowork_gateway::bootstrap::orchestrator::BootstrapPhase;
use acowork_gateway::gateway::state::GatewayState;
use acowork_gateway::http::routes::SharedHttpState;
use acowork_gateway::mqtt::broker::start_broker;
use acowork_gateway::mqtt::client::GatewayMqttClient;
use acowork_gateway::mqtt::dispatch;
use acowork_gateway::operation_store::OperationStore;

const NODE_ID: &str = "verify-node";
const SUBSYSTEM: &str = "node.verify-node";

/// Pick a fresh MQTT port per test invocation (the broker thread never
/// exits, so reusing a port would leak retained state across runs).
fn unique_port(label: &str) -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(18960);
    let p = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    eprintln!("[node_ready_e2e] {label} using port {p}");
    p
}

/// Poll `cond` until it returns true or the deadline elapses.
async fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cond()
}

/// Build the dispatch surface: registry (only the subsystem under
/// test), orchestrator, shared GatewayState, and a Gateway MQTT client
/// whose callback routes messages through the real dispatcher.
async fn build_surface(port: u16) -> (Arc<BootstrapOrchestrator>, GatewayMqttClient) {
    let registry = SubsystemReadinessRegistry::new_shared();
    let orchestrator = BootstrapOrchestrator::new("test-instance".to_string(), registry.clone());
    // Register only the subsystem under test — the full startup sequence
    // (vault / mqtt / publisher / local_node / system_agent) is not part
    // of this test's scope (system_agent wiring lands in Phase 5).
    let _handle = registry.register(SUBSYSTEM, ReadinessKind::Required);

    let gw_state: SharedHttpState = Arc::new(RwLock::new(GatewayState::new(
        "/tmp/acowork-node-ready-test-vault",
    )));
    gw_state.write().await.bootstrap_orchestrator = Some(orchestrator.clone());

    let runtime_http_registry = acowork_gateway::http::proxy::new_shared_registry();
    let agent_registry = acowork_gateway::mqtt::agent_registry::new_shared_registry();
    let node_registry = acowork_gateway::mqtt::node_registry::new_shared_registry();
    let operation_store = OperationStore::new_shared();
    let reg_for_cb = registry.clone();
    let state_for_cb = gw_state.clone();
    let http_reg_for_cb = runtime_http_registry.clone();
    let agent_reg_for_cb = agent_registry.clone();
    let node_reg_for_cb = node_registry.clone();
    let op_store_for_cb = operation_store.clone();
    let dispatch_slot: Arc<tokio::sync::Mutex<Option<Arc<GatewayMqttClient>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let slot_for_cb = dispatch_slot.clone();
    let callback: acowork_gateway::mqtt::MqttMessageCallback = Arc::new(move |topic, payload| {
        let slot = slot_for_cb.clone();
        let reg = reg_for_cb.clone();
        let state = state_for_cb.clone();
        let http_reg = http_reg_for_cb.clone();
        let agent_reg = agent_reg_for_cb.clone();
        let node_reg = node_reg_for_cb.clone();
        let op_store = op_store_for_cb.clone();
        tokio::spawn(async move {
            let client = slot.lock().await.clone();
            dispatch::handle_plaintext_message(
                &topic,
                &payload,
                &http_reg,
                &agent_reg,
                &node_reg,
                client.as_ref(),
                &state,
                None,
                None,
                None,
                false,
                Some(&reg),
                Some(&op_store),
            );
        });
    });

    let gw_client = GatewayMqttClient::new_publisher_with_callback("127.0.0.1", port, callback)
        .await
        .expect("gateway client should connect");
    *dispatch_slot.lock().await = Some(Arc::new(gw_client.clone()));
    // Let the ConnAck subscription round settle before the node speaks.
    tokio::time::sleep(Duration::from_millis(200)).await;

    (orchestrator, gw_client)
}

/// Open a raw MQTT client that impersonates a Node publishing its
/// own `NodeReady` (retained) or clearing it (empty payload).
async fn node_publisher(port: u16, client_id: &str) -> AsyncClient {
    let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = AsyncClient::new(opts, 32);
    tokio::spawn(async move {
        // Keep the event loop alive so queued publishes flush; exits
        // when the client is dropped / the connection dies.
        loop {
            if eventloop.poll().await.is_err() {
                break;
            }
        }
    });
    // Give the CONNECT packet time to flush.
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
}

fn ready_envelope(node_id: &str) -> Vec<u8> {
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeReady(
            acowork_core::mqtt_proto::NodeReady {
                node_id: node_id.to_string(),
                protocol_version: 1,
            },
        )),
    };
    envelope.encode_to_vec()
}

/// 1. Node publishes retained NodeReady → `node.{id}` ready → phase READY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_ready_announcement_drives_phase_ready() {
    let port = unique_port("ready");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let (orchestrator, gw_client) = build_surface(port).await;

    let node = node_publisher(port, "node:verify-node").await;
    node.publish(
        node_ready_topic(NODE_ID),
        QoS::AtLeastOnce,
        true,
        ready_envelope(NODE_ID),
    )
    .await
    .expect("publish NodeReady");

    let snapshot = orchestrator.clone();
    let ready = wait_until(Duration::from_secs(5), move || {
        snapshot.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(ready, "phase must reach READY after NodeReady announcement");

    drop(node);
    drop(gw_client);
    drop(broker);
}

/// 2. Node restart: retained snapshot cleared (shutdown) demotes the
/// phase to BOOTING; the re-announced NodeReady from the new process
/// restores READY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_restart_redelivers_node_ready() {
    let port = unique_port("restart");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let (orchestrator, gw_client) = build_surface(port).await;

    // First process announces ready.
    let node_a = node_publisher(port, "node:verify-node-a").await;
    node_a
        .publish(
            node_ready_topic(NODE_ID),
            QoS::AtLeastOnce,
            true,
            ready_envelope(NODE_ID),
        )
        .await
        .expect("publish NodeReady (first boot)");
    let snapshot = orchestrator.clone();
    let first_ready = wait_until(Duration::from_secs(5), move || {
        snapshot.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(first_ready, "phase must reach READY after first NodeReady");

    // Graceful shutdown: the node clears its retained snapshot.
    node_a
        .publish(node_ready_topic(NODE_ID), QoS::AtLeastOnce, true, Vec::new())
        .await
        .expect("clear retained NodeReady");
    drop(node_a);

    let snapshot = orchestrator.clone();
    let demoted = wait_until(Duration::from_secs(5), move || {
        snapshot.snapshot().phase == BootstrapPhase::Booting
    })
    .await;
    assert!(demoted, "phase must drop back to BOOTING after retained clear");

    // Restart: the new process re-announces NodeReady.
    let node_b = node_publisher(port, "node:verify-node-b").await;
    node_b
        .publish(
            node_ready_topic(NODE_ID),
            QoS::AtLeastOnce,
            true,
            ready_envelope(NODE_ID),
        )
        .await
        .expect("publish NodeReady (restart)");
    let snapshot = orchestrator.clone();
    let re_ready = wait_until(Duration::from_secs(5), move || {
        snapshot.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(re_ready, "phase must return to READY after re-announced NodeReady");

    drop(node_b);
    drop(gw_client);
    drop(broker);
}

/// 3. LWT: a `status=offline` while the node is ready demotes the phase
/// back to BOOTING until the Node re-announces NodeReady.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_lwt_offline_demotes_readiness() {
    let port = unique_port("lwt");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let (orchestrator, gw_client) = build_surface(port).await;

    // Node announces ready.
    let node = node_publisher(port, "node:verify-node-lwt").await;
    node.publish(
        node_ready_topic(NODE_ID),
        QoS::AtLeastOnce,
        true,
        ready_envelope(NODE_ID),
    )
    .await
    .expect("publish NodeReady");
    let snapshot = orchestrator.clone();
    let ready = wait_until(Duration::from_secs(5), move || {
        snapshot.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(ready, "phase must reach READY first");

    // LWT: the broker publishes retained status=offline on the node's
    // status topic (the node process died ungracefully).
    node.publish(
        acowork_core::node::node_status_topic(NODE_ID),
        QoS::AtLeastOnce,
        true,
        "offline".as_bytes(),
    )
    .await
    .expect("publish status=offline");
    drop(node);

    let snapshot = orchestrator.clone();
    let demoted = wait_until(Duration::from_secs(5), move || {
        snapshot.snapshot().phase == BootstrapPhase::Booting
    })
    .await;
    assert!(demoted, "phase must drop to BOOTING after status=offline (LWT)");

    // Re-announcement from the reconnected node restores READY.
    let node2 = node_publisher(port, "node:verify-node-lwt-2").await;
    node2
        .publish(
            node_ready_topic(NODE_ID),
            QoS::AtLeastOnce,
            true,
            ready_envelope(NODE_ID),
        )
        .await
        .expect("publish NodeReady (reconnect)");
    let snapshot = orchestrator.clone();
    let re_ready = wait_until(Duration::from_secs(5), move || {
        snapshot.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(re_ready, "phase must return to READY after reconnect NodeReady");

    drop(node2);
    drop(gw_client);
    drop(broker);
}

/// 4. A NodeReady announcing a DIFFERENT node_id than the topic is
/// ignored — the announcing node may only mark its own subsystem.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_ready_with_mismatched_node_id_is_ignored() {
    let port = unique_port("mismatch");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let (orchestrator, gw_client) = build_surface(port).await;

    let node = node_publisher(port, "node:verify-node-x").await;
    // Topic says verify-node, payload claims gpu-1.
    node.publish(
        node_ready_topic(NODE_ID),
        QoS::AtLeastOnce,
        true,
        ready_envelope("gpu-1"),
    )
    .await
    .expect("publish mismatched NodeReady");

    // Give the dispatcher a moment to (reject and) process the message.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let snapshot = orchestrator.snapshot();
    assert_ne!(
        snapshot.phase,
        BootstrapPhase::Ready,
        "mismatched NodeReady must not mark the subsystem ready"
    );

    drop(node);
    drop(gw_client);
    drop(broker);
}
