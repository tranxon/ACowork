//! MQTT protocol E2E tests (ADR-033).
//!
//! Tests each protocol layer independently:
//! 1. Broker config building
//! 2. ControlCommand protobuf encoding/decoding
//! 3. control_handler::parse_control_payload
//! 4. GatewayMqttClient control publish (requires broker)
//!
//! Note: rumqttd 0.14's Broker::start() panics inside tokio runtime.
//! Broker startup tests run in a separate OS thread via start_broker_in_thread.

use acowork_core::mqtt_proto::{
    self, control_command::Command,
    ChatMessage, ControlCommand, DataEnvelope,
    data_envelope::Payload,
};
use acowork_gateway::mqtt::broker::build_broker_config;
use acowork_runtime::mqtt::control_handler::{self, ControlAction};
use prost::Message;

// ═══════════════════════════════════════════════════════════════════════
// Test 1: Broker config building (pure function, no runtime needed)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_build_broker_config() {
    let config = build_broker_config("127.0.0.1", 19875);
    let v4 = config.v4.as_ref().expect("v4 servers must be configured");
    let server = v4.get("acowork").expect("server 'acowork' must exist");
    assert_eq!(server.listen.to_string(), "127.0.0.1:19875");
    assert_eq!(config.router.max_connections, 100);
}

#[test]
fn test_build_broker_config_custom_port() {
    let config = build_broker_config("0.0.0.0", 32100);
    let v4 = config.v4.as_ref().expect("v4 servers must be configured");
    let server = v4.get("acowork").unwrap();
    assert_eq!(server.listen.to_string(), "0.0.0.0:32100");
}

// ═══════════════════════════════════════════════════════════════════════
// Test 2: ControlCommand protobuf round-trip (ADR-034 Phase 1A)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_control_command_encode_decode() {
    let cmd = ControlCommand {
        agent_id: "com.test.agent".into(),
        command: Some(Command::ChatMessage(ChatMessage {
            session_id: "sess-001".into(),
            message_id: "msg-001".into(),
            content: "Hello MQTT".into(),
            command: String::new(),
            params_json: String::new(),
        })),
    };

    let envelope = DataEnvelope {
        version: 1,
        payload: Some(Payload::ControlCommand(cmd)),
    };

    // Encode
    let bytes = envelope.encode_to_vec();
    assert!(!bytes.is_empty());

    // Decode
    let decoded = DataEnvelope::decode(bytes.as_slice()).expect("decode");
    assert_eq!(decoded.version, 1);

    match decoded.payload {
        Some(Payload::ControlCommand(cmd)) => {
            assert_eq!(cmd.agent_id, "com.test.agent");
            match cmd.command {
                Some(Command::ChatMessage(msg)) => {
                    assert_eq!(msg.content, "Hello MQTT");
                    assert_eq!(msg.session_id, "sess-001");
                }
                _ => panic!("Expected ChatMessage command"),
            }
        }
        _ => panic!("Expected ControlCommand"),
    }
}

#[test]
fn test_control_command_stop() {
    let cmd = ControlCommand {
        agent_id: "a".into(),
        command: Some(Command::Stop(mqtt_proto::Stop {
            session_id: "s".into(),
            reason: "user_requested".into(),
        })),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();
    assert!(matches!(decoded.payload, Some(Payload::ControlCommand(_))));
}

// ═══════════════════════════════════════════════════════════════════════
// Test 3: control_handler parsing
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_parse_control_message() {
    let cmd = ControlCommand {
        agent_id: "a".into(),
        command: Some(Command::ChatMessage(ChatMessage {
            session_id: "sid-1".into(),
            message_id: "mid-1".into(),
            content: "hi".into(),
            command: String::new(),
            params_json: String::new(),
        })),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();

    let action = control_handler::parse_control_payload(
        "acowork/agents/a/sessions/control/chat_message",
        &bytes,
    );

    match action {
        Some(ControlAction::SendMessage { session_id, content, .. }) => {
            assert_eq!(session_id, "sid-1");
            assert_eq!(content, "hi");
        }
        _ => panic!("Expected SendMessage, got {:?}", action),
    }
}

#[test]
fn test_parse_control_stop() {
    let cmd = ControlCommand {
        agent_id: "a".into(),
        command: Some(Command::Stop(mqtt_proto::Stop {
            session_id: "s".into(),
            reason: String::new(),
        })),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();

    let action = control_handler::parse_control_payload("acowork/agents/a/sessions/control/stop", &bytes);
    assert!(matches!(action, Some(ControlAction::StopGeneration { .. })));
}

// ADR-045: CancelTool payload must parse to ControlAction::CancelTool
// carrying both session_id and tool_call_id verbatim.
#[test]
fn test_parse_control_cancel_tool() {
    let cmd = ControlCommand {
        agent_id: "a".into(),
        command: Some(Command::CancelTool(mqtt_proto::CancelTool {
            session_id: "s".into(),
            tool_call_id: "call_abc123".into(),
        })),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();

    let action = control_handler::parse_control_payload(
        "acowork/agents/a/sessions/control/cancel_tool",
        &bytes,
    );
    match action {
        Some(ControlAction::CancelTool { session_id, tool_call_id }) => {
            assert_eq!(session_id, "s");
            assert_eq!(tool_call_id, "call_abc123");
        }
        other => panic!("Expected ControlAction::CancelTool, got {:?}", other),
    }
}

#[test]
fn test_parse_control_create_session() {
    // ADR-034 Phase 1A: CreateSession has no fields (agent_id moved
    // to ControlCommand top-level, no per-subcommand fields).
    let cmd = ControlCommand {
        agent_id: "a".into(),
        command: Some(Command::CreateSession(mqtt_proto::CreateSession {})),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();
    let action = control_handler::parse_control_payload("", &bytes);
    assert!(matches!(action, Some(ControlAction::CreateSession)));
}

#[test]
fn test_parse_invalid_payload() {
    let action = control_handler::parse_control_payload("test", b"not valid protobuf");
    assert!(action.is_none());
}

// ═══════════════════════════════════════════════════════════════════════
// Test 4: AvailableProviders serialization
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_available_providers_roundtrip() {
    let providers = mqtt_proto::AvailableProviders {
        version: 42,
        providers: vec![mqtt_proto::ProviderRef {
            id: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol_type: mqtt_proto::LlmProtocol::Openai.into(),
            compact_model: String::new(),
            custom: false,
            models: vec![],
            api_key: String::new(),
        }],
    };

    let env = DataEnvelope { version: 1, payload: Some(Payload::AvailableProviders(providers)) };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();

    match decoded.payload {
        Some(Payload::AvailableProviders(p)) => {
            assert_eq!(p.version, 42);
            assert_eq!(p.providers.len(), 1);
            assert_eq!(p.providers[0].id, "openai");
        }
        _ => panic!("Expected AvailableProviders"),
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Phase 9 §12.1 regression matrix (5 scenarios — ADR-034 Phase 9 #4)
// ═════════════════════════════════════════════════════════════════════════
//
// Each scenario verifies protobuf round-trip + control_handler dispatch,
// pinning the wire format / dispatch contract so future refactors of the
// `*Command` → `ControlAction` mapping cannot silently regress the rich-field
// or fallback semantics that §12.1 calls out.

/// §12.1 #7-9: ChatMessage carries rich fields via `params_json`
/// (attached_items / content_parts). Verify all three sub-shapes
/// survive encode → decode → dispatch as a single opaque JSON
/// blob (Runtime is responsible for parsing the inner shape).
#[test]
fn phase9_chat_message_rich_fields_via_params_json() {
    // Composed payload mirroring a real frontend chat_send invocation:
    // - multimodal image_url part
    // - one uploaded document (file_upload)
    // - one attached file selection
    let rich_params = serde_json::json!({
        "attached_items": [
            {"type": "file_upload", "documentId": "doc-abc-123", "filename": "report.pdf", "format": "pdf", "sizeBytes": 12345},
            {"type": "attached_selection", "absPath": "/workspace/foo.rs", "name": "foo.rs", "startLine": 10, "endLine": 25},
        ],
        "content_parts": [
            {"type": "text", "text": "see this image:"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ],
    });
    let params_json = serde_json::to_string(&rich_params).unwrap();

    let cmd = ControlCommand {
        agent_id: "com.acowork.test".into(),
        command: Some(Command::ChatMessage(ChatMessage {
            session_id: "sess-rich".into(),
            message_id: "msg-rich".into(),
            content: "see this image:".into(),
            command: String::new(),
            params_json: params_json.clone(),
        })),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();
    let _decoded = DataEnvelope::decode(bytes.as_slice()).expect("decode");

    // Round-trip the full envelope (parse_control_payload expects DataEnvelope
    // bytes, not bare ControlCommand bytes) and verify the dispatch produces
    // SendMessage with the same rich JSON payload intact.
    let action = control_handler::parse_control_payload(
        "acowork/agents/com.acowork.test/sessions/sess-rich/control/chat_message",
        &bytes,
    );
    match action {
        Some(ControlAction::SendMessage {
            session_id,
            content,
            params_json: action_params,
            ..
        }) => {
            assert_eq!(session_id, "sess-rich");
            assert_eq!(content, "see this image:");
            // JSON-level deep equality after re-parse (since proto wire
            // preserves string bytes verbatim, but key order may differ).
            let lhs: serde_json::Value = serde_json::from_str(&action_params).unwrap();
            let rhs: serde_json::Value = serde_json::from_str(&params_json).unwrap();
            assert_eq!(lhs, rhs, "rich params_json must survive round-trip");
        }
        other => panic!("Expected SendMessage, got {:?}", other),
    }
}

/// §12.1 #10: Stop carries `reason` for logging. Verify that the free-form
/// `reason` string round-trips verbatim and surfaces in
/// `ControlAction::StopGeneration` so the downstream `tracing::warn!` /
/// distillation reason can carry the upstream context.
#[test]
fn phase9_stop_with_reason_roundtrip() {
    let cmd = ControlCommand {
        agent_id: "com.acowork.test".into(),
        command: Some(Command::Stop(mqtt_proto::Stop {
            session_id: "sess-stop".into(),
            reason: "iteration_limit".into(),
        })),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();
    // decode round-trip just verifies wire stability; dispatch reads the
    // original envelope bytes.
    let _ = decoded;

    let action = control_handler::parse_control_payload("acowork/agents/com.acowork.test/sessions/sess-stop/control/stop", &bytes);
    match action {
        Some(ControlAction::StopGeneration { session_id, reason }) => {
            assert_eq!(session_id, "sess-stop");
            assert_eq!(reason, "iteration_limit");
        }
        other => panic!("Expected StopGeneration, got {:?}", other),
    }
}

/// §12.1 #16-17: ModelSwitch preserves `provider_id` through the wire.
/// Empty provider_id must normalize to `None` (legacy "model-only" semantics),
/// non-empty provider_id must surface as `Some(provider_id)` so the Runtime
/// rebuilds the per-session Provider instance.
#[test]
fn phase9_model_switch_provider_id_normalization() {
    // Same-provider path: provider_id empty -> None
    let cmd_same = ControlCommand {
        agent_id: "com.acowork.test".into(),
        command: Some(Command::ModelSwitch(mqtt_proto::ModelSwitch {
            session_id: "s".into(),
            model_id: "gpt-4o-mini".into(),
            provider_id: String::new(),
        })),
    };
    let env_same = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd_same)) };
    let action_same = control_handler::parse_control_payload("acowork/agents/com.acowork.test/sessions/s/control/model_switch", &env_same.encode_to_vec());
    match action_same {
        Some(ControlAction::ModelSwitch { model_id, provider_id, .. }) => {
            assert_eq!(model_id, "gpt-4o-mini");
            assert_eq!(provider_id, None, "empty provider_id must normalize to None (legacy same-provider semantics)");
        }
        other => panic!("Expected ModelSwitch, got {:?}", other),
    }

    // Cross-provider path: provider_id "minimax" -> Some("minimax")
    let cmd_x = ControlCommand {
        agent_id: "com.acowork.test".into(),
        command: Some(Command::ModelSwitch(mqtt_proto::ModelSwitch {
            session_id: "s".into(),
            model_id: "MiniMax-Text-01".into(),
            provider_id: "minimax".into(),
        })),
    };
    let env_x = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd_x)) };
    let action_x = control_handler::parse_control_payload("acowork/agents/com.acowork.test/sessions/s/control/model_switch", &env_x.encode_to_vec());
    match action_x {
        Some(ControlAction::ModelSwitch { model_id, provider_id, .. }) => {
            assert_eq!(model_id, "MiniMax-Text-01");
            assert_eq!(provider_id, Some("minimax".to_string()), "non-empty provider_id must surface as Some for Runtime to rebuild Provider");
        }
        other => panic!("Expected ModelSwitch, got {:?}", other),
    }
}

/// §12.1 #22-23: CompressAction distinguishes SUMMARY (1) vs
/// TOOL_RESULTS (2). Verify both enum values round-trip verbatim as i32
/// and dispatch to `ControlAction::CompressAction` with the right value.
#[test]
fn phase9_compress_action_summary_vs_tool_results() {
    let cases = [
        (1i32, "COMPRESS_TYPE_SUMMARY"),
        (2i32, "COMPRESS_TYPE_TOOL_RESULTS"),
    ];
    for (compress_type_i32, label) in cases {
        let cmd = ControlCommand {
            agent_id: "com.acowork.test".into(),
            command: Some(Command::CompressAction(mqtt_proto::CompressAction {
                session_id: "s".into(),
                compress_type: compress_type_i32,
            })),
        };
        let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
        let bytes = env.encode_to_vec();
        let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();
        // decode round-trip verifies wire stability; dispatch reads envelope bytes.
        let _ = decoded;

        let action = control_handler::parse_control_payload("acowork/agents/com.acowork.test/sessions/s/control/compress_action", &bytes);
        match action {
            Some(ControlAction::CompressAction { session_id, compress_type }) => {
                assert_eq!(session_id, "s");
                assert_eq!(
                    compress_type, compress_type_i32,
                    "CompressType {} must round-trip as i32 {}",
                    label, compress_type_i32
                );
            }
            other => panic!("Expected CompressAction for {}, got {:?}", label, other),
        }
    }
}

/// §12.1 #20: WorkspaceSwitch with non-existent workspace_id still
/// dispatches to `ControlAction::WorkspaceSwitch` so the Runtime can
/// apply its `add_pending_workspace + fallback __agent_home__` rule
/// downstream. The fallback itself is exercised in the Runtime's
/// session manager tests; this pin guards the dispatch contract.
#[test]
fn phase9_workspace_switch_unknown_id_dispatches() {
    let cmd = ControlCommand {
        agent_id: "com.acowork.test".into(),
        command: Some(Command::WorkspaceSwitch(mqtt_proto::WorkspaceSwitch {
            session_id: "s".into(),
            workspace_id: "ghost-workspace-xyz".into(), // deliberately not installed
        })),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();
    // decode round-trip verifies wire stability; dispatch reads envelope bytes.
    let _ = decoded;

    let action = control_handler::parse_control_payload(
        "acowork/agents/com.acowork.test/sessions/s/control/workspace_switch",
        &bytes,
    );
    match action {
        Some(ControlAction::WorkspaceSwitch { session_id, workspace_id }) => {
            assert_eq!(session_id, "s");
            assert_eq!(workspace_id, "ghost-workspace-xyz",
                "control_handler must NOT pre-filter unknown IDs — fallback policy is downstream");
        }
        other => panic!("Expected WorkspaceSwitch, got {:?}", other),
    }
}

// ═════════════════════════════════════════════════════════════════════════
// ADR-038: Session lifecycle explicit model — OpenSession command + acks
// ═════════════════════════════════════════════════════════════════════════

#[test]
fn test_control_command_open_session_encode_decode() {
    // OpenSession carries only session_id (wire compat with legacy
    // activate_session envelope shape; new semantic per ADR-038).
    let cmd = ControlCommand {
        agent_id: "com.acowork.test".into(),
        command: Some(Command::OpenSession(mqtt_proto::OpenSession {
            session_id: "sess-closed-001".into(),
        })),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();
    match decoded.payload {
        Some(Payload::ControlCommand(cmd)) => match cmd.command {
            Some(Command::OpenSession(os)) => assert_eq!(os.session_id, "sess-closed-001"),
            other => panic!("Expected OpenSession, got {:?}", other),
        },
        _ => panic!("Expected ControlCommand"),
    }
}

#[test]
fn test_parse_control_open_session() {
    let cmd = ControlCommand {
        agent_id: "com.acowork.test".into(),
        command: Some(Command::OpenSession(mqtt_proto::OpenSession {
            session_id: "sess-002".into(),
        })),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::ControlCommand(cmd)) };
    let bytes = env.encode_to_vec();

    let action = control_handler::parse_control_payload(
        "acowork/agents/com.acowork.test/sessions/sess-002/control/open_session",
        &bytes,
    );
    match action {
        Some(ControlAction::OpenSession { session_id }) => {
            assert_eq!(session_id, "sess-002");
        }
        other => panic!("Expected OpenSession ControlAction, got {:?}", other),
    }
}

#[test]
fn test_session_opened_event_roundtrip() {
    let evt = mqtt_proto::SessionOpened {
        session_id: "sess-001".into(),
        status: "resumed_from_disk".into(),
        model: "gpt-4o".into(),
        provider: "openai".into(),
        last_active_at: "2026-07-17T12:34:56Z".into(),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::SessionOpened(evt)) };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();
    match decoded.payload {
        Some(Payload::SessionOpened(s)) => {
            assert_eq!(s.session_id, "sess-001");
            assert_eq!(s.status, "resumed_from_disk");
            assert_eq!(s.model, "gpt-4o");
            assert_eq!(s.provider, "openai");
            assert_eq!(s.last_active_at, "2026-07-17T12:34:56Z");
        }
        _ => panic!("Expected SessionOpened event"),
    }
}

#[test]
fn test_session_not_opened_event_roundtrip() {
    let evt = mqtt_proto::SessionNotOpened {
        session_id: "sess-closed-002".into(),
        attempted_command: "chat_message".into(),
        reason: "session_closed".into(),
    };
    let env = DataEnvelope { version: 1, payload: Some(Payload::SessionNotOpened(evt)) };
    let bytes = env.encode_to_vec();
    let decoded = DataEnvelope::decode(bytes.as_slice()).unwrap();
    match decoded.payload {
        Some(Payload::SessionNotOpened(s)) => {
            assert_eq!(s.session_id, "sess-closed-002");
            assert_eq!(s.attempted_command, "chat_message");
            assert_eq!(s.reason, "session_closed");
        }
        _ => panic!("Expected SessionNotOpened event"),
    }
}

#[test]
fn test_session_lifecycle_state_machine_enum() {
    // Pure type-level smoke test: verifies SessionLifecycleState variants
    // exist and compare correctly. Runtime semantics are covered by
    // SessionManager integration tests.
    use acowork_runtime::agent::session::{
        SessionLifecycleState as S, SessionOpenOutcome as O,
    };
    assert_eq!(S::NotFound, S::NotFound);
    assert_eq!(S::Closed, S::Closed);
    assert_eq!(S::Active, S::Active);
    assert_ne!(S::Active, S::Closed);
    assert_eq!(O::AlreadyActive, O::AlreadyActive);
    assert_eq!(O::ResumedFromDisk, O::ResumedFromDisk);
    assert_ne!(O::AlreadyActive, O::ResumedFromDisk);
}

// ═══════════════════════════════════════════════════════════════════════
// ADR-046: ChatMessage shape after image-pipeline merge
// ═══════════════════════════════════════════════════════════════════════
//
// `phase9_chat_message_rich_fields_via_params_json` (above) verifies that
// the wire-level `params_json` survives encode → decode → dispatch. That
// covers the bytes path. This test covers the *shape* path that the LLM
// actually consumes: when the desktop sends only `attached_items` (no
// inline `content_parts` — the ADR-046 default), the Runtime's
// `derive_image_parts` + `merge_content_parts` pipeline must produce a
// `ChatMessage` carrying `ContentPart::ImageUrl` entries with valid data
// URLs. If this test fails, the LLM would receive a plain text message
// and have no way to see the picture — exactly the user-reported bug.

#[tokio::test]
async fn adr046_image_pipeline_produces_multimodal_chat_message_shape() {
    use acowork_core::providers::traits::{ChatMessage as CoreChatMessage, ContentPart};
    use acowork_core::protocol::AttachedItem;
    use acowork_runtime::agent::attachment_to_image::{
        derive_image_parts, merge_content_parts,
    };
    use std::sync::Arc;

    // Stub AttachmentService: returns the fake bytes for `img-1` /
    // `img-2` and errors otherwise. Mirrors the shape used by
    // `attachment_to_image.rs` unit tests, lifted to a top-level
    // integration test so the public API surface stays pinned.
    struct FakeAttachment;
    #[async_trait::async_trait]
    impl acowork_runtime::usecases::AttachmentService for FakeAttachment {
        async fn upload_file(
            &self,
            _params: acowork_runtime::usecases::attachment::UploadFileParams,
        ) -> Result<
            acowork_runtime::usecases::attachment::UploadedFileResponse,
            acowork_runtime::usecases::attachment::AttachmentError,
        > {
            unimplemented!()
        }
        async fn read_file(
            &self,
            document_id: &str,
        ) -> Result<
            Vec<u8>,
            acowork_runtime::usecases::attachment::AttachmentError,
        > {
            match document_id {
                "img-1" => Ok(b"\x89PNG\r\n\x1a\nfake-png".to_vec()),
                "img-2" => Ok(b"\xff\xd8\xff\xe0fake-jpg".to_vec()),
                other => Err(
                    acowork_runtime::usecases::attachment::AttachmentError::NotFound(
                        other.to_string(),
                    ),
                ),
            }
        }
    }
    let svc: Arc<dyn acowork_runtime::usecases::AttachmentService> = Arc::new(FakeAttachment);

    // Simulated desktop chat_send payload: user typed "see these:" and
    // attached two images (no inline content_parts — this is the
    // ADR-046 default that triggers the bug if the pipeline is missing).
    let user_text = "see these:";
    let attached_items = vec![
        AttachedItem::ImageUpload {
            document_id: "img-1".into(),
            filename: "a.png".into(),
            format: "png".into(),
            size_bytes: 9,
            width: Some(640),
            height: Some(480),
        },
        AttachedItem::ImageUpload {
            document_id: "img-2".into(),
            filename: "b.jpg".into(),
            format: "jpg".into(),
            size_bytes: 12,
            width: None,
            height: None,
        },
    ];
    let frontend_content_parts: Option<Vec<ContentPart>> = None;

    // Run the same derivation SessionTask does:
    let derived = derive_image_parts(Some(&svc), &attached_items)
        .await
        .expect("derive_image_parts succeeds");
    let merged = merge_content_parts(frontend_content_parts, derived);

    // Build the exact ChatMessage shape the agent loop will pass to the
    // LLM. `user_multimodal` takes (text_for_logging, parts).
    let msg = CoreChatMessage::user_multimodal(user_text, merged.expect("merged must be Some"));

    // ── Assertions ──
    let parts = msg
        .content_parts
        .as_ref()
        .expect("user_multimodal must populate content_parts");
    assert_eq!(parts.len(), 2, "expected exactly 2 image parts");

    // Order must match attached_items order.
    match &parts[0] {
        ContentPart::ImageUrl { image_url } => {
            assert!(
                image_url.url.starts_with("data:image/png;base64,"),
                "first part must be PNG, got prefix {:?}",
                &image_url.url[..32.min(image_url.url.len())]
            );
            assert_eq!(image_url.width, Some(640));
            assert_eq!(image_url.height, Some(480));
        }
        other => panic!("expected ImageUrl for img-1, got {other:?}"),
    }
    match &parts[1] {
        ContentPart::ImageUrl { image_url } => {
            assert!(
                image_url.url.starts_with("data:image/jpeg;base64,"),
                "second part must be JPEG, got prefix {:?}",
                &image_url.url[..32.min(image_url.url.len())]
            );
            assert_eq!(image_url.width, None);
            assert_eq!(image_url.height, None);
        }
        other => panic!("expected ImageUrl for img-2, got {other:?}"),
    }

    // Regression guard against the `build_data_url` `;`-bug: the URL
    // MUST contain ";base64," (not "base64," without the semicolon).
    // This is the exact failure mode that made the LLM reject the data
    // URI as malformed even when the rest of the pipeline worked.
    for (i, part) in parts.iter().enumerate() {
        if let ContentPart::ImageUrl { image_url } = part {
            assert!(
                image_url.url.contains(";base64,"),
                "part[{i}] data URL must use ';base64,' (RFC 2397); got {:?}",
                &image_url.url[..32.min(image_url.url.len())]
            );
        }
    }
}
