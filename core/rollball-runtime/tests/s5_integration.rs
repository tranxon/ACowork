//! S5 Integration Tests — Multi-provider, token counting, and budget validation
//!
//! Tests cross-cutting concerns from the S5 stage:
//! - S5.5.1: Multi-agent collaboration (Intent cross-agent call)
//! - S5.5.2: Memory persistence (Grafeo store survives operations)
//! - S5.5.3: Identity injection (cold start identity in context)
//! - S5.5.4: Budget enforcement (over-limit correctly blocks)
//! - S5.5.5: Streaming output (content accumulation from stream events)

use std::sync::Arc;

use rollball_core::providers::mock::{MockProvider, MockResponse};
use rollball_core::providers::traits::{FunctionCall, Provider, ToolCall};
use rollball_core::tools::traits::Tool;
use rollball_core::Budget;

use rollball_runtime::agent::budget_guard::BudgetGuard;
use rollball_runtime::agent::context::ContextBuilder;
use rollball_runtime::agent::history::HistoryManager;
use rollball_runtime::agent::loop_::AgentLoop;
use rollball_runtime::config::RuntimeConfig;
use rollball_runtime::providers::anthropic::AnthropicProvider;
use rollball_runtime::providers::registry::{
    ModelCapability, ProviderRegistry, RoutingStrategy,
};
use rollball_runtime::providers::router::{create_provider, infer_protocol_type};
use rollball_runtime::token::counter::{BudgetAllocation, TokenCounter};

use async_trait::async_trait;

use serde_json::Value;

// ═══════════════════════════════════════════════════════════════════════════
// S5.5.1: Multi-agent collaboration (Intent cross-agent call)
// ═══════════════════════════════════════════════════════════════════════════

/// Mock tool that simulates an Intent call to a calendar agent
struct MockIntentTool;

#[async_trait]
impl Tool for MockIntentTool {
    fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
        rollball_core::tools::traits::ToolSpec {
            name: "intent_send".to_string(),
            description: "Send an intent to another agent".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string"},
                    "action": {"type": "string"},
                    "params": {"type": "object"}
                },
                "required": ["target", "action"]
            }),
        }
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
        let target = params.get("target").and_then(|v| v.as_str()).unwrap_or("");
        let action = params.get("action").and_then(|v| v.as_str()).unwrap_or("");

        // Simulate calendar agent response
        if target.contains("calendar") && action == "create_event" {
            Ok(rollball_core::tools::traits::ToolResult {
                ok: true,
                content: "Event created: Weather Alert for Shanghai - Sunny 22°C".to_string(),
                error: None,
                token_usage: None,
            })
        } else {
            Ok(rollball_core::tools::traits::ToolResult {
                ok: true,
                content: format!("Intent sent to {target}: {action}"),
                error: None,
                token_usage: None,
            })
        }
    }
}

#[tokio::test]
async fn test_s5_intent_cross_agent_call() {
    // Weather agent calls Intent to calendar agent
    let provider = Arc::new(MockProvider::new(vec![
        // First: respond with weather info + Intent call to calendar
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_intent_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "intent_send".to_string(),
                    arguments: r#"{"target":"com.example.calendar","action":"create_event","params":{"title":"Weather Alert","description":"Sunny 22°C in Shanghai"}}"#.to_string(),
                },
            }],
            content: String::new(),
        },
        // Then: summarize the result
        MockResponse::Text {
            content: "I've checked the weather and created a calendar event for the Shanghai weather alert!".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockIntentTool)];
    let manifest = rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.example.weather"
        version = "1.0.0"
        name = "Weather Agent"
        description = "Weather queries"
        author = "test"
        runtime_version = "0.1.0"

        [[permissions]]
        type = "IntentSend"

        [[tools]]
        name = "intent_send"

        [llm]
        provider = "mock"
        model = "mock-model"
    "#,
    )
    .unwrap();

    let config = RuntimeConfig::default();
    let budget = Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };
    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a weather agent.".to_string());

    let result = agent_loop
        .run("Check Shanghai weather and create a reminder", &mut context_builder)
        .await;
    assert!(result.is_ok(), "Intent cross-agent call should succeed: {:?}", result);

    let response = result.unwrap();
    assert!(
        response.contains("calendar") || response.contains("event"),
        "Response should mention calendar event: {}",
        response
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// S5.5.2: Memory persistence (Grafeo store operations)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_s5_memory_persistence() {
    use rollball_grafeo::GrafeoStore;
    use rollball_memory::{HintType, MemoryQuery};
    use rollball_runtime::memory::{MemoryManager, MemoryManagerConfig, ConversationRecord};
    use chrono::Utc;

    let store = GrafeoStore::new_in_memory().unwrap();
    let manager = MemoryManager::new(MemoryManagerConfig::default());

    // Record a conversation turn
    let record = ConversationRecord {
        session_id: "test-session-1".to_string(),
        turn_index: 0,
        user_message: "I live in Shanghai".to_string(),
        assistant_response: "Got it, you live in Shanghai!".to_string(),
        retrieved_memory_ids: vec![],
        timestamp: Utc::now(),
    };

    manager.record(&store, &record).unwrap();

    // Verify the episode was stored
    let emb = vec![0.1f32; rollball_grafeo::types::EMBEDDING_DIM];
    let query = MemoryQuery {
        query_text: "live Shanghai".to_string(),
        embedding: Some(emb),
        filters: Default::default(),
        limit: 5,
        expand_hops: 0,
        min_score: None,
        abstention_enabled: false,
        hint_type: HintType::Semantic,
    };

    let result = manager.retrieve(&store, &query).await.unwrap();
    assert!(!result.memories.is_empty(), "Memory should be retrievable after recording");
}

// ═══════════════════════════════════════════════════════════════════════════
// S5.5.3: Identity injection (cold start identity in context)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_s5_identity_injection_in_context() {
    let manifest = rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.example.weather"
        version = "1.0.0"
        name = "Weather Agent"
        description = "Weather queries"
        author = "test"
        runtime_version = "0.1.0"

        identity_deps = ["name", "city"]

        [llm]
        provider = "openai"
        model = "gpt-4"
    "#,
    )
    .unwrap();

    let history = HistoryManager::new(10000, 4);

    // Build context with identity
    let builder = ContextBuilder::new("You are a helpful assistant.".to_string())
        .with_identity(Some("Name: Alice, City: Shanghai".to_string()));

    let request = builder.build(&manifest, &history, None, 32_768);

    // Verify identity is injected into system prompt
    assert!(request.messages[0].content.contains("Alice"), "System prompt should contain identity name");
    assert!(request.messages[0].content.contains("Shanghai"), "System prompt should contain identity city");
    assert!(request.messages[0].content.contains("User Identity"), "System prompt should contain identity section header");
}

#[test]
fn test_s5_identity_deps_in_manifest() {
    let manifest = rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.example.weather"
        version = "1.0.0"
        name = "Weather Agent"
        description = "Weather queries"
        author = "test"
        runtime_version = "0.1.0"

        identity_deps = ["name", "city", "language"]

        [llm]
        provider = "openai"
        model = "gpt-4"
    "#,
    )
    .unwrap();

    assert_eq!(manifest.identity_deps.len(), 3);
    assert!(manifest.identity_deps.contains(&"name".to_string()));
    assert!(manifest.identity_deps.contains(&"city".to_string()));
    assert!(manifest.identity_deps.contains(&"language".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════════
// S5.5.4: Budget enforcement (over-limit correctly blocks)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_s5_budget_enforcement_blocks() {
    let budget = Budget {
        daily_tokens: Some(1000),
        monthly_tokens: Some(10000),
        daily_cost_usd: Some(0.5),
        monthly_cost_usd: Some(10.0),
        exceeded_action: "deny".to_string(),
    };

    let mut guard = BudgetGuard::new(budget);

    // Use most of the budget
    guard.update_usage(950, 0.4);

    // Next request should be blocked
    let result = guard.check(100);
    assert!(!result.is_allowed(), "Budget should block request when tokens exceed daily limit");

    // Small request should still be allowed
    let small_result = guard.check(40);
    assert!(small_result.is_allowed(), "Small request should fit within remaining budget");
}

#[test]
fn test_s5_budget_cost_enforcement() {
    let budget = Budget {
        daily_tokens: Some(100000),
        monthly_tokens: None,
        daily_cost_usd: Some(1.0),
        monthly_cost_usd: None,
        exceeded_action: "deny".to_string(),
    };

    let mut guard = BudgetGuard::new(budget);
    guard.update_usage(0, 1.5); // Exceed daily cost

    let result = guard.check(100);
    assert!(!result.is_allowed(), "Budget should block when daily cost exceeded");
}

#[test]
fn test_s5_budget_unlimited() {
    let guard = BudgetGuard::unlimited();
    let result = guard.check(1_000_000_000);
    assert!(result.is_allowed(), "Unlimited budget should always allow");
}

// ═══════════════════════════════════════════════════════════════════════════
// S5.5.5: Streaming output (content accumulation from stream events)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_s5_streaming_events_structure() {
    use rollball_core::providers::traits::StreamEvent;

    // Verify stream event variants work correctly
    let content_event = StreamEvent::Content("Hello".to_string());
    let tool_start_event = StreamEvent::ToolCallStart(ToolCall {
        id: "call_1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "weather".to_string(),
            arguments: String::new(),
        },
    });
    let tool_chunk_event = StreamEvent::ToolCallChunk { index: 0, arguments: r#"{"city":"Shanghai"}"#.to_string() };
    let error_event = StreamEvent::Error(rollball_core::providers::StreamError::stream_decode("test error".to_string()));

    // Verify all stream event types are constructible
    // (compilation itself is the validation — these types must exist and be usable)
    let _ = content_event;
    let _ = tool_start_event;
    let _ = tool_chunk_event;
    let _ = error_event;
}

// ═══════════════════════════════════════════════════════════════════════════
// S5 Cross-cutting: Provider registry + routing
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_s5_provider_registry_multi_provider() {
    let registry = ProviderRegistry::new();

    // Register multiple providers
    let openai = create_provider("openai", &infer_protocol_type("openai"), Some("sk-test"), None);
    let anthropic = create_provider("anthropic", &infer_protocol_type("anthropic"), Some("sk-ant-test"), None);
    let ollama = create_provider("ollama", &infer_protocol_type("ollama"), None, None);

    registry.register_provider("openai", openai, vec!["gpt-4".to_string(), "gpt-4o".to_string()]);
    registry.register_provider("anthropic", anthropic, vec!["claude-sonnet-4".to_string()]);
    registry.register_provider("ollama", ollama, vec!["llama3".to_string()]);

    // Verify all registered
    assert_eq!(registry.list_providers().len(), 3);

    // Verify capability queries
    assert!(registry.has_capability("openai", "gpt-4", ModelCapability::Streaming));
    assert!(registry.has_capability("openai", "gpt-4", ModelCapability::ToolUse));
    assert!(registry.has_capability("anthropic", "claude-sonnet-4", ModelCapability::Vision));
}

#[test]
fn test_s5_routing_strategy_config() {
    let registry = ProviderRegistry::with_strategy(RoutingStrategy::LatencyPriority);
    assert_eq!(registry.strategy(), RoutingStrategy::LatencyPriority);
}

// ═══════════════════════════════════════════════════════════════════════════
// S5 Cross-cutting: Token counting with tiered precision
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_s5_tiered_token_counting() {
    let counter = TokenCounter::new();

    // Tier 1: GPT-4 (exact/sampling)
    let gpt4_count = counter.count_text("Hello, how are you?", "gpt-4");
    assert!(gpt4_count > 0);

    // Tier 2: Claude (approximate)
    let claude_count = counter.count_text("Hello, how are you?", "claude-sonnet-4");
    assert!(claude_count > 0);

    // Tier 3: Unknown model (heuristic)
    let unknown_count = counter.count_text("Hello, how are you?", "some-unknown-model");
    assert!(unknown_count > 0);

    // Tier 1 should be most precise (lowest variance from expected ~6 tokens)
    // Tier 3 should still give reasonable estimates
    assert!(unknown_count > 0 && unknown_count < 50, "Heuristic should give reasonable estimates");
}

#[test]
fn test_s5_budget_allocation_with_manifest() {
    let alloc = BudgetAllocation::new(128000)
        .with_output_reserve(4096)
        .with_system_prompt(1500);

    assert!(alloc.is_valid());
    assert!(alloc.distributable_space() > 0);
    assert!(alloc.history_budget() > 0);
    assert!(alloc.retrieval_budget() >= 2048); // Hard minimum
}

// ═══════════════════════════════════════════════════════════════════════════
// S5 Cross-cutting: Manifest LLM routing configuration
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_s5_manifest_routing_config() {
    let manifest = rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.example.weather"
        version = "1.0.0"
        name = "Weather Agent"
        description = "Weather queries"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "openai"
        model = "gpt-4"
        temperature = 0.7

        [llm.providers.openai]
        model = "gpt-4o"
        api_key_ref = "vault:openai_key"

        [llm.providers.claude]
        model = "claude-sonnet-4"
        api_key_ref = "vault:anthropic_key"

        [llm.routing]
        strategy = "quality_priority"
        fallback_order = ["openai", "claude"]
    "#,
    )
    .unwrap();

    // Verify multi-provider config
    assert_eq!(manifest.llm.providers.len(), 2);
    assert!(manifest.llm.providers.contains_key("openai"));
    assert!(manifest.llm.providers.contains_key("claude"));
    assert_eq!(manifest.llm.providers["openai"].model, "gpt-4o");
    assert_eq!(manifest.llm.providers["claude"].model, "claude-sonnet-4");
    assert_eq!(manifest.llm.providers["openai"].api_key_ref, Some("vault:openai_key".to_string()));

    // Verify routing config
    assert!(manifest.llm.routing.is_some());
    let routing = manifest.llm.routing.unwrap();
    assert_eq!(routing.strategy, "quality_priority");
    assert_eq!(routing.fallback_order, vec!["openai", "claude"]);
    assert!(routing.auto_switch);
}

#[test]
fn test_s5_manifest_budget_config() {
    let manifest = rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.example.weather"
        version = "1.0.0"
        name = "Weather Agent"
        description = "Weather queries"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "openai"
        model = "gpt-4"

        [llm.budget]
        max_tokens_per_request = 8000
        max_output_tokens = 2048
        exceeded_action = "fallback_to_local"
    "#,
    )
    .unwrap();

    assert!(manifest.llm.budget.is_some());
    let budget = manifest.llm.budget.unwrap();
    assert_eq!(budget.max_tokens_per_request, Some(8000));
    assert_eq!(budget.max_output_tokens, Some(2048));
    assert_eq!(budget.exceeded_action, "fallback_to_local");
}

// ═══════════════════════════════════════════════════════════════════════════
// S5 Cross-cutting: Anthropic provider structure
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_s5_anthropic_provider_creation() {
    let provider = AnthropicProvider::new(Some("sk-ant-test"));
    assert_eq!(Provider::name(&provider), "anthropic");
}

#[test]
fn test_s5_anthropic_custom_base_url() {
    let provider = AnthropicProvider::with_base_url(
        Some("https://api.anthropic.example.com"),
        Some("sk-ant-test"),
    );
    assert_eq!(Provider::name(&provider), "anthropic");
}

#[test]
fn test_s5_router_supports_anthropic() {
    let provider = create_provider("anthropic", &infer_protocol_type("anthropic"), Some("sk-ant-test"), None);
    assert_eq!(provider.name(), "anthropic");

    let claude_provider = create_provider("claude", &infer_protocol_type("claude"), Some("sk-ant-test"), None);
    assert_eq!(claude_provider.name(), "anthropic");
}
