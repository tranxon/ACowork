//! Gateway routing integration tests
//!
//! Tests for the pure-routing Gateway message loop.
//! Covers: non-blocking routing, IPC disconnect recovery,
//! provider hot-update broadcast, nonexistent session errors,
//! and concurrent session creation + messaging.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rollball_core::providers::mock::{MockProvider, MockResponse};
use rollball_core::providers::traits::{FunctionCall, ToolCall};
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use rollball_core::Budget;
use rollball_runtime::agent::agent_core::AgentCore;
use rollball_runtime::agent::loop_::{ChunkEvent, SessionChunkEvent};
use rollball_runtime::agent::session::session_manager::{
    SessionManager, SessionManagerConfig,
};
use rollball_runtime::agent::session::session_task::SessionMessage;
use rollball_runtime::config::RuntimeConfig;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// A slow echo tool that sleeps for a configurable duration before returning.
/// Used to simulate long-running LLM calls that should not block other sessions.
struct SlowEchoTool {
    delay: Duration,
    call_count: Arc<AtomicU32>,
}

impl SlowEchoTool {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            call_count: Arc::new(AtomicU32::new(0)),
        }
    }
}

#[async_trait]
impl Tool for SlowEchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "slow_echo".to_string(),
            description: "Echoes back the input after a delay".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "Message to echo"}
                },
                "required": ["message"]
            }),
        }
    }

    async fn execute(&self, params: serde_json::Value) -> rollball_core::error::Result<ToolResult> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("no message");
        tokio::time::sleep(self.delay).await;
        Ok(ToolResult {
            ok: true,
            content: format!("SlowEcho: {message}"),
            error: None,
            token_usage: None,
        })
    }
}

fn test_manifest() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(r#"
        agent_id = "com.test.gateway-routing"
        version = "1.0.0"
        name = "Gateway Routing Test Agent"
        description = "Test"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "mock"
        model = "mock-model"

        [[tools]]
        name = "slow_echo"
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
    chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
) -> Arc<AgentCore> {
    Arc::new(AgentCore::new(
        test_config(),
        test_manifest(),
        provider,
        tools,
        chunk_tx,
    ))
}

fn make_session_config(budget: Budget, chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>) -> SessionManagerConfig {
    SessionManagerConfig {
        inbound_channel_capacity: 64,
        system_prompt: "You are a test assistant.".to_string(),
        per_session_budget: budget,
        history_max_tokens: 128_000,
        keep_full_results: 4,
        chunk_tx,
        tool_definitions: vec![],
        full_tool_specs: Vec::new(),
        identity_context: None,
        override_model: None,
    }
}

// ---------------------------------------------------------------------------
// SA-04: Gateway routing never blocks
// ---------------------------------------------------------------------------

/// Verify that routing a message to one session does not block routing to
/// another session, even when the first session's LLM call takes seconds.
///
/// Scenario:
/// - Create Session A and Session B
/// - Session A triggers a slow_echo tool (3s delay)
/// - Immediately route a message to Session B
/// - Verify that Session B's response arrives within 1s
///   (well before Session A's 3s tool completes)
#[tokio::test]
async fn sa_04_gateway_routing_never_blocks() {
    let slow_tool = Arc::new(SlowEchoTool::new(Duration::from_secs(3)));

    // Provider responses:
    //   Session A: tool call (slow_echo) -> then text
    //   Session B: quick text response
    let provider = Arc::new(MockProvider::new(vec![
        // Session A: triggers the slow tool
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_slow".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "slow_echo".to_string(),
                    arguments: r#"{"message": "hello_A"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        MockResponse::Text {
            content: "Session A done.".to_string(),
        },
        // Session B: quick text response
        MockResponse::Text {
            content: "Session B done.".to_string(),
        },
    ]));

    // Create a chunk channel so we can observe when each session completes.
    // IMPORTANT: chunk_tx goes to SessionManagerConfig, NOT to AgentCore::new(),
    // because SessionTask.clone_for_session() replaces AgentCore's on_chunk
    // with the one from SessionManagerConfig.
    let (chunk_tx, mut chunk_rx) = mpsc::channel::<SessionChunkEvent>(256);

    let tools: Vec<Arc<dyn Tool>> = vec![slow_tool];
    let core = make_core(provider, tools, None);
    let mut manager = SessionManager::new(core, make_session_config(test_budget(), Some(chunk_tx)), String::new());

    let session_a = manager.create_session().await.unwrap();
    let session_b = manager.create_session().await.unwrap();

    // Route message to Session A (will trigger slow_echo, takes 3s)
    manager
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "Hello A".to_string(),
            message_id: "msg-sa04-a".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Immediately route message to Session B
    let b_route_time = tokio::time::Instant::now();
    manager
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "Hello B".to_string(),
            message_id: "msg-sa04-b".to_string(),
            skill_instructions: None,
        })
        .unwrap();
    let route_elapsed = b_route_time.elapsed();

    // The routing call itself should be sub-millisecond (mpsc channel send)
    assert!(
        route_elapsed < Duration::from_millis(100),
        "Routing to Session B should be immediate, took {:?}",
        route_elapsed
    );

    // Wait for Session B's Done chunk (should arrive well before Session A's 3s)
    let b_deadline = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(event) = chunk_rx.recv().await {
            match &event.event {
                ChunkEvent::Done { message_id, .. } if message_id == "msg-sa04-b" => {
                    return true;
                }
                ChunkEvent::Error { message_id, .. } if message_id == "msg-sa04-b" => {
                    return false;
                }
                _ => {} // Ignore other events (Session A's intermediate chunks)
            }
        }
        false
    })
    .await;

    assert!(
        b_deadline.is_ok(),
        "Session B should complete within 1s, but Session A's 3s slow tool is blocking it"
    );
    assert!(
        b_deadline.unwrap(),
        "Session B should receive a Done chunk, not an Error"
    );

    // Clean up
    let _ = manager.destroy_session(&session_a).await;
    let _ = manager.destroy_session(&session_b).await;
}

// ---------------------------------------------------------------------------
// IPC-01: Gateway disconnect recovery
// ---------------------------------------------------------------------------

/// Verify that active sessions continue running when the Gateway IPC
/// connection is lost (simulated by dropping the chunk_rx receiver).
/// After "reconnecting" (creating a new chunk channel), messages should
/// still be routable to the surviving sessions.
#[tokio::test]
async fn ipc_01_gateway_disconnect_recovery() {
    let provider = Arc::new(MockProvider::single_text("OK"));

    // Phase 1: Normal operation with chunk channel
    // chunk_tx goes to SessionManagerConfig so SessionTask can send Done/Error events
    let (chunk_tx, chunk_rx) = mpsc::channel::<SessionChunkEvent>(256);

    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider.clone(), tools.clone(), None);
    let mut manager = SessionManager::new(core, make_session_config(test_budget(), Some(chunk_tx)), String::new());

    let session_a = manager.create_session().await.unwrap();
    let session_b = manager.create_session().await.unwrap();

    // Send a message to verify normal operation
    manager
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "Before disconnect".to_string(),
            message_id: "msg-ipc01-1".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Phase 2: Simulate Gateway disconnect by dropping the chunk receiver
    // The sessions should continue running despite the chunk receiver being dropped
    drop(chunk_rx);

    // Wait a moment for any pending sends to the closed channel to be detected
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both sessions should still be alive
    assert!(
        manager.get_session(&session_a).unwrap().is_alive(),
        "Session A should survive Gateway disconnect"
    );
    assert!(
        manager.get_session(&session_b).unwrap().is_alive(),
        "Session B should survive Gateway disconnect"
    );

    // Messages should still be routable after disconnect
    let route_result = manager.send_to_session(&session_a, SessionMessage::ChatMessage {
        content: "After disconnect".to_string(),
        message_id: "msg-ipc01-2".to_string(),
    skill_instructions: None,
    });
    assert!(
        route_result.is_ok(),
        "Should still be able to route messages after disconnect"
    );

    // Phase 3: Simulate Gateway reconnection with a new chunk channel
    let (new_chunk_tx, mut new_chunk_rx) = mpsc::channel::<SessionChunkEvent>(256);

    // Create a new session with the new chunk channel
    let new_core = make_core(provider, tools, None);
    let mut new_manager = SessionManager::new(new_core, make_session_config(test_budget(), Some(new_chunk_tx)), String::new());

    let session_c = new_manager.create_session().await.unwrap();

    // The new session with the reconnected chunk channel should work
    new_manager
        .send_to_session(&session_c, SessionMessage::ChatMessage {
            content: "After reconnect".to_string(),
            message_id: "msg-ipc01-3".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Verify the new session's response arrives on the new chunk channel
    let reconnect_result = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = new_chunk_rx.recv().await {
            match &event.event {
                ChunkEvent::Done { message_id, .. } if message_id == "msg-ipc01-3" => {
                    return true;
                }
                _ => {}
            }
        }
        false
    })
    .await;

    assert!(
        reconnect_result.is_ok(),
        "New session after reconnect should produce a response within 2s"
    );

    // Clean up
    let _ = manager.destroy_session(&session_a).await;
    let _ = manager.destroy_session(&session_b).await;
    let _ = new_manager.destroy_session(&session_c).await;
}

// ---------------------------------------------------------------------------
// IPC-02: Provider hot-update broadcast
// ---------------------------------------------------------------------------

/// Verify that when a provider configuration update is broadcast,
/// all active sessions receive the update without error.
#[tokio::test]
async fn ipc_02_provider_hot_update_broadcast() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools, None);
    let mut manager = SessionManager::new(core, make_session_config(test_budget(), None), String::new());

    // Create 3 sessions
    let session_a = manager.create_session().await.unwrap();
    let session_b = manager.create_session().await.unwrap();
    let session_c = manager.create_session().await.unwrap();

    // Verify all 3 sessions are alive before broadcast
    assert!(manager.get_session(&session_a).unwrap().is_alive());
    assert!(manager.get_session(&session_b).unwrap().is_alive());
    assert!(manager.get_session(&session_c).unwrap().is_alive());

    // Broadcast UpdateMaxOutputTokens to all sessions
    let failed = manager.broadcast(SessionMessage::UpdateMaxOutputTokens { limit: 8192 });
    assert!(
        failed.is_empty(),
        "Broadcast should succeed for all active sessions, but {:?} failed",
        failed
    );

    // Broadcast UpdateProvider to all sessions
    let failed = manager.broadcast(SessionMessage::UpdateProvider {
        provider_name: "mock".to_string(),
        protocol_type: rollball_core::protocol::ProtocolType::OpenAI,
        api_key: None,
        base_url: None,
        model: "mock-model-v2".to_string(),
    });
    assert!(
        failed.is_empty(),
        "Provider update broadcast should succeed for all sessions, but {:?} failed",
        failed
    );

    // Broadcast UpdateCapabilities to all sessions
    let caps = rollball_core::protocol::ModelCapabilitiesInfo {
        name: Some("mock-model-v2".to_string()),
        supports_tool_calling: true,
        supports_reasoning: Some(false),
        supports_attachment: Some(false),
        supports_temperature: Some(true),
        context_window: 128_000,
        max_output_tokens: 8192,
        max_input_tokens: None,
        cost: None,
        modalities: None,
        family: None,
        knowledge_cutoff: None,
    };
    let failed = manager.broadcast(SessionMessage::UpdateCapabilities { caps });
    assert!(
        failed.is_empty(),
        "Capabilities broadcast should succeed for all sessions, but {:?} failed",
        failed
    );

    // All sessions should still be alive after receiving broadcast messages
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        manager.get_session(&session_a).unwrap().is_alive(),
        "Session A should survive broadcast"
    );
    assert!(
        manager.get_session(&session_b).unwrap().is_alive(),
        "Session B should survive broadcast"
    );
    assert!(
        manager.get_session(&session_c).unwrap().is_alive(),
        "Session C should survive broadcast"
    );

    // Destroy one session, then verify broadcast only goes to remaining sessions
    manager.destroy_session(&session_b).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let failed = manager.broadcast(SessionMessage::UpdateMaxOutputTokens { limit: 4096 });
    assert!(
        failed.is_empty(),
        "Broadcast after destroy should still succeed for remaining sessions"
    );

    // Verify only 2 sessions remain
    assert_eq!(manager.session_count(), 2);

    // Clean up
    let _ = manager.destroy_session(&session_a).await;
    let _ = manager.destroy_session(&session_c).await;
}

// ---------------------------------------------------------------------------
// IPC-03: Message to nonexistent session_id
// ---------------------------------------------------------------------------

/// Verify that sending a message to a nonexistent session returns an error
/// without panicking, and does not affect other active sessions.
#[tokio::test]
async fn ipc_03_message_to_nonexistent_session() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools, None);
    let mut manager = SessionManager::new(core, make_session_config(test_budget(), None), String::new());

    // Create a real session
    let session_real = manager.create_session().await.unwrap();

    // Attempt to send to a nonexistent session
    let result = manager.send_to_session(
        "nonexistent-session-id",
        SessionMessage::ChatMessage {
            content: "This should fail".to_string(),
            message_id: "msg-ipc03-1".to_string(),
            skill_instructions: None,
        },
    );
    assert!(
        result.is_err(),
        "Sending to nonexistent session should return an error"
    );

    // Verify the error message is descriptive
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("nonexistent-session-id"),
        "Error message should mention the nonexistent session ID, got: {}",
        err_msg
    );

    // Verify the real session is unaffected
    assert!(
        manager.get_session(&session_real).unwrap().is_alive(),
        "Real session should be unaffected by send to nonexistent session"
    );

    // Real session should still accept messages
    let real_send_result = manager.send_to_session(&session_real, SessionMessage::ChatMessage {
        content: "Still works".to_string(),
        message_id: "msg-ipc03-2".to_string(),
    skill_instructions: None,
    });
    assert!(
        real_send_result.is_ok(),
        "Real session should still accept messages after failed send to nonexistent session"
    );

    // Attempt broadcast-style UpdateMaxOutputTokens to nonexistent session directly
    let result = manager.send_to_session(
        "another-nonexistent-id",
        SessionMessage::UpdateMaxOutputTokens { limit: 4096 },
    );
    assert!(
        result.is_err(),
        "Sending config update to nonexistent session should return an error"
    );

    // Multiple sends to nonexistent sessions should not cause panic
    for i in 0..5 {
        let result = manager.send_to_session(
            &format!("fake-session-{}", i),
            SessionMessage::ChatMessage {
                content: "test".to_string(),
                message_id: format!("msg-fake-{}", i),
                skill_instructions: None,
            },
        );
        assert!(result.is_err(), "Send {} to nonexistent session should fail", i);
    }

    // Real session should still be alive after all the failed sends
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        manager.get_session(&session_real).unwrap().is_alive(),
        "Real session should survive all sends to nonexistent sessions"
    );

    // Clean up
    let _ = manager.destroy_session(&session_real).await;
}

// ---------------------------------------------------------------------------
// LIFECYCLE-01: Create session while simultaneously receiving messages
// ---------------------------------------------------------------------------

/// Verify that messages sent to a session immediately after creation
/// are not lost. The session's inbound channel is created before the
/// tokio task is spawned, so messages should be queued and processed
/// once the task starts running.
#[tokio::test]
async fn lifecycle_01_create_session_while_receiving_messages() {
    // Use a chunk channel so we can observe when the session processes messages.
    // chunk_tx goes to SessionManagerConfig so SessionTask can forward events.
    let (chunk_tx, mut chunk_rx) = mpsc::channel::<SessionChunkEvent>(256);

    let provider = Arc::new(MockProvider::new(vec![
        // First message response
        MockResponse::Text {
            content: "First response.".to_string(),
        },
        // Second message response
        MockResponse::Text {
            content: "Second response.".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools, None);
    let mut manager = SessionManager::new(core, make_session_config(test_budget(), Some(chunk_tx)), String::new());

    // Create session and immediately send two messages back-to-back
    let session_id = manager.create_session().await.unwrap();

    // Send messages immediately after creation — the tokio task may not have
    // started yet, but the mpsc channel should buffer them
    manager
        .send_to_session(&session_id, SessionMessage::ChatMessage {
            content: "First message".to_string(),
            message_id: "msg-lifecycle-1".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    manager
        .send_to_session(&session_id, SessionMessage::ChatMessage {
            content: "Second message".to_string(),
            message_id: "msg-lifecycle-2".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Verify both messages are processed by waiting for Done chunks
    let mut received_first = false;
    let mut received_second = false;

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = chunk_rx.recv().await {
            match &event.event {
                ChunkEvent::Done { message_id, .. } => {
                    if message_id == "msg-lifecycle-1" {
                        received_first = true;
                    } else if message_id == "msg-lifecycle-2" {
                        received_second = true;
                    }
                }
                ChunkEvent::Error { message_id, .. } => {
                    panic!(
                        "Session should not error for message {}, got error",
                        message_id
                    );
                }
                _ => {}
            }

            if received_first && received_second {
                return true;
            }
        }
        false
    })
    .await;

    assert!(
        result.is_ok(),
        "Both messages should be processed within 5s"
    );
    assert!(
        received_first,
        "First message should be processed successfully"
    );
    assert!(
        received_second,
        "Second message should be processed successfully"
    );

    // Session should still be alive after processing both messages
    assert!(
        manager.get_session(&session_id).unwrap().is_alive(),
        "Session should be alive after processing concurrent messages"
    );

    // Clean up
    let _ = manager.destroy_session(&session_id).await;
}
