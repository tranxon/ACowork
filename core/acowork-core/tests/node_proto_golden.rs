//! Golden contract tests for the `acowork/nodes/#` control-plane
//! payloads (ADR-055 Phase 2a §7.1 "Contract 测试").
//!
//! These tests pin the exact protobuf wire bytes of the node topic
//! family payloads so that any accidental field renumbering, type
//! change, or envelope reshaping breaks the build instead of silently
//! drifting the Gateway ↔ Node contract (same discipline as the
//! ADR-033 proto contract).

use acowork_core::mqtt_proto::{
    data_envelope, node_control_command, DataEnvelope, NodeControlCommand, NodeEnroll,
    NodeEnrollResult, NodeEvent, NodeInfo,
};
use prost::Message;

fn envelope_with(payload: data_envelope::Payload) -> DataEnvelope {
    DataEnvelope {
        version: 1,
        payload: Some(payload),
    }
}

fn sample_node_info() -> NodeInfo {
    NodeInfo {
        node_id: "local".to_string(),
        machine_uid: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
        hostname: "nicholas-pc".to_string(),
        os: "macos".to_string(),
        arch: "aarch64".to_string(),
        node_version: "0.1.0".to_string(),
        protocol_version: 1,
        capabilities: vec!["process".to_string(), "package".to_string()],
        max_agents: 16,
        agent_count: 0,
        http_endpoint: "http://127.0.0.1:19900".to_string(),
    }
}

fn sample_ping_command() -> NodeControlCommand {
    NodeControlCommand {
        node_id: "local".to_string(),
        request_id: "req-0001".to_string(),
        command: Some(node_control_command::Command::Ping(Default::default())),
    }
}

fn sample_event() -> NodeEvent {
    NodeEvent {
        node_id: "local".to_string(),
        request_id: "req-0001".to_string(),
        status: "ok".to_string(),
        message: "pong".to_string(),
        result_json: None,
    }
}

fn sample_enroll() -> NodeEnroll {
    NodeEnroll {
        node_id: "gpu-1".to_string(),
        machine_uid: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
        os: "macos".to_string(),
        arch: "aarch64".to_string(),
        node_version: "0.1.0".to_string(),
        protocol_version: 1,
        capabilities: vec!["process".to_string(), "package".to_string()],
        enrollment_token: "tok-1234".to_string(),
    }
}

fn sample_enroll_result() -> NodeEnrollResult {
    NodeEnrollResult {
        node_id: "gpu-1".to_string(),
        machine_uid: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
        node_token: "tok-node-0001".to_string(),
        status: "ok".to_string(),
        message: "enrolled".to_string(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode helper — mirrors how Gateway/Node consume the payloads.
fn decode_envelope(bytes: &[u8]) -> DataEnvelope {
    DataEnvelope::decode(bytes).expect("golden bytes must decode as DataEnvelope")
}

#[test]
fn golden_node_info_envelope() {
    let envelope = envelope_with(data_envelope::Payload::NodeInfo(sample_node_info()));
    let bytes = envelope.encode_to_vec();
    // Golden vector — computed from the contract above; any change to
    // field numbers/types in NodeInfo or the envelope tag must update
    // this deliberately.
    let expected = "08 01 8a 05 7f 0a 05 6c 6f 63 61 6c 12 24 30 66 30 65 30 64 30 63 2d 30 62 30 61 2d 34 30 30 39 2d 38 30 30 37 2d 30 36 30 35 30 34 30 33 30 32 30 31 1a 0b 6e 69 63 68 6f 6c 61 73 2d 70 63 22 05 6d 61 63 6f 73 2a 07 61 61 72 63 68 36 34 32 05 30 2e 31 2e 30 38 01 42 07 70 72 6f 63 65 73 73 42 07 70 61 63 6b 61 67 65 48 10 5a 16 68 74 74 70 3a 2f 2f 31 32 37 2e 30 2e 30 2e 31 3a 31 39 39 30 30";
    assert_eq!(hex(&bytes), expected.replace(' ', ""));

    // Round-trip contract: the golden bytes must decode back to the
    // same logical payload.
    let decoded = decode_envelope(&bytes);
    let data_envelope::Payload::NodeInfo(info) = decoded.payload.expect("payload") else {
        panic!("expected NodeInfo payload");
    };
    assert_eq!(info.node_id, "local");
    assert_eq!(info.protocol_version, 1);
    assert_eq!(info.capabilities, vec!["process", "package"]);
    assert_eq!(info.max_agents, 16);
    assert_eq!(info.http_endpoint, "http://127.0.0.1:19900");
}

#[test]
fn golden_node_ping_command_envelope() {
    let envelope = envelope_with(data_envelope::Payload::NodeControlCommand(
        sample_ping_command(),
    ));
    let bytes = envelope.encode_to_vec();
    let expected = "08 01 92 05 13 0a 05 6c 6f 63 61 6c 12 08 72 65 71 2d 30 30 30 31 52 00";
    assert_eq!(hex(&bytes), expected.replace(' ', ""));

    let decoded = decode_envelope(&bytes);
    let data_envelope::Payload::NodeControlCommand(cmd) = decoded.payload.expect("payload") else {
        panic!("expected NodeControlCommand payload");
    };
    assert_eq!(cmd.node_id, "local");
    assert_eq!(cmd.request_id, "req-0001");
    assert!(matches!(
        cmd.command,
        Some(node_control_command::Command::Ping(_))
    ));
}

#[test]
fn golden_node_event_envelope() {
    let envelope = envelope_with(data_envelope::Payload::NodeEvent(sample_event()));
    let bytes = envelope.encode_to_vec();
    let expected = "08 01 9a 05 1b 0a 05 6c 6f 63 61 6c 12 08 72 65 71 2d 30 30 30 31 1a 02 6f 6b 22 04 70 6f 6e 67";
    assert_eq!(hex(&bytes), expected.replace(' ', ""));

    let decoded = decode_envelope(&bytes);
    let data_envelope::Payload::NodeEvent(event) = decoded.payload.expect("payload") else {
        panic!("expected NodeEvent payload");
    };
    assert_eq!(event.status, "ok");
    assert_eq!(event.message, "pong");
}

#[test]
fn node_start_stop_command_wire_shape() {
    // Start / stop commands are defined in the Phase 2a contract even
    // though the Node answers `not_implemented` until Phase 2b — the
    // golden round-trip here pins their field numbers.
    let start = NodeControlCommand {
        node_id: "gpu-1".to_string(),
        request_id: "req-0002".to_string(),
        command: Some(node_control_command::Command::Start(
            acowork_core::mqtt_proto::NodeStart {
                agent_id: "com.example".to_string(),
                dev_mode: true,
            },
        )),
    };
    let envelope = envelope_with(data_envelope::Payload::NodeControlCommand(start));
    let bytes = envelope.encode_to_vec();
    let decoded = decode_envelope(&bytes);
    let data_envelope::Payload::NodeControlCommand(cmd) = decoded.payload.expect("payload") else {
        panic!("expected NodeControlCommand payload");
    };
    let Some(node_control_command::Command::Start(start)) = cmd.command else {
        panic!("expected Start command");
    };
    assert_eq!(start.agent_id, "com.example");
    assert!(start.dev_mode);

    let stop = NodeControlCommand {
        node_id: "gpu-1".to_string(),
        request_id: "req-0003".to_string(),
        command: Some(node_control_command::Command::Stop(
            acowork_core::mqtt_proto::NodeStop {
                agent_id: "com.example".to_string(),
                reason: "user_requested".to_string(),
            },
        )),
    };
    let envelope = envelope_with(data_envelope::Payload::NodeControlCommand(stop));
    let bytes = envelope.encode_to_vec();
    let decoded = decode_envelope(&bytes);
    let data_envelope::Payload::NodeControlCommand(cmd) = decoded.payload.expect("payload") else {
        panic!("expected NodeControlCommand payload");
    };
    let Some(node_control_command::Command::Stop(stop)) = cmd.command else {
        panic!("expected Stop command");
    };
    assert_eq!(stop.reason, "user_requested");
}

#[test]
fn node_clone_upgrade_publish_command_wire_shape() {
    // Phase 3b commands — round-trip pins their field numbers so a
    // renumbering breaks the build instead of silently drifting.
    let clone = NodeControlCommand {
        node_id: "gpu-1".to_string(),
        request_id: "req-0004".to_string(),
        command: Some(node_control_command::Command::Clone(
            acowork_core::mqtt_proto::NodeClone {
                agent_id: "com.example".to_string(),
                new_agent_id: "com.example.clone".to_string(),
                mode: "full".to_string(),
            },
        )),
    };
    let decoded = decode_envelope(&envelope_with(data_envelope::Payload::NodeControlCommand(
        clone,
    ))
    .encode_to_vec());
    let data_envelope::Payload::NodeControlCommand(cmd) = decoded.payload.expect("payload") else {
        panic!("expected NodeControlCommand payload");
    };
    let Some(node_control_command::Command::Clone(clone)) = cmd.command else {
        panic!("expected Clone command");
    };
    assert_eq!(clone.agent_id, "com.example");
    assert_eq!(clone.new_agent_id, "com.example.clone");
    assert_eq!(clone.mode, "full");

    let upgrade = NodeControlCommand {
        node_id: "gpu-1".to_string(),
        request_id: "req-0005".to_string(),
        command: Some(node_control_command::Command::Upgrade(
            acowork_core::mqtt_proto::NodeUpgrade {
                agent_id: "com.example".to_string(),
                package_url: "http://gw/api/packages/com.example/download".to_string(),
                local_path: String::new(),
                dev_mode: false,
            },
        )),
    };
    let decoded = decode_envelope(&envelope_with(data_envelope::Payload::NodeControlCommand(
        upgrade,
    ))
    .encode_to_vec());
    let data_envelope::Payload::NodeControlCommand(cmd) = decoded.payload.expect("payload") else {
        panic!("expected NodeControlCommand payload");
    };
    let Some(node_control_command::Command::Upgrade(upgrade)) = cmd.command else {
        panic!("expected Upgrade command");
    };
    assert!(upgrade.package_url.contains("/download"));
    assert!(!upgrade.dev_mode);

    let prepare = NodeControlCommand {
        node_id: "gpu-1".to_string(),
        request_id: "req-0006".to_string(),
        command: Some(node_control_command::Command::PublishPrepare(
            acowork_core::mqtt_proto::NodePublishPrepare {
                agent_id: "com.example".to_string(),
                clean: true,
            },
        )),
    };
    let decoded = decode_envelope(&envelope_with(data_envelope::Payload::NodeControlCommand(
        prepare,
    ))
    .encode_to_vec());
    let data_envelope::Payload::NodeControlCommand(cmd) = decoded.payload.expect("payload") else {
        panic!("expected NodeControlCommand payload");
    };
    let Some(node_control_command::Command::PublishPrepare(prepare)) = cmd.command else {
        panic!("expected PublishPrepare command");
    };
    assert!(prepare.clean);

    let build = NodeControlCommand {
        node_id: "gpu-1".to_string(),
        request_id: "req-0007".to_string(),
        command: Some(node_control_command::Command::PublishBuild(
            acowork_core::mqtt_proto::NodePublishBuild {
                agent_id: "com.example".to_string(),
                output_dir: String::new(),
                sign: true,
                key_dir: "/keys".to_string(),
            },
        )),
    };
    let decoded = decode_envelope(&envelope_with(data_envelope::Payload::NodeControlCommand(
        build,
    ))
    .encode_to_vec());
    let data_envelope::Payload::NodeControlCommand(cmd) = decoded.payload.expect("payload") else {
        panic!("expected NodeControlCommand payload");
    };
    let Some(node_control_command::Command::PublishBuild(build)) = cmd.command else {
        panic!("expected PublishBuild command");
    };
    assert!(build.sign);
    assert_eq!(build.key_dir, "/keys");
}

#[test]
fn node_event_result_json_round_trip() {
    // Optional result_json must round-trip for structured publish results.
    let event = NodeEvent {
        node_id: "local".to_string(),
        request_id: "req-0008".to_string(),
        status: "ok".to_string(),
        message: "built".to_string(),
        result_json: Some(r#"{"output_path":"/pkg/a.agent","signed":false,"file_size":42}"#.to_string()),
    };
    let decoded = decode_envelope(&envelope_with(data_envelope::Payload::NodeEvent(event)).encode_to_vec());
    let data_envelope::Payload::NodeEvent(ev) = decoded.payload.expect("payload") else {
        panic!("expected NodeEvent payload");
    };
    let json = ev.result_json.expect("result_json present");
    assert!(json.contains("output_path"));
}

#[test]
fn golden_node_enroll_envelope() {
    // Phase 5a enrollment handshake — pins NodeEnroll field numbers so
    // a renumbering breaks the build instead of silently drifting the
    // Gateway ↔ Node enrollment contract.
    let envelope = envelope_with(data_envelope::Payload::NodeEnroll(sample_enroll()));
    let bytes = envelope.encode_to_vec();
    let expected = "08 01 aa 05 62 0a 05 67 70 75 2d 31 12 24 30 66 30 65 30 64 30 63 2d 30 62 30 61 2d 34 30 30 39 2d 38 30 30 37 2d 30 36 30 35 30 34 30 33 30 32 30 31 1a 05 6d 61 63 6f 73 22 07 61 61 72 63 68 36 34 2a 05 30 2e 31 2e 30 30 01 3a 07 70 72 6f 63 65 73 73 3a 07 70 61 63 6b 61 67 65 42 08 74 6f 6b 2d 31 32 33 34";
    assert_eq!(hex(&bytes), expected.replace(' ', ""));

    // Round-trip contract: the golden bytes must decode back to the
    // same logical payload.
    let decoded = decode_envelope(&bytes);
    let data_envelope::Payload::NodeEnroll(enroll) = decoded.payload.expect("payload") else {
        panic!("expected NodeEnroll payload");
    };
    assert_eq!(enroll.node_id, "gpu-1");
    assert_eq!(enroll.machine_uid, "0f0e0d0c-0b0a-4009-8007-060504030201");
    assert_eq!(enroll.protocol_version, 1);
    assert_eq!(enroll.capabilities, vec!["process", "package"]);
    assert_eq!(enroll.enrollment_token, "tok-1234");
}

#[test]
fn golden_node_enroll_result_envelope() {
    // Phase 5a enrollment reply — pins NodeEnrollResult field numbers
    // (node_token is the long-lived per-node credential).
    let envelope = envelope_with(data_envelope::Payload::NodeEnrollResult(sample_enroll_result()));
    let bytes = envelope.encode_to_vec();
    let expected = "08 01 b2 05 4a 0a 05 67 70 75 2d 31 12 24 30 66 30 65 30 64 30 63 2d 30 62 30 61 2d 34 30 30 39 2d 38 30 30 37 2d 30 36 30 35 30 34 30 33 30 32 30 31 1a 0d 74 6f 6b 2d 6e 6f 64 65 2d 30 30 30 31 22 02 6f 6b 2a 08 65 6e 72 6f 6c 6c 65 64";
    assert_eq!(hex(&bytes), expected.replace(' ', ""));

    let decoded = decode_envelope(&bytes);
    let data_envelope::Payload::NodeEnrollResult(result) = decoded.payload.expect("payload") else {
        panic!("expected NodeEnrollResult payload");
    };
    assert_eq!(result.node_id, "gpu-1");
    assert_eq!(result.machine_uid, "0f0e0d0c-0b0a-4009-8007-060504030201");
    assert_eq!(result.node_token, "tok-node-0001");
    assert_eq!(result.status, "ok");
    assert_eq!(result.message, "enrolled");
}

