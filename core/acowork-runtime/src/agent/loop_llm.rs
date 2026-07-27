//! LLM streaming module
//!
//! Extracted from [`super::loop_`] to provide reusable LLM streaming logic
//! shared between production [`AgentLoop::run`] and debug `DebugSessionTask`.
//!
//! Handles:
//! - Stream processing (Content, ReasoningContent, ToolCallStart, ToolCallChunk, etc.)
//! - Context overflow recovery (emergency trim + retry)
//! - Interrupt handling during streaming
//! - Tool call argument accumulation and dedup

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use acowork_core::providers::traits::{ChatResponse, StreamEvent, ToolCall};
use chrono::Utc;
use futures::StreamExt;

use super::context::ContextBuilder;
use super::loop_::{AgentLoop, ChunkEvent, ControlDecision};
use crate::cancellation::select_on_cancel;
use crate::error::{Result, RuntimeError};

impl AgentLoop {
    /// Call LLM with streaming, accumulating content and tool calls.
    ///
    /// Handles context overflow recovery by detecting relevant errors
    /// from the stream and retrying after emergency trim.
    pub(crate) async fn call_llm_streaming(
        &mut self,
        chat_request: &acowork_core::providers::traits::ChatRequest,
        context_builder: &ContextBuilder,
    ) -> Result<ChatResponse> {
        self.call_llm_streaming_inner(chat_request, Some(context_builder))
            .await
    }

    /// Single-attempt streaming call (no retry on context overflow).
    ///
    /// Used after emergency trim to avoid infinite recursion.
    pub(crate) fn call_llm_streaming_no_retry<'a>(
        &'a mut self,
        chat_request: &'a acowork_core::providers::traits::ChatRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ChatResponse>> + Send + 'a>>
    {
        Box::pin(async move { self.call_llm_streaming_inner(chat_request, None).await })
    }

    /// Common streaming implementation.
    ///
    /// When `context_builder` is `Some`, context overflow recovery is enabled
    /// (retry after emergency trim). When `None`, errors are returned directly.
    pub(crate) async fn call_llm_streaming_inner(
        &mut self,
        chat_request: &acowork_core::providers::traits::ChatRequest,
        context_builder: Option<&ContextBuilder>,
    ) -> Result<ChatResponse> {
        let retry_on_overflow = context_builder.is_some();

        tracing::debug!(
            system_prompt_len = chat_request
                .messages
                .first()
                .map(|m| m.content.len())
                .unwrap_or(0),
            tools_count = chat_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
            messages_count = chat_request.messages.len(),
            "Sending LLM request"
        );
        // ADR-021: Frontend polls via HTTP — ReasoningStarted is no longer
        // sent via channel. The frontend detects streaming state via poll.
        //
        // ADR-044 Phase 3 (TTFT stop bug fix, ADR §1.3 / §4.4): wrap the
        // `chat_stream()` future in `select_on_cancel` so a Stop signal that
        // arrives *during* TCP/TLS/HTTP-headers/SSE-first-chunk — any of
        // which can take 10–30s — drops the underlying `reqwest::send()`
        // future within microseconds. Without this wrapper, the bare
        // `.await` is not contained in any `tokio::select!` so neither the
        // 500ms idle poll nor a `Notify` can wake it; the user-perceived
        // stop latency equals the upstream TTFT.
        //
        // `select_on_cancel` races the future against the session's
        // `CancelHandle`. On cancel, the `chat_stream` future is
        // dropped (HTTP request aborted), we receive `Ok(None)` and return
        // a stopped ChatResponse so the upstream `loop_::run_inner` sees the
        // same shape it would after a mid-stream Stop. The chunk channel
        // also receives `ChunkEvent::Stopped { content: "" }` so the UI
        // can clear its streaming indicator immediately.
        //
        // ADR-044 §4.5: the handle here is the *current request's* handle
        // — `run_inner` called `begin_new_request()` before entering this
        // method, so a Cancel from a prior request cannot short-circuit
        // this run.
        let cancel_handle = self.session_core.cancel_handle();
        let provider_stream = select_on_cancel(
            cancel_handle.clone(),
            self.core.provider.chat_stream(chat_request.clone()),
        )
        .await?;
        let mut stream = match provider_stream {
            Some(s) => Box::into_pin(s),
            None => {
                tracing::info!(
                    "chat_stream() cancelled by user before first chunk arrived — \
                     aborting TTFT wait"
                );
                // Flush any half-written streaming line and emit a Stopped
                // chunk event so the UI drops its streaming indicator even
                // though no content was accumulated. Returns a stopped
                // ChatResponse so the upstream loop sees the same control
                // flow shape it would after a mid-stream Stop.
                self.session_core
                    .flush_streaming_line(self.session.conversation.as_deref());
                let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                    content: String::new(),
                });
                return Ok(build_stopped_response(String::new(), String::new()));
            }
        };
        let mut accumulated_content = String::new();
        let mut accumulated_reasoning_content = String::new();
        let mut tool_calls: Option<Vec<ToolCall>> = None;
        let mut usage = None;
        let mut finish_reason: Option<String> = None;
        let mut reasoning_started_at: Option<i64> = None;
        let mut reasoning_finished_at: Option<i64> = None;
        let mut reasoning_in_progress = false;

        // ADR-022: No think tag state machine needed here.
        // The provider layer (openai.rs ThinkTagParser) already splits
        // delta.content containing <!think> tags into ReasoningContent +
        // Content events. This loop only needs to handle the structural
        // events and flush the streaming line on role transitions.
        //
        // Reset the streaming flush counter at the start of each stream.
        // handle_text_response / prepare_tool_calls check this counter
        // to decide whether to skip legacy persistence paths.
        self.session_core.reset_streaming_flush_count();

        // ToolCallChunk accumulation buffer: indexed by tool_call sequential index
        let mut tool_call_args_buffer: HashMap<u64, String> = HashMap::new();
        // Track which tool_call indices have accumulated valid JSON so far.
        // Once complete JSON is formed, any further delta chunks for that index
        // are stale duplicates (observed with some OpenAI-compatible APIs) and
        // must be discarded to avoid corrupting the arguments.
        let mut finished_tool_indices: HashSet<u64> = HashSet::new();

        // ── Stream processing loop with periodic stop polling ──
        // Use tokio::select! to check for user stops during stream idle
        // periods (e.g., long LLM reasoning between chunks). Without this,
        // stream.next().await can block for tens of seconds without responding
        // to STOP signals, because poll_stop() would only run between
        // received chunks.
        //
        // When the stream actively sends data, the event branch wins immediately
        // (no 500ms latency). When idle, the sleep branch fires every 500ms.
        loop {
            // ADR-044 §4.5: bind the *current request's* handle to a
            // local so the Arc inside it outlives the `cancelled()` future.
            // Re-read on every iteration so any slot swap that happened
            // mid-stream is observed (defensive — `begin_new_request` is
            // only called from `run_inner` entry, so within one stream
            // consumption the slot is stable in practice).
            let cancel_handle = self.session_core.cancel_handle();
            tokio::select! {
                event = stream.next() => {
                    match event {
                        Some(event) => {
                            // Check for control signals before processing each stream event
                            match self.poll_control() {
                                ControlDecision::Stop => {
                                    tracing::info!("LLM stream stopped by user — aborting");
                                    // ADR-021: Flush partial content to JSONL before stopping
                                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                                    let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                                        content: accumulated_content.clone(),
                                    });
                                    return Ok(build_stopped_response(
                                        accumulated_content,
                                        accumulated_reasoning_content,
                                    ));
                                }
                                ControlDecision::Pause => {
                                    tracing::info!("LLM stream paused by debug — aborting");
                                    self.pending_interrupt = Some(ControlDecision::Pause);
                                    // ADR-021: Flush partial content to JSONL before pausing
                                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                                    let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                                        content: accumulated_content.clone(),
                                    });
                                    return Ok(build_stopped_response(
                                        accumulated_content,
                                        accumulated_reasoning_content,
                                    ));
                                }
                                ControlDecision::Continue => {}
                            }
                            match event {
                StreamEvent::Content(chunk) => {
                    // Mark reasoning finished when content starts after reasoning
                    if reasoning_in_progress {
                        reasoning_finished_at = Some(Utc::now().timestamp_millis());
                        reasoning_in_progress = false;
                    }
                    accumulated_content.push_str(&chunk);

                    // ADR-022: Role transition. If we were in a thought
                    // streaming line, flush it to JSONL and start a new
                    // assistant line. The provider layer already split think
                    // tags into separate ReasoningContent events, so a Content
                    // event here means we are in assistant mode.
                    self.session_core.flush_and_new_streaming_line(
                        "assistant", self.session.conversation.as_deref(),
                    );
                    self.session_core.append_streaming_delta("assistant", &chunk);
                    self.session_core.notify_new_data_available();
                }
                StreamEvent::ReasoningContent(chunk) => {
                    // Record start of reasoning on first chunk
                    if reasoning_started_at.is_none() {
                        reasoning_started_at = Some(Utc::now().timestamp_millis());
                    }
                    reasoning_in_progress = true;
                    accumulated_reasoning_content.push_str(&chunk);

                    // ADR-022: Role transition. If we were in an assistant
                    // streaming line, flush it to JSONL and start a new
                    // thought line.
                    self.session_core.flush_and_new_streaming_line(
                        "thought", self.session.conversation.as_deref(),
                    );
                    self.session_core.append_streaming_delta("thought", &chunk);
                    self.session_core.notify_new_data_available();
                }
                StreamEvent::ToolCallStart(tc) => {
                    // Mark reasoning finished when tool calls start after reasoning
                    if reasoning_in_progress {
                        reasoning_finished_at = Some(Utc::now().timestamp_millis());
                        reasoning_in_progress = false;
                    }

                    // ADR-021: Do NOT flush streaming line on ToolCallStart.
                    // The thought content is properly persisted with metadata
                    // via persist_think_to_conversation in prepare_tool_calls().
                    // Flushing here caused duplicate thought records (one
                    // without metadata from flush, one with metadata from persist).

                    tracing::info!(
                        tool_name = %tc.function.name,
                        tool_id = %tc.id,
                        initial_args = %tc.function.arguments,
                        "ToolCallStart received"
                    );
                    tool_calls.get_or_insert_with(Vec::new).push(tc);
                    // ADR-021: Notify frontend that a tool call has started.
                    // Without this, the frontend has no way to know that the LLM
                    // emitted tool calls until the entire agent loop iteration
                    // completes — leaving the UI blank while tools execute.
                    // (Text/reasoning chunks already notify during streaming;
                    //  this fills the gap for tool-call-only responses.)
                    self.session_core.notify_new_data_available();
                }
                StreamEvent::ToolCallChunk { index, arguments } => {
                    tracing::debug!(index, chunk_len = arguments.len(), "ToolCallChunk received");
                    // Discard stale delta chunks for tool calls that already have complete JSON
                    if !finished_tool_indices.contains(&index) {
                        let buffer = tool_call_args_buffer.entry(index).or_default();
                        buffer.push_str(&arguments);
                        // Check if accumulated arguments now form valid JSON
                        if serde_json::from_str::<serde_json::Value>(buffer).is_ok() {
                            finished_tool_indices.insert(index);
                        }
                    }
                }
                StreamEvent::Finished(resp) => {
                    // Mark reasoning finished on stream end (edge case: no Content/ToolCall after reasoning)
                    if reasoning_in_progress {
                        reasoning_finished_at = Some(Utc::now().timestamp_millis());
                    }
                    // Capture finish_reason for diagnostics
                    if resp.finish_reason.is_some() {
                        finish_reason = resp.finish_reason;
                    }
                    // Use final response data; prefer stream-accumulated content
                    if accumulated_content.is_empty() {
                        accumulated_content = resp.content;
                    }
                    if accumulated_reasoning_content.is_empty() {
                        accumulated_reasoning_content =
                            resp.reasoning_content.unwrap_or_default();
                    }
                    if resp.tool_calls.is_some() {
                        // Prefer Finished event's tool_calls as they are complete
                        tool_calls = resp.tool_calls;
                    } else if tool_calls.is_some() {
                        // Finished has no tool_calls — apply accumulated argument chunks
                        // from the stream to the ToolCallStart entries.
                        // When ToolCallStart already carries initial arguments
                        // (e.g. GLM/DeepSeek send name+args together), do NOT
                        // append buffer content — they are already complete.
                        if let Some(ref mut tcs) = tool_calls {
                            for (i, tc) in tcs.iter_mut().enumerate() {
                                if let Some(args) =
                                    tool_call_args_buffer.get(&(i as u64))
                                    && (tc.function.arguments.is_empty()
                                        || tc.function.arguments == "{}")
                                {
                                    // Validate JSON before applying — stream interruption can
                                    // leave incomplete arguments that would fail at tool execution.
                                    if serde_json::from_str::<serde_json::Value>(args).is_ok() {
                                        tc.function.arguments = args.clone();
                                    } else {
                                        tracing::error!(
                                            tool_name = %tc.function.name,
                                            index = i,
                                            raw_len = args.len(),
                                            raw_preview = %&args[..args.len().min(200)],
                                            "Accumulated tool call arguments are not valid JSON"
                                        );
                                        tc.function.arguments =
                                            make_incomplete_marker(&tc.function.name, args.len());
                                    }
                                }
                                // If arguments already non-empty (GLM/DeepSeek same-chunk pattern),
                                // they are already complete — do not append buffer content.
                                // DeepSeek sends duplicate complete arguments in subsequent chunks,
                                // appending would produce invalid JSON like {"path": "."}{"path": "."}
                            }
                        }
                    }
                    usage = resp.usage;

                    // Stream end semantics: deliver the trailing partial line
                    // (if any) as a final stream_delta, then freeze the
                    // streaming line as record_complete. Both events are
                    // needed for the frontend to render the complete reply:
                    //
                    //   - `force_flush_stream_delta` bypasses the 500ms
                    //     notify throttle that would otherwise drop short,
                    //     fast streams before record_complete lands. It
                    //     pushes the trailing partial line (no terminating
                    //     '\n') as a final stream_delta line so the last
                    //     line of the reply is not held back forever.
                    //   - `flush_streaming_line` writes the JSONL record
                    //     and emits the terminal `record_complete` event so
                    //     the frontend freezes its active stream buffer
                    //     into `messages[]`.
                    //
                    // The downstream `handle_text_response` will take Path 1
                    // (streaming_flush_count > 0), where its own call to
                    // `flush_streaming_line` is a no-op because
                    // `streaming_lines` is already empty — no duplicate
                    // JSONL write and no duplicate record_complete push.
                    self.session_core.force_flush_stream_delta();
                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());

                    // Diagnostic: log stream completion summary
                    tracing::info!(
                        finish_reason = ?finish_reason,
                        content_len = accumulated_content.len(),
                        reasoning_len = accumulated_reasoning_content.len(),
                        has_tool_calls = tool_calls.is_some(),
                        tool_call_count = tool_calls.as_ref().map(|t| t.len()).unwrap_or(0),
                        "LLM stream Finished event received"
                    );
                    break;
                }
                StreamEvent::Error(e) => {
                    // ADR-021: Flush partial content to JSONL on error
                    // so the user can see what the AI was trying to say
                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());

                    // Check for context overflow and attempt recovery.
                    // Use structured error_type instead of string matching.
                    if retry_on_overflow
                        && e.error_type == acowork_core::providers::traits::ProviderErrorType::ContextOverflow
                    {
                        tracing::warn!(
                            error = %e.message,
                            current_tokens = self.session.history.token_count(),
                            "Context overflow detected in stream, attempting emergency trim"
                        );
                        let removed = self.session.history.emergency_trim();
                        if removed > 0 {
                            tracing::info!(
                                removed,
                                remaining_tokens = self.session.history.token_count(),
                                "Emergency trim completed, retrying with trimmed context"
                            );
                            let model_name = self.resolve_current_model(context_builder);
                            let caps = self.get_model_capabilities(&model_name);
                            let max_output_limit = self.core.max_output_tokens_limit_for_model(&model_name);
                            let mut chat_request = context_builder.unwrap().build(
                                &self.core.manifest,
                                &self.session.history,
                                caps.as_ref(),
                                max_output_limit,
                            );
                            // Preserve reasoning_effort from the original request
                            // (already resolved in build_chat_request()).
                            chat_request.reasoning_effort =
                                self.last_reasoning_effort.clone();
                            chat_request.thinking_mode =
                                self.last_thinking_mode.clone();
                            return self
                                .call_llm_streaming_no_retry(&chat_request)
                                .await;
                        } else {
                            return Err(RuntimeError::StreamError(e));
                        }
                    }
                    return Err(RuntimeError::StreamError(e));
                }
            }
                        }
                        None => {
                            // Stream ended without a Finished event
                            // (common with OpenAI-compatible APIs like MiniMax).
                            // ADR-022: No scratch buffer to flush — the provider
                            // layer's ThinkTagParser already handled tag splitting,
                            // and each Content/ReasoningContent event was flushed
                            // to JSONL via flush_and_new_streaming_line.
                            break;
                        }
                    }
                }
                // Cancellation handle — fires when external sources (MQTT
                // `StopGeneration` dispatcher in `startup/gateway_loop.rs`,
                // debug server Stop, CLI cancel, test harness) call
                // `cancel()` on the session's `CancelHandle`.
                //
                // ADR-044 Phase 3: replaces the legacy `urgent_stop` Notify,
                // which was only triggered in the debug path. The handle's
                // level-triggered `Notify` + persistent `AtomicU8` state
                // means this branch wakes within microseconds of `cancel()`
                // and remains armed even if `cancel()` lands between select!
                // iterations (a race the legacy Notify could miss).
                //
                // ADR-044 §4.5: this is the *current request's* handle —
                // see `SessionCore::begin_new_request` for how the slot is
                // swapped at every `run_inner` entry.
                _ = cancel_handle.cancelled() => {
                    match self.poll_control() {
                        ControlDecision::Stop => {
                            tracing::info!("LLM stream stopped via cancellation token — aborting");
                        }
                        ControlDecision::Pause => {
                            tracing::info!("LLM stream paused via cancellation token — aborting");
                            self.pending_interrupt = Some(ControlDecision::Pause);
                        }
                        ControlDecision::Continue => {}
                    }
                    // ADR-021: Flush partial content to JSONL before stopping
                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                    let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                        content: accumulated_content.clone(),
                    });
                    return Ok(build_stopped_response(
                        accumulated_content,
                        accumulated_reasoning_content,
                    ));
                }
                // Periodic control polling during stream idle periods.
                // tokio::select! polls ALL branches simultaneously:
                // - When stream has data ready: event branch wins immediately, sleep is dropped
                // - When stream is idle (waiting for next chunk): sleep fires every 500ms
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    match self.poll_control() {
                        ControlDecision::Stop => {
                            tracing::info!(
                                "LLM stream stopped by user during idle period — aborting"
                            );
                            // ADR-021: Flush partial content to JSONL before stopping
                            self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                            let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                                content: accumulated_content.clone(),
                            });
                            return Ok(build_stopped_response(
                                accumulated_content,
                                accumulated_reasoning_content,
                            ));
                        }
                        ControlDecision::Pause => {
                            tracing::info!("LLM stream paused during idle period — aborting");
                            self.pending_interrupt = Some(ControlDecision::Pause);
                            // ADR-021: Flush partial content to JSONL before pausing
                            self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                            let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                                content: accumulated_content.clone(),
                            });
                            return Ok(build_stopped_response(
                                accumulated_content,
                                accumulated_reasoning_content,
                            ));
                        }
                        ControlDecision::Continue => {}
                    }
                }
            }
        }

        // Post-stream: Apply accumulated argument chunks to tool calls.
        // This handles the case where the OpenAI SSE stream ends without
        // a Finished event (common with OpenAI-compatible APIs like MiniMax).
        // When ToolCallStart already carries initial arguments from the same
        // SSE chunk (e.g. GLM, DeepSeek), do NOT append buffer content —
        // they are already complete.
        if tool_calls.is_some()
            && !tool_call_args_buffer.is_empty()
            && let Some(ref mut tcs) = tool_calls
        {
            for (i, tc) in tcs.iter_mut().enumerate() {
                if let Some(args) = tool_call_args_buffer.get(&(i as u64))
                    && (tc.function.arguments.is_empty() || tc.function.arguments == "{}")
                {
                    // Validate JSON before applying — stream interruption can
                    // leave incomplete arguments that would fail at tool execution.
                    if serde_json::from_str::<serde_json::Value>(args).is_ok() {
                        tracing::info!(
                            tool_name = %tc.function.name,
                            index = i,
                            accumulated_len = args.len(),
                            "Applying accumulated arguments to tool call"
                        );
                        tc.function.arguments = args.clone();
                    } else {
                        tracing::error!(
                            tool_name = %tc.function.name,
                            index = i,
                            raw_len = args.len(),
                            raw_preview = %&args[..args.len().min(200)],
                            "Accumulated tool call arguments are not valid JSON"
                        );
                        tc.function.arguments =
                            make_incomplete_marker(&tc.function.name, args.len());
                    }
                }
                // If arguments already non-empty (GLM/DeepSeek same-chunk pattern),
                // they are already complete — do not append buffer content.
                // DeepSeek sends duplicate complete arguments in subsequent chunks,
                // appending would produce invalid JSON like {"path": "."}{"path": "."}
            }
        }

        Ok(ChatResponse {
            content: accumulated_content,
            reasoning_content: if accumulated_reasoning_content.is_empty() {
                None
            } else {
                Some(accumulated_reasoning_content)
            },
            tool_calls,
            usage,
            reasoning_started_at,
            reasoning_finished_at,
            finish_reason,
        })
    }
}

/// Build a partial [`ChatResponse`] for stream stop.
///
/// Returns the accumulated content so far and discards any partial tool calls.
fn build_stopped_response(content: String, reasoning_content: String) -> ChatResponse {
    ChatResponse {
        content,
        reasoning_content: if reasoning_content.is_empty() {
            None
        } else {
            Some(reasoning_content)
        },
        tool_calls: None,
        usage: None,
        reasoning_started_at: None,
        reasoning_finished_at: None,
        finish_reason: Some("stopped".to_string()),
    }
}

/// Build a structured error marker for truncated/incomplete tool call arguments.
///
/// Returns valid JSON that `execute_single_tool` can parse and detect,
/// causing it to skip actual tool execution and return a clear error message
/// to the LLM. This avoids the "empty `{}`" silent degradation that previously
/// caused LLM retry loops.
///
/// IMPORTANT: The message string is a *prompt-level* constraint, not a code-level
/// guarantee — its effectiveness depends on the LLM's ability to follow instructions.
pub(crate) fn make_incomplete_marker(tool_name: &str, raw_len: usize) -> String {
    serde_json::json!({
        "error": "TOOL_CALL_INCOMPLETE",
        "message": format!(
            "Tool '{}' arguments were truncated during streaming \
             (received {} bytes, invalid JSON). \
             This call was NOT executed — do NOT retry with the same call. \
             If the task requires this tool, generate the full arguments in a new call.",
            tool_name, raw_len
        )
    })
    .to_string()
}
