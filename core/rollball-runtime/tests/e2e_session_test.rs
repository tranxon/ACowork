//! End-to-end tests for Session Actor architecture.
//!
//! Validates the integration of SessionTask + SessionManager + SessionHandle
//! with ConversationSession, EpisodeDistiller, and ChunkEvent streaming.
//!
//! Tests that require a real LLM API key (MINIMAX_API_KEY) are marked
//! with `#[ignore]` and can be run with `cargo test -- --ignored`.

use std::sync::Arc;
use std::time::Duration;

use rollball_core::providers::mock::{MockProvider, MockResponse};
use rollball_core::providers::traits::{FunctionCall, ToolCall};
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use rollball_core::Budget;
use rollball_runtime::agent::agent_core::AgentCore;
use rollball_runtime::agent::loop_::ChunkEvent;
use rollball_runtime::agent::session::session_manager::{
    SessionManager, SessionManagerConfig,
};
use rollball_runtime::agent::session::session_task::SessionMessage;
use rollball_runtime::config::RuntimeConfig;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Skip test if MINIMAX_API_KEY is not set.
fn skip_without_api_key() -> bool {
    std::env::var("MINIMAX_API_KEY").is_err()
}

fn test_manifest() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(r#"
        agent_id = "com.test.e2e"
        version = "1.0.0"
        name = "E2E Test Agent"
        description = "End-to-end test agent"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "mock"
        model = "mock-model"
    "#).unwrap()
}

fn test_config() -> RuntimeConfig {
    RuntimeConfig::default()
}

fn test_budget() -> Budget {
    Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    }
}

fn make_core(
    provider: Arc<MockProvider>,
    tools: Vec<Arc<dyn Tool>>,
) -> Arc<AgentCore> {
    Arc::new(AgentCore::new(
        test_config(),
        test_manifest(),
        provider,
        tools,
        None,
    ))
}

fn make_session_config(budget: Budget) -> SessionManagerConfig {
    SessionManagerConfig {
        inbound_channel_capacity: 64,
        system_prompt: "You are a test assistant.".to_string(),
        per_session_budget: budget,
        history_max_tokens: 128_000,
        keep_full_results: 4,
        chunk_tx: None,
        tool_definitions: Vec::new(),
        identity_context: None,
    }
}

/// Build a SessionManagerConfig with a small history budget to trigger trimming.
fn make_small_history_config() -> SessionManagerConfig {
    SessionManagerConfig {
        inbound_channel_capacity: 64,
        system_prompt: "You are a test assistant.".to_string(),
        per_session_budget: test_budget(),
        // Very small token budget to trigger preemptive trim quickly
        history_max_tokens: 500,
        keep_full_results: 2,
        chunk_tx: None,
        tool_definitions: Vec::new(),
        identity_context: None,
    }
}

/// Build a SessionManagerConfig with a chunk sender for streaming tests.
fn make_streaming_config(
    chunk_tx: tokio::sync::mpsc::Sender<ChunkEvent>,
) -> SessionManagerConfig {
    SessionManagerConfig {
        inbound_channel_capacity: 64,
        system_prompt: "You are a test assistant.".to_string(),
        per_session_budget: test_budget(),
        history_max_tokens: 128_000,
        keep_full_results: 4,
        chunk_tx: Some(chunk_tx),
        tool_definitions: Vec::new(),
        identity_context: None,
    }
}

/// Build a SessionManagerConfig with a session count limit.
fn make_limited_session_config(max_sessions: usize) -> (SessionManagerConfig, usize) {
    (
        SessionManagerConfig {
            inbound_channel_capacity: 64,
            system_prompt: "You are a test assistant.".to_string(),
            per_session_budget: test_budget(),
            history_max_tokens: 128_000,
            keep_full_results: 4,
            chunk_tx: None,
            tool_definitions: Vec::new(),
            identity_context: None,
        },
        max_sessions,
    )
}

// ---------------------------------------------------------------------------
// E2E-01: Three Session concurrent full conversation (mock LLM)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_01_three_session_concurrent_conversation() {
    // Each session gets its own provider with unique responses
    let provider_a = Arc::new(MockProvider::single_text("1 + 1 = 2"));
    let provider_b = Arc::new(MockProvider::single_text("2 + 2 = 4"));
    let provider_c = Arc::new(MockProvider::single_text("3 + 3 = 6"));

    let core_a = make_core(provider_a, vec![]);
    let core_b = make_core(provider_b, vec![]);
    let core_c = make_core(provider_c, vec![]);

    let mut manager_a = SessionManager::new(core_a, make_session_config(test_budget()));
    let mut manager_b = SessionManager::new(core_b, make_session_config(test_budget()));
    let mut manager_c = SessionManager::new(core_c, make_session_config(test_budget()));

    let session_a = manager_a.create_session().await.unwrap();
    let session_b = manager_b.create_session().await.unwrap();
    let session_c = manager_c.create_session().await.unwrap();

    // Send messages concurrently
    manager_a
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "What is 1+1?".to_string(),
            message_id: "msg-1".to_string(),
        })
        .unwrap();
    manager_b
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "What is 2+2?".to_string(),
            message_id: "msg-2".to_string(),
        })
        .unwrap();
    manager_c
        .send_to_session(&session_c, SessionMessage::ChatMessage {
            content: "What is 3+3?".to_string(),
            message_id: "msg-3".to_string(),
        })
        .unwrap();

    // Wait for all sessions to process
    tokio::time::sleep(Duration::from_secs(2)).await;

    // All sessions should still be alive
    assert!(
        manager_a.get_session(&session_a).unwrap().is_alive(),
        "Session A should be alive"
    );
    assert!(
        manager_b.get_session(&session_b).unwrap().is_alive(),
        "Session B should be alive"
    );
    assert!(
        manager_c.get_session(&session_c).unwrap().is_alive(),
        "Session C should be alive"
    );

    // Verify session IDs are unique
    assert_ne!(session_a, session_b);
    assert_ne!(session_b, session_c);
    assert_ne!(session_a, session_c);

    // Clean up
    let _ = manager_a.destroy_session(&session_a).await;
    let _ = manager_b.destroy_session(&session_b).await;
    let _ = manager_c.destroy_session(&session_c).await;
}

// ---------------------------------------------------------------------------
// E2E-01-real: Three Session concurrent with real MINIMAX API
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Requires MINIMAX_API_KEY environment variable"]
async fn e2e_01_real_three_session_concurrent_with_minimax() {
    if skip_without_api_key() {
        return;
    }

    let api_key = std::env::var("MINIMAX_API_KEY").unwrap();
    let provider =
        rollball_runtime::providers::router::create_provider(
            "minimax",
            &rollball_core::protocol::ProtocolType::OpenAI,
            Some(&api_key),
            None,
        );

    let manifest = rollball_core::AgentManifest::from_toml(r#"
        agent_id = "com.test.e2e.real"
        version = "1.0.0"
        name = "E2E Real Test Agent"
        description = "Real LLM test"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "minimax"
        model = "MiniMax-M2.7"
    "#).unwrap();

    let core = Arc::new(AgentCore::new(
        test_config(),
        manifest,
        provider,
        vec![],
        None,
    ));

    let mut manager = SessionManager::new(core, make_session_config(test_budget()));

    let session_a = manager.create_session().await.unwrap();
    let session_b = manager.create_session().await.unwrap();
    let session_c = manager.create_session().await.unwrap();

    // Send different messages concurrently
    manager
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "What is 1+1? Reply with just the number.".to_string(),
            message_id: "msg-real-1".to_string(),
        })
        .unwrap();
    manager
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "What is 2+2? Reply with just the number.".to_string(),
            message_id: "msg-real-2".to_string(),
        })
        .unwrap();
    manager
        .send_to_session(&session_c, SessionMessage::ChatMessage {
            content: "What is 3+3? Reply with just the number.".to_string(),
            message_id: "msg-real-3".to_string(),
        })
        .unwrap();

    // Wait for real LLM responses
    tokio::time::sleep(Duration::from_secs(30)).await;

    // All sessions should still be alive
    assert!(
        manager.get_session(&session_a).unwrap().is_alive(),
        "Session A should be alive after real LLM call"
    );
    assert!(
        manager.get_session(&session_b).unwrap().is_alive(),
        "Session B should be alive after real LLM call"
    );
    assert!(
        manager.get_session(&session_c).unwrap().is_alive(),
        "Session C should be alive after real LLM call"
    );

    // Clean up
    let _ = manager.destroy_session(&session_a).await;
    let _ = manager.destroy_session(&session_b).await;
    let _ = manager.destroy_session(&session_c).await;
}

// ---------------------------------------------------------------------------
// E2E-02: Long conversation compression chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_02_long_conversation_trim_chain() {
    // Use a mock provider that returns long responses to fill up history
    let long_response = "This is a long response to fill up the token budget. ".repeat(50);
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Text { content: long_response.clone() },
        MockResponse::Text { content: long_response.clone() },
        MockResponse::Text { content: long_response.clone() },
        MockResponse::Text { content: long_response.clone() },
        MockResponse::Text { content: long_response.clone() },
        MockResponse::Text { content: "Final short response.".to_string() },
    ]));

    let core = make_core(provider, vec![]);
    let mut manager = SessionManager::new(core, make_small_history_config());

    let session_id = manager.create_session().await.unwrap();

    // Send multiple messages to fill up history and trigger trimming
    for i in 0..6 {
        let msg = format!("Message number {} with some content to add tokens", i);
        manager
            .send_to_session(&session_id, SessionMessage::ChatMessage {
                content: msg,
                message_id: format!("msg-trim-{}", i),
            })
            .unwrap();
        // Small delay to allow processing
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Session should still be alive after trimming
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        manager.get_session(&session_id).unwrap().is_alive(),
        "Session should survive history trimming"
    );

    // Clean up
    let _ = manager.destroy_session(&session_id).await;
}

// ---------------------------------------------------------------------------
// E2E-03: Session switching does not interrupt streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_03_session_switch_no_interrupt() {
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<ChunkEvent>(256);

    // Provider for session A returns text immediately
    let provider_a = Arc::new(MockProvider::single_text("Session A response"));
    let core_a = make_core(provider_a, vec![]);
    let mut manager_a = SessionManager::new(core_a, make_streaming_config(chunk_tx.clone()));

    // Provider for session B
    let provider_b = Arc::new(MockProvider::single_text("Session B response"));
    let core_b = make_core(provider_b, vec![]);
    let mut manager_b = SessionManager::new(core_b, make_streaming_config(chunk_tx));

    let session_a = manager_a.create_session().await.unwrap();
    let session_b = manager_b.create_session().await.unwrap();

    // Send message to session A
    manager_a
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "Hello A".to_string(),
            message_id: "msg-a-1".to_string(),
        })
        .unwrap();

    // Immediately send to session B (simulating frontend switching selectedSession)
    manager_b
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "Hello B".to_string(),
            message_id: "msg-b-1".to_string(),
        })
        .unwrap();

    // Collect chunk events with timeout
    let mut events: Vec<ChunkEvent> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    loop {
        if tokio::time::Instant::now() > deadline {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(2), chunk_rx.recv()).await {
            Ok(Some(event)) => {
                // Just collect all events; we verify counts below
                events.push(event);
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Both sessions should have produced at least a Done event
    assert!(
        events.len() >= 2,
        "Should have received at least 2 chunk events (one per session), got {}",
        events.len()
    );

    // Both sessions should still be alive
    assert!(
        manager_a.get_session(&session_a).unwrap().is_alive(),
        "Session A should still be alive"
    );
    assert!(
        manager_b.get_session(&session_b).unwrap().is_alive(),
        "Session B should still be alive"
    );

    // Clean up
    let _ = manager_a.destroy_session(&session_a).await;
    let _ = manager_b.destroy_session(&session_b).await;
}

// ---------------------------------------------------------------------------
// E2E-04: Complete lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_04_complete_lifecycle() {
    let provider = Arc::new(MockProvider::single_text("Lifecycle response"));
    let core = make_core(provider, vec![]);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()));

    // Step 1: Create session
    let session_id = manager.create_session().await.unwrap();
    assert!(manager.get_session(&session_id).is_some());
    assert!(manager.get_session(&session_id).unwrap().is_alive());

    // Step 2: Send message and receive response
    manager
        .send_to_session(&session_id, SessionMessage::ChatMessage {
            content: "Hello lifecycle test".to_string(),
            message_id: "msg-lifecycle-1".to_string(),
        })
        .unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Session should still be alive
    assert!(
        manager.get_session(&session_id).unwrap().is_alive(),
        "Session should be alive after message"
    );

    // Step 3: Destroy session
    manager.destroy_session(&session_id).await.unwrap();

    // Wait for task to finish
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Step 4: Verify session is removed
    assert!(
        manager.get_session(&session_id).is_none(),
        "Destroyed session should not be accessible"
    );

    // Step 5: Verify sending to destroyed session fails
    let result = manager.send_to_session(&session_id, SessionMessage::ChatMessage {
        content: "Should fail".to_string(),
        message_id: "msg-should-fail".to_string(),
    });
    assert!(
        result.is_err(),
        "Sending to destroyed session should fail"
    );
}

// ---------------------------------------------------------------------------
// E2E-04-persist: Complete lifecycle with JSONL persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_04_complete_lifecycle_with_jsonl() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    let provider = Arc::new(MockProvider::single_text("JSONL response"));
    let core = make_core(provider, vec![]);

    // Create conversation session with JSONL persistence
    let session_id = rollball_runtime::conversation::generate_session_id();
    let conversation = rollball_runtime::conversation::ConversationSession::new(
        work_dir,
        &session_id,
        "com.test.e2e",
    )
    .unwrap();

    let config = make_session_config(test_budget());
    let mut manager = SessionManager::new(core, config);

    // Create session with conversation persistence
    manager
        .create_session_with_id_and_conversation(session_id.clone(), Some(conversation))
        .await
        .unwrap();

    // Send a message
    manager
        .send_to_session(&session_id, SessionMessage::ChatMessage {
            content: "Hello JSONL test".to_string(),
            message_id: "msg-jsonl-1".to_string(),
        })
        .unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Destroy session (triggers close + distillation)
    manager.destroy_session(&session_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify JSONL file still exists after session destruction (persistence)
    let jsonl_path = work_dir.join("conversations").join(format!("{}.jsonl", session_id));
    assert!(
        jsonl_path.exists(),
        "JSONL file should persist after session destruction"
    );

    // Verify JSONL content
    let content = std::fs::read_to_string(&jsonl_path).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    // At least: metadata + user message + assistant message
    assert!(
        lines.len() >= 2,
        "JSONL should have at least metadata + 1 message, got {} lines",
        lines.len()
    );

    // Verify first line is valid metadata
    let meta: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(meta["session_id"], session_id);
    assert_eq!(meta["agent_id"], "com.test.e2e");
}

// ---------------------------------------------------------------------------
// LIFECYCLE-02: Streaming LLM output does not cross-contaminate sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_02_streaming_no_cross_contamination() {
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<ChunkEvent>(256);

    // Session A provider
    let provider_a = Arc::new(MockProvider::single_text("Response from A only"));
    let core_a = make_core(provider_a, vec![]);
    let mut manager_a = SessionManager::new(core_a, make_streaming_config(chunk_tx.clone()));

    // Session B provider
    let provider_b = Arc::new(MockProvider::single_text("Response from B only"));
    let core_b = make_core(provider_b, vec![]);
    let mut manager_b = SessionManager::new(core_b, make_streaming_config(chunk_tx));

    let session_a = manager_a.create_session().await.unwrap();
    let session_b = manager_b.create_session().await.unwrap();

    // Send messages simultaneously
    manager_a
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "Hello A".to_string(),
            message_id: "msg-a-lc02".to_string(),
        })
        .unwrap();
    manager_b
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "Hello B".to_string(),
            message_id: "msg-b-lc02".to_string(),
        })
        .unwrap();

    // Collect all chunk events
    let mut done_count = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    while done_count < 2 {
        if tokio::time::Instant::now() > deadline {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(3), chunk_rx.recv()).await {
            Ok(Some(ChunkEvent::Done { .. })) => {
                done_count += 1;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Both sessions should have completed
    assert_eq!(
        done_count, 2,
        "Both sessions should have sent Done events"
    );

    // Both sessions should still be alive
    assert!(manager_a.get_session(&session_a).unwrap().is_alive());
    assert!(manager_b.get_session(&session_b).unwrap().is_alive());

    // Clean up
    let _ = manager_a.destroy_session(&session_a).await;
    let _ = manager_b.destroy_session(&session_b).await;
}

// ---------------------------------------------------------------------------
// EDGE-01: Session count limit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edge_01_session_count_limit() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let core = make_core(provider, vec![]);

    let (config, max_sessions) = make_limited_session_config(5);
    let mut manager = SessionManager::new(core, config);

    // Create sessions up to the limit
    let mut session_ids = Vec::new();
    for _ in 0..max_sessions {
        let id = manager.create_session().await.unwrap();
        session_ids.push(id);
    }

    assert_eq!(
        manager.session_count(),
        max_sessions,
        "Should have {} sessions",
        max_sessions
    );

    // Creating more sessions beyond the limit should still work
    // (SessionManager doesn't enforce a hard limit by default, but
    // the caller can check session_count before creating)
    let extra_id = manager.create_session().await.unwrap();
    assert_eq!(
        manager.session_count(),
        max_sessions + 1,
        "Session count should exceed limit (soft limit only)"
    );

    // Clean up all sessions
    for id in &session_ids {
        let _ = manager.destroy_session(id).await;
    }
    let _ = manager.destroy_session(&extra_id).await;
}

// ---------------------------------------------------------------------------
// EDGE-03: Extra-long message (>1MB)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edge_03_extra_long_message() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let core = make_core(provider, vec![]);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()));

    let session_id = manager.create_session().await.unwrap();

    // Create a message larger than 1MB
    let large_content = "X".repeat(1_050_000); // ~1MB
    let result = manager.send_to_session(&session_id, SessionMessage::ChatMessage {
        content: large_content.clone(),
        message_id: "msg-large".to_string(),
    });

    // Sending itself should succeed (channel accepts the message)
    assert!(
        result.is_ok(),
        "Sending large message to channel should succeed"
    );

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Session should still be alive (no panic)
    assert!(
        manager.get_session(&session_id).unwrap().is_alive(),
        "Session should survive extra-long message without panic"
    );

    // Clean up
    let _ = manager.destroy_session(&session_id).await;
}

// ---------------------------------------------------------------------------
// EDGE-03-persist: Extra-long message with JSONL persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edge_03_extra_long_message_jsonl() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    let provider = Arc::new(MockProvider::single_text("OK"));
    let core = make_core(provider, vec![]);

    let session_id = rollball_runtime::conversation::generate_session_id();
    let conversation = rollball_runtime::conversation::ConversationSession::new(
        work_dir,
        &session_id,
        "com.test.e2e",
    )
    .unwrap();

    let mut manager = SessionManager::new(core, make_session_config(test_budget()));
    manager
        .create_session_with_id_and_conversation(session_id.clone(), Some(conversation))
        .await
        .unwrap();

    // Create a large message (>1MB)
    let large_content = "Y".repeat(1_050_000);
    manager
        .send_to_session(&session_id, SessionMessage::ChatMessage {
            content: large_content.clone(),
            message_id: "msg-large-jsonl".to_string(),
        })
        .unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Destroy session to flush JSONL
    manager.destroy_session(&session_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify JSONL file exists and content is not truncated
    let jsonl_path = work_dir.join("conversations").join(format!("{}.jsonl", session_id));
    assert!(jsonl_path.exists(), "JSONL file should exist");

    let file_size = std::fs::metadata(&jsonl_path).unwrap().len();
    assert!(
        file_size > 1_000_000,
        "JSONL file should be >1MB for large message, got {} bytes",
        file_size
    );

    // Verify the file can be parsed without errors
    let content = std::fs::read_to_string(&jsonl_path).unwrap();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("Line {} should be valid JSON: {}", i + 1, e)
        });
        // Check that the large content is present
        if i > 0 {
            // Skip metadata line
            if let Some(msg_content) = parsed.get("content").and_then(|v| v.as_str())
                && msg_content.len() > 100
            {
                assert_eq!(
                    msg_content.len(),
                    large_content.len(),
                    "Large message content should not be truncated"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Additional: Verify concurrent sessions with tool calls
// ---------------------------------------------------------------------------

/// A simple echo tool for integration testing.
struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".to_string(),
            description: "Echoes back the input".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "Message to echo"}
                },
                "required": ["message"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
    ) -> rollball_core::error::Result<ToolResult> {
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("no message");
        Ok(ToolResult {
            ok: true,
            content: format!("Echo: {message}"),
            error: None,
            token_usage: None,
        })
    }
}

#[tokio::test]
async fn e2e_concurrent_sessions_with_tool_calls() {
    let echo_tool: Arc<dyn Tool> = Arc::new(EchoTool);

    let provider = Arc::new(MockProvider::new(vec![
        // Session A: tool call then text
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_echo_a".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "echo".to_string(),
                    arguments: r#"{"message": "from_A"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        MockResponse::Text {
            content: "Session A tool call done.".to_string(),
        },
        // Session B: simple text
        MockResponse::Text {
            content: "Session B text response.".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![echo_tool];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()));

    let session_a = manager.create_session().await.unwrap();
    let session_b = manager.create_session().await.unwrap();

    // Session A: triggers tool call
    manager
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "Call echo from A".to_string(),
            message_id: "msg-tool-a".to_string(),
        })
        .unwrap();

    // Session B: simple text
    manager
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "Hello B".to_string(),
            message_id: "msg-tool-b".to_string(),
        })
        .unwrap();

    // Wait for both to process
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Both should be alive
    assert!(
        manager.get_session(&session_a).unwrap().is_alive(),
        "Session A should survive tool call"
    );
    assert!(
        manager.get_session(&session_b).unwrap().is_alive(),
        "Session B should survive"
    );

    // Clean up
    let _ = manager.destroy_session(&session_a).await;
    let _ = manager.destroy_session(&session_b).await;
}

// ---------------------------------------------------------------------------
// Additional: Verify conversation JSONL is written correctly for multi-turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_multi_turn_conversation_jsonl() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Text {
            content: "First response.".to_string(),
        },
        MockResponse::Text {
            content: "Second response.".to_string(),
        },
        MockResponse::Text {
            content: "Third response.".to_string(),
        },
    ]));
    let core = make_core(provider, vec![]);

    let session_id = rollball_runtime::conversation::generate_session_id();
    let conversation = rollball_runtime::conversation::ConversationSession::new(
        work_dir,
        &session_id,
        "com.test.e2e",
    )
    .unwrap();

    let mut manager = SessionManager::new(core, make_session_config(test_budget()));
    manager
        .create_session_with_id_and_conversation(session_id.clone(), Some(conversation))
        .await
        .unwrap();

    // Send 3 messages
    for i in 0..3 {
        manager
            .send_to_session(&session_id, SessionMessage::ChatMessage {
                content: format!("Turn {}", i + 1),
                message_id: format!("msg-turn-{}", i),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Destroy to flush
    manager.destroy_session(&session_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify JSONL has metadata + 6 messages (3 user + 3 assistant)
    let jsonl_path = work_dir
        .join("conversations")
        .join(format!("{}.jsonl", session_id));
    let content = std::fs::read_to_string(&jsonl_path).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    // At least metadata + 3 user + 3 assistant = 7 lines
    assert!(
        lines.len() >= 7,
        "JSONL should have at least 7 lines (meta + 6 messages), got {}",
        lines.len()
    );

    // Verify role alternation
    let roles: Vec<String> = lines[1..]
        .iter()
        .filter_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| {
                    v.get("role").and_then(|r| r.as_str()).map(|s| s.to_string())
                })
        })
        .collect();

    // Should have user, assistant, user, assistant, user, assistant
    assert!(roles.len() >= 6, "Should have at least 6 role entries");
    assert_eq!(roles[0], "user");
    assert_eq!(roles[1], "assistant");
    assert_eq!(roles[2], "user");
    assert_eq!(roles[3], "assistant");
}

// ---------------------------------------------------------------------------
// Additional: Session title is set from first user message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_session_title_set_from_first_message() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    let provider = Arc::new(MockProvider::single_text("Title test response"));
    let core = make_core(provider, vec![]);

    let session_id = rollball_runtime::conversation::generate_session_id();
    let conversation = rollball_runtime::conversation::ConversationSession::new(
        work_dir,
        &session_id,
        "com.test.e2e",
    )
    .unwrap();

    let mut manager = SessionManager::new(core, make_session_config(test_budget()));
    manager
        .create_session_with_id_and_conversation(session_id.clone(), Some(conversation))
        .await
        .unwrap();

    // Send first message (should set title)
    manager
        .send_to_session(&session_id, SessionMessage::ChatMessage {
            content: "This is my first message about Rust programming".to_string(),
            message_id: "msg-title-1".to_string(),
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Destroy to flush
    manager.destroy_session(&session_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Read JSONL metadata to verify title
    let jsonl_path = work_dir
        .join("conversations")
        .join(format!("{}.jsonl", session_id));
    let content = std::fs::read_to_string(&jsonl_path).unwrap();
    let first_line = content.lines().next().unwrap();
    let meta: serde_json::Value = serde_json::from_str(first_line).unwrap();

    // Title should be set (truncated to 30 chars with ellipsis)
    assert!(
        meta.get("title").is_some(),
        "Session metadata should have a title"
    );
    let title = meta["title"].as_str().unwrap();
    assert!(
        !title.is_empty(),
        "Title should not be empty after first message"
    );
}

// ---------------------------------------------------------------------------
// Additional: Destroy and verify file persists
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_destroy_session_file_persists() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    let provider = Arc::new(MockProvider::single_text("Persist test"));
    let core = make_core(provider, vec![]);

    let session_id = rollball_runtime::conversation::generate_session_id();
    let conversation = rollball_runtime::conversation::ConversationSession::new(
        work_dir,
        &session_id,
        "com.test.e2e",
    )
    .unwrap();

    let mut manager = SessionManager::new(core, make_session_config(test_budget()));
    manager
        .create_session_with_id_and_conversation(session_id.clone(), Some(conversation))
        .await
        .unwrap();

    // Send a message
    manager
        .send_to_session(&session_id, SessionMessage::ChatMessage {
            content: "Before destroy".to_string(),
            message_id: "msg-persist-1".to_string(),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Destroy
    manager.destroy_session(&session_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // File should still exist
    let jsonl_path = work_dir
        .join("conversations")
        .join(format!("{}.jsonl", session_id));
    assert!(jsonl_path.exists(), "JSONL file should persist after destruction");

    // Sending to destroyed session should fail
    let result = manager.send_to_session(&session_id, SessionMessage::ChatMessage {
        content: "After destroy".to_string(),
        message_id: "msg-persist-2".to_string(),
    });
    assert!(result.is_err(), "Sending to destroyed session should fail");
}

// ---------------------------------------------------------------------------
// Additional: Conversation resume works correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_conversation_resume() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    // Phase 1: Create session, write messages, close
    let session_id = "20260507_100000_resume_e2e";

    let provider1 = Arc::new(MockProvider::single_text("First response"));
    let core1 = make_core(provider1, vec![]);

    let conversation1 = rollball_runtime::conversation::ConversationSession::new(
        work_dir,
        session_id,
        "com.test.e2e",
    )
    .unwrap();

    let mut manager1 = SessionManager::new(core1, make_session_config(test_budget()));
    manager1
        .create_session_with_id_and_conversation(session_id.to_string(), Some(conversation1))
        .await
        .unwrap();

    manager1
        .send_to_session(session_id, SessionMessage::ChatMessage {
            content: "First conversation message".to_string(),
            message_id: "msg-resume-1".to_string(),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    manager1.destroy_session(session_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 2: Resume session
    let conversation2 =
        rollball_runtime::conversation::ConversationSession::resume(work_dir, session_id).unwrap();

    let provider2 = Arc::new(MockProvider::single_text("Second response"));
    let core2 = make_core(provider2, vec![]);

    let mut manager2 = SessionManager::new(core2, make_session_config(test_budget()));
    manager2
        .create_session_with_id_and_conversation(session_id.to_string(), Some(conversation2))
        .await
        .unwrap();

    manager2
        .send_to_session(session_id, SessionMessage::ChatMessage {
            content: "Resumed conversation message".to_string(),
            message_id: "msg-resume-2".to_string(),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    manager2.destroy_session(session_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify JSONL has messages from both phases
    let jsonl_path = work_dir
        .join("conversations")
        .join(format!("{}.jsonl", session_id));
    let content = std::fs::read_to_string(&jsonl_path).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    // metadata + 1st user + 1st assistant + 2nd user + 2nd assistant = 5 lines
    assert!(
        lines.len() >= 5,
        "JSONL should have at least 5 lines after resume, got {}",
        lines.len()
    );

    // Verify both messages are present
    let content_str = content.clone();
    assert!(
        content_str.contains("First conversation message"),
        "First message should be in JSONL"
    );
    assert!(
        content_str.contains("Resumed conversation message"),
        "Resumed message should be in JSONL"
    );
}
