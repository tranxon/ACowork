//! Context building (system prompt + history + memory + identity + skills)
//!
//! Builds the complete context for LLM requests following the priority order
//! defined in docs/03-agent-runtime.md §3.1.

use acowork_core::manifest::AgentManifest;
use acowork_core::protocol::ModelCapabilitiesInfo;
use acowork_core::providers::traits::{ChatMessage, ChatRequest, ContentPart, MessageRole, ReasoningEffort};

use crate::agent::history::HistoryManager;
use crate::token::counter::TokenCounter;

/// Context builder for LLM requests
pub struct ContextBuilder {
    /// System prompt from package
    system_prompt: String,
    /// Identity context (from Gateway injection)
    identity_context: Option<String>,
    /// Workspace context (self-formatted from agent_workspaces.json)
    workspace_context: Option<String>,
    /// Workspace prompt file content (CLAUDE.md / AGENTS.md) for injection
    workspace_prompt_file: Option<String>,
    /// Environment info override (for debug patching).
    /// When set, takes precedence over auto-detected platform info.
    /// Stored as the full formatted string (e.g. "## Environment\n- OS: ...\n- Shell: ...")
    environment_override: Option<String>,
    /// Tool definitions as JSON
    tool_definitions: Option<Vec<serde_json::Value>>,
    /// Model override (set by `model_switch` or session initialization;
    /// takes precedence over session default).
    override_model: Option<String>,
    /// Retrieved memory context (from Grafeo) for injection into system prompt.
    /// Set by AgentLoop before each build via `set_retrieved_memory()`.
    retrieved_memory: Option<String>,
    /// P3-4: Ambiguous conflict confirmation hint — when ≥ 3 pending
    /// ambiguous conflicts exist, this hint guides the Agent to naturally
    /// ask the user for disambiguation. Injected after retrieved memory.
    ambiguous_confirmation_hint: Option<String>,
    /// Skill instructions override (for debug patching and runtime config).
    /// Injected into system prompt after identity and before memory sections.
    skill_instructions: Option<String>,
    /// Todo list context for injection into the system prompt.
    /// Set by AgentLoop before each build() from SessionState.todos.
    todo_context: Option<String>,
    /// Reasoning effort level for the LLM request.
    /// Resolved from ModelCapabilitiesInfo.default_reasoning_effort in
    /// `build_chat_request()` each iteration (supports mid-session model switch).
    /// Can also be overridden per-session via `set_reasoning_effort()`.
    reasoning_effort: Option<ReasoningEffort>,
    /// Anthropic thinking mode: "extended" or "adaptive".
    /// Resolved from ModelCapabilitiesInfo.thinking_mode in `build_chat_request()`.
    thinking_mode: Option<String>,
    /// Reusable token counter for system prompt estimation.
    counter: TokenCounter,
}

impl ContextBuilder {
    /// Create a new context builder
    pub fn new(system_prompt: String) -> Self {
        Self {
            system_prompt,
            identity_context: None,
            workspace_context: None,
            workspace_prompt_file: None,
            environment_override: None,
            tool_definitions: None,
            override_model: None,
            retrieved_memory: None,
            ambiguous_confirmation_hint: None,
            skill_instructions: None,
            todo_context: None,
            reasoning_effort: None,
            thinking_mode: None,
            counter: TokenCounter::new(),
        }
    }

    /// Set identity context (from Gateway)
    pub fn with_identity(mut self, identity: Option<String>) -> Self {
        self.identity_context = identity;
        self
    }

    /// Set workspace context (from Runtime self-formatting)
    pub fn with_workspace_context(mut self, workspace: Option<String>) -> Self {
        self.workspace_context = workspace;
        self
    }

    /// Set workspace prompt file content (CLAUDE.md / AGENTS.md)
    pub fn with_workspace_prompt_file(mut self, content: Option<String>) -> Self {
        self.workspace_prompt_file = content;
        self
    }

    /// Set tool definitions
    pub fn with_tools(mut self, tools: Vec<serde_json::Value>) -> Self {
        self.tool_definitions = Some(tools);
        self
    }

    /// Set the reasoning effort level for LLM requests.
    ///
    /// Called by `build_chat_request()` each iteration after resolving
    /// from the current model's capabilities (supports mid-session
    /// model switch). Can also be set directly for per-session overrides.
    pub fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.reasoning_effort = effort;
    }

    /// Set the Anthropic thinking mode ("extended" or "adaptive").
    pub fn set_thinking_mode(&mut self, mode: Option<String>) {
        self.thinking_mode = mode;
    }

    /// Get the current reasoning effort level, if set.
    pub fn reasoning_effort(&self) -> Option<&ReasoningEffort> {
        self.reasoning_effort.as_ref()
    }

    /// Set model override (from `model_switch` or session initialization)
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

    /// Update workspace context in-place (from WorkspaceConfigUpdate push or self-formatting)
    pub fn set_workspace_context(&mut self, context_text: String) {
        tracing::info!(
            context_len = context_text.len(),
            "ContextBuilder workspace context updated"
        );
        self.workspace_context = Some(context_text);
    }

    /// Update workspace prompt file content in-place (from workspace selection)
    pub fn set_workspace_prompt_file(&mut self, content: Option<String>) {
        if let Some(ref text) = content {
            tracing::info!(
                content_len = text.len(),
                "ContextBuilder workspace prompt file updated"
            );
        } else {
            tracing::info!("ContextBuilder workspace prompt file cleared");
        }
        self.workspace_prompt_file = content;
    }

    /// Set environment override (for debug patching).
    /// Takes precedence over auto-detected platform info in build().
    pub fn set_environment_override(&mut self, env_text: String) {
        tracing::info!(
            len = env_text.len(),
            "ContextBuilder environment override set via debug patch"
        );
        if env_text.is_empty() {
            self.environment_override = None;
        } else {
            self.environment_override = Some(env_text);
        }
    }

    /// Clear the environment override, reverting to auto-detection.
    pub fn clear_environment_override(&mut self) {
        self.environment_override = None;
    }

    /// Set retrieved memory context for injection into the system prompt.
    ///
    /// Called by AgentLoop before each `build()` invocation with memories
    /// retrieved from Grafeo via MemoryManager.
    pub fn set_retrieved_memory(&mut self, memory_text: String) {
        if !memory_text.is_empty() {
            tracing::debug!(
                memory_len = memory_text.len(),
                "ContextBuilder retrieved memory context set"
            );
            self.retrieved_memory = Some(memory_text);
        }
    }

    /// Set the base system prompt (for debug patching).
    pub fn set_system_prompt(&mut self, prompt: String) {
        tracing::info!(
            old_len = self.system_prompt.len(),
            new_len = prompt.len(),
            "ContextBuilder system prompt updated via debug patch"
        );
        self.system_prompt = prompt;
    }

    /// Set tool definitions (for debug patching).
    pub fn set_tool_definitions(&mut self, tools: Vec<serde_json::Value>) {
        tracing::info!(
            tool_count = tools.len(),
            "ContextBuilder tool definitions updated via debug patch"
        );
        self.tool_definitions = Some(tools);
    }

    /// Set identity context in-place (for debug patching).
    pub fn set_identity_context(&mut self, identity: String) {
        if identity.is_empty() {
            tracing::info!(
                old_len = self.identity_context.as_ref().map(|s| s.len()).unwrap_or(0),
                "ContextBuilder identity context cleared"
            );
            self.identity_context = None;
        } else {
            tracing::info!(
                old_len = self.identity_context.as_ref().map(|s| s.len()).unwrap_or(0),
                new_len = identity.len(),
                "ContextBuilder identity context updated via debug patch"
            );
            self.identity_context = Some(identity);
        }
    }

    /// Set skill instructions (for debug patching and runtime skill injection).
    /// Empty instructions are treated as a clear signal, consistent with
    /// `set_environment_override()` and `set_retrieved_memory_patch()`.
    pub fn set_skill_instructions(&mut self, instructions: String) {
        if instructions.is_empty() {
            self.clear_skill_instructions();
        } else {
            tracing::info!(
                len = instructions.len(),
                "ContextBuilder skill instructions updated"
            );
            self.skill_instructions = Some(instructions);
        }
    }

    /// Set todo list context for injection into the system prompt.
    /// Pass `None` to clear the todo section (when the list is empty).
    pub fn set_todo_context(&mut self, text: Option<String>) {
        self.todo_context = text;
    }

    /// Clear skill instructions, removing them from the system prompt.
    /// Called when a ChatMessage arrives without a skill command, preventing
    /// stale skill instructions from leaking across conversation turns.
    pub fn clear_skill_instructions(&mut self) {
        if self.skill_instructions.is_some() {
            tracing::debug!("ContextBuilder skill instructions cleared");
            self.skill_instructions = None;
        }
    }

    /// Set retrieved memory text in-place (for debug patching).
    /// Note: this differs from `set_retrieved_memory` in that it doesn't
    /// skip empty strings (allows clearing the memory section).
    pub fn set_retrieved_memory_patch(&mut self, memory_text: String) {
        if memory_text.is_empty() {
            tracing::debug!("ContextBuilder retrieved memory cleared via debug patch");
            self.retrieved_memory = None;
        } else {
            tracing::debug!(
                len = memory_text.len(),
                "ContextBuilder retrieved memory updated via debug patch"
            );
            self.retrieved_memory = Some(memory_text);
        }
    }

    /// Apply a debug PatchSet to the context builder.
    ///
    /// Only non-None fields in the patch are applied; existing fields
    /// that are not patched remain unchanged.
    pub fn apply_patches(&mut self, patches: &crate::debug::protocol::PatchSet) {
        if let Some(ref prompt) = patches.system_prompt {
            self.set_system_prompt(prompt.clone());
        }
        if let Some(ref workspace) = patches.workspace_context {
            self.set_workspace_context(workspace.clone());
        }
        if let Some(ref env) = patches.environment {
            self.set_environment_override(env.clone());
        }
        if let Some(ref tools) = patches.tool_definitions {
            self.set_tool_definitions(tools.clone());
        }
        if let Some(ref skills) = patches.skill_instructions {
            self.set_skill_instructions(skills.clone());
        }
        if let Some(ref memory) = patches.retrieved_memory {
            self.set_retrieved_memory_patch(memory.to_string());
        }
        if let Some(ref identity) = patches.identity_context {
            self.set_identity_context(identity.to_string());
        }
    }

    /// Clear retrieved memory context.
    ///
    /// Must be called at the start of each `run()` invocation to prevent
    /// stale memory from previous turns leaking into the next LLM call.
    /// See P0 fix: ContextBuilder reused across turns in SessionTask loop.
    pub fn clear_retrieved_memory(&mut self) {
        if self.retrieved_memory.is_some() {
            tracing::debug!("ContextBuilder retrieved memory context cleared (stale prevention)");
            self.retrieved_memory = None;
        }
        if self.ambiguous_confirmation_hint.is_some() {
            self.ambiguous_confirmation_hint = None;
        }
    }

    /// P3-4: Set ambiguous conflict confirmation hint for injection into
    /// the system prompt. When ≥ 3 pending ambiguous conflicts exist,
    /// this hint guides the Agent to naturally ask the user about them.
    pub fn set_ambiguous_confirmation_hint(&mut self, hint: String) {
        self.ambiguous_confirmation_hint = Some(hint);
    }

    // ── Section accessors for debug ContextSnapshot ──

    /// Get the base system prompt (before identity/memory/workspace injection).
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Get the identity context text, if set.
    pub fn identity_context(&self) -> Option<&str> {
        self.identity_context.as_deref()
    }

    /// Get the tool definitions as JSON values, if set.
    pub fn tool_definitions(&self) -> Option<&[serde_json::Value]> {
        self.tool_definitions.as_deref()
    }

    /// Get the retrieved memory text, if set.
    pub fn retrieved_memory(&self) -> Option<&str> {
        self.retrieved_memory.as_deref()
    }

    /// Get the workspace context text, if set.
    pub fn workspace_context(&self) -> Option<&str> {
        self.workspace_context.as_deref()
    }

    /// Get the workspace prompt file content (e.g. CLAUDE.md / AGENTS.md), if set.
    pub fn workspace_prompt_file(&self) -> Option<&str> {
        self.workspace_prompt_file.as_deref()
    }

    /// Get the environment override text, if set.
    pub fn environment_override(&self) -> Option<&str> {
        self.environment_override.as_deref()
    }

    /// Get the skill instructions text, if set.
    /// Returns the full skill instructions that will be injected into the
    /// system prompt under the "## Skill Instructions" section.
    pub fn skill_instructions(&self) -> Option<&str> {
        self.skill_instructions.as_deref()
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

        // 2.5 Retrieved memory context from Grafeo (long-term memory)
        if let Some(ref memory) = self.retrieved_memory {
            system_content.push_str(&format!("\n\n## Relevant Memories\n{memory}"));
        }

        // 2.5b P3-4: Ambiguous conflict confirmation hint
        // When ≥ 3 pending ambiguous conflicts exist, inject a hint that
        // guides the Agent to naturally ask the user for disambiguation.
        if let Some(ref hint) = self.ambiguous_confirmation_hint {
            system_content.push_str(&format!(
                "\n\n## Memory Conflicts Needing Confirmation\n{hint}"
            ));
        }

        // 2.6 Skill instructions (debug patching or runtime config)
        if let Some(ref skills) = self.skill_instructions {
            system_content.push_str(&format!("\n\n## Skill Instructions\n{skills}"));
        }

        // 2.7 Todo list (session-level task tracking)
        if let Some(ref todos) = self.todo_context {
            system_content.push_str(&format!("\n\n## Active Task List\nUse the `todo_write` tool to manage this list. Current tasks:\n{todos}"));
        }

        // 3. Environment platform info
        // Debug override takes precedence over auto-detected platform info,
        // allowing the debugger to modify environment context without changing
        // the actual runtime environment.
        if let Some(ref env_override) = self.environment_override {
            system_content.push_str(&format!("\n\n{env_override}"));
        } else {
            system_content.push_str(&format!("\n\n{}", detect_environment_text()));
        }

        // 3.2 Workspace prompt file content (CLAUDE.md / AGENTS.md)
        // Injected at the end for maximum visibility.
        if let Some(ref prompt_file) = self.workspace_prompt_file {
            system_content.push_str(&format!("\n\n## Workspace Prompt File\n{prompt_file}"));
        }

        // 3.5 Tool definitions are passed separately in ChatRequest

        messages.push(ChatMessage::system(system_content));

        // Estimate system prompt tokens for observability
        let system_msg = messages.last().unwrap();
        let system_tokens = self.counter.count_message(system_msg, "", None);
        tracing::debug!(system_tokens, "System prompt token estimation");

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

        // 7.6 Filter image content_parts for non-vision models
        // When the model doesn't support image input (modalities known and
        // lacks "image"), strip ImageUrl parts from multimodal content to
        // prevent API 400 errors. This handles the case where a user switches
        // from a vision model to a text-only model mid-session — the session
        // history still contains base64 image data that would otherwise be
        // sent to a model that cannot process it.
        if let Some(caps) = gateway_capabilities {
            let supports_image = caps
                .modalities
                .as_ref()
                .map(|m| m.input.iter().any(|s| s == "image"))
                .unwrap_or(true); // true=don't filter when modalities unknown
            if !supports_image {
                for msg in &mut messages {
                    if let Some(ref mut parts) = msg.content_parts {
                        // Keep only Text parts, strip ImageUrl parts
                        parts.retain(|p| matches!(p, ContentPart::Text { .. }));
                        // If no text parts remain, set content_parts to None
                        // to avoid sending an empty array to the API.
                        if parts.is_empty() {
                            msg.content_parts = None;
                        }
                    }
                }
            }
        }

        // Determine the model to use.
        // Model comes from override_model (set by model_switch or session init).
        // When absent, model is empty — the LLM call will fail with a clear error.
        let model = self.override_model.clone().unwrap_or_default();

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
            let raw = caps.max_output_tokens;

            if raw == 0 {
                // If max_output_tokens is 0, it means the value was not provided
                // (e.g. locally-discovered models without capability info).
                // Don't guess — omit max_tokens entirely and let the model
                // use its own default (typically the full context window).
                tracing::info!(
                    model = %model,
                    "max_output_tokens not configured, omitting max_tokens from request"
                );
                None
            } else {
                // Cap max_output_tokens: it should never exceed context_window.
                // models.dev data or user input may provide inflated values that
                // the actual API rejects (e.g. alibaba-cn proxy limits kimi-k2.6
                // max_tokens to 98304, but models.dev reports 384000).
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
            }
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
                // Build a combined text from message content + tool_call arguments
                // for model-aware token counting via the unified API.
                let combined: String = messages.iter().fold(String::new(), |mut acc, m| {
                    acc.push_str(&m.content);
                    if let Some(ref tcs) = m.tool_calls {
                        for tc in tcs {
                            acc.push_str(&tc.function.name);
                            acc.push_str(&tc.function.arguments);
                        }
                    }
                    acc
                });
                // Safety margin: +10% overhead for role labels, formatting, and special tokens.
                let approx_msg_tokens =
                    (crate::token::count_text(&combined, &model) as f64 * 1.1).ceil() as u64;
                if (approx_msg_tokens + mt as u64) > context_window {
                    let safe_max =
                        (context_window.saturating_sub(approx_msg_tokens)).max(256) as u32;
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
            reasoning_effort: self.reasoning_effort.clone(),
            thinking_mode: self.thinking_mode.clone(),
        }
    }
}

/// Detect and format the environment info text that gets injected into
/// the system prompt. Used by debug snapshot capture and ContextBuilder::build().
pub fn detect_environment_text() -> String {
    let shell_info = crate::platform::detected_shell();
    let available_shells = crate::platform::detected_shells();
    let shell_tools_desc: Vec<String> = available_shells
        .iter()
        .map(|s| {
            let primary = if s.is_primary {
                " (primary)"
            } else {
                " (fallback)"
            };
            format!("{}{}", s.tool_name, primary)
        })
        .collect();
    format!(
        "## Environment\n- Operating System: {}\n- Architecture: {}\n- Shell: {}\n- Available Shell Tools: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        shell_info.display_name,
        shell_tools_desc.join(", ")
    )
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest() -> AgentManifest {
        AgentManifest::from_toml(
            r#"
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
        "#,
        )
        .unwrap()
    }

    #[test]
    fn test_context_builder_basic() {
        let manifest = test_manifest();
        let mut history = HistoryManager::new(10000);
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
        let history = HistoryManager::new(10000);

        let builder = ContextBuilder::new("You are a helper.".to_string())
            .with_identity(Some("Name: Alice, City: Shanghai".to_string()));

        let request = builder.build(&manifest, &history, None, 32_768);
        assert!(request.messages[0].content.contains("Alice"));
    }

    /// Helper: build a simple ModelCapabilitiesInfo for testing.
    fn test_caps(context_window: u64, max_output_tokens: u64) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
        }
    }

    #[test]
    fn test_build_context_usage_from_persisted_matches_compute() {
        let caps = test_caps(128_000, 16_384);
        let max_output_limit = 32_768u64;

        // Via compute_context_usage with real UsageInfo
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: 45_000,
            completion_tokens: 1_200,
            total_tokens: 46_200,
            ..Default::default()
        };
        let fresh = compute_context_usage(&caps, &usage, max_output_limit);

        // Via build_context_usage_from_persisted with same numbers
        let persisted =
            build_context_usage_from_persisted(&caps, 45_000, 1_200, max_output_limit);

        assert_eq!(fresh.context_window, persisted.context_window);
        assert_eq!(fresh.input_tokens, persisted.input_tokens);
        assert_eq!(fresh.output_tokens, persisted.output_tokens);
        assert_eq!(fresh.total_tokens, persisted.total_tokens);
        assert_eq!(fresh.max_input_tokens, persisted.max_input_tokens);
        assert_eq!(fresh.usable_context, persisted.usable_context);
        assert_eq!(fresh.usage_percent, persisted.usage_percent);
    }

    #[test]
    fn test_build_context_usage_from_persisted_zero_tokens() {
        // New session with no token data yet → should produce 0 input/output
        let caps = test_caps(200_000, 8_192);
        let info = build_context_usage_from_persisted(&caps, 0, 0, 32_768);
        assert_eq!(info.input_tokens, 0);
        assert_eq!(info.output_tokens, 0);
        assert_eq!(info.total_tokens, 0);
        assert_eq!(info.usage_percent, 0);
    }
}

/// Compute the total character count of a ChatRequest for token ratio calibration.
///
/// Counts ALL text that the LLM will receive in the prompt:
/// - Message content + name + tool_call function name/arguments
/// - Tool definitions JSON (see [ADR about including tool_defs])
///
/// This total is used with API `prompt_tokens` to calibrate the chars/token ratio:
/// ```text
/// ratio = count_chat_request_chars(request) / prompt_tokens
/// ```
pub fn count_chat_request_chars(request: &ChatRequest) -> usize {
    // 1. Count message text
    let msg_chars: usize = request
        .messages
        .iter()
        .map(|m| {
            let mut chars = m.content.len();
            if let Some(ref name) = m.name {
                chars += name.len();
            }
            if let Some(ref tool_calls) = m.tool_calls {
                for tc in tool_calls {
                    chars += tc.function.name.len();
                    chars += tc.function.arguments.len();
                }
            }
            chars
        })
        .sum();

    // 2. Count tool definition JSON text
    let tool_chars: usize = request
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0);

    msg_chars + tool_chars
}

/// Compute context usage info from model capabilities and API usage response.
///
/// Usable context is derived from [`ModelCapabilitiesInfo::effective_input_budget`],
/// which uses `max_input_tokens` when available, or reserves output space capped
/// by `max_output_tokens_limit` (default 32K) otherwise.
pub fn compute_context_usage(
    caps: &ModelCapabilitiesInfo,
    usage: &acowork_core::providers::traits::UsageInfo,
    max_output_tokens_limit: u64,
) -> acowork_core::protocol::ContextUsageInfo {
    let usable = caps.effective_input_budget(max_output_tokens_limit);
    let total = usage.prompt_tokens + usage.completion_tokens;
    let percent = if usable > 0 {
        ((total as f64 / usable as f64) * 100.0).min(100.0) as u8
    } else {
        0
    };
    acowork_core::protocol::ContextUsageInfo {
        context_window: caps.context_window,
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        total_tokens: total,
        max_input_tokens: caps.max_input_tokens,
        usable_context: usable,
        usage_percent: percent,
    }
}

/// Build a [`ContextUsageInfo`] from persisted last-token counts.
///
/// This produces the same structure as [`compute_context_usage`] but from
/// the raw `last_input_tokens` / `last_output_tokens` values stored in
/// JSONL metadata, filling zero for cache/reasoning breakdown fields.
///
/// Callers must supply the *current* [`ModelCapabilitiesInfo`] because
/// window-derived fields (`context_window`, `usable_context`, `usage_percent`)
/// are model-dependent and become stale if the user switched models between
/// sessions.
pub fn build_context_usage_from_persisted(
    caps: &ModelCapabilitiesInfo,
    last_input_tokens: u64,
    last_output_tokens: u64,
    max_output_tokens_limit: u64,
) -> acowork_core::protocol::ContextUsageInfo {
    let usage = acowork_core::providers::traits::UsageInfo {
        prompt_tokens: last_input_tokens,
        completion_tokens: last_output_tokens,
        total_tokens: last_input_tokens + last_output_tokens,
        ..Default::default()
    };
    compute_context_usage(caps, &usage, max_output_tokens_limit)
}
