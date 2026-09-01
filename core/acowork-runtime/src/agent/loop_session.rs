//! Session lifecycle management for AgentLoop.
//!
//! Extracted from loop_.rs as part of ADR-014 Phase 5.
//!
//! Contains:
//! - Session status transitions
//! - Session close with distillation
//! - Session metadata updates (title, workspace_id)
//! - Think block utilities (extract, strip, build metadata)

use acowork_core::providers::traits::{ChatMessage, ChatResponse, MessageRole, Provider};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::agent::context::build_context_usage_from_persisted;
use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};
use crate::agent::loop_context::DistillTier;
use crate::agent::session_state::SessionStatus;
use crate::error::Result;

impl super::loop_::AgentLoop {
    // ── Session lifecycle methods ──────────────────────────────────────────

    /// Transition session status and emit SessionStateChanged event if the status changed.
    ///
    /// ADR-014 helper: ensures every status transition is paired with an event emission.
    /// Returns true if the status actually changed (and event was emitted).
    pub(crate) fn transition_status(&mut self, new_status: SessionStatus) -> bool {
        if self.session.set_status(new_status) {
            self.emit_session_state();
            true
        } else {
            false
        }
    }

    /// Emit a SessionStateChanged event with the current session state.
    ///
    /// Called on status transitions (via [`transition_status`]) and at the
    /// end of each iteration checkpoint in [`execute_single_iteration`], so
    /// the frontend always sees the latest status, ratio and todos.
    ///
    /// ADR-039: persisted fields (model, provider, workspace_id,
    /// reasoning_effort, temperature) are no longer emitted in this event —
    /// they are broadcast through the `session_meta` MQTT channel which
    /// carries the authoritative values from `data/meta/{session_id}.json`.
    pub(crate) fn emit_session_state(&mut self) {
        let status = self.session.status.clone();
        // Build context_usage from persisted session tokens (if available)
        // so the frontend can show token counts immediately on session state push,
        // without waiting for the first LLM call or a MQTT context_usage event.
        let context_usage = self.session.conversation.as_ref().and_then(|conv| {
            let persisted = conv.tokens()?;
            let model_name = self.session.model().unwrap_or("unknown");
            let caps = self.core.get_model_capabilities(model_name)?;
            let max_output = self.core.max_output_tokens_limit_for_model(model_name);
            let ctx = build_context_usage_from_persisted(
                &caps,
                persisted.last_input,
                persisted.last_output,
                max_output,
                self.core.context_window_override,
                Some(&persisted),
            );
            let json = serde_json::to_string(&ctx).ok();
            // LOG-001: fires on every emit_session_state (multiple per turn)
            // and only confirms the JSON was built — the value itself is
            // carried in the MQTT state snapshot. Demoted to DEBUG.
            tracing::debug!(
                session_id = %self.session_core.session_id.as_deref().unwrap_or("?"),
                model = %model_name,
                last_input = persisted.last_input,
                last_output = persisted.last_output,
                has_json = json.is_some(),
                "emit_session_state: built context_usage"
            );
            json
        });
        if context_usage.is_none() {
            let has_conv = self.session.conversation.is_some();
            let has_tokens = self.session.conversation.as_ref()
                .and_then(|c| c.tokens())
                .is_some();
            let model_for_caps = self.session.model().unwrap_or("unknown");
            let has_caps = self.core.get_model_capabilities(model_for_caps).is_some();
            tracing::warn!(
                session_id = %self.session_core.session_id.as_deref().unwrap_or("?"),
                has_conv,
                has_tokens,
                model = %model_for_caps,
                has_caps,
                "emit_session_state: context_usage is None"
            );
        }

        // ADR-043: Update the runtime state cache on ConversationSession
        // and notify the state relay. The relay publishes a retained
        // SessionState snapshot to `sessions/{sid}/state`.
        // This replaces the old ChunkEvent::SessionStateChanged path.
        if let Some(ref conv) = self.session.conversation {
            let status = serde_json::to_string(&status)
                .unwrap_or_else(|_| r#""idle""#.to_string());
            let ratio = self.session.model_ratio().unwrap_or(0.0);
            let cu = context_usage.clone().unwrap_or_default();
            conv.update_runtime_state_cache(&status, ratio, &cu);
            conv.notify_state_change();
        }
        // Update watch channel for SessionHandle reads.
        // Use `send_modify` instead of `send` because `send` silently
        // fails (returns Err without updating) when there are no
        // receivers – e.g. after the SessionHandle is dropped but the
        // agent loop hasn't exited yet.
        if let Some(ref tx) = self.session_core.status_tx {
            tx.send_modify(|v| *v = status.clone());
        }
        // Update shared snapshot for Gateway pull API.
        // The snapshot Arc is shared between SessionState and SessionHandle;
        // writes here are immediately visible to snapshot_session_state().
        {
            let status_json = serde_json::to_string(&status).unwrap_or_else(|_| r#""idle""#.to_string());
            // Serialize the todo list to JSON for the snapshot. Skip the allocation
            // entirely when the list is empty (common case — most iterations have
            // no active todo list). Errors are logged so they can be distinguished
            // from "no todos" on the consumer side.
            let todos_json = if self.session.todos.is_empty() {
                None
            } else {
                let sid = self.session_core.session_id.clone().unwrap_or_default();
                match serde_json::to_string(&self.session.todos) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!(
                            session_id = %sid,
                            error = %e,
                            "Failed to serialize todos for session state snapshot; emitting empty"
                        );
                        None
                    }
                }
            };
            if let Ok(mut guard) = self.session.snapshot.write() {
                guard.status = status_json;
                // ADR-039 (revised): mirror model + provider into the runtime
                // snapshot so SessionManager::current_model_name and
                // current_model_and_provider have sync access without
                // hitting the meta file. The authoritative value still lives
                // in data/meta/{session_id}.json; this mirror may briefly
                // lag by one emit cycle.
                guard.model = self.session.model().map(|s| s.to_string());
                guard.provider = self.session.provider().map(|s| s.to_string());
                guard.ratio = self.session.model_ratio();
                guard.todos_json = todos_json;
                // Only overwrite context_usage if we successfully computed a
                // new value.  Otherwise preserve the existing value set by
                // build_initial_session_state — emit_session_state may fail to
                // compute context_usage (e.g. model capabilities not yet available
                // for the session's model name), and unconditionally writing None
                // would wipe the initial snapshot populated at session creation.
                if context_usage.is_some() || guard.context_usage.is_none() {
                    guard.context_usage = context_usage;
                }
            }
        }
    }

    /// Get the current conversation session ID (S1.14)
    ///
    /// Returns the session ID of the active ConversationSession,
    /// or None if no session is active.
    pub fn current_session_id(&self) -> Option<&str> {
        self.session.conversation.as_ref().map(|c| c.session_id())
    }

    /// Update the title of the currently active conversation session.
    ///
    /// Returns `Some(true)` if the title was actually written (different from current),
    /// `Some(false)` if the title was already the same (no-op),
    /// or `None` if no active session exists.
    pub fn update_session_title(&mut self, title: &str) -> Option<bool> {
        self.session
            .conversation
            .as_ref()
            .map(|conv| conv.update_title_force(title))
    }

    /// Lazy-persist any async-generated session title to the conversation
    /// JSONL metadata and index.json.
    ///
    /// The title generation task (spawned in [`super::AgentLoop::run_inner`])
    /// writes the LLM-generated title to `session_core.title` asynchronously.
    /// This method is called at iteration checkpoints to pick up the title
    /// at the earliest opportunity and persist it to disk.
    ///
    /// Idempotent: `ConversationSession::update_title_force` is a no-op if
    /// the title hasn't changed.
    pub(crate) fn flush_pending_title(&mut self) {
        if let Some(title) = self.session_core.title.read().unwrap().clone()
            && let Some(ref conversation) = self.session.conversation
        {
            conversation.update_title_force(&title);
        }
    }

    /// Close the conversation session and trigger session-level distillation.
    ///
    /// This method:
    /// 1. Spawns an async distillation task for the entire session
    /// 2. Closes the conversation writer
    ///
    /// Distillation is best-effort and non-blocking.
    pub async fn close_session_with_distillation(&mut self) -> Result<()> {
        self.close_session_inner().await
    }

    /// Inner implementation for closing the current session.
    ///
    /// Per [ADR-011]: uses `last_compaction_index()` to determine the tail
    /// distillation range. The `is_compacted` flag is used as a fast-path hint
    /// but is NOT sufficient alone — the assistant response from the same round
    /// that triggered compaction may land after the compaction marker, and must
    /// still be distilled.
    async fn close_session_inner(&mut self) -> Result<()> {
        if let Some(ref conversation) = self.session.conversation {
            let session_id = conversation.session_id().to_string();

            // ADR-051 P3: Auto-generate Relationship nodes at session-end.
            // Delegates to run_post_compaction_memory_tasks() which includes
            // relationship generation (idempotent - creates or updates the
            // same node).
            self.run_post_compaction_memory_tasks().await;

            // Determine tail range: everything after the last compaction marker,
            // or full history (skipping leading system messages) if never compacted.
            let tail_start = self
                .session
                .history
                .last_compaction_index()
                .map(|idx| idx + 1) // Start after compaction marker
                .unwrap_or_else(|| {
                    // No compaction ever — skip leading system messages
                    self.session
                        .history
                        .messages()
                        .iter()
                        .take_while(|m| matches!(m.role, MessageRole::System))
                        .count()
                });

            let messages = self.session.history.messages();
            let tail_messages: Vec<ChatMessage> = messages[tail_start..].to_vec();

            if tail_messages.is_empty() {
                tracing::info!(
                    session_id = %session_id,
                    is_compacted = self.session.is_compacted,
                    "No tail messages to distill — skipping"
                );
            } else {
                let memory_provider = self.core.memory_provider().cloned();
                let emb_provider = self.core.embedding_provider.clone();
                // ADR-027: clone ConversationSession so the spawned task can
                // record raw Provider usage from the tail-distillation call
                // into the session token accumulator — independent of the
                // parent's `.close()` call below.
                let conversation_clone = self.session.conversation.clone();
                // ADR-028: clone the AgentCore so the spawned task can also
                // feed the agent-scoped token counters for this distillation
                // LLM call.
                let core_clone = self.core.clone();
                let distill_max_tokens = self.core.config.distill_max_tokens;
                // Snapshot user identity (small text block) so the spawned
                // task is independent of `self` and so the summary is written
                // in the user's preferred language.
                let identity_context = self.session.identity_context().map(String::from);
                // Clone the chunk sender so the spawned task can surface an
                // all-tiers-failed error to the frontend even though the
                // session is closing (best-effort; the channel may already be
                // gone by then, in which case the error stays in the logs).
                let chunk_tx = self.session_core.chunk_tx.clone();

                // ADR-056 call-phase fallback: resolve the FULL ordered list of
                // distillation targets (GlobalDefault → ProviderCompact →
                // CurrentChat) and pre-resolve each (provider, model) pair so
                // the spawned task only owns Arc handles. Mirrors
                // `compact_history_if_needed` — the previous single-target
                // attempt silently dropped the tail memory when the provider
                // was down.
                let resolved_targets: Vec<(Arc<dyn Provider>, String, DistillTier)> = self
                    .resolve_distill_targets()
                    .into_iter()
                    .map(|t| self.distill_provider(&t))
                    .collect();

                tracing::info!(
                    session_id = %session_id,
                    tail_start,
                    tail_message_count = tail_messages.len(),
                    is_compacted = self.session.is_compacted,
                    fallback_targets = resolved_targets.len().saturating_sub(1),
                    "Spawning tail distillation for session close (with call-phase fallback chain)"
                );

                // Spawn tail distillation (best-effort, non-blocking)
                tokio::spawn(async move {
                    let mut last_err: Option<crate::error::RuntimeError> = None;
                    for (compact_provider, model_name, tier) in &resolved_targets {
                        match crate::episode_distill::EpisodeDistiller::compact_messages(
                            &tail_messages,
                            compact_provider.as_ref(),
                            model_name,
                            distill_max_tokens,
                            identity_context.as_deref(),
                            core_clone.compaction_prompt.as_deref(),
                        )
                        .await
                        {
                            Ok((summary, usage)) => {
                                // ADR-027: record raw Provider usage from tail
                                // distillation into session token accumulator.
                                if let Some(ref conv) = conversation_clone {
                                    conv.accumulate_llm_usage(&usage);
                                }
                                // ADR-028: also feed the agent-scoped counters
                                // so the agent-total line in Results Panel
                                // accounts for this distillation call.
                                core_clone.accumulate_llm_usage(&usage);
                                if let Err(e) = crate::episode_distill::EpisodeDistiller::write_summary_to_provider(
                                    &summary,
                                    &session_id,
                                    &memory_provider,
                                    emb_provider.as_deref(),
                                )
                                .await
                                {
                                    // Write failure is infrastructure-level:
                                    // log it — the summary itself passed the
                                    // quality gate, so no user-facing error.
                                    tracing::error!(
                                        session_id = %session_id,
                                        error = %e,
                                        "Tail distillation: failed to write summary to provider"
                                    );
                                }
                                tracing::info!(
                                    session_id = %session_id,
                                    summary_len = summary.len(),
                                    tier = ?tier,
                                    "Tail distillation completed for session close"
                                );
                                last_err = None;
                                break;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    session_id = %session_id,
                                    tier = ?tier,
                                    error = %e,
                                    "Tail distillation LLM call failed, trying next tier"
                                );
                                // LowQuality is a model-capability problem —
                                // stepping down the chain only gets
                                // cheaper/weaker, so discard instead of retry.
                                let non_retryable = matches!(
                                    &e,
                                    crate::error::RuntimeError::Summary(se) if !se.is_retryable()
                                );
                                last_err = Some(e);
                                if non_retryable {
                                    break;
                                }
                            }
                        }
                    }

                    // P3: all fallback tiers failed → notify the user.
                    if let Some(err) = last_err {
                        tracing::error!(
                            session_id = %session_id,
                            error = %err,
                            "Tail distillation failed for session close (all tiers)"
                        );
                        if let Some(tx) = chunk_tx.as_ref() {
                            let _ = tx.try_send(SessionChunkEvent {
                                session_id: session_id.clone(),
                                event: ChunkEvent::Error {
                                    user_message: "Session memory distillation failed. The conversation is saved, but its summary memory was not written."
                                        .to_string(),
                                    detail: err.to_string(),
                                    error_type: "DistillationFailed".to_string(),
                                    message_id: format!("tail-distill-{session_id}"),
                                },
                            });
                        }
                    }
                });
            }

            // Close the conversation writer
            conversation.close().await?;
        }
        Ok(())
    }

    // ── Iteration result helpers ──────────────────────────────────────────

    /// Handle pure text response (no tool calls).
    ///
    /// Persists think block + assistant response to JSONL, appends to
    /// in-memory history, increments turn counter, and emits debug
    /// phase events. Returns `TextResponse(content)`.
    pub(crate) async fn handle_text_response(
        &mut self,
        response: &ChatResponse,
        iteration: u32,
    ) -> super::loop_::IterationResult {
        let content = response.content.clone();

        // Diagnostic: log detailed response summary for post-mortem analysis
        let reasoning_len = response
            .reasoning_content
            .as_ref()
            .map(|r| r.len())
            .unwrap_or(0);
        let reasoning_tokens = response
            .usage
            .as_ref()
            .map(|u| u.reasoning_tokens)
            .unwrap_or(0);
        let completion_tokens = response
            .usage
            .as_ref()
            .map(|u| u.completion_tokens)
            .unwrap_or(0);
        if content.is_empty() {
            tracing::warn!(
                iteration,
                finish_reason = ?response.finish_reason,
                reasoning_len,
                reasoning_tokens,
                completion_tokens,
                has_reasoning = response.reasoning_content.is_some(),
                "Empty text response — model may have exhausted token budget on reasoning"
            );
        } else {
            tracing::info!(
                iteration,
                content_len = content.len(),
                reasoning_len,
                reasoning_tokens,
                completion_tokens,
                finish_reason = ?response.finish_reason,
                "Agent returned text response"
            );
        }

        // ADR-022: Persist response to JSONL.
        //
        // Two paths:
        // 1. Streaming flush occurred (role transitions already wrote content
        //    to JSONL during the stream). Just flush any remaining content in
        //    the last streaming line — no legacy persistence needed.
        // 2. No streaming flush occurred (non-streaming provider, or all
        //    content arrived in the Finished event). Use the legacy path:
        //    persist_think_to_conversation + strip_think_block.
        let streamed = self.session_core.streaming_flush_count.load(Ordering::Relaxed) > 0;

        if streamed {
            // Path 1: Content was already flushed on role transitions.
            // Flush the last streaming line (e.g., final assistant segment).
            self.session_core.flush_streaming_line(self.session.conversation.as_deref());
            tracing::debug!(
                iteration,
                "ADR-022: streaming flush path — skipping legacy persistence"
            );
        } else if let Some(ref conversation) = self.session.conversation {
            // Path 2: Legacy persistence for non-streaming responses.
            super::loop_session::persist_think_to_conversation(conversation, response);
            let assistant_text = strip_think_block(&content);
            if !assistant_text.is_empty() {
                conversation.append_message("assistant", &assistant_text, None);
            }

            // ADR-021: Remove streaming line after legacy persistence
            // (handle_text_response already wrote thought + assistant to JSONL)
            self.session_core.remove_streaming_line();
        }

        // Persist the final assistant turn to in-memory history.
        //
        // IMPORTANT: We DELIBERATELY do not store `reasoning_content` here,
        // even though it is available on `response`. This is by design and
        // aligns with DeepSeek's thinking mode contract:
        //
        //   - If the user turn triggered NO tool calls, DeepSeek says the
        //     intermediate `reasoning_content` "无需参与上下文拼接,在后续
        //     轮次中将其传入 API 会被忽略" — passing it is harmless but
        //     bloats context for nothing, so we drop it.
        //   - Tool-call rounds are handled in `loop_tools.rs::prepare_tool_calls`
        //     where we DO persist `reasoning_content` because DeepSeek REQUIRES
        //     round-tripping it on tool-call turns.
        //
        // Reference: https://api-docs.deepseek.com/zh-cn/guides/thinking_mode
        //
        // Notes:
        //  - This drops reasoning ONLY for the text-final path. If the assistant
        //    happens to have called tools earlier in the same user turn, the
        //    earlier tool-turn `reasoning_content` is already in history via
        //    `prepare_tool_calls` — that is the DeepSeek-required signal.
        //  - Anthropic currently parses thinking into `reasoning_content` but
        //    never reads it back from history (Anthropic echoes thinking blocks,
        //    not a single string field); the field is effectively inert for
        //    Anthropic and harmless.
        //  - For OpenAI's Chat Completions (o-series), `response.reasoning_content`
        //    is always None, so dropping it is a no-op.
        self.session.history.append(ChatMessage {
            ..ChatMessage::assistant(content.clone())
        });

        // Note: the primary "Agent returned text response" log is now
        // emitted above with full diagnostic fields. The legacy log here
        // is kept for backward compatibility with existing log parsers.
        if content.is_empty() {
            tracing::info!(iteration, "Agent returned EMPTY text response");
        }

        // Debug: enter AppendHistory phase and push step event
        self.core
            .debug_observer
            .on_phase_enter(crate::debug::protocol::DebugPhase::AppendHistory)
            .await;
        self.core.debug_observer.on_phase_step(
            crate::debug::protocol::DebugPhase::Idle,
            None,
            Some(serde_json::json!({"content": content})),
        );
        self.core.debug_observer.on_phase_step_done().await;

        super::loop_::IterationResult::TextResponse(content)
    }
}

// ── Think block utilities (free functions) ──────────────────────────────
// Note: These are public so loop_llm.rs and loop_.rs can use them via
// `crate::agent::loop_session::{extract_think_block, strip_think_block, build_think_metadata}`.

/// Extract content inside `<think>...</think>` tags if present.
pub fn extract_think_block(content: &str) -> Option<String> {
    let start_tag = "<think>";
    let end_tag = "</think>";
    let start = content.find(start_tag)?;
    let end = content.find(end_tag)?;
    if end <= start + start_tag.len() {
        return None;
    }
    Some(content[start + start_tag.len()..end].trim().to_string())
}

/// Remove think blocks from content, returning the remaining text.
///
/// Supports two tag formats (matching `ThinkTagParser` in the provider layer):
///   - Standard: `<think>...</think>`
///   - MiniMax:  `<!think>...willReturn`
///
/// All occurrences of both formats are stripped. If no think blocks are found,
/// the original content is returned unchanged.
pub fn strip_think_block(content: &str) -> String {
    const PAIRS: &[(&str, &str)] = &[
        ("<think>", "</think>"),
        ("<!think>", "willReturn"),
    ];

    let mut result = content.to_string();
    for &(start_tag, end_tag) in PAIRS {
        while let Some(start) = result.find(start_tag) {
            if let Some(end) = result[start + start_tag.len()..].find(end_tag) {
                let end_abs = start + start_tag.len() + end + end_tag.len();
                let before = &result[..start];
                let after = &result[end_abs..];
                result = format!("{}{}", before, after);
            } else {
                // No closing tag — strip everything from the opening tag onward.
                result = result[..start].to_string();
                break;
            }
        }
    }
    result.trim().to_string()
}

/// Build think message metadata with timing info from ChatResponse.
pub fn build_think_metadata(response: &ChatResponse) -> Option<serde_json::Value> {
    if response.reasoning_started_at.is_some() || response.reasoning_finished_at.is_some() {
        Some(serde_json::json!({
            "startTime": response.reasoning_started_at,
            "endTime": response.reasoning_finished_at,
        }))
    } else {
        None
    }
}

/// Persist think block to conversation JSONL (if present).
///
/// Shared by text response path and tool calls path — D2 deduplication.
/// DeepSeek `reasoning_content` (separate field) takes priority over
/// `<think />` tags embedded in `content`.
pub fn persist_think_to_conversation(
    conversation: &crate::conversation::ConversationSession,
    response: &ChatResponse,
) {
    let think_meta = build_think_metadata(response);
    if let Some(ref reasoning) = response.reasoning_content {
        if !reasoning.is_empty() {
            conversation.append_message("thought", reasoning, think_meta);
        }
    } else if let Some(think_content) = extract_think_block(&response.content) {
        conversation.append_message("thought", &think_content, think_meta);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_think_no_tags() {
        assert_eq!(strip_think_block("hello world"), "hello world");
    }

    #[test]
    fn strip_think_standard_single() {
        assert_eq!(
            strip_think_block("<think>reasoning</think>answer"),
            "answer"
        );
    }

    #[test]
    fn strip_think_standard_with_surrounding() {
        assert_eq!(
            strip_think_block("before<think>inner</think>after"),
            "beforeafter"
        );
    }

    #[test]
    fn strip_think_minimax_single() {
        assert_eq!(
            strip_think_block("<!think>reasoningwillReturnanswer"),
            "answer"
        );
    }

    #[test]
    fn strip_think_minimax_with_surrounding() {
        assert_eq!(
            strip_think_block("before<!think>innerwillReturnafter"),
            "beforeafter"
        );
    }

    #[test]
    fn strip_think_multiple_blocks() {
        assert_eq!(
            strip_think_block("A<think>T1</think>B<think>T2</think>C"),
            "ABC"
        );
    }

    #[test]
    fn strip_think_mixed_formats() {
        assert_eq!(
            strip_think_block("A<think>T1</think>B<!think>T2willReturnC"),
            "ABC"
        );
    }

    #[test]
    fn strip_think_unclosed_standard() {
        assert_eq!(
            strip_think_block("answer<think>unclosed reasoning"),
            "answer"
        );
    }

    #[test]
    fn strip_think_unclosed_minimax() {
        assert_eq!(
            strip_think_block("answer<!think>unclosed reasoning"),
            "answer"
        );
    }

    #[test]
    fn strip_think_empty_content() {
        assert_eq!(strip_think_block(""), "");
    }

    #[test]
    fn strip_think_only_think_block() {
        assert_eq!(strip_think_block("<think>just thinking</think>"), "");
    }
}
