//! Runtime main loop integration test

use std::sync::Arc;

use rollball_core::providers::mock::{MockProvider, MockResponse};
use rollball_core::providers::traits::{
    FunctionCall, ToolCall,
};
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use rollball_core::Budget;

use rollball_runtime::agent::context::ContextBuilder;
use rollball_runtime::agent::loop_::AgentLoop;
use rollball_runtime::config::RuntimeConfig;

use async_trait::async_trait;
use serde_json::Value;

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

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("no message");
        Ok(ToolResult {
            ok: true,
            content: format!("Echo: {message}"),
            error: None,
            token_usage: None,
        })
    }
}

fn test_manifest() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(r#"
        agent_id = "com.test.loop"
        version = "1.0.0"
        name = "Loop Test Agent"
        description = "Test"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "mock"
        model = "mock-model"

        [[tools]]
        name = "echo"
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

#[tokio::test]
async fn test_agent_loop_text_response() {
    let provider = Arc::new(MockProvider::single_text("Hello! I can help you."));

    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];
    let manifest = test_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest.clone(), provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());

    let result = agent_loop.run("Hi there!", &mut context_builder, None).await;
    assert!(result.is_ok(), "Agent loop should succeed");
    let response = result.unwrap();
    assert_eq!(response, "Hello! I can help you.");
}

#[tokio::test]
async fn test_agent_loop_tool_call_then_text() {
    let provider = Arc::new(MockProvider::tool_call_then_text(
        "echo",
        r#"{"message": "test"}"#,
        "I echoed your message!",
    ));

    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];
    let manifest = test_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest.clone(), provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());

    let result = agent_loop.run("Echo 'test'", &mut context_builder, None).await;
    assert!(result.is_ok(), "Agent loop with tool call should succeed");
    let response = result.unwrap();
    assert_eq!(response, "I echoed your message!");

    let history = agent_loop.history();
    let messages = history.messages();
    assert!(messages.len() >= 4, "History should have at least 4 messages");
}

#[tokio::test]
async fn test_agent_loop_deduplication() {
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls {
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "echo".to_string(),
                        arguments: r#"{"message": "dup"}"#.to_string(),
                    },
                },
                ToolCall {
                    id: "call_2".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "echo".to_string(),
                        arguments: r#"{"message": "dup"}"#.to_string(),
                    },
                },
            ],
            content: String::new(),
        },
        MockResponse::Text {
            content: "Done after dedup.".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];
    let manifest = test_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest.clone(), provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());

    let result = agent_loop.run("Echo 'dup'", &mut context_builder, None).await;
    assert!(result.is_ok(), "Agent loop with dedup should succeed");
}
