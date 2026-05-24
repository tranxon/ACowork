//! End-to-end weather agent test with MockProvider
//!
//! Simulates a complete weather agent conversation flow:
//! 1. Load weather-agent manifest
//! 2. Build tools (http_request, memory_store, memory_recall)
//! 3. Create MockProvider that simulates weather tool calls
//! 4. Run the agent loop with user messages
//! 5. Verify tool calls, memory operations, and final responses

use std::sync::Arc;

use rollball_core::providers::mock::{MockProvider, MockResponse};
use rollball_core::providers::traits::{FunctionCall, ToolCall};
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use rollball_core::Budget;

use rollball_runtime::agent::context::ContextBuilder;
use rollball_runtime::agent::loop_::AgentLoop;
use rollball_runtime::config::RuntimeConfig;

use async_trait::async_trait;
use serde_json::Value;

// ── Mock weather tool ──────────────────────────────────────────────────

/// Mock weather tool that returns canned weather data
struct MockWeatherTool;

#[async_trait]
impl Tool for MockWeatherTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "http_request".to_string(),
            description: "Make an HTTP request. For weather, use GET https://wttr.in/{city}?format=3".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "DELETE"]},
                    "url": {"type": "string", "description": "Request URL"},
                    "headers": {"type": "object", "description": "Optional headers"},
                    "body": {"type": "string", "description": "Optional request body"}
                },
                "required": ["method", "url"]
            }),
        }
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let url = params.get("url").and_then(|v| v.as_str()).unwrap_or("");

        // Simulate wttr.in response
        if url.contains("wttr.in") {
            let city = url
                .split("wttr.in/")
                .nth(1)
                .and_then(|s| s.split('?').next())
                .unwrap_or("Unknown");
            let weather = format!("{}: ☀️ Sunny, 22°C", city);
            Ok(ToolResult {
                ok: true,
                content: weather,
                error: None,
                token_usage: None,
            })
        } else {
            Ok(ToolResult {
                ok: true,
                content: format!("HTTP response from {}", url),
                error: None,
                token_usage: None,
            })
        }
    }
}

// ── Test helpers ────────────────────────────────────────────────────────

fn weather_manifest() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.example.weather"
        version = "1.0.0"
        name = "Weather Agent"
        description = "A weather query agent"
        author = "Rollball Team"
        runtime_version = "0.1.0"

        [[permissions]]
        type = "Network"
        value = "https://wttr.in"

        [[permissions]]
        type = "MemoryRead"

        [[permissions]]
        type = "MemoryWrite"

        [[tools]]
        name = "http_request"

        [[tools]]
        name = "memory_store"

        [[tools]]
        name = "memory_recall"

        [llm]
        provider = "mock"
        model = "mock-model"
        temperature = 0.7
    "#,
    )
    .unwrap()
}

fn weather_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(MockWeatherTool),
        Arc::new(rollball_runtime::tools::builtin::memory_store::MemoryStoreTool::new("com.example.weather")),
        Arc::new(rollball_runtime::tools::builtin::memory_recall::MemoryRecallTool::new("com.example.weather")),
    ]
}

fn weather_config() -> RuntimeConfig {
    RuntimeConfig::default()
}

fn weather_budget() -> Budget {
    Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    }
}

fn system_prompt() -> String {
    "You are a helpful weather assistant. You can query weather using the http_request tool (GET https://wttr.in/{city}?format=3). Remember user city preferences with memory tools.".to_string()
}

// ── E2E Tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_weather_simple_query() {
    // Mock provider: LLM calls http_request tool, then returns text
    let provider = Arc::new(MockProvider::tool_call_then_text(
        "http_request",
        r#"{"method":"GET","url":"https://wttr.in/Shanghai?format=3"}"#,
        "The weather in Shanghai is sunny, 22°C!",
    ));

    let tools = weather_tools();
    let manifest = weather_manifest();
    let config = weather_config();
    let budget = weather_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new(system_prompt());

    let result = agent_loop.run("What's the weather in Shanghai?", &mut context_builder, None).await;
    assert!(result.is_ok(), "Weather query should succeed: {:?}", result);

    let response = result.unwrap();
    assert!(
        response.contains("Shanghai") || response.contains("22"),
        "Response should mention Shanghai or temperature: {}",
        response
    );
}

#[tokio::test]
async fn test_e2e_weather_with_memory_store() {
    // Simulate: LLM first queries weather, then stores city preference
    let provider = Arc::new(MockProvider::new(vec![
        // First: call http_request to get weather
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "http_request".to_string(),
                    arguments: r#"{"method":"GET","url":"https://wttr.in/Beijing?format=3"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        // Second: store city preference in memory
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_2".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "memory_store".to_string(),
                    arguments: r#"{"key":"preferred_city","content":"Beijing","category":"core"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        // Finally: text response
        MockResponse::Text {
            content: "The weather in Beijing is sunny, 20°C. I've remembered Beijing as your preferred city!".to_string(),
        },
    ]));

    let tools = weather_tools();
    let manifest = weather_manifest();
    let config = weather_config();
    let budget = weather_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new(system_prompt());

    let result = agent_loop.run("What's the weather in Beijing? Remember this city!", &mut context_builder, None).await;
    assert!(result.is_ok(), "Weather + memory query should succeed: {:?}", result);

    let response = result.unwrap();
    assert!(
        response.contains("Beijing") || response.contains("20"),
        "Response should mention Beijing or temperature: {}",
        response
    );

    // Verify history has the tool calls
    let history = agent_loop.history();
    let messages = history.messages();
    // Should have: user, assistant (http_request), tool, assistant (memory_store), tool, assistant (text)
    assert!(messages.len() >= 5, "History should have multiple messages, got {}", messages.len());
}

#[tokio::test]
async fn test_e2e_weather_text_only_response() {
    // Simulate: LLM responds directly without tool calls
    let provider = Arc::new(MockProvider::single_text(
        "I can help you check the weather! Which city would you like to know about?",
    ));

    let tools = weather_tools();
    let manifest = weather_manifest();
    let config = weather_config();
    let budget = weather_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new(system_prompt());

    let result = agent_loop.run("Hello!", &mut context_builder, None).await;
    assert!(result.is_ok(), "Simple text response should succeed");

    let response = result.unwrap();
    assert!(
        response.contains("weather") || response.contains("city"),
        "Response should mention weather or city: {}",
        response
    );
}

#[tokio::test]
async fn test_e2e_weather_manifest_loads_correctly() {
    let manifest = weather_manifest();

    assert_eq!(manifest.agent_id, "com.example.weather");
    assert_eq!(manifest.name, "Weather Agent");
    assert_eq!(manifest.llm.suggested_provider, "mock");
    assert!(manifest.has_tool("http_request"));
    assert!(manifest.has_tool("memory_store"));
    assert!(manifest.has_tool("memory_recall"));
    assert!(!manifest.has_tool("shell"), "Shell should not be declared");
}

#[tokio::test]
async fn test_e2e_weather_tool_spec_available() {
    let tools = weather_tools();
    let tool_names: Vec<String> = tools.iter().map(|t| t.name()).collect();

    assert!(tool_names.contains(&"http_request".to_string()), "http_request tool should be available");
    assert!(tool_names.contains(&"memory_store".to_string()), "memory_store tool should be available");
    assert!(tool_names.contains(&"memory_recall".to_string()), "memory_recall tool should be available");
}

#[tokio::test]
async fn test_e2e_weather_mock_tool_execution() {
    let tool = MockWeatherTool;

    // Test weather query
    let result = tool
        .execute(serde_json::json!({
            "method": "GET",
            "url": "https://wttr.in/Shanghai?format=3"
        }))
        .await
        .unwrap();

    assert!(result.ok);
    assert!(result.content.contains("Shanghai"), "Should contain city name");
    assert!(result.content.contains("Sunny") || result.content.contains("22"), "Should contain weather info");
}
