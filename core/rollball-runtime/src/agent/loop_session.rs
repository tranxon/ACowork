//! Session lifecycle management for AgentLoop.
//!
//! Extracted from loop_.rs as part of ADR-014 Phase 5.
//!
//! Contains:
//! - Session status transitions
//! - Session close with distillation
//! - Session metadata updates (title, workspace_id)
//! - Think block utilities (extract, strip, build metadata)

use rollball_core::providers::traits::{ChatMessage, ChatResponse, MessageRole};

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
            let status = self.session.status.clone();
            // Emit chunk event to Gateway → frontend
            if !self.core.try_send_chunk(super::loop_::ChunkEvent::SessionStateChanged {
                status: status.clone(),
                model: self.session.model().map(|s| s.to_string()),
                provider: self.session.provider().map(|s| s.to_string()),
                workspace_id: self.session.workspace_id(),
            }) {
                tracing::warn!(
                    "SessionStateChanged event dropped (channel full/closed), status={:?}. Pull repair will correct frontend.",
                    status
                );
            }
            // Update watch channel for SessionHandle reads
            if let Some(ref tx) = self.core.status_tx {
                let _ = tx.send(status);
            }
            true
        } else {
            false
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
        self.session.conversation.as_ref().map(|conv| conv.update_title_force(title))
    }

    /// Persist the per-session workspace selection to the JSONL conversation file.
    ///
    /// Only effective when the session has an active `ConversationSession`.
    pub fn update_session_workspace_id(&mut self, workspace_id: &str) {
        if let Some(ref conv) = self.session.conversation {
            conv.update_workspace_id(workspace_id);
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
                let provider = self.core.provider.clone();
                let memory_store = self.core.memory_store().cloned();
                let emb_provider = self.core.embedding_provider.clone();
                // Build combined text for model-aware token counting via the unified API.
                let combined_text: String = tail_messages
                    .iter()
                    .fold(String::new(), |mut acc, m| {
                        acc.push_str(&m.content);
                        acc.push('\n');
                        acc
                    });
                let model_name = self.resolve_distill_model(&combined_text);
                let distill_max_tokens = self.core.config.distill_max_tokens;

                tracing::info!(
                    session_id = %session_id,
                    tail_start,
                    tail_message_count = tail_messages.len(),
                    is_compacted = self.session.is_compacted,
                    model = %model_name,
                    "Spawning tail distillation for session close"
                );

                // Spawn tail distillation (best-effort, non-blocking)
                tokio::spawn(async move {
                    match crate::episode_distill::EpisodeDistiller::compact_messages(
                        &tail_messages,
                        provider.as_ref(),
                        &model_name,
                        distill_max_tokens,
                    )
                    .await
                    {
                        Ok(summary) => {
                            crate::episode_distill::EpisodeDistiller::write_summary_to_grafeo(
                                &summary,
                                &session_id,
                                &memory_store,
                                emb_provider.as_deref(),
                            ).await;
                            tracing::info!(
                                session_id = %session_id,
                                summary_len = summary.len(),
                                "Tail distillation completed for session close"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %e,
                                "Tail distillation failed (non-fatal)"
                            );
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

        // Persist think block + assistant response to JSONL
        if let Some(ref conversation) = self.session.conversation {
            super::loop_session::persist_think_to_conversation(conversation, response);
            let assistant_text = strip_think_block(&content);
            conversation.append_message("assistant", &assistant_text, None);
        }

        self.session.history.append(ChatMessage {
            ..ChatMessage::assistant(content.clone())
        });

        // Per ADR-011: per-turn episodic writes are removed.
        // Grafeo is now written only via compaction summaries and
        // session-close distillation.
        self.session.turn_counter += 1;

        tracing::info!(iteration, "Agent returned text response");

        // Debug: enter AppendHistory phase and push step event
        self.core.debug_observer.on_phase_enter(
            crate::debug::protocol::DebugPhase::AppendHistory,
        )
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

/// Remove `<think>...</think>` block from content, returning the remaining text.
pub fn strip_think_block(content: &str) -> String {
    let start_tag = "<think>";
    let end_tag = "</think>";
    if let Some(start) = content.find(start_tag)
        && let Some(end) = content.find(end_tag)
    {
        let before = &content[..start];
        let after = &content[end + end_tag.len()..];
        return format!("{}{}", before.trim(), after.trim()).trim().to_string();
    }
    content.to_string()
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
