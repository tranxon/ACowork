//! S1.18 End-to-end integration tests — Groups E/F/G/H (T30–T50)
//!
//! E: Episode distillation (T30–T34)
//! F: Agent package data isolation (T38–T42)
//! G: End-to-end full pipeline (T43–T45)
//! H: Boundary & error cases (T46–T50)
//!
//! These tests cover cross-module integrations:
//! - rollball-runtime (ConversationSession, EpisodeDistiller)
//! - rollball-core (PackageOptions, ModelCapabilitiesInfo)
//! - rollball-grafeo (GrafeoStore, export_nodes_filtered)

use std::io::Write as IoWrite;

use tempfile::TempDir;

use rollball_core::packaging::{PackageOptions, should_exclude_path};
use rollball_core::protocol::{ModelCapabilitiesInfo, ModelCostInfo};
use rollball_grafeo::export::FilteredNode;
use rollball_grafeo::grafeo::GrafeoStore;
use rollball_runtime::conversation::{
    ConversationSession, SessionMetadata,
    generate_session_id, read_messages_paginated, read_session_metadata,
};
use rollball_runtime::episode_distill::{DistilledEpisode, EpisodeDistiller, model_cost_score};

// ═══════════════════════════════════════════════════════════════════════
// Group E: Episode distillation tests (T30–T34)
// ═══════════════════════════════════════════════════════════════════════

/// T30: Model selection — select_cheapest_model returns the cheapest model.
#[test]
fn test_t30_select_cheapest_model() {
    let models = vec![
        ModelCapabilitiesInfo {
            context_window: 32768,
            max_output_tokens: 4096,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: Some(ModelCostInfo {
                input_per_million: Some(5.0),
                output_per_million: Some(15.0),
            }),
            modalities: None,
            name: Some("expensive-model".to_string()),
            family: None,
            knowledge_cutoff: None,
        },
        ModelCapabilitiesInfo {
            context_window: 8192,
            max_output_tokens: 2048,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: Some(ModelCostInfo {
                input_per_million: Some(0.1),
                output_per_million: Some(0.2),
            }),
            modalities: None,
            name: Some("cheap-model".to_string()),
            family: None,
            knowledge_cutoff: None,
        },
        ModelCapabilitiesInfo {
            context_window: 128000,
            max_output_tokens: 8192,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: Some("unknown-cost-model".to_string()),
            family: None,
            knowledge_cutoff: None,
        },
    ];

    let selected = EpisodeDistiller::select_cheapest_model(&models);
    assert!(selected.is_some());
    assert_eq!(selected.unwrap().name.as_deref(), Some("cheap-model"));
}

/// T30b: Empty model list returns None.
#[test]
fn test_t30b_select_cheapest_empty() {
    let models: Vec<ModelCapabilitiesInfo> = vec![];
    let selected = EpisodeDistiller::select_cheapest_model(&models);
    assert!(selected.is_none());
}

/// T31: Episode distillation output format validation.
#[test]
fn test_t31_distilled_episode_format() {
    let episode = DistilledEpisode {
        session_id: "20260503_120000_abc123".to_string(),
        summary: "User asked about Rust async patterns".to_string(),
        intent_type: "coding".to_string(),
        decision: Some("Use tokio for async runtime".to_string()),
        tool_summary: Some("file_read(3 times), search(1 time)".to_string()),
        keywords: vec!["rust".to_string(), "tokio".to_string(), "async".to_string()],
        importance: 0.85,
        source_session_id: "20260503_120000_abc123".to_string(),
        consolidated: false,
        distill_offset: 42,
    };

    assert_eq!(episode.session_id, "20260503_120000_abc123");
    assert_eq!(episode.summary, "User asked about Rust async patterns");
    assert_eq!(episode.intent_type, "coding");
    assert!(episode.decision.is_some());
    assert!(episode.tool_summary.is_some());
    assert_eq!(episode.keywords.len(), 3);
    assert!((episode.importance - 0.85).abs() < 0.001);
    assert!(!episode.consolidated);
    assert_eq!(episode.distill_offset, 42);

    // Verify serializable
    let json = serde_json::to_string(&episode).unwrap();
    let restored: DistilledEpisode = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.session_id, episode.session_id);
    assert_eq!(restored.keywords, episode.keywords);
}

/// T32: All models unavailable — graceful degradation (no panic).
#[test]
fn test_t32_all_models_unavailable_no_panic() {
    let models = vec![
        ModelCapabilitiesInfo {
            context_window: 8192,
            max_output_tokens: 2048,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: Some("no-cost-1".to_string()),
            family: None,
            knowledge_cutoff: None,
        },
        ModelCapabilitiesInfo {
            context_window: 8192,
            max_output_tokens: 2048,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: Some("no-cost-2".to_string()),
            family: None,
            knowledge_cutoff: None,
        },
    ];

    let selected = EpisodeDistiller::select_cheapest_model(&models);
    assert!(
        selected.is_some(),
        "Should still return a model when all have no cost info"
    );

    for model in &models {
        let score = model_cost_score(model);
        assert_eq!(
            score, f64::MAX,
            "Models without cost info should have MAX cost score"
        );
    }
}

/// T33: Repeated distillation prevention — distill_offset mechanism.
#[test]
fn test_t33_distill_offset_prevention() {
    let mut episode = DistilledEpisode {
        session_id: "sess-1".to_string(),
        summary: "Test".to_string(),
        intent_type: "coding".to_string(),
        decision: None,
        tool_summary: None,
        keywords: vec![],
        importance: 0.5,
        source_session_id: "sess-1".to_string(),
        consolidated: false,
        distill_offset: 10,
    };

    // First distillation batch ends at line 10
    assert_eq!(episode.distill_offset, 10);

    // Second batch would start from offset 10
    episode.distill_offset = 25;
    assert_eq!(episode.distill_offset, 25);

    // Verify the offset is persisted in JSON
    let json = serde_json::to_string(&episode).unwrap();
    assert!(json.contains("\"distill_offset\":25"));
}

/// T34: Consolidated flag — new Episode is always false.
#[test]
fn test_t34_new_episode_consolidated_false() {
    let episode = DistilledEpisode {
        session_id: "sess-1".to_string(),
        summary: "Freshly distilled".to_string(),
        intent_type: "planning".to_string(),
        decision: None,
        tool_summary: None,
        keywords: vec!["test".to_string()],
        importance: 0.7,
        source_session_id: "sess-1".to_string(),
        consolidated: false,
        distill_offset: 0,
    };

    assert!(
        !episode.consolidated,
        "New episodes must have consolidated=false"
    );

    let mut consolidated_episode = episode.clone();
    consolidated_episode.consolidated = true;
    assert!(consolidated_episode.consolidated, "Consolidation should mark it true");
}

// ═══════════════════════════════════════════════════════════════════════
// Group F: Agent package data isolation tests (T38–T42)
// ═══════════════════════════════════════════════════════════════════════

/// T38: Default PackageOptions validation — correct include/exclude defaults.
#[test]
fn test_t38_default_package_options() {
    let opts = PackageOptions::default();

    assert!(!opts.include_conversations, "conversations excluded by default");
    assert!(!opts.include_episodes, "episodes excluded by default");
    assert!(!opts.include_private_knowledge, "private knowledge excluded by default");
    assert!(!opts.include_config, "config excluded by default");

    assert!(opts.include_procedural, "procedural included by default");
    assert!(opts.include_autobiographical, "autobiographical included by default");
    assert!(opts.include_public_knowledge, "public knowledge included by default");
}

/// T39: should_exclude_path excludes conversations directory by default.
#[test]
fn test_t39_conversations_excluded_by_default() {
    let opts = PackageOptions::default();
    assert!(should_exclude_path("conversations/session.jsonl", &opts));
    assert!(should_exclude_path("conversations/20260503_abc.jsonl", &opts));
    assert!(should_exclude_path("conversations/subdir/file.jsonl", &opts));
}

/// T40: should_exclude_path includes conversations when include_conversations=true.
#[test]
fn test_t40_conversations_included_when_opted_in() {
    let opts = PackageOptions {
        include_conversations: true,
        ..Default::default()
    };
    assert!(!should_exclude_path("conversations/session.jsonl", &opts));
    assert!(!should_exclude_path("conversations/20260503_abc.jsonl", &opts));
}

/// T41: export_nodes_filtered by default excludes Episode and Private Knowledge.
#[test]
fn test_t41_export_filtered_default_excludes_episodes_and_private() {
    let store = GrafeoStore::new_in_memory().unwrap();

    // Store an Episode
    let test_dt = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    store
        .store_episode(&rollball_grafeo::types::Episode {
            id: None,
            session_id: "sess-1".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: "episode data".to_string(),
            content_type: rollball_grafeo::types::ContentType::Informational,
            embedding: None,
            timestamp: test_dt,
            consolidated: false,
            metadata: std::collections::HashMap::new(),
            artifact_refs: vec![],
            importance: 0.5,
        })
        .unwrap();

    // Store a public Knowledge node
    let mut pub_meta = std::collections::HashMap::new();
    pub_meta.insert("privacy".to_string(), serde_json::Value::String("Public".to_string()));
    store
        .store_knowledge(&rollball_grafeo::types::KnowledgeNode {
            id: None,
            subject: "agent".to_string(),
            predicate: "framework".to_string(),
            object: "RollBall".to_string(),
            sub_type: rollball_grafeo::types::KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: rollball_grafeo::types::NodeStatus::Active,
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            updated_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            metadata: pub_meta,
        })
        .unwrap();

    // Store a private Knowledge node
    let mut priv_meta = std::collections::HashMap::new();
    priv_meta.insert("privacy".to_string(), serde_json::Value::String("Personal".to_string()));
    store
        .store_knowledge(&rollball_grafeo::types::KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "name".to_string(),
            object: "Alice".to_string(),
            sub_type: rollball_grafeo::types::KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: rollball_grafeo::types::NodeStatus::Active,
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            updated_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            metadata: priv_meta,
        })
        .unwrap();

    let opts = PackageOptions::default();
    let filtered = store.export_nodes_filtered(&opts).unwrap();

    let filtered_labels: Vec<&str> = filtered.iter().map(|n| n.label.as_str()).collect();

    assert!(
        !filtered_labels.contains(&"Episodic"),
        "Episodes should be excluded by default"
    );

    let knowledge_nodes: Vec<&FilteredNode> =
        filtered.iter().filter(|n| n.label == "Knowledge").collect();
    assert_eq!(knowledge_nodes.len(), 1, "Only public knowledge should be included");
    assert_eq!(knowledge_nodes[0].data["subject"], "agent");
}

/// T42: export_nodes_filtered with all options includes all nodes.
#[test]
fn test_t42_export_filtered_all_included() {
    let store = GrafeoStore::new_in_memory().unwrap();

    // Store an Episode
    let test_dt = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    store
        .store_episode(&rollball_grafeo::types::Episode {
            id: None,
            session_id: "sess-1".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: "episode data".to_string(),
            content_type: rollball_grafeo::types::ContentType::Informational,
            embedding: None,
            timestamp: test_dt,
            consolidated: false,
            metadata: std::collections::HashMap::new(),
            artifact_refs: vec![],
            importance: 0.5,
        })
        .unwrap();

    // Store a Procedural node
    store
        .store_procedural(&rollball_grafeo::types::ProceduralNode {
            id: None,
            name: "deploy_flow".to_string(),
            trigger_condition: "always".to_string(),
            action_pattern: "do stuff".to_string(),
            success_count: 5,
            fail_count: 0,
            confidence: 0.9,
            embedding: None,
            status: rollball_grafeo::types::NodeStatus::Active,
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            updated_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            metadata: std::collections::HashMap::new(),
        })
        .unwrap();

    // Store an Autobiographical node
    store
        .store_autobiographical(&rollball_grafeo::types::AutobiographicalNode {
            id: None,
            category: rollball_grafeo::types::AutobioCategory::Identity,
            key: "name".to_string(),
            value: "TestBot".to_string(),
            confidence: 1.0,
            source_episode_id: None,
            embedding: None,
            status: rollball_grafeo::types::NodeStatus::Active,
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            updated_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            metadata: std::collections::HashMap::new(),
        })
        .unwrap();

    // Store public + private Knowledge
    let mut pub_meta = std::collections::HashMap::new();
    pub_meta.insert("privacy".to_string(), serde_json::Value::String("Public".to_string()));
    store
        .store_knowledge(&rollball_grafeo::types::KnowledgeNode {
            id: None,
            subject: "agent".to_string(),
            predicate: "framework".to_string(),
            object: "RollBall".to_string(),
            sub_type: rollball_grafeo::types::KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: rollball_grafeo::types::NodeStatus::Active,
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            updated_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            metadata: pub_meta,
        })
        .unwrap();

    let mut priv_meta = std::collections::HashMap::new();
    priv_meta.insert("privacy".to_string(), serde_json::Value::String("Personal".to_string()));
    store
        .store_knowledge(&rollball_grafeo::types::KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "name".to_string(),
            object: "Alice".to_string(),
            sub_type: rollball_grafeo::types::KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: rollball_grafeo::types::NodeStatus::Active,
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            updated_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            metadata: priv_meta,
        })
        .unwrap();

    let opts = PackageOptions {
        include_conversations: true,
        include_episodes: true,
        include_private_knowledge: true,
        include_procedural: true,
        include_autobiographical: true,
        include_public_knowledge: true,
        include_config: true,
    };
    let filtered = store.export_nodes_filtered(&opts).unwrap();

    let filtered_labels: Vec<&str> = filtered.iter().map(|n| n.label.as_str()).collect();

    assert!(filtered_labels.contains(&"Episodic"), "Episodes should be included");
    assert!(
        filtered.iter().filter(|n| n.label == "Knowledge").count() == 2,
        "Both public and private knowledge should be included"
    );
    assert!(filtered_labels.contains(&"Procedural"), "Procedural should be included");
    assert!(
        filtered_labels.contains(&"Autobiographical"),
        "Autobiographical should be included"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Group G: End-to-end full pipeline tests (T43–T45)
// ═══════════════════════════════════════════════════════════════════════

/// T43: Full pipeline — create session → write → read → verify recovery.
#[test]
fn test_t43_full_pipeline_create_write_read_verify() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session_id = generate_session_id();

    let session = ConversationSession::new(work_dir, &session_id, "com.test.agent").unwrap();
    session.append_message("user", "Hello, can you help me?", None);
    session.append_message("assistant", "Of course! What do you need?", None);
    session.append_message("user", "I need help with Rust.", None);
    session.append_message("assistant", "Sure, I'm happy to help with Rust!", None);

    std::thread::sleep(std::time::Duration::from_millis(50));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { session.close().await.unwrap(); });

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    assert!(jsonl_path.exists(), "Session file should exist");

    let meta = read_session_metadata(&jsonl_path).unwrap();
    assert_eq!(meta.session_id, session_id);
    assert_eq!(meta.agent_id, "com.test.agent");

    let page = read_messages_paginated(&jsonl_path, None, 100, "backward").unwrap();
    assert_eq!(page.messages.len(), 4, "All 4 messages should be readable");

    assert_eq!(page.messages[0].role, "user");
    assert_eq!(page.messages[0].content, "Hello, can you help me?");
    assert_eq!(page.messages[1].role, "assistant");
    assert_eq!(page.messages[1].content, "Of course! What do you need?");
    assert_eq!(page.messages[2].role, "user");
    assert_eq!(page.messages[2].content, "I need help with Rust.");
    assert_eq!(page.messages[3].role, "assistant");
    assert_eq!(page.messages[3].content, "Sure, I'm happy to help with Rust!");
}

/// T44: Long conversation — 50+ messages → paginated read → verify complete.
#[test]
fn test_t44_long_conversation_paginated_read() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session_id = generate_session_id();

    let session = ConversationSession::new(work_dir, &session_id, "com.test.agent").unwrap();

    for i in 0..60 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        session.append_message(role, &format!("Message {i}"), None);
    }

    std::thread::sleep(std::time::Duration::from_millis(100));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { session.close().await.unwrap(); });

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    assert!(jsonl_path.exists());

    // Read all at once
    let all_page = read_messages_paginated(&jsonl_path, None, 100, "backward").unwrap();
    assert_eq!(all_page.messages.len(), 60, "Should read all 60 messages");

    // Read with pagination — first page of 25
    let page1 = read_messages_paginated(&jsonl_path, None, 25, "backward").unwrap();
    assert_eq!(page1.messages.len(), 25);
    assert!(page1.has_more, "Should have more messages");
    // Verify cursor is offset format
    let cursor1 = page1.cursor.as_ref().unwrap().clone();
    assert!(cursor1.starts_with("offset:"), "Cursor should be offset format, got: {}", cursor1);

    // Continue backward from cursor
    let page2 = read_messages_paginated(&jsonl_path, Some(cursor1), 25, "backward").unwrap();
    assert_eq!(page2.messages.len(), 25);
    assert!(page2.has_more, "Should still have more messages");
    let cursor2 = page2.cursor.as_ref().unwrap().clone();
    assert!(cursor2.starts_with("offset:"));

    // Third page — remaining 10 messages
    let page3 = read_messages_paginated(&jsonl_path, Some(cursor2), 25, "backward").unwrap();
    assert_eq!(page3.messages.len(), 10);
    assert!(!page3.has_more, "No more messages");
}

/// T45: Multi-agent isolation — sessions from different workspaces don't interfere.
#[test]
fn test_t45_multi_agent_isolation() {
    let temp_dir1 = TempDir::new().unwrap();
    let temp_dir2 = TempDir::new().unwrap();
    let work_dir1 = temp_dir1.path();
    let work_dir2 = temp_dir2.path();
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Agent 1
    let session1 = ConversationSession::new(work_dir1, "com.agent.one", "com.agent.one").unwrap();
    session1.append_message("user", "Message for agent 1", None);
    std::thread::sleep(std::time::Duration::from_millis(100));
    rt.block_on(async { session1.close().await.unwrap(); });

    // Agent 2
    let session2 = ConversationSession::new(work_dir2, "com.agent.two", "com.agent.two").unwrap();
    session2.append_message("user", "Message for agent 2", None);
    std::thread::sleep(std::time::Duration::from_millis(100));
    rt.block_on(async { session2.close().await.unwrap(); });

    // Verify agent 1's data
    let conv_dir1 = work_dir1.join("conversations");
    let files1: Vec<_> = std::fs::read_dir(&conv_dir1).unwrap().collect();
    assert_eq!(files1.len(), 1);

    let jsonl1 = files1[0].as_ref().unwrap().path();
    let page1 = read_messages_paginated(&jsonl1, None, 100, "backward").unwrap();
    assert_eq!(page1.messages.len(), 1);
    assert_eq!(page1.messages[0].content, "Message for agent 1");

    // Verify agent 2's data
    let conv_dir2 = work_dir2.join("conversations");
    let files2: Vec<_> = std::fs::read_dir(&conv_dir2).unwrap().collect();
    assert_eq!(files2.len(), 1);

    let jsonl2 = files2[0].as_ref().unwrap().path();
    let page2 = read_messages_paginated(&jsonl2, None, 100, "backward").unwrap();
    assert_eq!(page2.messages.len(), 1);
    assert_eq!(page2.messages[0].content, "Message for agent 2");

    assert_ne!(jsonl1, jsonl2, "Session files should be in different directories");
}

// ═══════════════════════════════════════════════════════════════════════
// Group H: Boundary & error cases (T46–T50)
// ═══════════════════════════════════════════════════════════════════════

/// T46: conversations directory does not exist — auto-creates on ConversationSession::new().
#[test]
fn test_t46_auto_creates_conversations_dir() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    assert!(
        !work_dir.join("conversations").exists(),
        "conversations dir should not exist initially"
    );

    let session = ConversationSession::new(work_dir, "auto-create-test", "com.test.agent").unwrap();
    assert!(
        work_dir.join("conversations").exists(),
        "conversations dir should be auto-created"
    );

    std::thread::sleep(std::time::Duration::from_millis(50));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { session.close().await.unwrap(); });
}

/// T47: JSONL file read-only permission error handling (Unix only).
///
/// This test is only meaningful on Unix where file permissions work.
/// On Windows, it is skipped via cfg attribute.
#[cfg(unix)]
#[test]
fn test_t47_readonly_jsonl_error_handling() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    let session = ConversationSession::new(work_dir, "readonly-test", "com.test.agent").unwrap();
    session.append_message("user", "Before readonly", None);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { session.close().await.unwrap(); });

    // Make the JSONL file read-only
    let conv_dir = work_dir.join("conversations");
    let jsonl_path = std::fs::read_dir(&conv_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();

    let mut perms = std::fs::metadata(&jsonl_path).unwrap().permissions();
    perms.set_mode(0o444);
    std::fs::set_permissions(&jsonl_path, perms).unwrap();

    // Attempting to resume should fail on read-only file
    let session_id = jsonl_path.file_stem().unwrap().to_string_lossy().to_string();
    let result = ConversationSession::resume(work_dir, &session_id);
    assert!(result.is_err(), "Should fail to resume a read-only JSONL file");

    // Restore permissions for cleanup
    let mut perms = std::fs::metadata(&jsonl_path).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&jsonl_path, perms).unwrap();
}

/// T48: Extremely long session_id handling (>255 chars).
#[test]
fn test_t48_extremely_long_session_id() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    let long_id = format!("20260503_120000_{}", "x".repeat(300));

    let result = ConversationSession::new(work_dir, &long_id, "com.test.agent");
    if let Ok(session) = result {
        session.append_message("user", "Long ID test", None);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { session.close().await.unwrap(); });
    }
    // Either works or fails gracefully — no panic
}

/// T49: Session ID with special characters handling.
#[tokio::test]
async fn test_t49_session_id_special_characters() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let conv_dir = work_dir.join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    // Create a JSONL file with a session ID containing spaces
    let special_id = "20260503_120000_test with spaces";
    let file_path = conv_dir.join(format!("{special_id}.jsonl"));

    let meta = SessionMetadata {
        version: 1,
        session_id: special_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        agent_id: "com.test".to_string(),
        title: None,
        updated_at: None,
        message_count: Some(0),
        corrupted: false,
    };
    let mut file = std::fs::File::create(&file_path).unwrap();
    serde_json::to_writer(&mut file, &meta).unwrap();
    writeln!(file).unwrap();

    // Also create a normal session file
    let normal_id = "20260503_130000_normal";
    let normal_path = conv_dir.join(format!("{normal_id}.jsonl"));
    let normal_meta = SessionMetadata {
        version: 1,
        session_id: normal_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        agent_id: "com.test".to_string(),
        title: None,
        updated_at: None,
        message_count: Some(0),
        corrupted: false,
    };
    let mut normal_file = std::fs::File::create(&normal_path).unwrap();
    serde_json::to_writer(&mut normal_file, &normal_meta).unwrap();
    writeln!(normal_file).unwrap();

    // Scanning should work without panic
    let handle = rollball_runtime::conversation::scan_sessions_async(conv_dir);
    let sessions = handle.await.unwrap();

    assert!(
        !sessions.is_empty(),
        "Should find at least the normal session"
    );
}

/// T50: Concurrent ConversationSession creation — thread safety.
#[test]
fn test_t50_concurrent_session_creation() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = std::sync::Arc::new(temp_dir.path().to_path_buf());
    let mut handles = Vec::new();

    for i in 0..5 {
        let wd = work_dir.clone();
        handles.push(std::thread::spawn(move || {
            let agent_id = format!("com.test.agent{i}");
            let session_id = generate_session_id();
            let session = ConversationSession::new(&wd, &session_id, &agent_id).unwrap();
            session.append_message("user", &format!("Concurrent message {i}"), None);
            std::thread::sleep(std::time::Duration::from_millis(100));
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async { session.close().await.unwrap(); });
            session_id
        }));
    }

    let session_ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let conv_dir = work_dir.join("conversations");
    let files: Vec<_> = std::fs::read_dir(&conv_dir).unwrap().collect();
    assert_eq!(files.len(), 5, "Should have 5 session files");

    for sid in &session_ids {
        let path = conv_dir.join(format!("{sid}.jsonl"));
        assert!(path.exists(), "Session file should exist for {sid}");
        let page = read_messages_paginated(&path, None, 100, "backward").unwrap();
        assert_eq!(page.messages.len(), 1, "Each session should have 1 message");
    }
}

/// T51: Large file (100+ messages) — backward pagination with byte-offset cursors.
#[test]
fn test_t51_large_file_backward_pagination() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session_id = generate_session_id();

    let session = ConversationSession::new(work_dir, &session_id, "com.test.agent").unwrap();

    for i in 0..150 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        session.append_message(role, &format!("Large file message {:03}", i), None);
    }

    std::thread::sleep(std::time::Duration::from_millis(100));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { session.close().await.unwrap(); });

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    assert!(jsonl_path.exists());

    // Verify all messages are readable
    let all_page = read_messages_paginated(&jsonl_path, None, 200, "backward").unwrap();
    assert_eq!(all_page.messages.len(), 150, "Should read all 150 messages");

    // Paginate backward with limit 50
    let page1 = read_messages_paginated(&jsonl_path, None, 50, "backward").unwrap();
    assert_eq!(page1.messages.len(), 50);
    assert!(page1.has_more);
    // First page should contain messages 100-149
    assert_eq!(page1.messages[0].content, "Large file message 100");
    assert_eq!(page1.messages[49].content, "Large file message 149");

    let cursor1 = page1.cursor.unwrap();
    assert!(cursor1.starts_with("offset:"));
    let cursor1_clone = cursor1.clone();

    let page2 = read_messages_paginated(&jsonl_path, Some(cursor1), 50, "backward").unwrap();
    assert_eq!(page2.messages.len(), 50);
    assert!(page2.has_more);
    // Second page should contain messages 50-99
    assert_eq!(page2.messages[0].content, "Large file message 050");
    assert_eq!(page2.messages[49].content, "Large file message 099");

    let cursor2 = page2.cursor.unwrap();
    assert!(cursor2.starts_with("offset:"));

    let page3 = read_messages_paginated(&jsonl_path, Some(cursor2), 50, "backward").unwrap();
    assert_eq!(page3.messages.len(), 50);
    // Page3 contains the oldest messages (0-49), so has_more is false —
    // message 0 starts at exactly meta_end, so first_offset == meta_end.
    assert!(!page3.has_more, "No more older messages after reaching the beginning");
    // Third page should contain messages 0-49
    assert_eq!(page3.messages[0].content, "Large file message 000");
    assert_eq!(page3.messages[49].content, "Large file message 049");
    // Since has_more is false, cursor is None — no page4 exists
    assert!(page3.cursor.is_none());

    // Forward direction from offset cursor
    let fwd_page = read_messages_paginated(&jsonl_path, Some(cursor1_clone), 10, "forward").unwrap();
    assert_eq!(fwd_page.messages.len(), 10);
    assert!(fwd_page.has_more);
    // Forward from page1 cursor should read messages starting at cursor1 offset
    assert_eq!(fwd_page.messages[0].content, "Large file message 100");
}
