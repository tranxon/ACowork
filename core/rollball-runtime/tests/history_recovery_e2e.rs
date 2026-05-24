//! End-to-end integration tests for agent history sanitization and recovery
//!
//! Validates the sanitize_messages mechanism in HistoryManager that prevents
//! LLM 400 errors caused by corrupted tool_call data when conversation history
//! is replayed after an agent restart.
//!
//! Covers:
//! 1. Invalid tool_call arguments cleaning (non-JSON → `{}`)
//! 2. Orphaned tool result removal (no matching assistant tool_call)
//! 3. Orphaned tool_call removal (no matching tool result)
//! 4. Agent restart simulation with polluted history
//! 5. Mixed corruption scenario
//! 6. Sanitize idempotency
//! 7. Valid history preservation
//! 8. Real LLM closed-loop with tool_call_id field (#[ignore])

use std::sync::Arc;

use rollball_core::providers::mock::{MockProvider, MockResponse};
use rollball_core::providers::traits::{ChatMessage, FunctionCall, MessageRole, ToolCall};
use rollball_core::tools::traits::Tool;
use rollball_core::Budget;

use rollball_runtime::agent::context::ContextBuilder;
use rollball_runtime::agent::history::HistoryManager;
use rollball_runtime::agent::loop_::AgentLoop;
use rollball_runtime::config::RuntimeConfig;
use rollball_runtime::tools::builtin;
use rollball_runtime::tools::workspace_resolver::WorkspaceResolver;

// ── Test helpers ─────────────────────────────────────────────────────────

/// Create a basic user message
fn make_user_message(content: &str) -> ChatMessage {
    ChatMessage::user(content)
}

/// Create a tool call with given id, name, and arguments
fn make_tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: name.to_string(),
            arguments: arguments.to_string(),
        },
    }
}

/// Create a tool result message referencing a tool_call_id
fn make_tool_result(tool_call_id: &str, content: &str) -> ChatMessage {
    ChatMessage::tool(tool_call_id, content)
}

/// Create an assistant message with tool_calls
fn make_assistant_with_tool_calls(content: &str, tool_calls: Vec<ToolCall>) -> ChatMessage {
    ChatMessage::assistant_with_tools(content, tool_calls)
}

/// Create an assistant message with text only
fn make_assistant_text(content: &str) -> ChatMessage {
    ChatMessage::assistant(content)
}

/// Create a manifest with all permissions and declared tools
fn full_manifest() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.test.history-recovery"
        version = "1.0.0"
        name = "History Recovery E2E Test Agent"
        description = "Agent history sanitization and recovery e2e test"
        author = "test"
        runtime_version = "0.1.0"

        [[permissions]]
        type = "Shell"

        [[permissions]]
        type = "Network"

        [[permissions]]
        type = "MemoryRead"

        [[permissions]]
        type = "MemoryWrite"

        [[permissions]]
        type = "FilesystemRead"

        [[permissions]]
        type = "FilesystemWrite"

        [[permissions]]
        type = "IntentSend"

        [[permissions]]
        type = "IdentityWrite"

        [[permissions]]
        type = "IdentityRead"

        [[tools]]
        name = "shell"

        [[tools]]
        name = "file_read"

        [[tools]]
        name = "file_write"

        [[tools]]
        name = "file_edit"

        [[tools]]
        name = "glob_search"

        [[tools]]
        name = "content_search"

        [[tools]]
        name = "memory_store"

        [[tools]]
        name = "memory_recall"

        [[tools]]
        name = "http_request"

        [[tools]]
        name = "intent_send"

        [llm]
        provider = "mock"
        model = "mock-model"
    "#,
    )
    .unwrap()
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

// ═══════════════════════════════════════════════════════════════════════
// 1. Invalid tool_call arguments cleaning
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_sanitize_invalid_tool_call_arguments() {
    // Build a conversation with corrupted tool_call arguments:
    // - user message
    // - assistant message with tool_calls where arguments = "not valid json {"
    // - tool result message
    // Verify that after ContextBuilder.build(), the tool_call arguments
    // have been fixed to "{}" by sanitize_messages.
    let mut history = HistoryManager::new(10000, 4);
    history.append(make_user_message("Read a file"));
    history.append(make_assistant_with_tool_calls(
        "",
        vec![
            make_tool_call("tc_bad", "file_read", "not valid json {"),
            make_tool_call("tc_good", "file_read", r#"{"path":"/tmp"}"#),
        ],
    ));
    history.append(make_tool_result("tc_bad", "result 1"));
    history.append(make_tool_result("tc_good", "result 2"));

    let manifest = full_manifest();
    let builder = ContextBuilder::new("You are a test assistant.".to_string());
    let request = builder.build(&manifest, &history, None, 32_768);

    // Find the assistant message with tool_calls in the built request
    let assistant_msg = request
        .messages
        .iter()
        .find(|m| m.tool_calls.is_some())
        .expect("Should have an assistant message with tool_calls");

    let tool_calls = assistant_msg.tool_calls.as_ref().unwrap();
    // Invalid arguments should be fixed to "{}"
    assert_eq!(tool_calls[0].function.arguments, "{}");
    // Valid arguments should be unchanged
    assert_eq!(tool_calls[1].function.arguments, r#"{"path":"/tmp"}"#);
}

// ═══════════════════════════════════════════════════════════════════════
// 2. Orphaned tool result removal
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_sanitize_orphaned_tool_result() {
    // Build a conversation with an orphaned tool result:
    // - user message
    // - tool result message (tool_call_id does not match any assistant tool_call)
    // Verify that after build(), the orphaned tool result is removed.
    let mut history = HistoryManager::new(10000, 4);
    history.append(make_user_message("Hello"));
    history.append(make_assistant_with_tool_calls(
        "I'll help you",
        vec![make_tool_call("tc_1", "file_read", "{}")],
    ));
    history.append(make_tool_result("tc_1", "result 1"));
    history.append(make_tool_result("tc_orphan", "orphaned result"));

    let manifest = full_manifest();
    let builder = ContextBuilder::new("You are a test assistant.".to_string());
    let request = builder.build(&manifest, &history, None, 32_768);

    // Only tc_1's result should remain; tc_orphan should be removed
    let tool_results: Vec<_> = request
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .collect();
    assert_eq!(tool_results.len(), 1, "Orphaned tool result should be removed");
    assert_eq!(tool_results[0].tool_call_id, Some("tc_1".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════
// 3. Orphaned tool_call removal (no corresponding result)
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_sanitize_orphaned_tool_call() {
    // Build a conversation with an orphaned tool_call:
    // - user message
    // - assistant message with tool_calls (but no corresponding tool result follows)
    // This simulates an agent crash after the LLM emitted a tool_call
    // but before the tool result was recorded.
    // Verify that after build(), the orphaned tool_call is removed.
    let mut history = HistoryManager::new(10000, 4);
    history.append(make_user_message("Do something"));
    history.append(make_assistant_with_tool_calls(
        "",
        vec![
            make_tool_call("tc_1", "file_read", "{}"),
            make_tool_call("tc_2", "file_write", "{}"),
        ],
    ));
    // Only tc_1 has a result; tc_2 is orphaned
    history.append(make_tool_result("tc_1", "result 1"));

    let manifest = full_manifest();
    let builder = ContextBuilder::new("You are a test assistant.".to_string());
    let request = builder.build(&manifest, &history, None, 32_768);

    // Find the assistant message and verify only tc_1 remains
    let assistant_msg = request
        .messages
        .iter()
        .find(|m| m.tool_calls.is_some())
        .expect("Should have an assistant message with tool_calls");

    let tool_calls = assistant_msg.tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls.len(), 1, "Orphaned tool_call tc_2 should be removed");
    assert_eq!(tool_calls[0].id, "tc_1");
}

// ═══════════════════════════════════════════════════════════════════════
// 4. Agent restart simulation — polluted history does not cause 400
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_agent_restart_with_polluted_history_no_400() {
    // Simulate a full AgentLoop restart with corrupted history:
    // 1. Manually inject polluted history into HistoryManager
    // 2. Send a new user message
    // 3. Verify AgentLoop runs normally — no panic, no invalid request
    // Key: sanitize is called inside build(), fixing the history before
    // it is sent to the LLM.
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Write a test file for the tool to read
    std::fs::write(tmp.path().join("test.txt"), "Hello from file!").unwrap();

    // Use Arc<MockProvider> so we can inspect the last request after the run
    let mock_provider = Arc::new(MockProvider::new(vec![
        // The mock will be called with sanitized history; it should receive valid messages
        MockResponse::Text {
            content: "I have recovered and processed your request.".to_string(),
        },
    ]));
    let provider: Arc<dyn rollball_core::providers::traits::Provider> = mock_provider.clone();

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(builtin::file_read::FileReadTool::new(&work_dir)),
    ];

    let manifest = full_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);

    // Inject polluted history — invalid arguments, orphaned tool result
    {
        let history = agent_loop.history_mut();
        history.append(make_user_message("Previous request"));
        history.append(make_assistant_with_tool_calls(
            "",
            vec![make_tool_call("tc_bad_args", "file_read", "broken json {{{")],
        ));
        history.append(make_tool_result("tc_bad_args", "partial result"));
        history.append(make_tool_result("tc_orphan", "orphaned result"));
    }

    // Run with a new user message — should NOT panic or error
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());
    let result = agent_loop
        .run("New message after restart", &mut context_builder, None)
        .await;
    assert!(
        result.is_ok(),
        "Agent should recover from polluted history: {:?}",
        result
    );

    // Verify the provider received a valid request (no corrupted messages)
    let last_request = mock_provider
        .last_request()
        .expect("should have a last request");

    // Verify no orphaned tool results in the request sent to LLM
    let tool_results: Vec<_> = last_request
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .collect();
    for tr in &tool_results {
        assert!(
            tr.tool_call_id.as_ref().is_some_and(|id| id == "tc_bad_args"),
            "Only tc_bad_args result should remain, found: {:?}",
            tr.tool_call_id
        );
    }

    // Verify all tool_call arguments are valid JSON
    for msg in &last_request.messages {
        if let Some(ref tool_calls) = msg.tool_calls {
            for tc in tool_calls {
                assert!(
                    serde_json::from_str::<serde_json::Value>(&tc.function.arguments).is_ok(),
                    "Tool call arguments should be valid JSON after sanitize, got: {}",
                    tc.function.arguments
                );
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 5. Mixed corruption scenario
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_sanitize_mixed_corruption() {
    // Build a conversation with multiple types of corruption simultaneously:
    // - Invalid JSON arguments
    // - Orphaned tool result
    // - Empty assistant message
    // Verify all are correctly sanitized and remaining messages are valid.
    let mut messages = vec![
        make_user_message("Do multiple things"),
        // Empty assistant message (should be removed)
        ChatMessage::assistant(""),
        // Assistant with invalid arguments + valid tool_call
        make_assistant_with_tool_calls(
            "",
            vec![
                make_tool_call("tc_invalid", "file_read", "not json at all"),
                make_tool_call("tc_valid", "shell", r#"{"command":"ls"}"#),
            ],
        ),
        make_tool_result("tc_invalid", "result for invalid args"),
        make_tool_result("tc_valid", "result for valid args"),
        // Orphaned tool result
        make_tool_result("tc_ghost", "nobody called me"),
        make_user_message("Continue"),
        make_assistant_text("Done"),
    ];

    HistoryManager::sanitize_messages(&mut messages);

    // Original 8 messages: user, empty_asst, asst_w_calls, tool_invalid, tool_valid, tool_ghost, user, asst
    // After sanitize:
    //   - empty assistant removed (no content, no tool_calls)
    //   - orphaned tool result tc_ghost removed (no matching tool_call)
    //   - invalid arguments fixed to "{}"
    // Remaining 6 messages: user, asst_w_calls, tool_invalid, tool_valid, user, asst
    assert_eq!(messages.len(), 6, "Should have 6 messages after sanitize");

    // Message 0: user
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].content, "Do multiple things");

    // Message 1: assistant with tool_calls (empty assistant was removed)
    assert_eq!(messages[1].role, MessageRole::Assistant);
    let tool_calls = messages[1].tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls.len(), 2);
    // Invalid arguments fixed to "{}"
    assert_eq!(tool_calls[0].function.arguments, "{}");
    // Valid arguments unchanged
    assert_eq!(tool_calls[1].function.arguments, r#"{"command":"ls"}"#);

    // Message 2: tool result for tc_invalid
    assert_eq!(messages[2].role, MessageRole::Tool);
    assert_eq!(messages[2].tool_call_id, Some("tc_invalid".to_string()));

    // Message 3: tool result for tc_valid
    assert_eq!(messages[3].role, MessageRole::Tool);
    assert_eq!(messages[3].tool_call_id, Some("tc_valid".to_string()));

    // Message 4: user "Continue"
    assert_eq!(messages[4].role, MessageRole::User);
    assert_eq!(messages[4].content, "Continue");

    // Message 5: assistant "Done"
    assert_eq!(messages[5].role, MessageRole::Assistant);
    assert_eq!(messages[5].content, "Done");
}

// ═══════════════════════════════════════════════════════════════════════
// 6. Sanitize idempotency
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_sanitize_is_idempotent() {
    // Apply sanitize_messages twice to the same input.
    // Verify the results are identical.
    let mut messages = vec![
        make_user_message("Test idempotency"),
        make_assistant_with_tool_calls(
            "",
            vec![make_tool_call("tc_1", "file_read", "broken json")],
        ),
        make_tool_result("tc_1", "result 1"),
        make_tool_result("tc_orphan", "orphan"),
    ];

    // First sanitize
    HistoryManager::sanitize_messages(&mut messages);
    let first_result = messages.clone();

    // Second sanitize
    HistoryManager::sanitize_messages(&mut messages);

    // Verify identical results
    assert_eq!(messages.len(), first_result.len());
    for (a, b) in messages.iter().zip(first_result.iter()) {
        assert_eq!(a.role, b.role, "Roles should match after second sanitize");
        assert_eq!(a.content, b.content, "Content should match after second sanitize");
        assert_eq!(a.tool_call_id, b.tool_call_id, "tool_call_id should match");
        // Compare tool_calls
        match (&a.tool_calls, &b.tool_calls) {
            (None, None) => {}
            (Some(a_tc), Some(b_tc)) => {
                assert_eq!(a_tc.len(), b_tc.len());
                for (ac, bc) in a_tc.iter().zip(b_tc.iter()) {
                    assert_eq!(ac.id, bc.id);
                    assert_eq!(ac.function.name, bc.function.name);
                    assert_eq!(ac.function.arguments, bc.function.arguments);
                }
            }
            _ => panic!("tool_calls mismatch: one is Some, other is None"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 7. Valid history preservation
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_sanitize_preserves_valid_history() {
    // Build a perfectly valid conversation with complete tool_call + tool result pairs.
    // Verify that sanitize does not alter message count or content.
    let mut messages = vec![
        ChatMessage::system("You are a helpful assistant."),
        make_user_message("Read the file"),
        make_assistant_with_tool_calls(
            "Let me check that for you.",
            vec![make_tool_call("tc_1", "file_read", r#"{"path":"test.txt"}"#)],
        ),
        make_tool_result("tc_1", "File contents: Hello world"),
        make_assistant_text("The file contains: Hello world"),
        make_user_message("Thanks!"),
        make_assistant_text("You're welcome!"),
    ];

    let original_len = messages.len();
    let original_content: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();

    HistoryManager::sanitize_messages(&mut messages);

    // Verify message count unchanged
    assert_eq!(
        messages.len(),
        original_len,
        "Valid history should not lose messages"
    );

    // Verify content unchanged
    for (i, msg) in messages.iter().enumerate() {
        assert_eq!(
            msg.content, original_content[i],
            "Message {} content should be preserved",
            i
        );
    }

    // Verify all tool_calls and tool_results still properly paired
    let tool_call_ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flat_map(|tcs| tcs.iter().map(|tc| tc.id.clone()))
        .collect();
    let tool_result_ids: Vec<String> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    assert_eq!(tool_call_ids, tool_result_ids, "Tool call/result IDs should match");
}

// ═══════════════════════════════════════════════════════════════════════
// 8. Real LLM closed-loop with tool_call_id field (#[ignore])
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore]
async fn test_llm_tool_call_with_proper_tool_call_id() {
    // Use real MiniMax API to verify the full tool_call roundtrip:
    // user message → LLM generates tool_call → construct tool result with
    // tool_call_id → send back to LLM → final response.
    // This validates that the ChatMessage.tool_call_id field properly
    // resolves the 400 error issue with real providers.
    use rollball_core::providers::traits::{ChatRequest, Provider};
    use rollball_runtime::providers::openai::OpenAIProvider;
    use std::env;

    let api_key = match env::var("MINIMAX_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("MINIMAX_API_KEY not set, skipping real LLM test");
            return;
        }
    };

    let provider = OpenAIProvider::with_base_url(
        Some("https://api.minimax.chat/v1"),
        Some(&api_key),
    );

    // Build tool definitions in OpenAI format
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();
    std::fs::write(tmp.path().join("hello.txt"), "Hello from RollBall!").unwrap();

    let resolver = Arc::new(std::sync::RwLock::new(WorkspaceResolver::new(&work_dir)));
    let builtin_tools = builtin::all_builtin_tools(&resolver, "com.test.history-recovery");
    let tool_jsons: Vec<serde_json::Value> = builtin_tools
        .iter()
        .map(|t| serde_json::to_value(t.spec()).unwrap())
        .collect();
    let openai_tools: Vec<serde_json::Value> = tool_jsons
        .iter()
        .map(|tool| {
            let name = tool["name"].as_str().unwrap_or("unknown");
            let description = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let parameters = match tool.get("parameters") {
                Some(p) if p.is_object() => p.clone(),
                _ => serde_json::json!({"type": "object", "properties": {}}),
            };
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                }
            })
        })
        .collect();

    // Step 1: Send user message with tools
    let request1 = ChatRequest {
        model: "MiniMax-M2.5".to_string(),
        messages: vec![
            ChatMessage::system("You are a helpful assistant. Use the file_read tool when asked to read files."),
            ChatMessage::user("Please read the file hello.txt"),
        ],
        temperature: Some(0.1),
        max_tokens: None,
        tools: Some(openai_tools.clone()),
    };

    let response1 = provider.chat(request1).await.expect("First LLM call should succeed");
    assert!(
        response1.tool_calls.is_some(),
        "LLM should return tool_calls for file read request"
    );

    let tool_calls = response1.tool_calls.unwrap();
    assert!(!tool_calls.is_empty(), "Should have at least one tool_call");

    // Step 2: Build tool result message using tool_call_id from the LLM response
    let tc = &tool_calls[0];
    let tool_result_msg = ChatMessage::tool(tc.id.clone(), "File contents: Hello from RollBall!");

    // Step 3: Send back the tool result — this should NOT produce a 400 error
    let request2 = ChatRequest {
        model: "MiniMax-M2.5".to_string(),
        messages: vec![
            ChatMessage::system("You are a helpful assistant."),
            ChatMessage::user("Please read the file hello.txt"),
            ChatMessage {
                tool_calls: Some(tool_calls.clone()),
                ..ChatMessage::assistant(response1.content.clone())
            },
            tool_result_msg,
        ],
        temperature: Some(0.1),
        max_tokens: None,
        tools: Some(openai_tools),
    };

    let response2 = provider.chat(request2).await;
    assert!(
        response2.is_ok(),
        "Second LLM call with tool_call_id should not produce 400 error: {:?}",
        response2
    );
}
