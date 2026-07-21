//! Full E2E integration test for ADR-033 MQTT migration.
//!
//! Tests the complete MQTT flow with real broker + real clients:
//!   Gateway MQTT client → Broker → Runtime MQTT client (control_rx)
//!
//! No real Gateway/Runtime binaries needed — uses MQTT module-level clients.
//! No real LLM either — we only verify the MQTT control channel.

use std::time::Duration;

use acowork_core::mqtt_proto::{
    self, control_command::Command, data_envelope::Payload, ChatMessage,
    ControlCommand, DataEnvelope,
};
use acowork_gateway::mqtt::{start_broker_in_thread, GatewayMqttClient};
use acowork_runtime::mqtt::{new_shared_cache, MqttConnectConfig, RuntimeMqttClient};
use prost::Message as _;

const BROKER_PORT: u16 = 19975;

// ═══════════════════════════════════════════════════════════════════════
// Test 1: Broker starts in separate thread
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_broker_starts() {
    let broker = start_broker_in_thread("127.0.0.1", BROKER_PORT)
        .expect("broker should start in separate thread");
    assert_eq!(broker.listen_addr.to_string(), "127.0.0.1:19975");
    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 2: Gateway + Runtime MQTT clients connect to broker
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_gateway_and_runtime_connect() {
    let broker = start_broker_in_thread("127.0.0.1", BROKER_PORT).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Gateway publisher connects
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", BROKER_PORT)
            .await
            .expect("gateway connect");

        // Runtime client connects
        let cache = new_shared_cache();
        let (control_tx, mut _control_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = RuntimeMqttClient::connect(
            MqttConnectConfig {
                host: "127.0.0.1",
                port: BROKER_PORT,
                agent_id: "com.test.agent",
                agent_name: "Test Agent",
                agent_version: "1.0.0",
                avatar: None,
                builtin_avatar: None,
                config_json: "{}",
                available_cache: cache,
                control_tx,
                identity_update_tx: None,
            },
        )
        .await
        .expect("runtime connect");

        // Both connected — verify they're alive (can be dropped)
        drop(gw);
        drop(runtime);

        // Give broker time to process disconnect
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 3: Gateway publishes control message → Runtime receives via control_rx
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_control_message_flow() {
    let broker = start_broker_in_thread("127.0.0.1", BROKER_PORT).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Gateway publisher
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", BROKER_PORT)
            .await.unwrap();

        // Runtime with control_rx
        let cache = new_shared_cache();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let _runtime = RuntimeMqttClient::connect(
            MqttConnectConfig {
                host: "127.0.0.1",
                port: BROKER_PORT,
                agent_id: "com.test.agent",
                agent_name: "Test",
                agent_version: "1.0.0",
                avatar: None,
                builtin_avatar: None,
                config_json: "{}",
                available_cache: cache,
                control_tx,
                identity_update_tx: None,
            },
        ).await.unwrap();

        // Publish control message from Gateway
        let cmd = ControlCommand {
            agent_id: "com.test.agent".into(),
            command: Some(Command::ChatMessage(ChatMessage {
                session_id: "sess-e2e".into(),
                message_id: "msg-e2e".into(),
                content: "Hello from E2E test".into(),
                command: String::new(),
                params_json: String::new(),
            })),
        };
        gw.publish_control_command("com.test.agent", cmd)
            .await
            .expect("publish");

        // Runtime receives via control_rx (with timeout)
        let received = tokio::time::timeout(
            Duration::from_secs(3),
            control_rx.recv(),
        ).await
            .expect("timeout")
            .expect("control_rx closed");

        let (_topic, payload_bytes) = received;

        // Verify it's valid ControlCommand protobuf
        let env = DataEnvelope::decode(payload_bytes.as_slice())
            .expect("decode DataEnvelope");

        match env.payload {
            Some(Payload::ControlCommand(ctrl)) => {
                assert_eq!(ctrl.agent_id, "com.test.agent");
                match ctrl.command {
                    Some(Command::ChatMessage(msg)) => {
                        assert_eq!(msg.content, "Hello from E2E test");
                        assert_eq!(msg.session_id, "sess-e2e");
                    }
                    other => panic!("Expected ChatMessage, got {:?}", other),
                }
            }
            other => panic!("Expected ControlCommand, got {:?}", other),
        }

        drop(gw);
        drop(_runtime);
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 4: Gateway publishes stop → Runtime receives
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_control_stop_flow() {
    let broker = start_broker_in_thread("127.0.0.1", BROKER_PORT).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", BROKER_PORT).await.unwrap();

        let cache = new_shared_cache();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let _rt = RuntimeMqttClient::connect(
            MqttConnectConfig {
                host: "127.0.0.1",
                port: BROKER_PORT,
                agent_id: "com.test.agent",
                agent_name: "Test",
                agent_version: "1.0.0",
                avatar: None,
                builtin_avatar: None,
                config_json: "{}",
                available_cache: cache,
                control_tx,
                identity_update_tx: None,
            },
        ).await.unwrap();

        let cmd = ControlCommand {
            agent_id: "com.test.agent".into(),
            command: Some(Command::Stop(mqtt_proto::Stop {
                session_id: "sess-stop".into(),
                reason: String::new(),
            })),
        };
        gw.publish_control_command("com.test.agent", cmd).await.unwrap();

        let (_topic, payload) = tokio::time::timeout(Duration::from_secs(2), control_rx.recv())
            .await.unwrap().unwrap();

        let env = DataEnvelope::decode(payload.as_slice()).unwrap();
        assert!(matches!(env.payload, Some(Payload::ControlCommand(
            ControlCommand { command: Some(Command::Stop(_)), .. }
        ))));

        drop(gw); drop(_rt);
    });
    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 5: Multiple messages in sequence
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_multiple_messages() {
    let broker = start_broker_in_thread("127.0.0.1", BROKER_PORT).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", BROKER_PORT).await.unwrap();
        let cache = new_shared_cache();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let _rt = RuntimeMqttClient::connect(
            MqttConnectConfig {
                host: "127.0.0.1",
                port: BROKER_PORT,
                agent_id: "com.test.agent",
                agent_name: "Test",
                agent_version: "1.0.0",
                avatar: None,
                builtin_avatar: None,
                config_json: "{}",
                available_cache: cache,
                control_tx,
                identity_update_tx: None,
            },
        ).await.unwrap();
        let messages = ["msg-1", "msg-2", "msg-3"];
        for (i, content) in messages.iter().enumerate() {
            let cmd = ControlCommand {
                agent_id: "com.test.agent".into(),
                command: Some(Command::ChatMessage(ChatMessage {
                    session_id: "sess-seq".into(),
                    message_id: format!("mid-{}", i),
                    content: content.to_string(),
                    command: String::new(),
                    params_json: String::new(),
                })),
            };
            gw.publish_control_command("com.test.agent", cmd).await.unwrap();
        }

        let mut received = Vec::new();
        for _ in 0..3 {
            let (_, payload) = tokio::time::timeout(Duration::from_secs(2), control_rx.recv())
                .await.unwrap().unwrap();
            let env = DataEnvelope::decode(payload.as_slice()).unwrap();
            if let Some(Payload::ControlCommand(ctrl)) = env.payload
                && let Some(Command::ChatMessage(msg)) = ctrl.command
            {
                received.push(msg.content);
            }
        }

        assert_eq!(received, ["msg-1", "msg-2", "msg-3"]);

        drop(gw); drop(_rt);
    });
    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 6: LWT — Runtime disconnect triggers offline status
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_lwt_offline_on_disconnect() {
    let broker = start_broker_in_thread("127.0.0.1", BROKER_PORT).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cache = new_shared_cache();
        let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = RuntimeMqttClient::connect(
            MqttConnectConfig {
                host: "127.0.0.1",
                port: BROKER_PORT,
                agent_id: "com.test.lwt",
                agent_name: "LWT Agent",
                agent_version: "1.0.0",
                avatar: None,
                builtin_avatar: None,
                config_json: "{}",
                available_cache: cache,
                control_tx,
                identity_update_tx: None,
            },
        ).await.unwrap();

        // Drop Runtime — LWT should publish "offline" retained to status topic
        drop(control_rx);
        drop(runtime);

        // Wait for LWT to propagate
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Connect a fresh client to check the retained message
        let mut opts = rumqttc::MqttOptions::new("e2e:lwt:checker", "127.0.0.1", BROKER_PORT);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut events) = rumqttc::AsyncClient::new(opts, 10);

        client.subscribe("acowork/agents/com.test.lwt/status", rumqttc::QoS::AtLeastOnce)
            .await.unwrap();

        let status = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match events.poll().await {
                    Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                        if p.topic.contains("/com.test.lwt/status") {
                            return String::from_utf8_lossy(&p.payload).to_string();
                        }
                    }
                    _ => continue,
                }
            }
        }).await.expect("should receive retained LWT message");

        assert_eq!(status, "offline", "LWT should publish offline status");

        drop(client);
    });
    drop(broker);
}
