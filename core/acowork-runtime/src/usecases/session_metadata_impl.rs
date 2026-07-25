//! RuntimeSessionMetadataService — implements SessionMetadataService.
//!
//! ADR-024 / ADR-028 / ADR-040: wraps session scanning, message reading,
//! and agent-level token merging into a single-use case implementation.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use crate::conversation;
use crate::error::{Result, RuntimeError};
use crate::usecases::agent_token::AgentTokenService;
use crate::usecases::session_metadata::{
    MessagesResponse, SessionDetail, SessionMetadataService, SessionSummary, SessionsListResponse,
};

pub struct RuntimeSessionMetadataService {
    work_dir: PathBuf,
    agent_token: Arc<dyn AgentTokenService>,
    session_snapshots: SharedSessionSnapshots,
    latest_session: SharedLatestSession,
}

impl RuntimeSessionMetadataService {
    pub fn new(
        work_dir: PathBuf,
        agent_token: Arc<dyn AgentTokenService>,
        session_snapshots: SharedSessionSnapshots,
        latest_session: SharedLatestSession,
    ) -> Self {
        Self {
            work_dir,
            agent_token,
            session_snapshots,
            latest_session,
        }
    }
}

#[async_trait]
impl SessionMetadataService for RuntimeSessionMetadataService {
    async fn list_sessions(&self, page: u32, size: u32) -> Result<SessionsListResponse> {
        let conversations = self.work_dir.join("conversations");
        let join = conversation::scan_sessions_async(conversations, Some(page), Some(size));
        let (sessions, total_count, (disk_in, disk_out)) = join
            .await
            .map_err(|e| RuntimeError::Io(std::io::Error::other(e)))?;

        // ADR-028: merge disk totals into live counters and read back.
        self.agent_token.merge_token_totals((Some(disk_in), Some(disk_out)));
        let (agent_in, agent_out) = self.agent_token.agent_token_totals();

        let page_sessions: Vec<SessionSummary> = sessions
            .into_iter()
            .map(|s| SessionSummary {
                session_id: s.session_id,
                title: s.title,
                created_at: s.created_at,
                last_active_at: s.last_active_at,
                message_count: s.message_count,
                workspace_id: s.workspace_id,
                model: s.model,
                provider: s.provider,
            })
            .collect();

        let total_pages = if total_count == 0 {
            0
        } else {
            (total_count as u32).div_ceil(size)
        };

        Ok(SessionsListResponse {
            sessions: page_sessions,
            total_count,
            total_pages,
            page,
            size,
            agent_total_input_tokens: agent_in,
            agent_total_output_tokens: agent_out,
        })
    }

    async fn get_latest_session(&self) -> Result<Option<(String, Option<String>)>> {
        self.latest_session.read().map(|g| g.clone()).map_err(|_| {
            RuntimeError::Io(std::io::Error::other(
                "latest_session lock poisoned",
            ))
        })
    }

    async fn get_session(&self, session_id: &str) -> Result<SessionDetail> {
        let meta = conversation::read_session_meta(
            &self.work_dir.join("conversations"),
            session_id,
        )?;

        // Live state snapshot from SessionManager's shared snapshots.
        // Construct the full JSON object the desktop panel expects
        // (status, model, provider, ratio, todos, context_usage).
        let live_state = {
            let snaps = self
                .session_snapshots
                .read()
                .map_err(|_| RuntimeError::Io(std::io::Error::other("lock poisoned")))?;
            match snaps.get(session_id) {
                Some(snap) => {
                    match snap.read() {
                        Ok(guard) => {
                            let status: serde_json::Value =
                                serde_json::from_str(&guard.status_json)
                                    .unwrap_or(serde_json::Value::Null);
                            let todos: Option<serde_json::Value> = guard
                                .todos_json
                                .as_deref()
                                .and_then(|s| serde_json::from_str(s).ok());
                            let context_usage: Option<serde_json::Value> = guard
                                .context_usage_json
                                .as_deref()
                                .and_then(|s| serde_json::from_str(s).ok());
                            Some(serde_json::json!({
                                "status": status,
                                "model": guard.model,
                                "provider": guard.provider,
                                "ratio": guard.ratio,
                                "todos": todos,
                                "context_usage": context_usage,
                            }))
                        }
                        Err(_) => None,
                    }
                }
                None => None,
            }
        };

        Ok(SessionDetail {
            session_id: meta.session_id,
            title: meta.title,
            created_at: meta.created_at,
            last_active_at: meta.last_active_at,
            message_count: meta.message_count as u32,
            model: meta.model,
            provider: meta.provider,
            workspace_id: meta.workspace_id,
            live_state,
        })
    }

    async fn get_messages(
        &self,
        session_id: &str,
        offset: Option<u64>,
        limit: Option<u32>,
    ) -> Result<MessagesResponse> {
        let file_path = self
            .work_dir
            .join("conversations")
            .join(format!("{}.jsonl", session_id));

        let off = offset.unwrap_or(0);
        let lim = limit.unwrap_or(50).clamp(1, 500);

        let paginated = conversation::read_messages_paginated(&file_path, off, lim)?;

        // ADR-035 D9.2: truncate tool_result content to first 5 lines
        // for display in ALL HTTP paths. Full content stays in JSONL
        // for LLM context.
        let messages: Vec<serde_json::Value> = paginated
            .messages
            .into_iter()
            .map(|mut entry| {
                if entry.role == "tool_result" {
                    entry.content = truncate_tool_result(&entry.content);
                }
                serde_json::to_value(&entry).unwrap_or(serde_json::Value::Null)
            })
            .collect();
        let count = messages.len();

        Ok(MessagesResponse {
            session_id: session_id.to_string(),
            messages,
            offset: paginated.offset,
            limit: paginated.limit,
            total: paginated.total,
            count,
        })
    }
}

/// Truncate tool_result content to the first 5 lines for HTTP display.
///
/// ADR-035 D9.2: full content stays in JSONL for LLM context; HTTP
/// responses only carry the truncated preview.
///
/// Single canonical home for this rule (ADR-040): the HTTP layer calls
/// this through [`crate::usecases::SessionMetadataService::get_messages`],
/// and `cli.rs::truncate_tool_result_for_display` is a `pub(crate)`
/// re-export so any CLI preview path stays consistent without
/// duplicating the lines-count constant.
pub(crate) fn truncate_tool_result(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= 5 {
        return content.to_string();
    }
    let mut truncated = lines.into_iter().take(5).collect::<Vec<_>>().join("\n");
    truncated.push_str("\n...(truncated)");
    truncated
}
