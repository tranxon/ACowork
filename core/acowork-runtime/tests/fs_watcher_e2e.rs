//! ADR-058 full-chain E2E test: real FS changes → WorkspaceFsWatcher
//! aggregation → real MQTT broker → subscriber decodes the envelope.
//!
//! This is the test the W3 spec asked for ("fake MQTT broker verifying
//! the event payload") — it exercises the *production* publish path:
//! `WorkspaceFsWatcher` (real notify::PollWatcher) → `MqttFsEventSink`
//! (DataEnvelope encoding, topic assembly, QoS 1) → rumqttd broker →
//! a subscriber that decodes `DataEnvelope` exactly like the Desktop
//! Tauri backend does (`commands/chat_mqtt.rs`).
//!
//! Covered scenarios (mirroring real user/business flows):
//! 1. External CLI-style write of a new file (two writes in one window
//!    coalesce to a single `Created`)
//! 2. External modification of a pre-existing file → `Modified`
//! 3. External deletion → `Deleted`
//! 4. Workspace added via CRUD-style resolver re-sync → its changes
//!    are pushed too (the path the HTTP `create_workspace` hook takes)

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use acowork_core::mqtt_proto::{DataEnvelope, FsChangeKind, data_envelope};
use acowork_gateway::mqtt::start_broker;
use acowork_runtime::http::server::SharedMqttClientSlot;
use acowork_runtime::mqtt::{MqttConnectConfig, RuntimeMqttClient, new_shared_cache};
use acowork_runtime::tools::workspace_resolver::{WorkspaceAccess, WorkspaceDir, WorkspaceResolver};
use acowork_runtime::workspace::WorkspaceWatcherSet;
use prost::Message;
use tokio::sync::mpsc;

/// Unique broker port per test run (same pattern as `mqtt_e2e_full.rs`).
fn fresh_broker_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(20175);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

const AGENT_ID: &str = "com.test.agent";

/// Drain deadline: poll cycle (500ms) + aggregation window (500ms)
/// plus transport margin.
const SETTLE_MS: u64 = 1300;

/// Collect every publish landing on the fs-changed topic filter into a
/// decoded list of `WorkspaceFsChangeEvent`. Spawned as a background
/// task so the SUBSCRIBE packet actually goes out on the wire while
/// the main flow performs file operations.
async fn spawn_fs_subscriber(
    port: u16,
) -> mpsc::UnboundedReceiver<acowork_core::mqtt_proto::WorkspaceFsChangeEvent> {
    let mut opts = rumqttc::MqttOptions::new("e2e:fs:subscriber", "127.0.0.1", port);
    opts.set_clean_session(true);
    let (client, mut eventloop) = rumqttc::AsyncClient::new(opts, 10);

    client
        .subscribe(
            format!("acowork/agents/{}/workspaces/+/fs-changed", AGENT_ID),
            rumqttc::QoS::AtLeastOnce,
        )
        .await
        .expect("subscribe");

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                    // Decode exactly like the Desktop Tauri backend does.
                    let Ok(envelope) = DataEnvelope::decode(p.payload.as_ref()) else {
                        continue;
                    };
                    if let Some(data_envelope::Payload::WorkspaceFsChangeEvent(ev)) = envelope.payload
                    {
                        let _ = tx.send(ev);
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    rx
}

fn temp_workspace(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("acowork-fs-e2e-{}-{}", name, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp workspace dir");
    dir
}

fn resolver_for(dirs: Vec<(&str, std::path::PathBuf)>) -> WorkspaceResolver {
    let allowed = dirs
        .into_iter()
        .map(|(id, path)| WorkspaceDir {
            id: id.to_string(),
            path: path.to_string_lossy().to_string(),
            access: WorkspaceAccess::ReadWrite,
            last_active: false,
            prompt_file: None,
        })
        .collect();
    WorkspaceResolver::new_for_test(allowed)
}

/// True when the collected events contain a change of `kind` on `path`.
fn saw(events: &[acowork_core::mqtt_proto::WorkspaceFsChangeEvent], path: &str, kind: FsChangeKind) -> bool {
    events
        .iter()
        .flat_map(|e| e.changes.iter())
        .any(|c| c.path == path && c.kind == kind as i32)
}

#[test]
fn fs_watcher_full_chain_e2e() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).expect("broker start");

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        // ── Desktop-side subscriber, subscribed BEFORE the watcher exists
        //    (QoS 1 non-retained events are lost for absent subscribers).
        let mut events_rx = spawn_fs_subscriber(port).await;

        // ── Real Runtime MQTT client (the production publisher), placed
        //    into the same late-bind slot type the HTTP server holds.
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let runtime_client = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: AGENT_ID,
            agent_name: "Test Agent",
            agent_version: "1.0.0",
            avatar: None,
            builtin_avatar: None,
            config_json: "{}",
            available_cache: new_shared_cache(),
            control_tx,
            identity_update_tx: None,
            provider_update_tx: None,
            search_update_tx: None,
            embedding_update_tx: None,
            node_id: None,
            lsps_update_tx: None,
            work_dir: std::env::temp_dir().join(format!("acowork-fs-e2e-{}", uuid::Uuid::new_v4())),
            username: None,
            password: None,
        })
        .await
        .expect("runtime mqtt connect");

        let slot: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
            tokio::sync::Mutex::new(runtime_client),
        ))));

        // ── Workspace A with a pre-existing file (PollWatcher baselines
        //    on watch() — pre-existing files produce no events).
        let ws_a = temp_workspace("a");
        std::fs::write(ws_a.join("external.txt"), b"v1").expect("seed file");

        let mut set = WorkspaceWatcherSet::new(AGENT_ID.to_string(), slot);
        set.sync_from_resolver(&resolver_for(vec![("ws-a", ws_a.clone())]));
        assert_eq!(set.watcher_count(), 1);
        // Demand-driven: watch the workspace root (NonRecursive) so the
        // root-level file ops below are observed.
        set.set_watch_targets("ws-a", vec![std::path::PathBuf::from("")]);

        // Watch targets travel over an async control channel to the
        // watcher task, and PollWatcher only baselines a path once its
        // `watch()` lands. Wait out a poll cycle so the root watch is
        // actually live before the first file op below — otherwise a
        // pre-watch create is never observed (the file already exists at
        // baseline time and produces no event).
        tokio::time::sleep(Duration::from_millis(600)).await;

        // ── Scenario 1: external CLI creates a new file (two rapid
        //    writes in one aggregation window → a single Created).
        std::fs::write(ws_a.join("created.txt"), b"hello").unwrap();
        std::fs::write(ws_a.join("created.txt"), b"hello world").unwrap();
        tokio::time::sleep(Duration::from_millis(SETTLE_MS)).await;

        // ── Scenario 2: external modification of a pre-existing file.
        std::fs::write(ws_a.join("external.txt"), b"v2 - externally edited").unwrap();
        tokio::time::sleep(Duration::from_millis(SETTLE_MS)).await;

        // ── Scenario 3: external deletion of a file the watcher has
        //    already announced as Created.
        std::fs::remove_file(ws_a.join("created.txt")).unwrap();
        tokio::time::sleep(Duration::from_millis(SETTLE_MS)).await;

        // ── Scenario 4: workspace CRUD — a second workspace appears in
        //    the resolver (the same re-sync `create_workspace` runs);
        //    its changes must be pushed under its own topic.
        let ws_b = temp_workspace("b");
        set.sync_from_resolver(&resolver_for(vec![
            ("ws-a", ws_a.clone()),
            ("ws-b", ws_b.clone()),
        ]));
        assert_eq!(set.watcher_count(), 2);
        // The new workspace needs its own root watch target (the
        // re-sync keeps ws-a's existing target set).
        set.set_watch_targets("ws-b", vec![std::path::PathBuf::from("")]);
        // Same live-watch wait as ws-a (async target channel + poll
        // baseline), so the create below is actually observed.
        tokio::time::sleep(Duration::from_millis(600)).await;
        std::fs::write(ws_b.join("in-new-workspace.txt"), b"data").unwrap();
        tokio::time::sleep(Duration::from_millis(SETTLE_MS)).await;

        // Let the final flush propagate, then stop watchers.
        tokio::time::sleep(Duration::from_millis(400)).await;
        set.stop_all();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // ── Collect everything the subscriber received.
        let mut events = Vec::new();
        while let Ok(ev) = events_rx.try_recv() {
            events.push(ev);
        }
        assert!(
            !events.is_empty(),
            "subscriber received no fs-changed events at all"
        );

        // Envelope-level contract (what the Desktop decodes against).
        for ev in &events {
            assert_eq!(ev.agent_id, AGENT_ID, "agent_id must survive the envelope");
            assert!(ev.window_end_ms > 0);
            for c in &ev.changes {
                assert!(
                    !c.path.starts_with('/') && !c.path.contains('\\'),
                    "path must be forward-slash relative, got {:?}",
                    c.path
                );
            }
        }

        // Scenario assertions. ws-a events carry workspace_id "ws-a".
        let ws_a_events: Vec<acowork_core::mqtt_proto::WorkspaceFsChangeEvent> = events
            .iter()
            .filter(|e| e.workspace_id == "ws-a")
            .cloned()
            .collect();
        assert!(
            saw(&ws_a_events, "created.txt", FsChangeKind::Created),
            "created.txt must surface as Created, got {:?}",
            ws_a_events.iter().flat_map(|e| e.changes.iter().map(|c| (c.path.clone(), c.kind))).collect::<Vec<_>>()
        );
        assert!(
            saw(&ws_a_events, "external.txt", FsChangeKind::Modified),
            "external.txt must surface as Modified"
        );
        assert!(
            saw(&ws_a_events, "created.txt", FsChangeKind::Deleted),
            "created.txt deletion must surface as Deleted"
        );
        // The two rapid writes of created.txt must NOT surface as an
        // extra Modified — one window, one Created.
        let created_txt_kinds: Vec<i32> = ws_a_events
            .iter()
            .flat_map(|e| e.changes.iter().filter(|c| c.path == "created.txt"))
            .map(|c| c.kind)
            .collect();
        assert_eq!(
            created_txt_kinds,
            vec![FsChangeKind::Created as i32, FsChangeKind::Deleted as i32],
            "created.txt must be exactly [Created, Deleted] across windows, got {:?}",
            created_txt_kinds
        );

        // Scenario 4: the CRUD-added workspace pushes under its own id.
        assert!(
            saw(&events, "in-new-workspace.txt", FsChangeKind::Created),
            "ws-b file must surface as Created after CRUD re-sync"
        );
        assert!(
            events.iter().any(|e| e.workspace_id == "ws-b"),
            "ws-b events must carry workspace_id=ws-b"
        );

        let _ = std::fs::remove_dir_all(&ws_a);
        let _ = std::fs::remove_dir_all(&ws_b);
    });

    drop(broker);
}
