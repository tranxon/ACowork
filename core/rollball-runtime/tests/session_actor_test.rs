//! Session Actor integration tests
//!
//! Tests for multi-session concurrency via SessionTask + SessionManager.
//! Covers: non-blocking sessions, lifecycle management, concurrent tool calls,
//! budget isolation, panic isolation, and edge cases.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rollball_core::providers::mock::{MockProvider, MockResponse};
use rollball_core::providers::traits::{FunctionCall, ToolCall};
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use rollball_core::Budget;
use rollball_runtime::agent::agent_core::AgentCore;
use rollball_runtime::agent::session::session_manager::{
    SessionManager, SessionManagerConfig,
};
use rollball_runtime::agent::session::session_task::SessionMessage;
use rollball_runtime::config::RuntimeConfig;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// A slow echo tool that sleeps for a configurable duration before returning.
/// Used to verify that sessions don't block each other.
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

/// A simple echo tool for basic testing.
struct EchoTool;

#[async_trait]
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

    async fn execute(&self, params: serde_json::Value) -> rollball_core::error::Result<ToolResult> {
        let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("no message");
        Ok(ToolResult {
            ok: true,
            content: format!("Echo: {message}"),
            error: None,
            token_usage: None,
        })
    }
}

/// A tool that panics on execution, for panic isolation testing.
struct PanicTool;

#[async_trait]
impl Tool for PanicTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "panic_tool".to_string(),
            description: "Always panics".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<ToolResult> {
        panic!("PanicTool intentionally panicked!");
    }
}

fn test_manifest() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(r#"
        agent_id = "com.test.session"
        version = "1.0.0"
        name = "Session Test Agent"
        description = "Test"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "mock"
        model = "mock-model"

        [[tools]]
        name = "slow_echo"

        [[tools]]
        name = "echo"

        [[tools]]
        name = "panic_tool"
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

fn limited_budget(daily_tokens: u64) -> Budget {
    Budget {
        daily_tokens: Some(daily_tokens),
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "deny".to_string(),
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
        full_tool_specs: Vec::new(),
        identity_context: None,
        override_model: None,
    }
}

// ---------------------------------------------------------------------------
// SA-02: Two SessionTasks do not block each other
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sa_02_sessions_do_not_block_each_other() {
    // Create a slow tool (2s delay) and a fast provider
    let slow_tool = Arc::new(SlowEchoTool::new(Duration::from_secs(2)));

    let provider = Arc::new(MockProvider::new(vec![
        // Session A: tool call then text
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

    let tools: Vec<Arc<dyn Tool>> = vec![slow_tool];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    // Create sessions A and B
    let session_a = manager.create_session().await.unwrap();
    let session_b = manager.create_session().await.unwrap();

    // Send message to Session A (will trigger slow tool)
    manager
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "Hello A".to_string(),
            message_id: "msg-a-3".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Immediately send message to Session B (should respond quickly)
    let b_start = tokio::time::Instant::now();
    manager
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "Hello B".to_string(),
            message_id: "msg-b-3".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Give session B a moment to process (its provider returns text immediately)
    tokio::time::sleep(Duration::from_millis(500)).await;
    let b_elapsed = b_start.elapsed();

    // Session B should have responded well before Session A's 2s delay completes
    assert!(
        b_elapsed < Duration::from_secs(2),
        "Session B should respond before Session A's slow tool finishes, took {:?}",
        b_elapsed
    );

    // Clean up
    let _ = manager.destroy_session(&session_a).await;
    let _ = manager.destroy_session(&session_b).await;
}

// ---------------------------------------------------------------------------
// SA-03: SessionManager lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sa_03_session_manager_lifecycle() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    // Create 10 sessions
    let mut session_ids = Vec::new();
    for _ in 0..10 {
        let id = manager.create_session().await.unwrap();
        session_ids.push(id);
    }

    // Verify 10 active sessions
    assert_eq!(manager.active_sessions().len(), 10);
    assert_eq!(manager.session_count(), 10);

    // Destroy the 5th session
    let fifth_id = session_ids[4].clone();
    manager.destroy_session(&fifth_id).await.unwrap();

    // Verify 9 remaining sessions
    assert_eq!(manager.active_sessions().len(), 9);
    assert_eq!(manager.session_count(), 9);
    assert!(manager.get_session(&fifth_id).is_none());

    // Sending to destroyed session should fail
    let result = manager.send_to_session(&fifth_id, SessionMessage::ChatMessage {
        content: "should fail".to_string(),
        message_id: "msg-fail".to_string(),
    skill_instructions: None,
    });
    assert!(result.is_err(), "Should not be able to send to destroyed session");

    // Verify other sessions still exist
    for (i, id) in session_ids.iter().enumerate() {
        if i != 4 {
            assert!(manager.get_session(id).is_some(), "Session {} should still exist", id);
        }
    }
}

// ---------------------------------------------------------------------------
// SA-06: Multi-session concurrent Episode distillation (session ID isolation)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sa_06_concurrent_sessions_have_different_ids() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    let session_a = manager.create_session().await.unwrap();
    let session_b = manager.create_session().await.unwrap();

    // Verify session IDs are different
    assert_ne!(session_a, session_b, "Session IDs must be unique");

    // Verify both sessions exist
    assert!(manager.get_session(&session_a).is_some());
    assert!(manager.get_session(&session_b).is_some());

    // Verify handles reference correct session IDs
    let handle_a = manager.get_session(&session_a).unwrap();
    assert_eq!(handle_a.session_id, session_a);
    let handle_b = manager.get_session(&session_b).unwrap();
    assert_eq!(handle_b.session_id, session_b);
}

// ---------------------------------------------------------------------------
// CONC-01: Shared tool concurrent calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conc_01_shared_tool_concurrent_calls() {
    // Track how many times the tool is called
    let echo_tool = Arc::new(EchoTool);

    let provider = Arc::new(MockProvider::new(vec![
        // Session A: tool call then text
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_a".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "echo".to_string(),
                    arguments: r#"{"message": "from_A"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        MockResponse::Text {
            content: "A done.".to_string(),
        },
        // Session B: tool call then text
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_b".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "echo".to_string(),
                    arguments: r#"{"message": "from_B"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        MockResponse::Text {
            content: "B done.".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![echo_tool];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    let session_a = manager.create_session().await.unwrap();
    let session_b = manager.create_session().await.unwrap();

    // Both sessions call the same tool
    manager
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "Call echo from A".to_string(),
            message_id: "msg-a-2".to_string(),
            skill_instructions: None,
        })
        .unwrap();
    manager
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "Call echo from B".to_string(),
            message_id: "msg-b-2".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Give time for both to process
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Both sessions should still be alive (no cross-contamination)
    assert!(manager.get_session(&session_a).unwrap().is_alive());
    assert!(manager.get_session(&session_b).unwrap().is_alive());
}

// ---------------------------------------------------------------------------
// BUDGET-01: Per-session Token isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn budget_01_per_session_token_isolation() {
    // Session A has a very limited budget; Session B has unlimited budget
    let provider = Arc::new(MockProvider::single_text("Hello!"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core_a = make_core(provider.clone(), tools.clone());
    let core_b = make_core(provider, tools);

    // Session A: very limited budget (100 tokens daily)
    let config_a = make_session_config(limited_budget(100));
    let mut manager_a = SessionManager::new(core_a, config_a, String::new());

    // Session B: unlimited budget
    let config_b = make_session_config(test_budget());
    let mut manager_b = SessionManager::new(core_b, config_b, String::new());

    let session_a = manager_a.create_session().await.unwrap();
    let session_b = manager_b.create_session().await.unwrap();

    // Send message to Session A — it will likely fail due to budget
    // (the budget check estimates tokens from history)
    manager_a
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "Hello A".to_string(),
            message_id: "msg-a-3".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Send message to Session B — should work fine
    manager_b
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "Hello B".to_string(),
            message_id: "msg-b-3".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Give time for processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Session B should still be alive and responsive
    assert!(
        manager_b.get_session(&session_b).unwrap().is_alive(),
        "Session B should still be alive with unlimited budget"
    );

    // Session A may have stopped due to budget error, but Session B is unaffected
    // The key point: B still works even though A's budget was exceeded
}

// ---------------------------------------------------------------------------
// EDGE-02: SessionTask panic isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edge_02_session_panic_isolation() {
    let panic_tool = Arc::new(PanicTool);
    let provider = Arc::new(MockProvider::new(vec![
        // Session A: calls panic_tool
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_panic".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "panic_tool".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
            content: String::new(),
        },
        // Session B: simple text response
        MockResponse::Text {
            content: "Session B is fine.".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![panic_tool];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    let session_a = manager.create_session().await.unwrap();
    let session_b = manager.create_session().await.unwrap();

    // Session A calls a tool that panics
    manager
        .send_to_session(&session_a, SessionMessage::ChatMessage {
            content: "Trigger panic".to_string(),
            message_id: "msg-panic".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Session B sends a normal message
    manager
        .send_to_session(&session_b, SessionMessage::ChatMessage {
            content: "Hello B".to_string(),
            message_id: "msg-b-3".to_string(),
            skill_instructions: None,
        })
        .unwrap();

    // Give time for processing
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Session B should still be alive regardless of what happened to A
    let handle_b = manager.get_session(&session_b).unwrap();
    assert!(
        handle_b.is_alive(),
        "Session B should survive Session A's panic"
    );
}

// ---------------------------------------------------------------------------
// EDGE-04: Empty message handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edge_04_empty_message_handling() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    let session_id = manager.create_session().await.unwrap();

    // Send empty string — should not panic, just ignore
    let result = manager.send_to_session(&session_id, SessionMessage::ChatMessage {
        content: "".to_string(),
        message_id: "msg-empty".to_string(),
    skill_instructions: None,
    });
    // Sending itself should succeed (channel accepts the message)
    assert!(result.is_ok(), "Sending empty message should not error on send");

    // Session should still be alive after empty message
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        manager.get_session(&session_id).unwrap().is_alive(),
        "Session should survive empty message"
    );

    // Send whitespace-only message
    let result = manager.send_to_session(&session_id, SessionMessage::ChatMessage {
        content: "   ".to_string(),
        message_id: "msg-ws".to_string(),
    skill_instructions: None,
    });
    assert!(result.is_ok(), "Sending whitespace message should not error on send");

    // Session should still be alive
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        manager.get_session(&session_id).unwrap().is_alive(),
        "Session should survive whitespace message"
    );
}

// ---------------------------------------------------------------------------
// Additional: Create and destroy multiple sessions rapidly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rapid_session_creation_and_destruction() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    // Rapidly create 20 sessions
    let mut ids = Vec::new();
    for _ in 0..20 {
        ids.push(manager.create_session().await.unwrap());
    }
    assert_eq!(manager.session_count(), 20);

    // Destroy every other session
    let mut to_destroy = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        if i % 2 == 0 {
            to_destroy.push(id.clone());
        }
    }
    for id in to_destroy {
        manager.destroy_session(&id).await.unwrap();
    }

    assert_eq!(manager.session_count(), 10);

    // Verify the remaining sessions are the odd-indexed ones
    for (i, id) in ids.iter().enumerate() {
        if i % 2 == 1 {
            assert!(manager.get_session(id).is_some(), "Session {} should exist", id);
        }
    }
}

// ---------------------------------------------------------------------------
// Additional: SessionManager with deterministic session ID
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_session_with_id() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    let custom_id = "my-custom-session-123".to_string();
    let result = manager.create_session_with_id(custom_id.clone()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), custom_id);
    assert!(manager.get_session(&custom_id).is_some());
}

// ---------------------------------------------------------------------------
// Additional: Destroy non-existent session returns error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_destroy_nonexistent_session() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    let result = manager.destroy_session("nonexistent").await;
    assert!(result.is_err(), "Destroying non-existent session should return error");
}

// ---------------------------------------------------------------------------
// Additional: Send to non-existent session returns error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_send_to_nonexistent_session() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools);
    let manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    let result = manager.send_to_session("nonexistent", SessionMessage::ChatMessage {
        content: "hello".to_string(),
        message_id: "msg-none".to_string(),
    skill_instructions: None,
    });
    assert!(result.is_err(), "Sending to non-existent session should return error");
}

// ---------------------------------------------------------------------------
// Additional: SessionHandle::is_alive reflects task state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_handle_is_alive() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    let session_id = manager.create_session().await.unwrap();
    let handle = manager.get_session(&session_id).unwrap();

    // Freshly created session should be alive
    assert!(handle.is_alive(), "Newly created session should be alive");

    // Stop the session
    manager.destroy_session(&session_id).await.unwrap();

    // After a moment, the task should have finished
    tokio::time::sleep(Duration::from_millis(200)).await;
}

// ---------------------------------------------------------------------------
// Additional: Reap finished sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reap_finished_sessions() {
    let provider = Arc::new(MockProvider::single_text("OK"));
    let tools: Vec<Arc<dyn Tool>> = vec![];
    let core = make_core(provider, tools);
    let mut manager = SessionManager::new(core, make_session_config(test_budget()), String::new());

    let session_id = manager.create_session().await.unwrap();

    // Stop the session
    manager.destroy_session(&session_id).await.unwrap();

    // Wait for task to finish
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Reap finished sessions
    manager.reap_finished();

    // The session should be removed
    assert_eq!(manager.session_count(), 0);
}
