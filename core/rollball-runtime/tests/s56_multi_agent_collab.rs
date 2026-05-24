//! S5.6 Multi-Agent Collaboration Integration Tests
//!
//! Validates the three collaboration scenarios from the S5.6 plan:
//! - S5.6.1: Calendar Agent — create/query/delete events via Intent
//! - S5.6.2: Weather + Calendar collaboration — weather query triggers calendar event
//! - S5.6.3: Doc Writer Agent — multi-step task decomposition

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

// ═══════════════════════════════════════════════════════════════════════════
// Mock Tools for S5.6 scenarios
// ═══════════════════════════════════════════════════════════════════════════

/// Mock calendar tool that simulates calendar event operations
struct MockCalendarTool;

#[async_trait]
impl Tool for MockCalendarTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "calendar_event".to_string(),
            description: "Create, query, or delete calendar events".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "operation": {"type": "string", "enum": ["create", "query", "delete"], "description": "Operation type"},
                    "title": {"type": "string", "description": "Event title (for create)"},
                    "time": {"type": "string", "description": "Event time (for create)"},
                    "description": {"type": "string", "description": "Event description (for create)"},
                    "query": {"type": "string", "description": "Search query (for query)"},
                    "event_id": {"type": "string", "description": "Event ID (for delete)"}
                },
                "required": ["operation"]
            }),
        }
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let op = params.get("operation").and_then(|v| v.as_str()).unwrap_or("");

        match op {
            "create" => {
                let title = params.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
                let time = params.get("time").and_then(|v| v.as_str()).unwrap_or("TBD");
                let desc = params
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Ok(ToolResult {
                    ok: true,
                    content: format!(
                        "Event created: '{}' at {} (ID: evt-001){}",
                        title,
                        time,
                        if desc.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", desc)
                        }
                    ),
                    error: None,
                    token_usage: None,
                })
            }
            "query" => {
                let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
                Ok(ToolResult {
                    ok: true,
                    content: format!(
                        "Found 2 events matching '{}': [1] Team Meeting at 10:00, [2] Lunch at 12:00",
                        query
                    ),
                    error: None,
                    token_usage: None,
                })
            }
            "delete" => {
                let event_id = params
                    .get("event_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                Ok(ToolResult {
                    ok: true,
                    content: format!("Event {} deleted successfully", event_id),
                    error: None,
                    token_usage: None,
                })
            }
            _ => Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!("Unknown operation: {}", op)),
                token_usage: None,
            }),
        }
    }
}

/// Mock Intent send tool that simulates cross-agent communication
struct MockIntentSendTool {
    /// Simulated responses per (target, action) pair
    responses: Vec<(String, String, String)>, // (target, action, response_content)
}

impl MockIntentSendTool {
    fn new(responses: Vec<(String, String, String)>) -> Self {
        Self { responses }
    }
}

#[async_trait]
impl Tool for MockIntentSendTool {
    fn spec(&self) -> ToolSpec {
        rollball_runtime::tools::builtin::intent_send::IntentSendTool::new().spec()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let target = params
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Look for a matching simulated response
        for (t, a, response) in &self.responses {
            if t == target && a == action {
                return Ok(ToolResult {
                    ok: true,
                    content: response.clone(),
                    error: None,
                    token_usage: None,
                });
            }
        }

        // Default: generic confirmation
        Ok(ToolResult {
            ok: true,
            content: format!("Intent delivered to {} action={}", target, action),
            error: None,
            token_usage: None,
        })
    }
}

/// Mock doc writer tool that simulates document section writing
struct MockDocWriteTool;

#[async_trait]
impl Tool for MockDocWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "doc_write_section".to_string(),
            description: "Write or revise a document section".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "section": {"type": "string", "description": "Section name"},
                    "content": {"type": "string", "description": "Section content"},
                    "mode": {"type": "string", "enum": ["create", "revise"], "description": "Write or revise mode"}
                },
                "required": ["section", "content"]
            }),
        }
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let section = params
            .get("section")
            .and_then(|v| v.as_str())
            .unwrap_or("unnamed");
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("create");

        Ok(ToolResult {
            ok: true,
            content: format!("Section '{}' {} ({} chars)", section, mode, content.len()),
            error: None,
            token_usage: None,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// S5.6.1: Calendar Agent — create / query / delete events
// ═══════════════════════════════════════════════════════════════════════════

fn calendar_manifest() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.example.calendar"
        version = "1.0.0"
        name = "Calendar Agent"
        description = "Calendar event management"
        author = "Rollball Team"
        runtime_version = "0.1.0"

        identity_deps = ["timezone"]

        [[permissions]]
        type = "MemoryRead"

        [[permissions]]
        type = "MemoryWrite"

        [[permissions]]
        type = "IntentSend"

        [[permissions]]
        type = "IntentReceive"

        [[tools]]
        name = "calendar_event"

        [[tools]]
        name = "memory_store"

        [[tools]]
        name = "memory_recall"

        [[tools]]
        name = "intent_send"

        [llm]
        provider = "mock"
        model = "mock-model"
        temperature = 0.3

        [capabilities.event_create]
        description = "Create a calendar event"

        [capabilities.event_query]
        description = "Query calendar events"

        [capabilities.event_delete]
        description = "Delete a calendar event"
    "#,
    )
    .unwrap()
}

fn calendar_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(MockCalendarTool),
        Arc::new(rollball_runtime::tools::builtin::memory_store::MemoryStoreTool::new(
            "com.example.calendar",
        )),
        Arc::new(rollball_runtime::tools::builtin::memory_recall::MemoryRecallTool::new(
            "com.example.calendar",
        )),
        Arc::new(rollball_runtime::tools::builtin::intent_send::IntentSendTool::new()),
    ]
}

#[tokio::test]
async fn test_s56_calendar_create_event() {
    // LLM calls calendar_event tool to create an event, then summarizes
    let provider = Arc::new(MockProvider::tool_call_then_text(
        "calendar_event",
        r#"{"operation":"create","title":"Team Standup","time":"09:00","description":"Daily sync meeting"}"#,
        "I've created your 'Team Standup' event at 09:00 with the description 'Daily sync meeting'. The event ID is evt-001.",
    ));

    let tools = calendar_tools();
    let manifest = calendar_manifest();
    let config = RuntimeConfig::default();
    let budget = Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context = ContextBuilder::new(
        "You are a Calendar Agent. Help manage calendar events.".to_string(),
    );

    let result = agent_loop
        .run("Create a team standup event at 9am for daily sync", &mut context, None)
        .await;
    assert!(result.is_ok(), "Calendar create should succeed: {:?}", result);

    let response = result.unwrap();
    assert!(
        response.contains("Team Standup") || response.contains("evt-001"),
        "Response should mention the created event: {}",
        response
    );
}

#[tokio::test]
async fn test_s56_calendar_query_events() {
    let provider = Arc::new(MockProvider::tool_call_then_text(
        "calendar_event",
        r#"{"operation":"query","query":"meeting"}"#,
        "Found 2 events matching 'meeting': [1] Team Meeting at 10:00, [2] Lunch at 12:00.",
    ));

    let tools = calendar_tools();
    let manifest = calendar_manifest();
    let config = RuntimeConfig::default();
    let budget = Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context = ContextBuilder::new(
        "You are a Calendar Agent. Help manage calendar events.".to_string(),
    );

    let result = agent_loop
        .run("Show me my meetings", &mut context, None)
        .await;
    assert!(result.is_ok(), "Calendar query should succeed: {:?}", result);

    let response = result.unwrap();
    assert!(
        response.contains("Team Meeting") || response.contains("meeting"),
        "Response should mention queried events: {}",
        response
    );
}

#[tokio::test]
async fn test_s56_calendar_delete_event() {
    let provider = Arc::new(MockProvider::tool_call_then_text(
        "calendar_event",
        r#"{"operation":"delete","event_id":"evt-001"}"#,
        "Event evt-001 has been deleted successfully.",
    ));

    let tools = calendar_tools();
    let manifest = calendar_manifest();
    let config = RuntimeConfig::default();
    let budget = Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context = ContextBuilder::new(
        "You are a Calendar Agent. Help manage calendar events.".to_string(),
    );

    let result = agent_loop
        .run("Delete event evt-001", &mut context, None)
        .await;
    assert!(result.is_ok(), "Calendar delete should succeed: {:?}", result);

    let response = result.unwrap();
    assert!(
        response.contains("deleted") || response.contains("evt-001"),
        "Response should confirm deletion: {}",
        response
    );
}

#[test]
fn test_s56_calendar_manifest_capabilities() {
    let manifest = calendar_manifest();

    assert_eq!(manifest.agent_id, "com.example.calendar");
    assert!(manifest.capabilities.contains_key("event_create"));
    assert!(manifest.capabilities.contains_key("event_query"));
    assert!(manifest.capabilities.contains_key("event_delete"));
    assert!(manifest.has_tool("calendar_event"));
    assert!(manifest.has_tool("intent_send"));
    assert!(manifest.identity_deps.contains(&"timezone".to_string()));
}

#[test]
fn test_s56_calendar_capability_registry_integration() {
    use rollball_gateway::capability::CapabilityRegistry;

    let manifest = calendar_manifest();
    let mut registry = CapabilityRegistry::new();

    // Register calendar agent capabilities
    registry.register_from_manifest("com.example.calendar", &manifest);

    assert_eq!(registry.len(), 3, "Calendar should register 3 capabilities");
    assert!(
        registry.get("com.example.calendar", "event_create").is_some(),
        "event_create capability should be registered"
    );
    assert!(
        registry.get("com.example.calendar", "event_query").is_some(),
        "event_query capability should be registered"
    );
    assert!(
        registry.get("com.example.calendar", "event_delete").is_some(),
        "event_delete capability should be registered"
    );

    // Verify overview
    let overview = registry.overview();
    assert!(overview.by_agent.contains_key("com.example.calendar"));
    assert_eq!(overview.by_agent["com.example.calendar"].len(), 3);
}

// ═══════════════════════════════════════════════════════════════════════════
// S5.6.2: Weather + Calendar collaboration
// ═══════════════════════════════════════════════════════════════════════════

fn weather_manifest_with_intent() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.example.weather"
        version = "1.0.0"
        name = "Weather Agent"
        description = "Weather queries with calendar collaboration"
        author = "Rollball Team"
        runtime_version = "0.1.0"

        identity_deps = ["city"]

        [[permissions]]
        type = "Network"
        value = "https://wttr.in"

        [[permissions]]
        type = "MemoryRead"

        [[permissions]]
        type = "MemoryWrite"

        [[permissions]]
        type = "IntentSend"

        [[tools]]
        name = "http_request"

        [[tools]]
        name = "memory_store"

        [[tools]]
        name = "memory_recall"

        [[tools]]
        name = "intent_send"

        [llm]
        provider = "mock"
        model = "mock-model"
        temperature = 0.7

        [capabilities.weather_query]
        description = "Query weather for a location"
    "#,
    )
    .unwrap()
}

/// Mock weather tool for collaboration test
struct MockWeatherToolForCollab;

#[async_trait]
impl Tool for MockWeatherToolForCollab {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "http_request".to_string(),
            description: "HTTP request tool".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": {"type": "string"},
                    "url": {"type": "string"}
                },
                "required": ["method", "url"]
            }),
        }
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if url.contains("wttr.in") {
            Ok(ToolResult {
                ok: true,
                content: "Shanghai: ☀️ Sunny, 22°C".to_string(),
                error: None,
                token_usage: None,
            })
        } else {
            Ok(ToolResult {
                ok: true,
                content: "HTTP 200 OK".to_string(),
                error: None,
                token_usage: None,
            })
        }
    }
}

#[tokio::test]
async fn test_s56_weather_to_calendar_collaboration() {
    // Scenario: User asks about weather → weather agent checks weather
    // → then sends Intent to calendar agent to create a weather alert event
    let provider = Arc::new(MockProvider::new(vec![
        // Step 1: LLM calls http_request to get weather
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "http_request".to_string(),
                    arguments: r#"{"method":"GET","url":"https://wttr.in/Shanghai?format=3"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        // Step 2: LLM sends Intent to calendar agent
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_2".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "intent_send".to_string(),
                    arguments: r#"{"target":"com.example.calendar","action":"event_create","params":{"title":"Weather Alert: Sunny 22°C","time":"today","description":"Shanghai weather alert"}}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        // Step 3: LLM summarizes the result
        MockResponse::Text {
            content: "Shanghai weather is sunny at 22°C. I've also created a calendar alert for this weather update!".to_string(),
        },
    ]));

    let intent_tool = MockIntentSendTool::new(vec![(
        "com.example.calendar".to_string(),
        "event_create".to_string(),
        "Event created: 'Weather Alert: Sunny 22°C' at today (ID: evt-042)".to_string(),
    )]);

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(MockWeatherToolForCollab),
        Arc::new(rollball_runtime::tools::builtin::memory_store::MemoryStoreTool::new(
            "com.example.weather",
        )),
        Arc::new(rollball_runtime::tools::builtin::memory_recall::MemoryRecallTool::new(
            "com.example.weather",
        )),
        Arc::new(intent_tool),
    ];

    let manifest = weather_manifest_with_intent();
    let config = RuntimeConfig::default();
    let budget = Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context = ContextBuilder::new(
        "You are a Weather Agent. Check weather and create calendar alerts when needed.".to_string(),
    );

    let result = agent_loop
        .run("Check Shanghai weather and create a reminder about it", &mut context, None)
        .await;
    assert!(
        result.is_ok(),
        "Weather+Calendar collaboration should succeed: {:?}",
        result
    );

    let response = result.unwrap();
    assert!(
        response.contains("22") || response.contains("Sunny") || response.contains("Shanghai"),
        "Response should mention weather info: {}",
        response
    );
    assert!(
        response.contains("calendar") || response.contains("alert") || response.contains("reminder"),
        "Response should mention the calendar alert: {}",
        response
    );
}

#[tokio::test]
async fn test_s56_weather_intent_tool_spec_matches() {
    // Verify the intent_send tool spec is consistent between
    // the builtin tool and the mock tool
    let builtin_spec = rollball_runtime::tools::builtin::intent_send::IntentSendTool::new().spec();
    assert_eq!(builtin_spec.name, "intent_send");

    let schema = &builtin_spec.input_schema;
    assert!(schema["properties"]["target"].is_object());
    assert!(schema["properties"]["action"].is_object());
    assert!(schema["required"].as_array().unwrap().contains(&serde_json::Value::String("target".to_string())));
    assert!(schema["required"].as_array().unwrap().contains(&serde_json::Value::String("action".to_string())));
}

#[test]
fn test_s56_multi_agent_capability_cross_registration() {
    use rollball_gateway::capability::CapabilityRegistry;

    let weather_manifest = weather_manifest_with_intent();
    let cal_manifest = calendar_manifest();
    let mut registry = CapabilityRegistry::new();

    // Register both agents' capabilities
    registry.register_from_manifest("com.example.weather", &weather_manifest);
    registry.register_from_manifest("com.example.calendar", &cal_manifest);

    assert_eq!(registry.len(), 4, "Should have 4 total capabilities");

    // Weather can find calendar's event_create capability
    let calendar_create = registry.find_by_action("event_create");
    assert_eq!(calendar_create.len(), 1);
    assert_eq!(calendar_create[0].agent_id, "com.example.calendar");

    // Calendar can find weather's weather_query capability
    let weather_query = registry.find_by_action("weather_query");
    assert_eq!(weather_query.len(), 1);
    assert_eq!(weather_query[0].agent_id, "com.example.weather");

    // Overview should show both agents
    let overview = registry.overview();
    assert_eq!(overview.by_agent.len(), 2);
}

#[test]
fn test_s56_intent_routing_with_capabilities() {
    use rollball_gateway::capability::CapabilityRegistry;

    let weather_manifest = weather_manifest_with_intent();
    let cal_manifest = calendar_manifest();
    let mut registry = CapabilityRegistry::new();

    registry.register_from_manifest("com.example.weather", &weather_manifest);
    registry.register_from_manifest("com.example.calendar", &cal_manifest);

    // Simulate IntentRouter lookup: find target for "event_create" action
    let caps = registry.find_by_action("event_create");
    assert!(!caps.is_empty(), "Should find agent for event_create");

    let target_agent = &caps[0].agent_id;
    assert_eq!(target_agent, "com.example.calendar");

    // Verify the capability definition
    let cap = registry.get("com.example.calendar", "event_create").unwrap();
    assert_eq!(cap.definition.description, "Create a calendar event");
}

// ═══════════════════════════════════════════════════════════════════════════
// S5.6.3: Doc Writer Agent — multi-step task decomposition
// ═══════════════════════════════════════════════════════════════════════════

fn doc_writer_manifest() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.example.docwriter"
        version = "1.0.0"
        name = "Doc Writer Agent"
        description = "Document writing agent with multi-step task decomposition"
        author = "Rollball Team"
        runtime_version = "0.1.0"

        identity_deps = ["name", "language"]

        [[permissions]]
        type = "MemoryRead"

        [[permissions]]
        type = "MemoryWrite"

        [[tools]]
        name = "doc_write_section"

        [[tools]]
        name = "memory_store"

        [[tools]]
        name = "memory_recall"

        [llm]
        provider = "mock"
        model = "mock-model"
        temperature = 0.5

        [capabilities.document_write]
        description = "Write or revise a document based on instructions"
    "#,
    )
    .unwrap()
}

fn doc_writer_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(MockDocWriteTool),
        Arc::new(rollball_runtime::tools::builtin::memory_store::MemoryStoreTool::new(
            "com.example.docwriter",
        )),
        Arc::new(rollball_runtime::tools::builtin::memory_recall::MemoryRecallTool::new(
            "com.example.docwriter",
        )),
    ]
}

#[tokio::test]
async fn test_s56_doc_writer_multi_step_outline_and_write() {
    // Scenario: LLM first writes an outline section, then writes the introduction
    let provider = Arc::new(MockProvider::new(vec![
        // Step 1: Create outline section
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "doc_write_section".to_string(),
                    arguments: r#"{"section":"Outline","content":"1. Introduction\n2. Architecture\n3. Implementation\n4. Conclusion","mode":"create"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        // Step 2: Write Introduction section
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_2".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "doc_write_section".to_string(),
                    arguments: r#"{"section":"Introduction","content":"This document describes the system architecture and implementation details of the RollBall platform.","mode":"create"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        // Step 3: Store writing progress in memory
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_3".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "memory_store".to_string(),
                    arguments: r#"{"key":"doc_progress","content":"Outline and Introduction completed","category":"writing"}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        // Step 4: Final summary
        MockResponse::Text {
            content: "I've created the document outline with 4 sections and written the Introduction. Progress saved to memory.".to_string(),
        },
    ]));

    let tools = doc_writer_tools();
    let manifest = doc_writer_manifest();
    let config = RuntimeConfig::default();
    let budget = Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context = ContextBuilder::new(
        "You are a Document Writing Agent. Break down tasks into steps.".to_string(),
    );

    let result = agent_loop
        .run("Write a technical document about system architecture. Start with an outline and then write the introduction.", &mut context, None)
        .await;
    assert!(result.is_ok(), "Doc writer multi-step should succeed: {:?}", result);

    let response = result.unwrap();
    assert!(
        response.contains("Outline") || response.contains("Introduction"),
        "Response should mention document sections: {}",
        response
    );

    // Verify history has multiple tool calls (multi-step execution)
    let history = agent_loop.history();
    let messages = history.messages();
    assert!(
        messages.len() >= 7,
        "Multi-step execution should have multiple messages (user + 3 tool_calls + 3 tool results + final text), got {}",
        messages.len()
    );
}

#[tokio::test]
async fn test_s56_doc_writer_revise_section() {
    // Scenario: User asks to revise a section
    let provider = Arc::new(MockProvider::tool_call_then_text(
        "doc_write_section",
        r#"{"section":"Architecture","content":"The system uses a microservices architecture with event-driven communication.","mode":"revise"}"#,
        "I've revised the Architecture section with the updated content about microservices and event-driven communication.",
    ));

    let tools = doc_writer_tools();
    let manifest = doc_writer_manifest();
    let config = RuntimeConfig::default();
    let budget = Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context = ContextBuilder::new(
        "You are a Document Writing Agent. Revise sections as requested.".to_string(),
    );

    let result = agent_loop
        .run("Revise the Architecture section to mention microservices", &mut context, None)
        .await;
    assert!(result.is_ok(), "Doc writer revise should succeed: {:?}", result);

    let response = result.unwrap();
    assert!(
        response.contains("Architecture") || response.contains("revised") || response.contains("microservices"),
        "Response should mention the revision: {}",
        response
    );
}

#[test]
fn test_s56_doc_writer_manifest_validation() {
    let manifest = doc_writer_manifest();

    assert_eq!(manifest.agent_id, "com.example.docwriter");
    assert!(manifest.has_tool("doc_write_section"));
    assert!(manifest.has_tool("memory_store"));
    assert!(manifest.has_tool("memory_recall"));
    assert!(manifest.capabilities.contains_key("document_write"));
    assert!(manifest.identity_deps.contains(&"name".to_string()));
    assert!(manifest.identity_deps.contains(&"language".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════════
// S5.6 Cross-cutting: All agents registered in capability registry
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_s56_full_capability_registry_with_all_agents() {
    use rollball_gateway::capability::CapabilityRegistry;

    let weather_manifest = weather_manifest_with_intent();
    let cal_manifest = calendar_manifest();
    let doc_manifest = doc_writer_manifest();
    let mut registry = CapabilityRegistry::new();

    // Register all three agents
    registry.register_from_manifest("com.example.weather", &weather_manifest);
    registry.register_from_manifest("com.example.calendar", &cal_manifest);
    registry.register_from_manifest("com.example.docwriter", &doc_manifest);

    assert_eq!(registry.len(), 5, "Should have 5 total capabilities across 3 agents");

    // Verify each agent's capabilities
    let weather_caps = registry.capabilities_for_agent("com.example.weather");
    assert_eq!(weather_caps.len(), 1);

    let cal_caps = registry.capabilities_for_agent("com.example.calendar");
    assert_eq!(cal_caps.len(), 3);

    let doc_caps = registry.capabilities_for_agent("com.example.docwriter");
    assert_eq!(doc_caps.len(), 1);

    // Unregister calendar agent
    registry.unregister_agent("com.example.calendar");
    assert_eq!(registry.len(), 2, "After unregistering calendar, should have 2 capabilities");
    assert!(
        registry.get("com.example.calendar", "event_create").is_none(),
        "Calendar capabilities should be removed"
    );
    assert!(
        registry.get("com.example.weather", "weather_query").is_some(),
        "Weather capabilities should still exist"
    );
}

#[test]
fn test_s56_intent_privacy_filter_in_collaboration() {
    use rollball_gateway::intent::IntentRouter;
    use serde_json::json;

    let router = IntentRouter::new();

    // Simulate a response from calendar agent that includes some memory data
    let response = json!({
        "event_id": "evt-042",
        "title": "Weather Alert",
        "memories": [
            { "id": "m1", "content": "User prefers morning meetings", "metadata": null, "zone": "semantic", "privacy_level": "Public" },
            { "id": "m2", "content": "User SSN: 123-45-6789", "metadata": null, "zone": "semantic", "privacy_level": "Sensitive" }
        ]
    });

    let filtered = router.filter_response(response);

    // Sensitive memory should be filtered out
    let memories = filtered.get("memories").unwrap().as_array().unwrap();
    assert_eq!(memories.len(), 1, "Sensitive memory should be filtered");
    assert_eq!(memories[0]["content"], "User prefers morning meetings");

    // Non-memory fields should pass through
    assert_eq!(filtered["event_id"], "evt-042");
    assert_eq!(filtered["title"], "Weather Alert");
}

#[test]
fn test_s56_all_example_manifests_parse_correctly() {
    // Verify all example agent manifests from the examples/ directory can be parsed
    let manifests = vec![
        ("com.example.weather", r#"
            agent_id = "com.example.weather"
            version = "1.0.0"
            name = "Weather Agent"
            description = "Weather queries"
            author = "Rollball Team"
            runtime_version = "0.1.0"

            identity_deps = ["city"]

            [[permissions]]
            type = "IntentSend"

            [[tools]]
            name = "http_request"

            [[tools]]
            name = "intent_send"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [llm.providers.openai]
            model = "gpt-4o"
            api_key_ref = "vault:openai_key"

            [llm.routing]
            strategy = "cost_priority"
            fallback_order = ["openai"]

            [capabilities.weather_query]
            description = "Query weather for a location"
        "#),
        ("com.example.calendar", r#"
            agent_id = "com.example.calendar"
            version = "1.0.0"
            name = "Calendar Agent"
            description = "Calendar event management"
            author = "Rollball Team"
            runtime_version = "0.1.0"

            identity_deps = ["timezone"]

            [[permissions]]
            type = "IntentSend"

            [[permissions]]
            type = "IntentReceive"

            [[tools]]
            name = "intent_send"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [capabilities.event_create]
            description = "Create a calendar event"

            [capabilities.event_query]
            description = "Query events"

            [capabilities.event_delete]
            description = "Delete an event"
        "#),
        ("com.example.docwriter", r#"
            agent_id = "com.example.docwriter"
            version = "1.0.0"
            name = "Doc Writer Agent"
            description = "Document writing"
            author = "Rollball Team"
            runtime_version = "0.1.0"

            identity_deps = ["name", "language"]

            [[permissions]]
            type = "MemoryRead"

            [[permissions]]
            type = "MemoryWrite"

            [[tools]]
            name = "memory_store"

            [[tools]]
            name = "memory_recall"

            [llm]
            provider = "anthropic"
            model = "claude-sonnet-4"

            [llm.providers.anthropic]
            model = "claude-sonnet-4"
            api_key_ref = "vault:anthropic_key"

            [llm.providers.openai]
            model = "gpt-4o"
            api_key_ref = "vault:openai_key"

            [llm.routing]
            strategy = "quality_priority"
            fallback_order = ["anthropic", "openai"]

            [capabilities.document_write]
            description = "Write or revise a document"
        "#),
    ];

    for (expected_id, toml_str) in manifests {
        let manifest = rollball_core::AgentManifest::from_toml(toml_str)
            .unwrap_or_else(|e| panic!("Failed to parse manifest for {}: {}", expected_id, e));
        assert_eq!(manifest.agent_id, expected_id);
    }
}
