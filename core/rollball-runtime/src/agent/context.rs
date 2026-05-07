//! Context building (system prompt + history + memory + identity + skills)
//!
//! Builds the complete context for LLM requests following the priority order
//! defined in docs/03-agent-runtime.md §3.1.

use rollball_core::manifest::AgentManifest;
use rollball_core::protocol::ModelCapabilitiesInfo;
use rollball_core::providers::traits::{ChatMessage, ChatRequest, MessageRole};

use crate::agent::history::HistoryManager;

/// Context builder for LLM requests
pub struct ContextBuilder {
    /// System prompt from package
    system_prompt: String,
    /// Identity context (from Gateway injection)
    identity_context: Option<String>,
    /// Workspace context (from Gateway WorkspaceContextUpdate push)
    workspace_context: Option<String>,
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
            workspace_context: None,
            tool_definitions: None,
            override_model: None,
        }
    }

    /// Set identity context (from Gateway)
    pub fn with_identity(mut self, identity: Option<String>) -> Self {
        self.identity_context = identity;
        self
    }

    /// Set workspace context (from Gateway WorkspaceContextUpdate)
    pub fn with_workspace_context(mut self, workspace: Option<String>) -> Self {
        self.workspace_context = workspace;
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

    /// Get the override model name, if set
    pub fn override_model(&self) -> Option<&str> {
        self.override_model.as_deref()
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

    /// Set gateway model capabilities (from Gateway LLMConfigDelivery)
    /// DEPRECATED: Use the `gateway_capabilities` parameter in `build()` instead.
    /// This setter is kept for backward compat but is a no-op; capabilities
    /// are now passed at build time to avoid dual-holder sync issues.
    pub fn set_gateway_model_capabilities(&mut self, _caps: ModelCapabilitiesInfo) {
        // No-op: capabilities are passed via build() parameter instead
        tracing::debug!(
            "set_gateway_model_capabilities called on ContextBuilder (no-op, use build() parameter)"
        );
    }

    /// Update workspace context in-place (from Gateway WorkspaceContextUpdate push)
    pub fn set_workspace_context(&mut self, context_text: String) {
        tracing::info!(
            context_len = context_text.len(),
            "ContextBuilder workspace context updated via WorkspaceContextUpdate"
        );
        self.workspace_context = Some(context_text);
    }

    /// Build the complete ChatRequest for the LLM
    pub fn build(
        &self,
        manifest: &AgentManifest,
        history: &HistoryManager,
        gateway_capabilities: Option<&ModelCapabilitiesInfo>,
        max_output_tokens_limit: u64,
    ) -> ChatRequest {
        let mut messages = Vec::new();

        // 1. System prompt (always first, highest priority)
        let mut system_content = self.system_prompt.clone();

        // 2. Identity context (if available)
        if let Some(ref identity) = self.identity_context {
            system_content.push_str(&format!("\n\n## User Identity\n{identity}"));
        }

        // 2.2 Workspace context (if available, from Gateway push)
        if let Some(ref workspace) = self.workspace_context {
            system_content.push_str(&format!("\n\n{workspace}"));
        }

        // 2.5 Autobiographical context (Phase 1: skip, Phase 2: from Grafeo)

        // 3. Environment platform info (runtime detection)
        let shell_info = crate::platform::detected_shell();
        let available_shells = crate::platform::detected_shells();
        let shell_tools_desc: Vec<String> = available_shells
            .iter()
            .map(|s| {
                let primary = if s.is_primary { " (primary)" } else { " (fallback)" };
                format!("{}{}", s.tool_name, primary)
            })
            .collect();
        system_content.push_str(&format!(
            "\n\n## Environment\n- Operating System: {}\n- Architecture: {}\n- Shell: {}\n- Available Shell Tools: {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            shell_info.display_name,
            shell_tools_desc.join(", ")
        ));

        // 3.5 Tool definitions are passed separately in ChatRequest

        messages.push(ChatMessage::system(system_content));

        // 7. Conversation history
        // Filter out System messages from history — only the first system message
        // (created above) should exist. Some LLM providers (e.g. MiniMax) reject
        // system messages at non-first positions.
        messages.extend(
            history
                .messages()
                .iter()
                .filter(|m| !matches!(m.role, MessageRole::System))
                .cloned(),
        );

        // 7.5 Sanitize messages before sending to LLM
        // This fixes corrupted tool_call data that would cause 400 errors
        HistoryManager::sanitize_messages(&mut messages);

        // Determine the model to use
        let model = self.override_model.clone().unwrap_or_else(|| manifest.llm.suggested_model.clone());

        // Auto-set max_tokens based on model capabilities with the following priority:
        // 1. manifest.llm.max_tokens (user explicit config, backward compatible)
        // 2. Gateway model_capabilities.max_output_tokens
        // 3. Warn + conservative default 4096
        let max_tokens = if let Some(explicit) = manifest.llm.max_tokens {
            tracing::info!(
                max_tokens = explicit,
                source = "manifest",
                "Using explicitly configured max_tokens"
            );
            Some(explicit)
        } else if let Some(caps) = gateway_capabilities {
            // Cap max_output_tokens: it should never exceed context_window.
            // models.dev data or user input may provide inflated values that
            // the actual API rejects (e.g. alibaba-cn proxy limits kimi-k2.6
            // max_tokens to 98304, but models.dev reports 384000).
            let raw = caps.max_output_tokens;
            let context_window = caps.context_window;
            let recommended = if raw > context_window {
                tracing::warn!(
                    model = %model,
                    raw_max_output_tokens = raw,
                    context_window = context_window,
                    "max_output_tokens exceeds context_window, capping"
                );
                context_window
            } else {
                raw
            };
            // Hard cap: many provider APIs reject max_tokens above a certain limit.
            // This follows opencode's approach: Math.min(limit.output, 32000).
            // models.dev's limit.output can be inflated (e.g. 384000) but
            // actual API max_tokens parameter is usually capped much lower.
            // The limit is now configurable via Gateway config (max_output_tokens_limit).
            // Set to 0 to disable the limit.
            let hard_cap = if max_output_tokens_limit == 0 {
                u64::MAX // No limit
            } else {
                max_output_tokens_limit
            };
            let recommended = if recommended > hard_cap {
                tracing::warn!(
                    model = %model,
                    requested = recommended,
                    cap = hard_cap,
                    "max_output_tokens exceeds hard cap, capping"
                );
                hard_cap
            } else {
                recommended
            };
            let recommended = recommended.min(u32::MAX as u64) as u32;
            tracing::info!(
                model = %model,
                recommended_max_tokens = recommended,
                source = "gateway",
                "Auto-setting max_tokens from Gateway model capabilities"
            );
            Some(recommended)
        } else {
            tracing::warn!(
                model = %model,
                "No model capabilities received from Gateway, using conservative default max_tokens=4096. Configure model capabilities in Desktop App settings."
            );
            Some(4096)
        };

        // Safety check: ensure max_tokens does not exceed context window capacity
        let max_tokens = max_tokens.map(|mt| {
            if let Some(caps) = gateway_capabilities {
                let context_window = caps.context_window;
                // Count both message content and tool_call arguments for token estimation
                let total_chars: usize = messages.iter().map(|m| {
                    let content_len = m.content.len();
                    let tool_calls_len = m.tool_calls.as_ref().map(|tcs| {
                        tcs.iter().map(|tc| {
                            tc.function.name.len() + tc.function.arguments.len()
                        }).sum::<usize>()
                    }).unwrap_or(0);
                    content_len + tool_calls_len
                }).sum();
                // Add 10% overhead for role labels, formatting, and special tokens
                let approx_msg_tokens = ((total_chars as f64 / 4.0) * 1.1).ceil() as u64;
                if (approx_msg_tokens + mt as u64) > context_window {
                    let safe_max = (context_window.saturating_sub(approx_msg_tokens)).max(256) as u32;
                    tracing::warn!(
                        model = %model,
                        requested_max_tokens = mt,
                        safe_max_tokens = safe_max,
                        approx_msg_tokens = approx_msg_tokens,
                        context_window = context_window,
                        "max_tokens would exceed context window, reducing to safe value"
                    );
                    safe_max
                } else {
                    mt
                }
            } else {
                // No gateway capabilities available — Runtime does not speculate.
                // Trust the max_tokens value already determined above.
                mt
            }
        });

        tracing::info!(
            model = %model,
            max_tokens = ?max_tokens,
            "Final max_tokens for ChatRequest"
        );

        ChatRequest {
            model,
            messages,
            temperature: manifest.llm.temperature,
            max_tokens,
            tools: self.tool_definitions.clone(),
        }
    }
}

/// Build tool definitions from manifest tool declarations.
///
/// Handles shell tool aliasing: if the manifest declares "shell", "bash",
/// or "powershell", ALL platform-available shell tool specs are included
/// (bash + powershell on Windows, shell on Linux/macOS) so the LLM always
/// sees the full set regardless of which name was declared.
pub fn build_tool_definitions(
    manifest: &AgentManifest,
    tool_specs: &[(String, serde_json::Value)], // (name, schema) pairs
) -> Vec<serde_json::Value> {
    /// Shell tool names that are interchangeable in manifest declarations.
    const SHELL_NAMES: &[&str] = &["shell", "bash", "powershell"];

    // No tool declarations → all tools available (consistent with Registry.activate)
    if manifest.tools.is_empty() {
        return tool_specs.iter().map(|(_, schema)| schema.clone()).collect();
    }

    let has_shell_decl = manifest.tools.iter().any(|t| SHELL_NAMES.contains(&t.name.as_str()));

    let mut defs: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();

    // First pass: add exact matches for declared tools (with shell aliasing)
    for decl in &manifest.tools {
        let name = decl.name.as_str();
        if seen.contains(name) {
            continue;
        }
        // Direct match
        if let Some((_, schema)) = tool_specs.iter().find(|(n, _)| n == name) {
            seen.insert(name);
            defs.push(schema.clone());
        } else if SHELL_NAMES.contains(&name) {
            // Shell alias: any shell name in manifest → all available shell specs
            for (n, schema) in tool_specs {
                if SHELL_NAMES.contains(&n.as_str()) && !seen.contains(n.as_str()) {
                    seen.insert(n.as_str());
                    defs.push(schema.clone());
                }
            }
        }
    }

    // Second pass: if manifest has shell declaration, also include any shell
    // specs not captured above (e.g. manifest says "bash" but "powershell"
    // is also available)
    if has_shell_decl {
        for (name, schema) in tool_specs {
            if SHELL_NAMES.contains(&name.as_str()) && !seen.contains(name.as_str()) {
                seen.insert(name.as_str());
                defs.push(schema.clone());
            }
        }
    }

    defs
}

#[allow(clippy::items_after_test_module)]
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
        history.append(ChatMessage::user("Hello"));

        let builder = ContextBuilder::new("You are a helpful assistant.".to_string());
        let request = builder.build(&manifest, &history, None, 32_768);

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

        let request = builder.build(&manifest, &history, None, 32_768);
        assert!(request.messages[0].content.contains("Alice"));
    }
}

/// Compute context usage info from model capabilities and API usage response.
/// Follows opencode's approach: usable context = context_window - maxOutputTokens,
/// or max_input_tokens - reserved if available.
pub fn compute_context_usage(
    caps: &ModelCapabilitiesInfo,
    usage: &rollball_core::providers::traits::UsageInfo,
    max_output_tokens_limit: u64,
) -> rollball_core::protocol::ContextUsageInfo {
    // Cap max_output_tokens at the configured limit (same cap used in build())
    // Set max_output_tokens_limit to 0 to disable the limit.
    let effective_limit = if max_output_tokens_limit == 0 { u64::MAX } else { max_output_tokens_limit };
    let max_output = caps.max_output_tokens.min(effective_limit);
    let usable = caps
        .max_input_tokens
        .map(|input| input.saturating_sub(max_output))
        .unwrap_or_else(|| caps.context_window.saturating_sub(max_output));
    let total = usage.prompt_tokens + usage.completion_tokens;
    let percent = if usable > 0 {
        ((total as f64 / usable as f64) * 100.0).min(100.0) as u8
    } else {
        0
    };
    rollball_core::protocol::ContextUsageInfo {
        context_window: caps.context_window,
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        total_tokens: total,
        max_input_tokens: caps.max_input_tokens,
        usable_context: usable,
        usage_percent: percent,
    }
}
