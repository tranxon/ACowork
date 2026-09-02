//! Context building (system prompt + history + memory + identity + skills)
//!
//! Builds the complete context for LLM requests following the priority order
//! defined in docs/03-agent-runtime.md §3.1.

use acowork_core::manifest::AgentManifest;
use acowork_core::protocol::ModelCapabilitiesInfo;
use acowork_core::providers::traits::{
    CacheControl, ChatMessage, ChatRequest, ContentPart, MessageRole, ReasoningEffort,
};
use std::sync::OnceLock;

use crate::agent::history::HistoryManager;
use crate::config::DEFAULT_TEMPERATURE;
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
    /// G9: Abstention guidance prompt — when retrieval returns nothing and
    /// abstention is enabled, this prompt tells the Agent to say "I'm not
    /// sure" rather than fabricate an answer. Injected after retrieved
    /// memory (same slot as `ambiguous_confirmation_hint`).
    abstention_prompt: Option<String>,
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
    /// LLM temperature override. `None` means fall through the per-agent chain:
    /// `agent_config.json.temperature` (Layer 1) → `manifest.llm.temperature` (Layer 2) → `DEFAULT_TEMPERATURE` (Layer 3).
    /// Set per-session via `set_temperature()` from AgentLoop before each build.
    temperature: Option<f32>,
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
            abstention_prompt: None,
            skill_instructions: None,
            todo_context: None,
            reasoning_effort: None,
            thinking_mode: None,
            temperature: None,
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

    /// Set the LLM temperature override for this builder. `None` clears the
    /// override and falls back to [`DEFAULT_TEMPERATURE`] when building the
    /// ChatRequest. Called by AgentLoop each iteration from session/core state.
    pub fn set_temperature(&mut self, temperature: Option<f32>) {
        self.temperature = temperature;
    }

    /// Get the current reasoning effort level, if set.
    pub fn reasoning_effort(&self) -> Option<&ReasoningEffort> {
        self.reasoning_effort.as_ref()
    }

    /// Get the current temperature override, if set.
    ///
    /// `None` means the per-agent fallback chain applies at build time
    /// (`agent_config.temperature` → `manifest.llm.temperature` →
    /// [`DEFAULT_TEMPERATURE`]). ADR-054: exposed so the debug snapshot
    /// can report the request params the LLM actually received.
    pub fn temperature(&self) -> Option<f32> {
        self.temperature
    }

    /// Get the current Anthropic thinking mode ("extended" | "adaptive"), if set.
    ///
    /// ADR-054: exposed so the debug snapshot can report the request
    /// params the LLM actually received.
    pub fn thinking_mode(&self) -> Option<&str> {
        self.thinking_mode.as_deref()
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
    /// Only sections present in the patch are applied; sections that are
    /// not patched remain unchanged. Unknown section keys are rejected
    /// (ADR-054 §6 typo safety) and a `TypeMismatch` error is returned if
    /// a patch value's variant doesn't match the section's expected type.
    ///
    /// Per-section patch semantics (type validation, empty-string clearing,
    /// tool_definitions array check) live in [`resolve_patch`] — the SAME
    /// resolution path used by `handle_patch_context` for snapshot preview,
    /// so preview and apply-time behavior can never drift.
    pub fn apply_patches(
        &mut self,
        patches: &crate::debug::protocol::PatchSet,
    ) -> Result<(), crate::debug::protocol::PatchError> {
        for (key, value) in &patches.patches {
            match resolve_patch(key, value)? {
                ResolvedPatch::Text(content) => match key.as_str() {
                    "system_prompt" => self.set_system_prompt(content),
                    "workspace_context" => self.set_workspace_context(content),
                    "environment" => self.set_environment_override(content),
                    "skill_instructions" => self.set_skill_instructions(content),
                    "workspace_prompt_file" => self.set_workspace_prompt_file(Some(content)),
                    "todo_context" => self.set_todo_context(Some(content)),
                    "ambiguous_confirmation_hint" => {
                        self.set_ambiguous_confirmation_hint(content);
                    }
                    _ => unreachable!("resolve_patch only yields Text for text sections"),
                },
                ResolvedPatch::Json(value) => match key.as_str() {
                    "retrieved_memory" => self.set_retrieved_memory_patch(value.to_string()),
                    "identity_context" => self.set_identity_context(value.to_string()),
                    _ => unreachable!("resolve_patch only yields Json for json sections"),
                },
                ResolvedPatch::ToolDefinitions(defs) => {
                    debug_assert_eq!(key, "tool_definitions");
                    self.set_tool_definitions(defs);
                }
                // Empty string clears the section — build() falls back:
                // environment → auto-detect; the three ADR-054 sections → omitted.
                ResolvedPatch::Clear => match key.as_str() {
                    "environment" => self.set_environment_override(String::new()),
                    "workspace_prompt_file" => self.set_workspace_prompt_file(None),
                    "todo_context" => self.set_todo_context(None),
                    "ambiguous_confirmation_hint" => self.clear_ambiguous_confirmation_hint(),
                    _ => unreachable!("resolve_patch only yields Clear for clearable sections"),
                },
            }
        }
        Ok(())
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
        if self.abstention_prompt.is_some() {
            self.abstention_prompt = None;
        }
    }

    /// P3-4: Set ambiguous conflict confirmation hint for injection into
    /// the system prompt. When ≥ 3 pending ambiguous conflicts exist,
    /// this hint guides the Agent to naturally ask the user about them.
    pub fn set_ambiguous_confirmation_hint(&mut self, hint: String) {
        self.ambiguous_confirmation_hint = Some(hint);
    }

    /// G9: Set the abstention guidance prompt for injection into the
    /// system prompt. When retrieval returns nothing and abstention is
    /// enabled, this prompt steers the Agent to abstain rather than guess.
    pub fn set_abstention_prompt(&mut self, prompt: String) {
        self.abstention_prompt = Some(prompt);
    }

    /// G9: Clear the abstention guidance prompt.
    pub fn clear_abstention_prompt(&mut self) {
        self.abstention_prompt = None;
    }

    /// G9: Get the abstention guidance prompt text, if set.
    pub fn abstention_prompt(&self) -> Option<&str> {
        self.abstention_prompt.as_deref()
    }

    /// Clear the ambiguous-confirmation hint.
    ///
    /// ADR-054: debug patch empty-string clearing semantics — an empty
    /// string patch removes the section so `build()` omits it entirely
    /// (consistent with `workspace_prompt_file` / `todo_context`).
    pub fn clear_ambiguous_confirmation_hint(&mut self) {
        self.ambiguous_confirmation_hint = None;
    }

    /// Get the ambiguous-confirmation hint text, if set.
    ///
    /// ADR-054: exposed so the debug snapshot can surface this section
    /// (previously invisible — a common "why is the agent asking
    /// disambiguation questions" blind spot).
    pub fn ambiguous_confirmation_hint(&self) -> Option<&str> {
        self.ambiguous_confirmation_hint.as_deref()
    }

    /// Get the todo list context text, if set.
    ///
    /// ADR-054: exposed so the debug snapshot can surface this section
    /// (previously invisible — a common "why is the agent looping on a
    /// stale todo list" blind spot).
    pub fn todo_context(&self) -> Option<&str> {
        self.todo_context.as_deref()
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

    /// Build the complete ChatRequest for the LLM.
    ///
    /// ADR-060: output is reorganized into four cache-friendly blocks:
    /// - Block A: static system kernel (single SystemMessage + ephemeral breakpoint)
    /// - Block B: append-only conversation history
    /// - Block C: dynamic todo snapshot (User role, own breakpoint)
    /// - Block D: current user message, passed explicitly by the caller
    ///   (`None` during tool-loop iterations / debug replay — see §5.5).
    pub fn build(
        &self,
        manifest: &AgentManifest,
        history: &HistoryManager,
        current_user_message: Option<&ChatMessage>,
        gateway_capabilities: Option<&ModelCapabilitiesInfo>,
        max_output_tokens_limit: u64,
    ) -> ChatRequest {
        let mut messages = Vec::new();

        // ── Block A: static kernel (ADR-060 §5.2) ──
        // Byte-stable across iterations: no dynamic block (retrieved_memory,
        // todo_context, ambiguous_confirmation_hint) is embedded here.
        let mut system_content = self.system_prompt.clone();

        // 2. Identity context (if available)
        if let Some(ref identity) = self.identity_context {
            system_content.push_str(&format!(
                "\n\n## User Identity\n{identity}\n\n\
                 Reply in the language specified by the Language field above."
            ));
        }

        // 2.2 Workspace context (if available, from Gateway push)
        if let Some(ref workspace) = self.workspace_context {
            system_content.push_str(&format!("\n\n{workspace}"));
        }

        // 2.5 Retrieved memory context from Grafeo (long-term memory)
        if let Some(ref memory) = self.retrieved_memory {
            system_content.push_str(&format!("\n\n## Relevant Memories\n{memory}"));
        }

        // 2.5.1 Abstention guidance (G9): injected only when retrieval
        // returned nothing and abstention was enabled. In the abstention
        // case `retrieved_memory` is empty, so this slot is otherwise
        // unused — Block A stays byte-stable on the normal path. Position
        // matches the historical `ambiguous_confirmation_hint` slot.
        if let Some(ref prompt) = self.abstention_prompt {
            system_content.push_str(&format!("\n\n## Memory Abstention Guidance\n{prompt}"));
        }

        // 2.6 Skill instructions (debug patching or runtime config)
        if let Some(ref skills) = self.skill_instructions {
            system_content.push_str(&format!("\n\n## Skill Instructions\n{skills}"));
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

        // ADR-060: `todo_context` and `ambiguous_confirmation_hint` moved OUT
        // of Block A (dynamic content would invalidate the whole stable
        // prefix). Todo now lives in Block C; the hint is not injected this
        // round (ADR-060 §5.2).

        // 3.5 Tool definitions are passed separately in ChatRequest

        // Block A carries the ephemeral cache breakpoint (ADR-060 §5.1/§5.6).
        let mut system_msg = ChatMessage::system(system_content);
        system_msg.cache_control = Some(CacheControl::Ephemeral);
        messages.push(system_msg);

        // Estimate system prompt tokens for observability
        let system_msg = messages.last().unwrap();
        let system_tokens = self.counter.count_message(system_msg, "", None);
        tracing::debug!(system_tokens, "System prompt token estimation");

        // 7. Conversation history — Block B (ADR-060 §5.3)
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

        // ── Block C: dynamic todo snapshot (ADR-060 §5.4) ──
        // User role: avoids Anthropic system-elevation overwrite and
        // MiniMax/o1 "system must be first" constraints; consecutive user
        // messages are auto-merged by Anthropic/OpenAI without semantic loss.
        if let Some(ref todos) = self.todo_context {
            messages.push(ChatMessage {
                role: MessageRole::User,
                content: format!(
                    "## Todo Task List\nThis is your todo task list. If any task status needs updating, use the `todo_write` tool to update it. If nothing needs updating, just do nothing and keep quiet.\n\n{todos}"
                ),
                cache_control: Some(CacheControl::Ephemeral),
                ..Default::default()
            });
        }

        // ── Block D: current user message (ADR-060 §5.5) ──
        // Explicitly passed by the caller — never inferred from the history
        // tail (tool iterations leave Tool messages at the tail). Note the
        // message is ALSO part of Block B (it is persisted in history); Block
        // D is its duplicate clone at the end of the request.
        if let Some(user_msg) = current_user_message {
            messages.push(user_msg.clone());
        }

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
            temperature: Some(self.temperature.unwrap_or(DEFAULT_TEMPERATURE) as f64),
            max_tokens,
            tools: self.tool_definitions.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            thinking_mode: self.thinking_mode.clone(),
        }
    }
}

/// Build a [`PatchError::TypeMismatch`] for a section patch whose value
/// variant doesn't match the section's expected type.
fn patch_type_mismatch(
    key: &str,
    expected: &'static str,
    value: &crate::debug::protocol::PatchValue,
) -> crate::debug::protocol::PatchError {
    crate::debug::protocol::PatchError::TypeMismatch {
        section: key.to_string(),
        expected,
        actual: value.variant_name(),
    }
}

/// Result of resolving a debug patch value against a section key.
///
/// ADR-054: the per-section patch semantics (type validation, empty-string
/// clearing, tool_definitions array check) live in ONE place — [`resolve_patch`]
/// — shared by `ContextBuilder::apply_patches` (actual application at build
/// time) and `handle_patch_context` (snapshot preview at RPC time), so the
/// two can never drift on edge cases (type mismatch, empty string, non-array
/// tool_definitions).
pub enum ResolvedPatch {
    /// String content for text-valued sections.
    Text(String),
    /// JSON value for `retrieved_memory` / `identity_context`.
    Json(serde_json::Value),
    /// Tool definitions — validated as a JSON array.
    ToolDefinitions(Vec<serde_json::Value>),
    /// Section cleared: `build()` falls back (environment → auto-detect;
    /// workspace_prompt_file / todo_context / ambiguous_confirmation_hint → omitted).
    Clear,
}

/// Resolve a `(section key, patch value)` pair into the final content to
/// apply/store, or [`ResolvedPatch::Clear`] for empty-string clearing.
///
/// Validates the value variant against the section's expected type and
/// rejects mismatches with [`PatchError::TypeMismatch`] (ADR-054 §6 typo
/// safety). Unknown keys are rejected with [`PatchError::UnknownSection`].
pub fn resolve_patch(
    key: &str,
    value: &crate::debug::protocol::PatchValue,
) -> Result<ResolvedPatch, crate::debug::protocol::PatchError> {
    use crate::debug::protocol::{PatchError, PatchValue};

    match key {
        "system_prompt" | "workspace_context" | "skill_instructions" => match value {
            PatchValue::Text { value } => Ok(ResolvedPatch::Text(value.clone())),
            _ => Err(patch_type_mismatch(key, "text", value)),
        },
        // environment: empty string clears the override — build() falls
        // back to auto-detected platform info.
        "environment" => match value {
            PatchValue::Text { value } if value.is_empty() => Ok(ResolvedPatch::Clear),
            PatchValue::Text { value } => Ok(ResolvedPatch::Text(value.clone())),
            _ => Err(patch_type_mismatch(key, "text", value)),
        },
        "tool_definitions" => match value {
            PatchValue::Json { value } => match value.as_array() {
                Some(defs) => Ok(ResolvedPatch::ToolDefinitions(defs.clone())),
                None => Err(PatchError::TypeMismatch {
                    section: key.to_string(),
                    expected: "json array",
                    actual: "json non-array",
                }),
            },
            _ => Err(patch_type_mismatch(key, "json", value)),
        },
        "retrieved_memory" | "identity_context" => match value {
            PatchValue::Json { value } => Ok(ResolvedPatch::Json(value.clone())),
            _ => Err(patch_type_mismatch(key, "json", value)),
        },
        // ADR-054 step 3 sections: empty string clears (consistent clearing
        // semantics across all three — previously ambiguous_confirmation_hint
        // had none).
        "workspace_prompt_file" | "todo_context" | "ambiguous_confirmation_hint" => match value {
            PatchValue::Text { value } if value.is_empty() => Ok(ResolvedPatch::Clear),
            PatchValue::Text { value } => Ok(ResolvedPatch::Text(value.clone())),
            _ => Err(patch_type_mismatch(key, "text", value)),
        },
        _ => Err(PatchError::UnknownSection(key.to_string())),
    }
}

/// Cached environment text — computed once per process (ADR-060 §6.4).
///
/// Environment info is static within a process lifetime; caching it makes
/// every `build()` call cheaper and guarantees byte stability across
/// iterations (a cache-friendly Block A requirement).
static CACHED_ENV_TEXT: OnceLock<String> = OnceLock::new();

/// Detect and format the environment info text that gets injected into
/// the system prompt. Used by debug snapshot capture and ContextBuilder::build().
///
/// ADR-060 §6.4: result is memoized in a process-global [`OnceLock`]; the
/// first call formats the text, subsequent calls return the cached `&str`.
pub fn detect_environment_text() -> &'static str {
    CACHED_ENV_TEXT.get_or_init(|| {
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
    })
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
        "#,
        )
        .unwrap()
    }

    #[test]
    fn test_context_builder_basic() {
        let manifest = test_manifest();
        let mut history = HistoryManager::new(10000);
        history.append(ChatMessage::user("Hello"));

        let builder = ContextBuilder::new("You are a helpful assistant.".to_string())
            .with_override_model("gpt-4".to_string());
        let request = builder.build(&manifest, &history, None, None, 32_768);

        assert_eq!(request.model, "gpt-4");
        assert_eq!(request.messages.len(), 2); // system + user
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert_eq!(request.messages[1].role, MessageRole::User);
        // ADR-060: Block A carries the ephemeral cache breakpoint.
        assert_eq!(
            request.messages[0].cache_control,
            Some(acowork_core::providers::traits::CacheControl::Ephemeral)
        );
    }

    #[test]
    fn test_context_builder_with_identity() {
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);

        let builder = ContextBuilder::new("You are a helper.".to_string())
            .with_identity(Some("Name: Alice, City: Shanghai".to_string()));

        let request = builder.build(&manifest, &history, None, None, 32_768);
        assert!(request.messages[0].content.contains("Alice"));
    }

    /// Sentinel test: when identity context contains a Language field, the
    /// built system prompt must include the language directive so the LLM
    /// replies in the user's preferred language. Guards against accidental
    /// removal during future refactors of `ContextBuilder::build()`.
    #[test]
    fn test_context_builder_injects_language_directive_when_identity_has_language() {
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);

        let identity =
            "- Display Name: Alice\n- Language: zh-CN\n- Timezone: Asia/Shanghai".to_string();
        let builder = ContextBuilder::new("You are a helper.".to_string())
            .with_identity(Some(identity));

        let request = builder.build(&manifest, &history, None, None, 32_768);
        let system = &request.messages[0].content;
        assert!(
            system.contains("Language field above"),
            "system prompt must instruct the LLM to follow the Language field; got:\n{system}"
        );
        assert!(
            system.contains("zh-CN"),
            "system prompt must contain the raw Language value so the directive is resolvable; got:\n{system}"
        );
    }

    // ── apply_patches / resolve_patch (ADR-054 single-source semantics) ──

    #[test]
    fn apply_patches_empty_string_clears_adr054_sections() {
        use crate::debug::protocol::{PatchSet, PatchValue};
        use std::collections::HashMap;

        let mut builder = ContextBuilder::new("base".to_string())
            .with_override_model("gpt-4".to_string());
        builder.set_todo_context(Some("task A".to_string()));
        builder.set_ambiguous_confirmation_hint("hint".to_string());
        builder.set_workspace_prompt_file(Some("CLAUDE.md content".to_string()));

        // Empty string clears all three ADR-054 step-3 sections consistently.
        let patches = PatchSet {
            patches: HashMap::from([
                (
                    "todo_context".to_string(),
                    PatchValue::Text { value: String::new() },
                ),
                (
                    "ambiguous_confirmation_hint".to_string(),
                    PatchValue::Text { value: String::new() },
                ),
                (
                    "workspace_prompt_file".to_string(),
                    PatchValue::Text { value: String::new() },
                ),
            ]),
        };
        builder.apply_patches(&patches).expect("clear must succeed");

        assert!(builder.todo_context().is_none(), "todo_context cleared");
        assert!(
            builder.ambiguous_confirmation_hint().is_none(),
            "ambiguous_confirmation_hint cleared"
        );
        assert!(
            builder.workspace_prompt_file().is_none(),
            "workspace_prompt_file cleared"
        );

        // build() must omit the cleared sections entirely.
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);
        let request = builder.build(&manifest, &history, None, None, 32_768);
        let system = &request.messages[0].content;
        assert!(!system.contains("Todo Task List"), "todo omitted");
        assert!(
            !system.contains("Memory Conflicts Needing Confirmation"),
            "ambiguous hint omitted"
        );
        assert!(!system.contains("Workspace Prompt File"), "prompt file omitted");
        // ADR-060: cleared todo must not appear as a Block C message either.
        assert!(
            !request
                .messages
                .iter()
                .any(|m| m.content.contains("Todo Task List")),
            "todo omitted from Block C after clear"
        );
    }

    // ── ADR-060: Block A/B/C/D layout ──

    #[test]
    fn test_build_block_layout_a_b_c_d() {
        let manifest = test_manifest();
        let mut history = HistoryManager::new(10000);
        history.append(ChatMessage::user("First turn"));
        history.append(ChatMessage::assistant("First reply"));

        let mut builder = ContextBuilder::new("Kernel".to_string())
            .with_override_model("gpt-4".to_string());
        builder.set_todo_context(Some("- task1\n- task2".to_string()));

        // Block D: current user message (explicitly passed).
        let current = ChatMessage::user("Second turn");
        let request = builder.build(&manifest, &history, Some(&current), None, 32_768);

        // [0] Block A (System + ephemeral breakpoint)
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert_eq!(
            request.messages[0].cache_control,
            Some(acowork_core::providers::traits::CacheControl::Ephemeral)
        );
        // Block A must NOT contain dynamic todo content.
        assert!(!request.messages[0].content.contains("Todo Task List"));
        // Block A must NOT contain the ambiguous hint section.
        assert!(!request.messages[0].content.contains("Memory Conflicts"));

        // [1..2] Block B: history turns (user + assistant)
        assert_eq!(request.messages[1].role, MessageRole::User);
        assert_eq!(request.messages[1].content, "First turn");
        assert_eq!(request.messages[2].role, MessageRole::Assistant);
        assert_eq!(request.messages[2].content, "First reply");

        // [3] Block C: todo snapshot — User role (never System), breakpoint set.
        assert_eq!(request.messages.len(), 5);
        let block_c = &request.messages[3];
        assert_eq!(
            block_c.role, MessageRole::User,
            "Block C must use User role (ADR-060 §5.4)"
        );
        assert!(block_c.content.contains("Todo Task List"));
        assert!(block_c.content.contains("task1"));
        assert_eq!(
            block_c.cache_control,
            Some(acowork_core::providers::traits::CacheControl::Ephemeral)
        );

        // [4] Block D: exact duplicate of the current user message.
        let block_d = &request.messages[4];
        assert_eq!(block_d.role, MessageRole::User);
        assert_eq!(block_d.content, current.content);
        assert_eq!(block_d.cache_control, current.cache_control);
    }

    #[test]
    fn test_build_block_d_none_in_tool_iteration() {
        use acowork_core::providers::traits::{FunctionCall, ToolCall};

        // Tool-loop iterations pass `None`: request has no Block D.
        let manifest = test_manifest();
        let mut history = HistoryManager::new(10000);
        history.append(ChatMessage::user("Turn"));
        // A REAL tool_call on the assistant turn keeps the tool result
        // from being classified as orphaned by sanitize_messages.
        history.append(ChatMessage::assistant_with_tools(
            "",
            vec![ToolCall {
                id: "toolu_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "test_tool".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        ));
        history.append(ChatMessage::tool("toolu_1", "ok"));

        let mut builder = ContextBuilder::new("Kernel".to_string())
            .with_override_model("gpt-4".to_string());
        builder.set_todo_context(Some("- t".to_string()));

        let request = builder.build(&manifest, &history, None, None, 32_768);
        // [0] A, [1..3] B, [4] C — no D.
        assert_eq!(request.messages.len(), 5);
        assert_eq!(request.messages[4].role, MessageRole::User);
        assert!(request.messages[4].content.contains("Todo Task List"));
    }

    // ── G9: Abstention guidance injection ──

    #[test]
    fn test_build_injects_abstention_prompt_when_set() {
        // G9: when abstention triggers (empty retrieval + enabled), the
        // prompt is injected into the system prompt (Block A) after the
        // retrieved-memory slot. On the normal path it is absent.
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);

        // Normal path: no abstention prompt set → Block A unchanged.
        let builder = ContextBuilder::new("Kernel".to_string())
            .with_override_model("gpt-4".to_string());
        let request = builder.build(&manifest, &history, None, None, 32_768);
        assert!(!request.messages[0].content.contains("Memory Abstention Guidance"));

        // Abstention path: prompt set → injected into Block A.
        let mut builder = ContextBuilder::new("Kernel".to_string())
            .with_override_model("gpt-4".to_string());
        builder.set_abstention_prompt("When you are not confident, say you're not sure.".to_string());
        let request = builder.build(&manifest, &history, None, None, 32_768);
        assert!(
            request.messages[0].content.contains("## Memory Abstention Guidance"),
            "Block A must contain the abstention guidance section"
        );
        assert!(
            request.messages[0].content.contains("not confident"),
            "Block A must contain the abstention prompt text"
        );

        // clear_abstention_prompt removes it (stale prevention path).
        let mut builder = ContextBuilder::new("Kernel".to_string())
            .with_override_model("gpt-4".to_string());
        builder.set_abstention_prompt("prompt".to_string());
        builder.clear_retrieved_memory(); // clears memory + hint + abstention
        let request = builder.build(&manifest, &history, None, None, 32_768);
        assert!(
            !request.messages[0].content.contains("Memory Abstention Guidance"),
            "abstention prompt must be cleared with stale memory prevention"
        );
    }

    #[test]
    fn test_build_todo_snapshot_byte_stable_across_builds() {
        // ADR-060 §5.4: unchanged todo content must produce byte-identical
        // Block C across builds (deterministic format_todos contract).
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);
        let mut builder = ContextBuilder::new("Kernel".to_string())
            .with_override_model("gpt-4".to_string());
        builder.set_todo_context(Some("- stable item".to_string()));

        let r1 = builder.build(&manifest, &history, None, None, 32_768);
        let r2 = builder.build(&manifest, &history, None, None, 32_768);
        let c1 = r1.messages.iter().find(|m| m.content.contains("Todo Task List")).unwrap();
        let c2 = r2.messages.iter().find(|m| m.content.contains("Todo Task List")).unwrap();
        assert_eq!(c1.content, c2.content, "Block C bytes must be deterministic");
        assert_eq!(r1.messages[0].content, r2.messages[0].content, "Block A bytes must be deterministic");
    }

    #[test]
    fn apply_patches_rejects_type_mismatch_and_non_array_tools() {
        use crate::debug::protocol::{PatchError, PatchSet, PatchValue};
        use std::collections::HashMap;

        let mut builder = ContextBuilder::new("base".to_string());

        // JSON patch on a text section → TypeMismatch.
        let err = builder
            .apply_patches(&PatchSet {
                patches: HashMap::from([(
                    "system_prompt".to_string(),
                    PatchValue::Json {
                        value: serde_json::json!({ "not": "text" }),
                    },
                )]),
            })
            .expect_err("json patch for text section must error");
        assert!(matches!(
            err,
            PatchError::TypeMismatch { section, .. } if section == "system_prompt"
        ));

        // Non-array JSON on tool_definitions → TypeMismatch.
        let err = builder
            .apply_patches(&PatchSet {
                patches: HashMap::from([(
                    "tool_definitions".to_string(),
                    PatchValue::Json {
                        value: serde_json::json!({ "not": "an array" }),
                    },
                )]),
            })
            .expect_err("non-array tool_definitions must error");
        assert!(matches!(
            err,
            PatchError::TypeMismatch { section, .. } if section == "tool_definitions"
        ));
    }

    #[test]
    fn apply_patches_empty_environment_falls_back_to_detect() {
        use crate::debug::protocol::{PatchSet, PatchValue};
        use std::collections::HashMap;

        let mut builder = ContextBuilder::new("base".to_string());
        builder.set_environment_override("custom env".to_string());
        assert!(builder.environment_override().is_some());

        builder
            .apply_patches(&PatchSet {
                patches: HashMap::from([(
                    "environment".to_string(),
                    PatchValue::Text { value: String::new() },
                )]),
            })
            .expect("clear environment must succeed");
        assert!(
            builder.environment_override().is_none(),
            "empty string clears the override — build() falls back to auto-detect"
        );
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
        let fresh = compute_context_usage(&caps, &usage, max_output_limit, None);

        // Via build_context_usage_from_persisted with same numbers (no cumulative)
        let persisted = build_context_usage_from_persisted(
            &caps,
            45_000,
            1_200,
            max_output_limit,
            None,
            None,
        );

        assert_eq!(fresh.context_window, persisted.context_window);
        assert_eq!(fresh.input_tokens, persisted.input_tokens);
        assert_eq!(fresh.output_tokens, persisted.output_tokens);
        assert_eq!(fresh.total_tokens, persisted.total_tokens);
        assert_eq!(fresh.max_input_tokens, persisted.max_input_tokens);
        assert_eq!(fresh.usable_context, persisted.usable_context);
        assert_eq!(fresh.usage_percent, persisted.usage_percent);
        // Without cumulative tokens supplied, the new fields stay None.
        assert_eq!(persisted.total_input_tokens, None);
        assert_eq!(persisted.total_output_tokens, None);
    }

    #[test]
    fn test_build_context_usage_from_persisted_zero_tokens() {
        // New session with no token data yet → should produce 0 input/output
        let caps = test_caps(200_000, 8_192);
        let info = build_context_usage_from_persisted(&caps, 0, 0, 32_768, None, None);
        assert_eq!(info.input_tokens, 0);
        assert_eq!(info.output_tokens, 0);
        assert_eq!(info.total_tokens, 0);
        assert_eq!(info.usage_percent, 0);
        assert_eq!(info.total_input_tokens, None);
        assert_eq!(info.total_output_tokens, None);
    }

    #[test]
    fn test_build_context_usage_from_persisted_populates_cumulative_totals() {
        // When SessionTokens is supplied, the resulting ContextUsageInfo must
        // carry the cumulative total_input_tokens / total_output_tokens.
        // This is the path used by session_task.rs on resume to give the
        // frontend both per-turn (last) and cumulative (total) figures.
        use crate::conversation::SessionTokens;

        let caps = test_caps(128_000, 16_384);
        let cumulative = SessionTokens {
            last_input: 45_000,
            last_output: 1_200,
            total_input: 250_000,
            total_output: 7_500,
        };

        let info = build_context_usage_from_persisted(
            &caps,
            cumulative.last_input,
            cumulative.last_output,
            32_768,
            None,
            Some(&cumulative),
        );

        // Per-turn fields populated from last_input/last_output scalars.
        assert_eq!(info.input_tokens, 45_000);
        assert_eq!(info.output_tokens, 1_200);
        assert_eq!(info.total_tokens, 46_200);

        // Cumulative fields populated from SessionTokens.total_*.
        assert_eq!(info.total_input_tokens, Some(250_000));
        assert_eq!(info.total_output_tokens, Some(7_500));

        // Cumulative totals must be distinct from per-turn values (the
        // whole point of having both sets of fields).
        assert_ne!(
            info.total_input_tokens.unwrap(),
            info.input_tokens,
            "cumulative total_input_tokens must NOT equal per-turn input_tokens",
        );
        assert!(info.total_input_tokens.unwrap() > info.input_tokens);
    }

    #[test]
    fn test_context_usage_with_override_caps_window() {
        // Model has 200K window. User overrides to 100K.
        let caps = test_caps(200_000, 16_384);
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: 60_000,
            completion_tokens: 5_000,
            total_tokens: 65_000,
            ..Default::default()
        };
        let info = compute_context_usage(&caps, &usage, 32_768, Some(100_000));
        // effective_window = min(100_000, 200_000) = 100_000
        assert_eq!(info.context_window, 100_000);
        // effective_usable = min(usable, effective_window)
        // usable = 200_000 - 16_384 = 183_616 (no max_input_tokens)
        // effective_usable = min(183_616, 100_000) = 100_000
        assert_eq!(info.usable_context, 100_000);
        // percent = 65_000 / 100_000 * 100 = 65%
        assert_eq!(info.usage_percent, 65);
    }

    #[test]
    fn test_context_usage_with_zero_override_uses_model_window() {
        // Some(0) = "no limit" → use model's full window
        let caps = test_caps(128_000, 8_192);
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: 50_000,
            completion_tokens: 2_000,
            total_tokens: 52_000,
            ..Default::default()
        };
        let info = compute_context_usage(&caps, &usage, 32_768, Some(0));
        assert_eq!(info.context_window, 128_000);
    }

    #[test]
    fn test_context_usage_none_override_uses_model_window() {
        let caps = test_caps(128_000, 8_192);
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: 30_000,
            completion_tokens: 1_000,
            total_tokens: 31_000,
            ..Default::default()
        };
        let info = compute_context_usage(&caps, &usage, 32_768, None);
        assert_eq!(info.context_window, 128_000);
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
///
/// `context_window_cap` is the per-agent context window override (ADR-026):
/// - `None` — not set, use model's full `context_window`.
/// - `Some(0)` — "no limit" (user explicitly chose unlimited), use model's full.
/// - `Some(n)` where `n > 0` — cap the effective window at `min(n, model_window)`.
///
/// Both `context_window` and `usable_context` in the output reflect the
/// effective (capped) values, so the frontend status panel and context-usage
/// popup show numbers consistent with the user's per-agent setting.
pub fn compute_context_usage(
    caps: &ModelCapabilitiesInfo,
    usage: &acowork_core::providers::traits::UsageInfo,
    max_output_tokens_limit: u64,
    context_window_cap: Option<u64>,
) -> acowork_core::protocol::ContextUsageInfo {
    let model_window = caps.context_window;
    let effective_window = match context_window_cap {
        Some(0) | None => model_window,
        Some(cap) => cap.min(model_window),
    };
    let usable = caps.effective_input_budget(max_output_tokens_limit);
    let effective_usable = usable.min(effective_window);
    let total = usage.prompt_tokens + usage.completion_tokens;
    let percent = if effective_usable > 0 {
        ((total as f64 / effective_usable as f64) * 100.0).min(100.0) as u8
    } else {
        0
    };
    acowork_core::protocol::ContextUsageInfo {
        context_window: effective_window,
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        total_tokens: total,
        max_input_tokens: caps.max_input_tokens,
        usable_context: effective_usable,
        usage_percent: percent,
        // compute_context_usage only sees per-turn UsageInfo. Cumulative
        // session totals (total_input_tokens / total_output_tokens) must
        // be patched by the caller after consulting SessionTokens. They
        // default to None here so per-turn callers (e.g. loop_context.rs
        // fallback path) don't accidentally surface a stale cumulative
        // figure from a previous round.
        total_input_tokens: None,
        total_output_tokens: None,
        // ADR-028: agent-scoped cumulative tokens (patched by caller).
        agent_total_input_tokens: None,
        agent_total_output_tokens: None,
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
///
/// `context_window_cap` mirrors the same parameter on [`compute_context_usage`].
///
/// `cumulative_tokens` optionally carries the full [`crate::conversation::SessionTokens`]
/// so the cumulative `total_input_tokens` / `total_output_tokens` fields can be
/// populated on the resulting [`ContextUsageInfo`]. Pass `None` when only the
/// last-turn snapshot is available (e.g. unit tests exercising the legacy
/// scalar path).
pub fn build_context_usage_from_persisted(
    caps: &ModelCapabilitiesInfo,
    last_input_tokens: u64,
    last_output_tokens: u64,
    max_output_tokens_limit: u64,
    context_window_cap: Option<u64>,
    cumulative_tokens: Option<&crate::conversation::SessionTokens>,
) -> acowork_core::protocol::ContextUsageInfo {
    let mut info = {
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: last_input_tokens,
            completion_tokens: last_output_tokens,
            total_tokens: last_input_tokens + last_output_tokens,
            ..Default::default()
        };
        compute_context_usage(caps, &usage, max_output_tokens_limit, context_window_cap)
    };
    if let Some(t) = cumulative_tokens {
        info.total_input_tokens = Some(t.total_input);
        info.total_output_tokens = Some(t.total_output);
    }
    info
}
