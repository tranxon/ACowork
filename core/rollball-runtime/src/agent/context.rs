//! Context building (system prompt + history + memory + identity + skills)
//!
//! Builds the complete context for LLM requests following the priority order
//! defined in docs/03-agent-runtime.md §3.1.

use rollball_core::manifest::AgentManifest;
use rollball_core::providers::traits::{ChatMessage, ChatRequest, MessageRole};

use crate::agent::history::HistoryManager;

/// Context builder for LLM requests
pub struct ContextBuilder {
    /// System prompt from package
    system_prompt: String,
    /// Identity context (from Gateway injection)
    identity_context: Option<String>,
    /// Tool definitions as JSON
    tool_definitions: Option<Vec<serde_json::Value>>,
    /// Model override from Gateway LLMConfigDelivery (takes precedence over manifest suggested_model)
    override_model: Option<String>,
}

impl ContextBuilder {
    /// Create a new context builder
    pub fn new(system_prompt: String) -> Self {
        Self {
            system_prompt,
            identity_context: None,
            tool_definitions: None,
            override_model: None,
        }
    }

    /// Set identity context (from Gateway)
    pub fn with_identity(mut self, identity: Option<String>) -> Self {
        self.identity_context = identity;
        self
    }

    /// Set tool definitions
    pub fn with_tools(mut self, tools: Vec<serde_json::Value>) -> Self {
        self.tool_definitions = Some(tools);
        self
    }

    /// Set model override (from Gateway LLMConfigDelivery)
    pub fn with_override_model(mut self, model: String) -> Self {
        self.override_model = Some(model);
        self
    }

    /// Update model override in-place (from model_switch message at runtime)
    pub fn set_override_model(&mut self, model: String) {
        let old = self.override_model.clone();
        tracing::info!(
            old_model = ?old,
            new_model = %model,
            "ContextBuilder model override updated via model_switch"
        );
        self.override_model = Some(model);
    }

    /// Build the complete ChatRequest for the LLM
    pub fn build(
        &self,
        manifest: &AgentManifest,
        history: &HistoryManager,
    ) -> ChatRequest {
        let mut messages = Vec::new();

        // 1. System prompt (always first, highest priority)
        let mut system_content = self.system_prompt.clone();

        // 2. Identity context (if available)
        if let Some(ref identity) = self.identity_context {
            system_content.push_str(&format!("\n\n## User Identity\n{identity}"));
        }

        // 2.5 Autobiographical context (Phase 1: skip, Phase 2: from Grafeo)

        // 3. Tool definitions are passed separately in ChatRequest

        messages.push(ChatMessage {
            role: MessageRole::System,
            content: system_content,
            name: None,
            tool_calls: None,
        });

        // 7. Conversation history
        messages.extend(history.messages().iter().cloned());

        ChatRequest {
            model: self.override_model.clone().unwrap_or_else(|| manifest.llm.suggested_model.clone()),
            messages,
            temperature: manifest.llm.temperature,
            max_tokens: manifest.llm.max_tokens,
            tools: self.tool_definitions.clone(),
        }
    }
}

/// Build tool definitions from manifest tool declarations
pub fn build_tool_definitions(
    manifest: &AgentManifest,
    tool_specs: &[(String, serde_json::Value)], // (name, schema) pairs
) -> Vec<serde_json::Value> {
    manifest
        .tools
        .iter()
        .filter_map(|decl| {
            tool_specs
                .iter()
                .find(|(name, _)| name == &decl.name)
                .map(|(_, schema)| schema.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest() -> AgentManifest {
        AgentManifest::from_toml(r#"
            agent_id = "com.test.ctx"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
            temperature = 0.7
        "#).unwrap()
    }

    #[test]
    fn test_context_builder_basic() {
        let manifest = test_manifest();
        let mut history = HistoryManager::new(10000, 4);
        history.append(ChatMessage {
            role: MessageRole::User,
            content: "Hello".to_string(),
            name: None,
            tool_calls: None,
        });

        let builder = ContextBuilder::new("You are a helpful assistant.".to_string());
        let request = builder.build(&manifest, &history);

        assert_eq!(request.model, "gpt-4");
        assert_eq!(request.messages.len(), 2); // system + user
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert_eq!(request.messages[1].role, MessageRole::User);
    }

    #[test]
    fn test_context_builder_with_identity() {
        let manifest = test_manifest();
        let history = HistoryManager::new(10000, 4);

        let builder = ContextBuilder::new("You are a helper.".to_string())
            .with_identity(Some("Name: Alice, City: Shanghai".to_string()));

        let request = builder.build(&manifest, &history);
        assert!(request.messages[0].content.contains("Alice"));
    }
}
