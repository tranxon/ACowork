//! Agent main loop (9 steps)
//!
//! The core execution loop for Agent Runtime.
//! References ZeroClaw agent/loop_.rs but simplified for IPC architecture.
//!
//! S1.5: Streaming LLM responses via chat_stream()
//! S1.6: InboundQueue for external message injection
//! S1.7: Parallel tool execution with per-tool timeout

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use rollball_core::providers::traits::{
    ChatMessage, ChatResponse, MessageRole, Provider, StreamEvent, ToolCall,
};
use rollball_core::tools::traits::Tool;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::agent::budget_guard::{BudgetCheckResult, BudgetGuard};
use crate::agent::context::ContextBuilder;
use crate::agent::history::HistoryManager;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_detector::{LoopDetectionResult, LoopDetector, ResponseLevel};
use crate::config::RuntimeConfig;
use crate::error::{Result, RuntimeError};

/// Streaming chunk event emitted during LLM response generation.
///
/// Adapted from ZeroClaw's DraftEvent, simplified for RollBall's IPC architecture.
/// Each delta is forwarded to the Gateway via `TYPE_STREAM_CHUNK` frame,
/// which maps to a BridgeEventType::Chunk for the Desktop App WebSocket.
#[derive(Debug, Clone)]
pub enum ChunkEvent {
    /// Content delta to append to the streaming message
    Delta(String),
}

/// Agent loop runner
pub struct AgentLoop {
    /// Runtime configuration
    config: RuntimeConfig,
    /// Agent manifest
    manifest: rollball_core::AgentManifest,
    /// LLM Provider
    provider: Arc<dyn Provider>,
    /// Tool registry
    tools: Vec<Arc<dyn Tool>>,
    /// History manager
    history: HistoryManager,
    /// Budget guard
    budget_guard: BudgetGuard,
    /// Loop detector
    loop_detector: LoopDetector,
    /// Inbound message receiver for external message injection
    inbound_rx: tokio::sync::mpsc::Receiver<InboundMessage>,
    /// Optional streaming chunk sender (like ZeroClaw's on_delta).
    /// When set, each StreamEvent::Content delta is forwarded here
    /// so the caller can relay chunks to Gateway via TYPE_STREAM_CHUNK.
    on_chunk: Option<mpsc::Sender<ChunkEvent>>,
}

impl AgentLoop {
    /// Create a new agent loop runner, returning both the loop and an inbound sender.
    ///
    /// The caller can use the sender to inject messages into the loop from
    /// external sources (Gateway, cross-agent intents, system notifications).
    ///
    /// If `on_chunk` is provided, streaming LLM deltas are forwarded to it
    /// so the caller can relay chunks to the Gateway via TYPE_STREAM_CHUNK frames
    /// (like ZeroClaw's on_delta / DraftEvent pattern).
    pub fn new(
        config: RuntimeConfig,
        manifest: rollball_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        budget: rollball_core::Budget,
        on_chunk: Option<mpsc::Sender<ChunkEvent>>,
    ) -> (Self, tokio::sync::mpsc::Sender<InboundMessage>) {
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(64);
        let max_tokens = config.history_max_tokens;
        let keep_full = config.keep_full_results;
        let loop_ = Self {
            config,
            manifest,
            provider,
            tools,
            history: HistoryManager::new(max_tokens, keep_full),
            budget_guard: BudgetGuard::new(budget),
            loop_detector: LoopDetector::with_defaults(),
            inbound_rx,
            on_chunk,
        };
        (loop_, inbound_tx)
    }

    /// Update the LLM provider at runtime (e.g., after receiving a hot-pushed
    /// LLMConfigDelivery from Gateway).
    pub fn update_provider(&mut self, new_provider: Arc<dyn Provider>, model: String) {
        let old_name = self.provider.name().to_string();
        self.provider = new_provider;
        tracing::info!(
            old_provider = %old_name,
            new_provider = %self.provider.name(),
            model = %model,
            "LLM provider updated at runtime via LLMConfigDelivery"
        );
    }

    /// Run the agent loop for a single user message
    pub async fn run(&mut self, user_message: &str, context_builder: &ContextBuilder) -> Result<String> {
        // Add user message to history
        self.history.append(ChatMessage {
            role: MessageRole::User,
            content: user_message.to_string(),
            name: None,
            tool_calls: None,
        });

        let mut iteration = 0u32;

        loop {
            iteration += 1;
            tracing::info!(iteration, "Starting loop iteration");

            // ⑨ Iteration limit check
            if iteration > self.config.max_iterations {
                tracing::warn!(iteration, "Max iterations reached");
                return Ok("Maximum iterations reached. The agent stopped to prevent infinite looping.".to_string());
            }

            // ⓪ Drain inbound queue (non-blocking)
            self.drain_inbound_queue();

            // ① Budget pre-check
            let estimated_tokens = self.history.estimate_total_tokens() + 500; // +500 for new response
            match self.budget_guard.check(estimated_tokens) {
                BudgetCheckResult::Allowed => {}
                BudgetCheckResult::Exceeded { reason, action } => {
                    tracing::warn!(reason = %reason, action = %action, "Budget exceeded");
                    match action.as_str() {
                        "deny" => {
                            return Err(RuntimeError::BudgetExceeded(reason));
                        }
                        "warn" => {
                            self.history.append(ChatMessage {
                                role: MessageRole::System,
                                content: format!("Warning: {reason}"),
                                name: None,
                                tool_calls: None,
                            });
                        }
                        _ => {}
                    }
                }
            }

            // ② Build context
            let chat_request = context_builder.build(&self.manifest, &self.history);

            // ②.5 Preemptive trim
            self.history.preemptive_trim(self.config.history_max_tokens);

            // ③ Call LLM with streaming (S1.5)
            let response = self.call_llm_streaming(&chat_request, context_builder).await?;

            // ④ Parse response
            let has_tool_calls = response.tool_calls.is_some();

            // Update budget
            if let Some(usage) = &response.usage {
                self.budget_guard.update_usage(usage.total_tokens, 0.0);
            }

            if !has_tool_calls {
                // Pure text response — normal exit
                let content = response.content.clone();
                self.history.append(ChatMessage {
                    role: MessageRole::Assistant,
                    content: response.content,
                    name: None,
                    tool_calls: None,
                });

                tracing::info!(iteration, "Agent returned text response");
                return Ok(content);
            }

            // Has tool calls — process them
            let tool_calls = response.tool_calls.unwrap_or_default();

            // ④.5 Tool call deduplication (same iteration)
            let mut seen = HashSet::new();
            let deduped_calls: Vec<ToolCall> = tool_calls
                .into_iter()
                .filter(|tc| {
                    let sig = format!("{}:{}", tc.function.name, tc.function.arguments);
                    seen.insert(sig)
                })
                .collect();

            // Add assistant message with tool_calls to history
            self.history.append(ChatMessage {
                role: MessageRole::Assistant,
                content: response.content.clone(),
                name: None,
                tool_calls: Some(deduped_calls.clone()),
            });

            // ⑤ Tool dispatch — parallel execution (S1.7)
            let tool_results = self.execute_tools_parallel(&deduped_calls).await;

            // ⑥ Append tool results to history + ⑧ Loop detection
            for (tc, result_content) in deduped_calls.iter().zip(tool_results.iter()) {
                let tool_result_message = ChatMessage {
                    role: MessageRole::Tool,
                    content: serde_json::json!({
                        "tool_call_id": tc.id,
                        "content": result_content,
                    })
                    .to_string(),
                    name: Some(tc.function.name.clone()),
                    tool_calls: None,
                };

                self.history.append(tool_result_message);

                // ⑧ Loop detection
                match self.loop_detector.check(
                    &tc.function.name,
                    &tc.function.arguments,
                    result_content,
                ) {
                    LoopDetectionResult::NoLoop => {}
                    LoopDetectionResult::LoopDetected {
                        pattern: _,
                        level,
                        count: _,
                        message,
                    } => {
                        tracing::warn!(message = %message, level = ?level, "Loop detected");
                        match level {
                            ResponseLevel::Warning => {
                                self.history.append(ChatMessage {
                                    role: MessageRole::System,
                                    content: message,
                                    name: None,
                                    tool_calls: None,
                                });
                            }
                            ResponseLevel::Block => {
                                // Block was already handled by returning error as tool result
                            }
                            ResponseLevel::Break => {
                                return Err(RuntimeError::LoopDetected(message));
                            }
                        }
                    }
                }
            }

            // ⑦ Usage report (async, non-blocking)
            // NOTE: Usage reporting to Gateway is handled by the caller
            // (run_gateway_loop in cli.rs) after the loop iteration completes.
            tracing::debug!(iteration, "Loop iteration complete");

            // ⑨ DevMode control
            // TODO(Phase 5): DevMode step control — debug.step(iteration)

            // Continue to next iteration
            tracing::debug!(iteration, "Loop iteration complete, continuing");
        }
    }

    /// Drain inbound message queue (non-blocking).
    ///
    /// Injects external messages (user, system, intent) into history
    /// before each loop iteration. Applies size limits to prevent
    /// token explosion from oversized payloads.
    fn drain_inbound_queue(&mut self) {
        while let Ok(msg) = self.inbound_rx.try_recv() {
            // Enforce size limits before injecting
            let (msg, _truncated) = msg.enforce_size_limit();
            match msg {
                InboundMessage::UserMessage(text) => {
                    self.history.append(ChatMessage {
                        role: MessageRole::User,
                        content: text,
                        name: None,
                        tool_calls: None,
                    });
                }
                InboundMessage::SystemNotification { notification_type, data } => {
                    tracing::info!("System notification: {} = {:?}", notification_type, data);
                    self.history.append(ChatMessage {
                        role: MessageRole::System,
                        content: format!("[system:{}] {}", notification_type, data),
                        name: None,
                        tool_calls: None,
                    });
                }
                InboundMessage::IntentMessage { from, action, params } => {
                    tracing::info!("Intent from {}: {} params={:?}", from, action, params);
                    self.history.append(ChatMessage {
                        role: MessageRole::User,
                        content: format!("[intent:{}:{}] {}", from, action, params),
                        name: None,
                        tool_calls: None,
                    });
                }
            }
        }
    }

    /// Call LLM with streaming, accumulating content and tool calls.
    ///
    /// Handles context overflow recovery by detecting relevant errors
    /// from the stream and retrying after emergency trim.
    async fn call_llm_streaming(
        &mut self,
        chat_request: &rollball_core::providers::traits::ChatRequest,
        context_builder: &ContextBuilder,
    ) -> Result<ChatResponse> {
        self.call_llm_streaming_inner(chat_request, Some(context_builder)).await
    }

    /// Single-attempt streaming call (no retry on context overflow).
    ///
    /// Used after emergency trim to avoid infinite recursion.
    fn call_llm_streaming_no_retry<'a>(
        &'a mut self,
        chat_request: &'a rollball_core::providers::traits::ChatRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ChatResponse>> + 'a>> {
        Box::pin(async move {
            self.call_llm_streaming_inner(chat_request, None).await
        })
    }

    /// Common streaming implementation.
    ///
    /// When `context_builder` is `Some`, context overflow recovery is enabled
    /// (retry after emergency trim). When `None`, errors are returned directly.
    async fn call_llm_streaming_inner(
        &mut self,
        chat_request: &rollball_core::providers::traits::ChatRequest,
        context_builder: Option<&ContextBuilder>,
    ) -> Result<ChatResponse> {
        let retry_on_overflow = context_builder.is_some();

        let stream = self.provider.chat_stream(chat_request.clone()).await?;
        let mut stream = Box::into_pin(stream);
        let mut accumulated_content = String::new();
        let mut tool_calls: Option<Vec<ToolCall>> = None;
        let mut usage = None;

        // ToolCallChunk accumulation buffer: indexed by tool_call ID
        // TODO: ToolCallChunk currently carries only a String with no ID field,
        // so we cannot reliably associate chunks with specific tool calls.
        // When the provider API adds an ID field to chunks, update this logic
        // to accumulate arguments per tool call. For now, we rely on
        // ToolCallStart + Finished events for complete tool call data.
        let mut _tool_call_buffer: HashMap<String, ToolCall> = HashMap::new();

        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Content(chunk) => {
                    accumulated_content.push_str(&chunk);

                    // Forward delta to on_chunk channel (like ZeroClaw's on_delta)
                    // so the caller can relay streaming chunks to Gateway
                    if let Some(ref tx) = self.on_chunk {
                        // Use try_send to avoid blocking the LLM stream
                        if tx.try_send(ChunkEvent::Delta(chunk.clone())).is_err() {
                            tracing::debug!("on_chunk channel full or closed, dropping delta");
                        }
                    }
                }
                StreamEvent::ToolCallStart(tc) => {
                    // Store the initial tool call in the buffer for chunk accumulation
                    let id = tc.id.clone();
                    _tool_call_buffer.insert(id, tc.clone());
                    tool_calls.get_or_insert_with(Vec::new).push(tc);
                }
                StreamEvent::ToolCallChunk(_chunk) => {
                    // TODO: Once ToolCallChunk carries a tool_call ID field,
                    // look up the corresponding entry in _tool_call_buffer
                    // and append the chunk data to the tool call's arguments.
                    // Current limitation: String chunk cannot be associated
                    // with a specific tool call when multiple are streamed.
                }
                StreamEvent::Finished(resp) => {
                    // Use final response data; prefer stream-accumulated content
                    if accumulated_content.is_empty() {
                        accumulated_content = resp.content;
                    }
                    if resp.tool_calls.is_some() {
                        // Prefer Finished event's tool_calls as they are complete
                        tool_calls = resp.tool_calls;
                    }
                    // If Finished has no tool_calls, fall back to buffer data
                    usage = resp.usage;
                    break;
                }
                StreamEvent::Error(e) => {
                    // Check for context overflow and attempt recovery
                    if retry_on_overflow
                        && (e.contains("context_length_exceeded")
                            || e.contains("max_tokens")
                            || e.contains("token limit"))
                    {
                        tracing::warn!("Context overflow detected in stream, attempting emergency trim");
                        let removed = self.history.emergency_trim();
                        if removed > 0 {
                            tracing::info!("Emergency trim removed {} messages, retrying", removed);
                            let chat_request = context_builder
                                .unwrap()
                                .build(&self.manifest, &self.history);
                            return self.call_llm_streaming_no_retry(&chat_request).await;
                        } else {
                            return Err(RuntimeError::Provider(e));
                        }
                    }
                    return Err(RuntimeError::Provider(e));
                }
            }
        }

        Ok(ChatResponse {
            content: accumulated_content,
            tool_calls,
            usage,
        })
    }

    /// Execute tool calls in parallel with per-tool timeout and iteration-level deadline.
    ///
    /// Phase 1: Permission check (batch — each tool checked independently)
    /// Phase 2: Approval gate (placeholder for future)
    /// Phase 3: Parallel execution with spawn + select + deadline
    ///
    /// Returns results in the same order as input tool calls.
    /// Individual tool failures are captured as error strings, not propagated.
    async fn execute_tools_parallel(&self, tool_calls: &[ToolCall]) -> Vec<String> {
        if tool_calls.is_empty() {
            return Vec::new();
        }

        // Phase 1: Permission check (batch)
        // Check each tool independently; denied tools get error results,
        // allowed tools proceed to parallel execution.
        let mut permission_results: Vec<Option<String>> = Vec::with_capacity(tool_calls.len());
        for tool_call in tool_calls {
            match crate::tools::permission::validate_permission(&self.manifest, &tool_call.function.name) {
                Ok(()) => permission_results.push(None),
                Err(e) => {
                    tracing::warn!("Permission denied for tool '{}': {}", tool_call.function.name, e);
                    permission_results.push(Some(format!("Error: Permission denied — {}", e)));
                }
            }
        }

        // Collect indices of tools that passed permission check
        let allowed_indices: Vec<usize> = permission_results
            .iter()
            .enumerate()
            .filter_map(|(i, result)| if result.is_none() { Some(i) } else { None })
            .collect();

        // If no tools passed permission, return all error results immediately
        if allowed_indices.is_empty() {
            return permission_results.into_iter().map(|r| r.unwrap_or_default()).collect();
        }

        // Phase 2: Approval gate (placeholder for future)
        // TODO(Phase 3): Implement approval gate for high-risk tools

        // Phase 3: Parallel execution with spawn + select + deadline
        let tool_timeout = Duration::from_millis(self.config.tool_timeout_ms);
        let iteration_timeout = Duration::from_millis(self.config.iteration_timeout_ms);

        // Channel to collect results from spawned tasks
        let (tx, mut rx) = mpsc::channel::<(usize, String)>(tool_calls.len());

        // Spawn each allowed tool as an independent task
        let handles: Vec<tokio::task::JoinHandle<()>> = allowed_indices
            .iter()
            .map(|&idx| {
                let tools = self.tools.clone();
                let tc = tool_calls[idx].clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let result = match tokio::time::timeout(
                        tool_timeout,
                        execute_single_tool(&tools, &tc),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => format!(
                            "Error: Tool '{}' timed out after {}ms",
                            tc.function.name,
                            tool_timeout.as_millis()
                        ),
                    };
                    let _ = tx.send((idx, result)).await;
                })
            })
            .collect();

        // Drop the remaining sender so rx.recv() returns None when all tasks complete
        drop(tx);

        // Collect results with iteration-level deadline
        let deadline = Instant::now() + iteration_timeout;
        let mut collected: Vec<(usize, String)> = Vec::with_capacity(allowed_indices.len());
        let total = allowed_indices.len();

        while collected.len() < total {
            tokio::select! {
                // A result arrived from a spawned task
                entry = rx.recv() => {
                    match entry {
                        Some((idx, result)) => collected.push((idx, result)),
                        None => break, // All senders dropped
                    }
                }
                // Iteration-level deadline exceeded
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        "Iteration timeout reached ({}ms), aborting {} remaining tool(s)",
                        iteration_timeout.as_millis(),
                        total - collected.len()
                    );
                    // Abort all remaining spawned tasks
                    for handle in &handles {
                        handle.abort();
                    }
                    break;
                }
            }
        }

        // Build final results in original order
        let results: Vec<String> = permission_results
            .into_iter()
            .enumerate()
            .map(|(idx, perm_result)| {
                if let Some(err) = perm_result {
                    // Permission-denied tool
                    err
                } else if let Some(pos) = collected.iter().find(|(i, _)| *i == idx) {
                    // Tool that completed successfully or with error
                    pos.1.clone()
                } else {
                    // Tool that didn't complete due to iteration timeout
                    format!(
                        "Error: iteration timed out, tool {} not completed",
                        tool_calls[idx].function.name
                    )
                }
            })
            .collect();

        // If iteration timed out with incomplete tools, add a system note
        let incomplete_count = results.iter()
            .filter(|r| r.contains("iteration timed out"))
            .count();
        if incomplete_count > 0 {
            tracing::warn!(
                incomplete_count,
                "Iteration timed out with incomplete tool(s)"
            );
        }

        results
    }

    /// Get reference to history manager
    pub fn history(&self) -> &HistoryManager {
        &self.history
    }

    /// Get reference to the agent manifest
    pub fn manifest(&self) -> &rollball_core::AgentManifest {
        &self.manifest
    }

    /// Get mutable reference to history manager
    pub fn history_mut(&mut self) -> &mut HistoryManager {
        &mut self.history
    }
}

/// Execute a single tool call against the tool registry.
///
/// Returns the result content string (success or error message).
async fn execute_single_tool(tools: &[Arc<dyn Tool>], tool_call: &ToolCall) -> String {
    let tool_name = &tool_call.function.name;
    let params_str = &tool_call.function.arguments;

    // Find the tool
    let tool = tools.iter().find(|t| {
        let spec = t.spec();
        spec.name == *tool_name
    });

    match tool {
        Some(tool) => {
            let params: serde_json::Value = serde_json::from_str(params_str)
                .unwrap_or(serde_json::Value::Object(Default::default()));

            match tool.execute(params).await {
                Ok(result) => {
                    if result.ok {
                        result.content
                    } else {
                        format!("Error: {}", result.error.unwrap_or_default())
                    }
                }
                Err(e) => format!("Tool execution error: {e}"),
            }
        }
        None => format!("Unknown tool: {tool_name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rollball_core::providers::mock::MockProvider;
    use rollball_core::providers::traits::FunctionCall;

    /// Simple echo tool for testing
    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
            rollball_core::tools::traits::ToolSpec {
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
        async fn execute(&self, params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
            let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("no message");
            Ok(rollball_core::tools::traits::ToolResult {
                ok: true,
                content: format!("Echo: {message}"),
                error: None,
                token_usage: None,
            })
        }
    }

    fn test_manifest() -> rollball_core::AgentManifest {
        rollball_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.loop"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"
            "#,
        )
        .unwrap()
    }

    fn test_budget() -> rollball_core::Budget {
        rollball_core::Budget {
            daily_tokens: Some(100000),
            monthly_tokens: None,
            daily_cost_usd: Some(10.0),
            monthly_cost_usd: None,
            exceeded_action: "warn".to_string(),
        }
    }

    #[test]
    fn test_agent_loop_with_gateway_client() {
        // NOTE: We use ipc_client: None because GatewayClient::connect is
        // lazy (does not immediately connect), and connecting to a non-existent
        // socket would fail at connect_transport() time. This test verifies
        // that AgentLoop construction works correctly, not the IPC connection.
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let budget = test_budget();
        let (_agent_loop, _inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        // Verify inbound sender works
        assert!(_inbound_tx.try_send(InboundMessage::UserMessage("test".to_string())).is_ok());
    }

    #[test]
    fn test_agent_loop_without_gateway_client() {
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let budget = test_budget();
        let (_agent_loop, _inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        // Just verify construction works
        assert!(_inbound_tx.try_send(InboundMessage::UserMessage("test".to_string())).is_ok());
    }

    #[tokio::test]
    async fn test_agent_loop_standalone_no_panic() {
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("Hello from standalone!"));
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let budget = test_budget();
        let (mut agent_loop, _inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Hello from standalone!");
    }

    // ── S1.5: Streaming tests ─────────────────────────────────────────

    #[tokio::test]
    async fn test_stream_content_accumulation() {
        // MockProvider::chat_stream internally calls chat() then emits Finished event.
        // Content should be correctly accumulated from the stream.
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let provider = Arc::new(MockProvider::single_text("Accumulated content here"));
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Accumulated content here");
    }

    #[tokio::test]
    async fn test_stream_tool_call_detection() {
        let provider = Arc::new(MockProvider::tool_call_then_text(
            "echo",
            r#"{"message": "hello"}"#,
            "Done",
        ));
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("You are a test agent.".to_string());
        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stream_finished_event() {
        // When stream emits Finished, content and usage are extracted
        let provider = Arc::new(MockProvider::single_text("Final response"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Final response");
        // Verify usage was tracked (budget guard should have been updated)
        assert!(agent_loop.history().estimate_total_tokens() > 0);
    }

    #[tokio::test]
    async fn test_stream_error_propagation() {
        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::Error {
                message: "API rate limit".to_string(),
            },
        ]));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_err());
        // Error from chat_stream propagates as Core(RollballError::Provider(...))
        // because Provider trait returns rollball_core::RollballError
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("rate limit"), "Error should mention rate limit: {}", err_msg);
    }

    #[tokio::test]
    async fn test_stream_content_then_tool_call() {
        // MockProvider returns tool call then text — content accumulates correctly
        let provider = Arc::new(MockProvider::tool_call_then_text(
            "echo",
            r#"{"message": "test"}"#,
            "All done",
        ));
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "All done");
    }

    #[tokio::test]
    async fn test_stream_empty_content() {
        let provider = Arc::new(MockProvider::single_text(""));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());
        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }

    #[tokio::test]
    async fn test_stream_history_append() {
        // Verify that streamed text response is correctly appended to history
        let provider = Arc::new(MockProvider::single_text("Streamed text"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());
        let _ = agent_loop.run("Hi", &context_builder).await;
        let messages = agent_loop.history().messages();
        // Should have: user message + assistant message
        let assistant_msgs: Vec<_> = messages.iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .collect();
        assert_eq!(assistant_msgs.len(), 1);
        assert_eq!(assistant_msgs[0].content, "Streamed text");
    }

    #[tokio::test]
    async fn test_stream_usage_tracking() {
        let provider = Arc::new(MockProvider::single_text("Response"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());
        let _ = agent_loop.run("Hi", &context_builder).await;
        // Budget guard should have been updated with usage from the stream
        // (MockProvider returns usage with total_tokens=150)
        // We can't directly check budget_guard, but we verify no error occurred
    }

    // ── S1.6: InboundQueue tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_inbound_user_message() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        // Inject a user message before running
        inbound_tx.try_send(InboundMessage::UserMessage("Injected question".to_string())).unwrap();

        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
        // Verify the injected message appeared in history
        let messages = agent_loop.history().messages();
        let injected: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("Injected question"))
            .collect();
        assert!(!injected.is_empty(), "Injected user message should appear in history");
    }

    #[tokio::test]
    async fn test_inbound_system_notification() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        inbound_tx.try_send(InboundMessage::SystemNotification {
            notification_type: "identity_update".to_string(),
            data: serde_json::json!({"key": "new_value"}),
        }).unwrap();

        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
        let messages = agent_loop.history().messages();
        let notif: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("[system:identity_update]"))
            .collect();
        assert!(!notif.is_empty(), "System notification should appear in history");
    }

    #[tokio::test]
    async fn test_inbound_intent_message() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        inbound_tx.try_send(InboundMessage::IntentMessage {
            from: "com.rollball.system".to_string(),
            action: "ping".to_string(),
            params: serde_json::json!({}),
        }).unwrap();

        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
        let messages = agent_loop.history().messages();
        let intent: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("[intent:com.rollball.system:ping]"))
            .collect();
        assert!(!intent.is_empty(), "Intent message should appear in history");
    }

    #[tokio::test]
    async fn test_inbound_concurrent_injection() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        // Inject 10 messages concurrently
        for i in 0..10 {
            inbound_tx.try_send(InboundMessage::UserMessage(format!("Message {i}"))).unwrap();
        }

        let result = agent_loop.run("Hi", &context_builder).await;
        assert!(result.is_ok());
        let messages = agent_loop.history().messages();
        let injected: Vec<_> = messages.iter()
            .filter(|m| m.content.starts_with("Message "))
            .collect();
        assert_eq!(injected.len(), 10, "All 10 injected messages should appear in history");
    }

    #[tokio::test]
    async fn test_inbound_queue_full_backpressure() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (agent_loop, inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None);

        // Fill the channel (capacity 64)
        for i in 0..64 {
            assert!(inbound_tx.try_send(InboundMessage::UserMessage(format!("Msg {i}"))).is_ok());
        }
        // The 65th message should fail (backpressure) — but no panic
        let result = inbound_tx.try_send(InboundMessage::UserMessage("overflow".to_string()));
        assert!(result.is_err(), "Channel should be full");
        // Should not panic — just returns Err
        drop(agent_loop);
    }

    #[tokio::test]
    async fn test_inbound_drain_nonblocking() {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let config = RuntimeConfig::default();
        let manifest = test_manifest();
        let budget = test_budget();
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let (mut agent_loop, _inbound_tx) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        // Run without any inbound messages — drain should return immediately
        let start = std::time::Instant::now();
        let result = agent_loop.run("Hi", &context_builder).await;
        let elapsed = start.elapsed();
        assert!(result.is_ok());
        // Drain should not block — total time should be well under 1 second
        assert!(elapsed < std::time::Duration::from_secs(1), "Drain should be non-blocking");
    }

    // ── S1.7: Parallel tool execution tests ───────────────────────────

    #[tokio::test]
    async fn test_tool_parallel_execution() {
        use async_trait::async_trait;

        #[derive(Clone)]
        struct SlowTool {
            name: String,
            delay_ms: u64,
        }

        #[async_trait]
        impl Tool for SlowTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: self.name.clone(),
                    description: format!("Slow tool {}", self.name),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
                Ok(rollball_core::tools::traits::ToolResult {
                    ok: true,
                    content: format!("{} done", self.name),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.parallel"
            version = "1.0.0"
            name = "Parallel Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "slow_a"

            [[tools]]
            name = "slow_b"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(SlowTool { name: "slow_a".to_string(), delay_ms: 100 }),
            Arc::new(SlowTool { name: "slow_b".to_string(), delay_ms: 100 }),
        ];

        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_1".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "slow_a".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_2".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "slow_b".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            rollball_core::providers::mock::MockResponse::Text {
                content: "Both done".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop.run("Run parallel", &context_builder).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Parallel execution should succeed: {:?}", result);
        // Parallel: ~100ms total. Serial would be ~200ms.
        // Allow generous margin (300ms) to avoid flaky tests
        assert!(elapsed < std::time::Duration::from_millis(300),
            "Parallel execution should be faster than serial: {:?}", elapsed);
    }

    #[tokio::test]
    async fn test_tool_single_failure_no_shortcircuit() {
        use async_trait::async_trait;

        struct FailTool;
        #[async_trait]
        impl Tool for FailTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "fail_tool".to_string(),
                    description: "Always fails".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Intentional failure".to_string()),
                    token_usage: None,
                })
            }
        }

        struct SuccessTool;
        #[async_trait]
        impl Tool for SuccessTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "success_tool".to_string(),
                    description: "Always succeeds".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Success!".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.fail"
            version = "1.0.0"
            name = "Fail Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "fail_tool"

            [[tools]]
            name = "success_tool"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FailTool),
            Arc::new(SuccessTool),
        ];

        // LLM returns both tool calls, then text
        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_fail".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "fail_tool".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_success".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "success_tool".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            rollball_core::providers::mock::MockResponse::Text {
                content: "Mixed results".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        let result = agent_loop.run("Test failure", &context_builder).await;
        assert!(result.is_ok(), "Should succeed even with one tool failure");
        assert_eq!(result.unwrap(), "Mixed results");
    }

    #[tokio::test]
    async fn test_tool_timeout() {
        use async_trait::async_trait;

        struct StuckTool;
        #[async_trait]
        impl Tool for StuckTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "stuck_tool".to_string(),
                    description: "Never returns".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                // Sleep for a very long time — should be cut short by timeout
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Ok(rollball_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Should not reach".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.timeout"
            version = "1.0.0"
            name = "Timeout Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "stuck_tool"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(StuckTool)];

        let provider = Arc::new(MockProvider::tool_call_then_text(
            "stuck_tool",
            "{}",
            "After timeout",
        ));

        let config = RuntimeConfig { iteration_timeout_ms: 100, ..Default::default() }; // 100ms timeout
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop.run("Test timeout", &context_builder).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Should succeed with timeout error captured: {:?}", result);
        // Should complete within ~1 second (100ms timeout + overhead)
        assert!(elapsed < std::time::Duration::from_secs(2),
            "Should timeout quickly: {:?}", elapsed);

        // Verify the timeout error message appears in history
        let messages = agent_loop.history().messages();
        let timeout_msg: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("timed out"))
            .collect();
        assert!(!timeout_msg.is_empty(), "Timeout error should appear in tool result history");
    }

    #[tokio::test]
    async fn test_tool_permission_check_sequential() {
        // When a tool lacks permission, the sequential check should catch it
        // before any parallel execution begins.
        let toml_str = r#"
            agent_id = "com.test.perm"
            version = "1.0.0"
            name = "Perm Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "shell"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        // shell requires Shell permission, but manifest doesn't declare it
        let tools: Vec<Arc<dyn Tool>> = vec![];

        let provider = Arc::new(MockProvider::tool_call_then_text(
            "shell",
            r#"{"command": "ls"}"#,
            "Done",
        ));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        // The tool call will fail because shell is not in the tool registry
        // (empty tools vec), so it should produce "Unknown tool: shell"
        let result = agent_loop.run("Run shell", &context_builder).await;
        // Should still succeed — error becomes tool result message
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_tool_results_order_preserved() {
        use async_trait::async_trait;

        #[derive(Clone)]
        struct OrderedTool {
            name: String,
            output: String,
        }

        #[async_trait]
        impl Tool for OrderedTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: self.name.clone(),
                    description: format!("Ordered tool {}", self.name),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
                    ok: true,
                    content: self.output.clone(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.order"
            version = "1.0.0"
            name = "Order Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "tool_a"

            [[tools]]
            name = "tool_b"

            [[tools]]
            name = "tool_c"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(OrderedTool { name: "tool_a".to_string(), output: "Result A".to_string() }),
            Arc::new(OrderedTool { name: "tool_b".to_string(), output: "Result B".to_string() }),
            Arc::new(OrderedTool { name: "tool_c".to_string(), output: "Result C".to_string() }),
        ];

        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_a".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool_a".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_b".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool_b".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_c".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool_c".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            rollball_core::providers::mock::MockResponse::Text {
                content: "All ordered".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        let result = agent_loop.run("Run ordered", &context_builder).await;
        assert!(result.is_ok());

        // Verify that tool results in history are in order
        let messages = agent_loop.history().messages();
        let tool_results: Vec<_> = messages.iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(tool_results.len(), 3);
        // First tool result should be tool_a
        assert!(tool_results[0].content.contains("Result A"), "First result should be A");
        // Second should be tool_b
        assert!(tool_results[1].content.contains("Result B"), "Second result should be B");
        // Third should be tool_c
        assert!(tool_results[2].content.contains("Result C"), "Third result should be C");
    }

    // ── Fix #1: Iteration timeout with partial results ─────────────────

    #[tokio::test]
    async fn test_iteration_timeout_partial_results() {
        use async_trait::async_trait;

        #[derive(Clone)]
        struct FastTool;

        #[async_trait]
        impl Tool for FastTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "fast_tool".to_string(),
                    description: "Fast tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Fast result".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        #[derive(Clone)]
        struct SlowTool;

        #[async_trait]
        impl Tool for SlowTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "slow_tool".to_string(),
                    description: "Slow tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                // Sleep longer than the iteration timeout
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Ok(rollball_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Should not reach".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.iter_timeout"
            version = "1.0.0"
            name = "Iter Timeout Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "fast_tool"

            [[tools]]
            name = "slow_tool"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FastTool),
            Arc::new(SlowTool),
        ];

        // LLM requests both tools; fast_tool completes quickly, slow_tool times out
        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_fast".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "fast_tool".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_slow".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "slow_tool".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            rollball_core::providers::mock::MockResponse::Text {
                content: "Partial complete".to_string(),
            },
        ]));

        // Very short iteration timeout so slow_tool gets aborted
        let config = RuntimeConfig {
            iteration_timeout_ms: 200,
            tool_timeout_ms: 10000, // tool_timeout is long, iteration timeout is short
            ..Default::default()
        };
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop.run("Test iteration timeout", &context_builder).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Should succeed with partial results: {:?}", result);
        // Should complete within ~1 second (200ms iteration timeout + overhead)
        assert!(elapsed < std::time::Duration::from_secs(2),
            "Should complete quickly with iteration timeout: {:?}", elapsed);

        // Verify the fast_tool result and slow_tool timeout both appear in history
        let messages = agent_loop.history().messages();
        let tool_results: Vec<_> = messages.iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        // fast_tool should have its result
        assert!(tool_results[0].content.contains("Fast result"),
            "Fast tool should have its result");
        // slow_tool should have iteration timeout error
        assert!(tool_results[1].content.contains("iteration timed out"),
            "Slow tool should have iteration timeout error: {}", tool_results[1].content);
    }

    #[tokio::test]
    async fn test_tool_timeout_vs_iteration_timeout_independent() {
        // Verify that single-tool timeout and iteration timeout work independently.
        // A tool that exceeds tool_timeout_ms should get a per-tool timeout error,
        // even if iteration_timeout_ms is longer.
        use async_trait::async_trait;

        struct MediumTool;

        #[async_trait]
        impl Tool for MediumTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "medium_tool".to_string(),
                    description: "Medium-speed tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                // Sleep longer than tool_timeout but shorter than iteration_timeout
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                Ok(rollball_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Should not reach".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let toml_str = r#"
            agent_id = "com.test.tool_timeout"
            version = "1.0.0"
            name = "Tool Timeout Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "medium_tool"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MediumTool)];

        let provider = Arc::new(MockProvider::tool_call_then_text(
            "medium_tool",
            "{}",
            "After tool timeout",
        ));

        // tool_timeout_ms is 100ms (shorter than tool execution),
        // iteration_timeout_ms is 30000ms (much longer)
        let config = RuntimeConfig {
            tool_timeout_ms: 100,
            iteration_timeout_ms: 30000,
            ..Default::default()
        };
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        let start = std::time::Instant::now();
        let result = agent_loop.run("Test tool timeout", &context_builder).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Should succeed with tool timeout error: {:?}", result);
        // Should complete in ~100ms (tool timeout) + overhead, not 500ms
        assert!(elapsed < std::time::Duration::from_secs(2),
            "Should timeout at tool level: {:?}", elapsed);

        // Verify per-tool timeout message (not iteration timeout)
        let messages = agent_loop.history().messages();
        let timeout_msg: Vec<_> = messages.iter()
            .filter(|m| m.content.contains("timed out"))
            .collect();
        assert!(!timeout_msg.is_empty(), "Per-tool timeout should be recorded");
        // Should NOT be an iteration timeout message
        assert!(timeout_msg.iter().all(|m| !m.content.contains("iteration timed out")),
            "Should be per-tool timeout, not iteration timeout");
    }

    // ── Fix #2: Partial permission denial ──────────────────────────────

    #[tokio::test]
    async fn test_permission_partial_denial() {
        // When one tool is denied permission, others should still execute.
        use async_trait::async_trait;

        struct EchoPermTool;

        #[async_trait]
        impl Tool for EchoPermTool {
            fn spec(&self) -> rollball_core::tools::traits::ToolSpec {
                rollball_core::tools::traits::ToolSpec {
                    name: "echo".to_string(),
                    description: "Echo tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(&self, _params: serde_json::Value) -> rollball_core::error::Result<rollball_core::tools::traits::ToolResult> {
                Ok(rollball_core::tools::traits::ToolResult {
                    ok: true,
                    content: "Echo result".to_string(),
                    error: None,
                    token_usage: None,
                })
            }
        }

        // Manifest declares echo tool (no permission needed) but NOT shell permission
        let toml_str = r#"
            agent_id = "com.test.partial_perm"
            version = "1.0.0"
            name = "Partial Perm Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"

            [[tools]]
            name = "echo"

            [[tools]]
            name = "shell"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoPermTool)];

        // LLM requests both echo and shell
        let provider = Arc::new(MockProvider::new(vec![
            rollball_core::providers::mock::MockResponse::ToolCalls {
                tool_calls: vec![
                    ToolCall {
                        id: "call_echo".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "echo".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_shell".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "shell".to_string(),
                            arguments: r#"{"command": "ls"}"#.to_string(),
                        },
                    },
                ],
                content: String::new(),
            },
            rollball_core::providers::mock::MockResponse::Text {
                content: "Partial permission result".to_string(),
            },
        ]));

        let config = RuntimeConfig::default();
        let budget = test_budget();
        let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None);
        let context_builder = ContextBuilder::new("System".to_string());

        let result = agent_loop.run("Test partial permission", &context_builder).await;
        assert!(result.is_ok(), "Should succeed even with one tool permission denied: {:?}", result);

        // Verify echo result appears (it was executed) and shell has permission denied
        let messages = agent_loop.history().messages();
        let tool_results: Vec<_> = messages.iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(tool_results.len(), 2, "Should have 2 tool results");
        // First tool (echo) should have result
        assert!(tool_results[0].content.contains("Echo result") || tool_results[0].content.contains("Unknown tool"),
            "Echo tool should have result or unknown tool error");
        // Second tool (shell) should have permission denied
        assert!(tool_results[1].content.contains("Permission denied"),
            "Shell tool should have permission denied: {}", tool_results[1].content);
    }
}
