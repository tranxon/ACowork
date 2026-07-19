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
        let state = {
            let snaps = self
                .session_snapshots
                .read()
                .map_err(|_| RuntimeError::Io(std::io::Error::other("lock poisoned")))?;
            match snaps.get(session_id) {
                Some(snap) => {
                    match snap.read() {
                        Ok(guard) => Some(guard.status_json.clone()),
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
            state,
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

        let paginated = conversation::read_messages_paginated(
            &file_path,
            offset.unwrap_or(0),
            limit.unwrap_or(50),
        )?;

        let messages: Vec<serde_json::Value> = paginated
            .messages
            .into_iter()
            .map(|entry| {
                serde_json::json!({
                    "id": entry.id,
                    "ts": entry.ts,
                    "role": entry.role,
                    "content": entry.content,
                    "metadata": entry.metadata,
                })
            })
            .collect();

        let count = paginated.total;

        Ok(MessagesResponse {
            session_id: session_id.to_string(),
            messages,
            count,
        })
    }
}
