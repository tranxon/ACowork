//! Per-session state for Agent Runtime.
//!
//! `SessionCore` holds all state that is specific to a single session:
//! session identity, chunk channel, notification control, JSONL counters,
//! streaming state, workspace directory, retry UX, and approval handle.
//!
//! Each `AgentLoop` owns one `SessionCore`, constructed from the shared
//! `AgentCore` template at session creation time.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};
use crate::agent::loop_approval::ApprovalHandle;
use crate::agent::session_state::SessionStatus;
use crate::conversation::StreamingStateMap;
use crate::providers::reliable::RetryWaitHandle;

/// Per-session state for one AgentLoop instance.
///
/// Constructed from the shared [`super::agent_core::AgentCore`] template
/// plus session-specific parameters (session_id, chunk_tx, committed_lines).
pub(crate) struct SessionCore {
    /// Session ID of the owning session.
    pub(crate) session_id: Option<String>,

    /// Single chunk sender for control events (Stopped, Done, Error,
    /// SessionStateChanged, ToolApprovalNeeded, AskQuestion, IterationLimitPaused,
    /// NewDataAvailable, ContextUsage, CompactingStarted, CompactingEnded).
    /// None in standalone mode.
    pub(crate) chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,

    /// Whether this session is allowed to send NewDataAvailable notifications.
    /// Defaults to false — set to true by EnableNotify message on session activation.
    pub(crate) notify_enabled: Arc<AtomicBool>,

    /// Unix timestamp (ms) of the last `NewDataAvailable` notification.
    /// Used for 500ms throttle — ADR-021 §难点 1.
    pub(crate) last_notify_ts: Arc<AtomicI64>,

    /// Notify throttle interval in ms (from DataFlowConfig).
    pub(crate) notify_interval_ms: u64,

    /// ADR-022: Committed JSONL line count — updated by the writer thread
    /// AFTER each entry is physically written to disk. Single authoritative
    /// count for `read_messages_since`, `notify_new_data_available`, and
    /// `ensure_streaming_line`.
    pub(crate) committed_lines: Arc<AtomicUsize>,

    /// ADR-022: Number of streaming flushes during the current LLM stream.
    /// Reset to 0 at the start of each `consume_stream` call.
    /// When > 0, `handle_text_response` and `prepare_tool_calls` skip their
    /// legacy persistence paths.
    pub(crate) streaming_flush_count: Arc<AtomicU64>,

    /// Shared map of in-progress streaming lines, keyed by session ID.
    /// Each session holds an Arc clone of the same shared map — created
    /// once in SessionManager, cloned into each SessionCore.
    pub(crate) streaming_lines: StreamingStateMap,

    /// Urgent stop notify — fired by Gateway to cancel tool execution
    /// immediately.  Each session gets its own independent Notify.
    pub(crate) urgent_stop: Option<Arc<Notify>>,

    /// Watch sender for session status (ADR-014).
    /// None for CLI-only sessions.
    pub(crate) status_tx:
        Option<tokio::sync::watch::Sender<SessionStatus>>,

    /// Shared session status for 429 retry UX.
    ///
    /// Initialized to `Streaming { message_id: None }` in `new()`.
    /// Written by [`AgentLoop::transition_status`] and
    /// [`crate::providers::reliable::ReliableProvider`] (retry pause/resume).
    /// Cloned to the ReliableProvider so it can emit `SessionStateChanged`
    /// events during long retry waits.
    pub(crate) retry_session_status:
        Option<Arc<std::sync::RwLock<SessionStatus>>>,

    /// Active retry-wait handle for 429 UX.
    ///
    /// Initialized in `new()`. [`session::SessionTask`] checks this when
    /// handling `ContinueExecution` to trigger `skip_notify` and wake the
    /// retry loop.
    pub(crate) retry_wait_handle:
        Option<crate::providers::reliable::RetryWaitHandle>,

    /// Per-session workspace ID. Single source of truth for workspace selection.
    /// Defaults to `"__agent_home__"`. Updated synchronously by SessionManager
    /// when the user switches workspace — no channel delay.
    ///
    /// Shared with [`SessionHandle`] so that `list_sessions` and
    /// `emit_session_state` always see the latest value.
    pub(crate) workspace_id: Arc<RwLock<String>>,

    /// Current workspace directory for tool execution.
    /// Resolved from `workspace_id` via WorkspaceResolver.
    /// Updated synchronously by SessionManager alongside workspace_id.
    pub(crate) current_work_dir: Arc<RwLock<Option<String>>>,

    /// Approval handle for shell command risk confirmation (Gateway mode).
    /// None in CLI mode.
    pub(crate) approval_handle: Option<ApprovalHandle>,

    /// Session title set by async LLM summarization.
    /// Written by the spawned title task in `AgentLoop::run_inner`, read by
    /// `notify_new_data_available()` to push the title to the frontend.
    pub(crate) title: Arc<RwLock<Option<String>>>,
}

impl SessionCore {
    /// Create a new SessionCore from the AgentCore template and session-specific parameters.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: String,
        chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
        committed_lines: Arc<AtomicUsize>,
        notify_interval_ms: u64,
        workspace_id: Arc<RwLock<String>>,
        current_work_dir: Arc<RwLock<Option<String>>>,
        streaming_lines: StreamingStateMap,
    ) -> Self {
        Self {
            session_id: Some(session_id),
            chunk_tx,
            notify_enabled: Arc::new(AtomicBool::new(false)),
            last_notify_ts: Arc::new(AtomicI64::new(0)),
            notify_interval_ms,
            committed_lines,
            streaming_flush_count: Arc::new(AtomicU64::new(0)),
            streaming_lines,
            urgent_stop: Some(Arc::new(Notify::new())),
            status_tx: None,
            retry_session_status: Some(Arc::new(std::sync::RwLock::new(
                SessionStatus::Streaming { message_id: None },
            ))),
            retry_wait_handle: Some(RetryWaitHandle::new()),
            workspace_id,
            current_work_dir,
            approval_handle: None,
            title: Arc::new(RwLock::new(None)),
        }
    }

    // ── Chunk event helpers ──────────────────────────────────────────

    /// Wrap a ChunkEvent into a SessionChunkEvent using this session's id.
    pub fn make_chunk_event(&self, event: ChunkEvent) -> Option<SessionChunkEvent> {
        self.session_id.as_ref().map(|sid| SessionChunkEvent {
            session_id: sid.clone(),
            event,
        })
    }

    /// Try-send a ChunkEvent via the chunk channel, wrapped with session_id.
    pub fn try_send_chunk(&self, event: ChunkEvent) -> bool {
        if let Some(wrapped) = self.make_chunk_event(event) {
            self.chunk_tx
                .as_ref()
                .map(|tx| tx.try_send(wrapped).is_ok())
                .unwrap_or(false)
        } else {
            tracing::debug!("Cannot send chunk event: session_id not set on SessionCore");
            false
        }
    }

    // ── JSONL line counter ───────────────────────────────────────────

    /// Get the committed JSONL line count, initializing from file on cold start.
    ///
    /// Falls back to cold-start file scan if the writer thread hasn't set
    /// the counter yet (resuming a session that hasn't received new writes).
    fn get_committed_lines(&self, session_id: &str) -> usize {
        let cached = self.committed_lines.load(Ordering::Relaxed);
        if cached > 0 {
            return cached;
        }
        // Cold start: scan file once to initialize.
        let count = self
            .current_work_dir
            .read()
            .unwrap()
            .as_ref()
            .map(|wd| {
                let jsonl_path = std::path::PathBuf::from(wd)
                    .join("conversations")
                    .join(format!("{}.jsonl", session_id));
                crate::conversation::count_jsonl_lines(&jsonl_path).unwrap_or(0)
            })
            .unwrap_or(0);
        // compare_exchange prevents overwriting a value the writer thread
        // may have set between our load (cached==0) and this store.
        let _ = self
            .committed_lines
            .compare_exchange(0, count, Ordering::Relaxed, Ordering::Relaxed);
        self.committed_lines.load(Ordering::Relaxed)
    }

    // ── Streaming line helpers ───────────────────────────────────────

    /// Ensure a `StreamingLine` exists for this session, creating one if needed.
    fn ensure_streaming_line(&self, role: &str) {
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };
        let mut map = self.streaming_lines.write().unwrap();
        if let Some(existing) = map.get_mut(&sid) {
            debug_assert_eq!(
                existing.role, role,
                "ADR-022 violation: append_streaming_delta called with role `{}` \
                 but current streaming line has role `{}`. \
                 Call flush_and_new_streaming_line before switching roles.",
                role, existing.role
            );
            return;
        }
        let line_number = self.get_committed_lines(&sid);
        map.insert(
            sid,
            crate::conversation::StreamingLine {
                line_number,
                role: role.to_string(),
                accumulated_content: String::new(),
                started_at: chrono::Utc::now().to_rfc3339(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
            },
        );
    }

    /// Append a delta to the streaming line for this session.
    pub(crate) fn append_streaming_delta(&self, role: &str, delta: &str) {
        self.ensure_streaming_line(role);
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };
        let mut map = self.streaming_lines.write().unwrap();
        if let Some(sl) = map.get_mut(&sid) {
            sl.accumulated_content.push_str(delta);
        }
    }

    /// Flush the current streaming line to JSONL and remove it from the map.
    pub(crate) fn flush_streaming_line(
        &self,
        conversation: Option<&crate::conversation::ConversationSession>,
    ) -> Option<String> {
        let sid = self.session_id.as_ref()?.clone();
        let removed = {
            let mut map = self.streaming_lines.write().unwrap();
            map.remove(&sid)
        };
        let sl = removed?;
        let content = sl.accumulated_content.clone();
        tracing::info!(
            role = %sl.role,
            content_len = content.len(),
            has_conversation = conversation.is_some(),
            "ADR-022 flush_streaming_line: flushing line"
        );
        if !content.trim().is_empty()
            && let Some(conv) = conversation
        {
            let metadata = if sl.role == "thought" {
                Some(serde_json::json!({
                    "startTime": sl.started_at_ms,
                    "endTime": chrono::Utc::now().timestamp_millis(),
                }))
            } else {
                None
            };
            conv.append_message(&sl.role, &content, metadata);
            self.streaming_flush_count.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                role = %sl.role,
                content_len = content.len(),
                "ADR-022 flush_streaming_line: wrote to JSONL"
            );
        } else if content.trim().is_empty() {
            tracing::warn!(
                role = %sl.role,
                content_len = content.len(),
                "ADR-022 flush_streaming_line: content is whitespace-only, skipping write"
            );
        }
        Some(content)
    }

    /// Remove the streaming line without persisting.
    pub(crate) fn remove_streaming_line(&self) {
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };
        self.streaming_lines.write().unwrap().remove(&sid);
    }

    /// Flush current streaming line (if non-empty), then ensure a new one
    /// with the given role.
    pub(crate) fn flush_and_new_streaming_line(
        &self,
        new_role: &str,
        conversation: Option<&crate::conversation::ConversationSession>,
    ) {
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };

        let need_flush = {
            let map = self.streaming_lines.read().unwrap();
            if let Some(sl) = map.get(&sid) {
                sl.role != new_role && !sl.accumulated_content.is_empty()
            } else {
                false
            }
        };

        if need_flush {
            self.flush_streaming_line(conversation);
        } else {
            // No existing line or empty — if there's a stale empty line with
            // a different role, discard it so ensure_streaming_line won't hit
            // the role-mismatch assertion.
            let mut map = self.streaming_lines.write().unwrap();
            if let Some(sl) = map.get(&sid)
                && sl.role != new_role
                && sl.accumulated_content.is_empty()
            {
                map.remove(&sid);
            }
        }

        self.ensure_streaming_line(new_role);
    }

    /// ADR-022: Reset the streaming flush counter at the start of each LLM stream.
    pub(crate) fn reset_streaming_flush_count(&self) {
        self.streaming_flush_count.store(0, Ordering::Relaxed);
    }

    // ── Notification ─────────────────────────────────────────────────

    /// ADR-021/025: Send a `NewDataAvailable` notification via the control channel.
    ///
    /// This is a **pure signal** — no state data is carried. The frontend
    /// triggers an incremental poll and the backend uses its own delivery
    /// cursor to determine what to return.
    ///
    /// Only sends if `notify_enabled` is true and at least `notify_interval_ms`
    /// have elapsed since the last notification (ADR-021 §难点 1).
    pub(crate) fn notify_new_data_available(&self) {
        if !self.notify_enabled.load(Ordering::Relaxed) {
            return;
        }

        let now = Utc::now().timestamp_millis();
        let last = self.last_notify_ts.load(Ordering::Relaxed);
        if now - last < self.notify_interval_ms as i64 {
            return;
        }
        if self
            .last_notify_ts
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };

        let title = self.title.read().unwrap().clone();

        let _ = self.try_send_chunk(ChunkEvent::NewDataAvailable {
            session_id: sid,
            interval_ms: self.notify_interval_ms,
            title,
        });
    }

    // ── Provider builder (retry UX) ──────────────────────────────────

    /// Rebuild Provider instance for a given provider_id from the global cache.
    /// Wires up 429-retry UX when session state is available.
    pub fn build_provider_for(
        &self,
        provider_id: &str,
        config: &crate::config::RuntimeConfig,
        global_provider_list: &std::sync::RwLock<Vec<acowork_core::protocol::ProviderListItem>>,
        provider_key_vault: &std::sync::RwLock<std::collections::HashMap<String, String>>,
    ) -> Option<Arc<dyn acowork_core::providers::traits::Provider>> {
        let provider_meta = {
            let list = global_provider_list.read().unwrap();
            list.iter().find(|p| p.id == provider_id).cloned()
        }?;

        let api_key = {
            let vault = provider_key_vault.read().unwrap();
            vault.get(provider_id).cloned()
        };

        let timeouts = Some(crate::providers::router::ProviderTimeouts::from(config));
        let raw = crate::providers::router::create_provider(
            &provider_meta.id,
            &provider_meta.protocol_type,
            api_key.as_deref(),
            if provider_meta.base_url.is_empty() {
                None
            } else {
                Some(&provider_meta.base_url)
            },
            timeouts,
        );
        let retry_config = crate::providers::reliable::RetryConfig::from(&config.timeouts.retry);
        let mut reliable =
            crate::providers::reliable::ReliableProvider::new(raw, retry_config);

        // Wire up 429 retry UX
        if let Some(status) = &self.retry_session_status
            && let Some(handle) = &self.retry_wait_handle
            && let Some(tx) = &self.chunk_tx
            && let Some(sid) = &self.session_id
        {
            reliable = reliable.with_retry_ux(
                crate::providers::reliable::RetryWaitHandle {
                    state: handle.state.clone(),
                    skip_notify: handle.skip_notify.clone(),
                },
                status.clone(),
                tx.clone(),
                sid.clone(),
            );
        }

        Some(Arc::new(reliable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    // ── Helpers ────────────────────────────────────────────────────────

    /// Create a SessionCore with a chunk channel for notification tests.
    fn make_core_with_notify(enabled: bool) -> (SessionCore, mpsc::Receiver<SessionChunkEvent>) {
        let (tx, rx) = mpsc::channel(16);
        let streaming_lines: StreamingStateMap =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let workspace_id = Arc::new(RwLock::new("__agent_home__".to_string()));
        let current_work_dir = Arc::new(RwLock::new(None));
        let core = SessionCore::new(
            "s1".to_string(),
            Some(tx),
            Arc::new(AtomicUsize::new(0)),
            500,
            workspace_id,
            current_work_dir,
            streaming_lines,
        );
        core.notify_enabled.store(enabled, Ordering::Relaxed);
        (core, rx)
    }

    use crate::conversation::{ConversationSession, SessionConfig};
    use std::path::Path;
    use tempfile::TempDir;

    /// Create a SessionCore + ConversationSession pair for ADR-022 tests.
    fn make_session_core_with_session(
        session_id: &str,
    ) -> (SessionCore, ConversationSession, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let work_dir = temp_dir.path().to_path_buf();
        let (tx, _rx) = mpsc::channel(16);
        let streaming_lines: StreamingStateMap =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let committed_lines = Arc::new(AtomicUsize::new(0));
        let workspace_id = Arc::new(RwLock::new("__agent_home__".to_string()));
        let current_work_dir = Arc::new(RwLock::new(Some(
            work_dir.to_string_lossy().to_string(),
        )));

        let core = SessionCore::new(
            session_id.to_string(),
            Some(tx),
            committed_lines.clone(),
            500,
            workspace_id,
            current_work_dir,
            streaming_lines,
        );
        core.notify_enabled.store(true, Ordering::Relaxed);

        let session = ConversationSession::new(
            &work_dir,
            session_id,
            SessionConfig {
                agent_id: "com.test.adr022".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            committed_lines,
        )
        .unwrap();

        (core, session, temp_dir)
    }

    /// Read all ConversationEntry lines from a session JSONL file
    /// (skipping the metadata header on line 0).
    fn read_jsonl_entries(
        work_dir: &Path,
        session_id: &str,
    ) -> Vec<crate::conversation::ConversationEntry> {
        let path = work_dir
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let content = std::fs::read_to_string(&path).unwrap();
        content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    // ── Notification tests (ADR-021) ───────────────────────────────────

    #[test]
    fn test_notify_disabled_suppresses_new_data_available() {
        // notify_enabled=false → NewDataAvailable is NOT sent
        let (core, mut rx) = make_core_with_notify(false);
        // Set up streaming_lines so notify_new_data_available has something to read
        core.streaming_lines.write().unwrap().insert(
            "s1".to_string(),
            crate::conversation::StreamingLine {
                line_number: 1,
                accumulated_content: String::new(),
                role: "assistant".to_string(),
                started_at: String::new(),
                started_at_ms: 0,
            },
        );
        core.notify_new_data_available();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_notify_enabled_sends_new_data_available() {
        // notify_enabled=true → NewDataAvailable IS sent
        let (core, mut rx) = make_core_with_notify(true);
        core.streaming_lines.write().unwrap().insert(
            "s1".to_string(),
            crate::conversation::StreamingLine {
                line_number: 1,
                accumulated_content: String::new(),
                role: "assistant".to_string(),
                started_at: String::new(),
                started_at_ms: 0,
            },
        );
        core.notify_new_data_available();
        let evt = rx.try_recv().unwrap();
        assert!(matches!(
            evt.event,
            ChunkEvent::NewDataAvailable { .. }
        ));
    }

    // ── 429 retry UX initialization tests ─────────────────────────────

    #[test]
    fn test_retry_ux_fields_initialized() {
        // SessionCore::new() must initialize retry_session_status and
        // retry_wait_handle so that build_provider_for can wire up UX.
        let (core, _rx) = make_core_with_notify(false);
        assert!(
            core.retry_session_status.is_some(),
            "retry_session_status must be Some for 429 retry UX wiring"
        );
        assert!(
            core.retry_wait_handle.is_some(),
            "retry_wait_handle must be Some for 429 retry UX wiring"
        );
    }

    #[test]
    fn test_retry_ux_initial_status_is_streaming() {
        // The initial retry_session_status should be Streaming
        // (the session hasn't been paused yet).
        let (core, _rx) = make_core_with_notify(false);
        let guard = core.retry_session_status.as_ref().unwrap().read().unwrap();
        assert!(
            matches!(*guard, SessionStatus::Streaming { .. }),
            "Initial retry status should be Streaming, got {:?}",
            *guard
        );
    }

    #[test]
    fn test_retry_ux_session_status_writable() {
        // Simulate what ReliableProvider::emit_retry_pause does:
        // write Paused with retry_info into the shared status lock.
        let (core, _rx) = make_core_with_notify(false);
        let status_lock = core.retry_session_status.as_ref().unwrap();
        {
            let mut guard = status_lock.write().unwrap();
            *guard = SessionStatus::Paused {
                iteration: None,
                max_iterations: None,
                retry_info: Some(crate::agent::session_state::RetryPauseInfo {
                    wait_ms: 10_500,
                    attempt: 1,
                    max_attempts: 3,
                    provider: "mock-provider".to_string(),
                }),
            };
        }
        // Verify the status is now Paused with retry info
        let guard = status_lock.read().unwrap();
        assert!(
            matches!(*guard, SessionStatus::Paused { .. }),
            "Status should be Paused after emit_retry_pause simulation"
        );
    }

    #[test]
    fn test_retry_ux_skip_notify_fires() {
        // Verify the skip_notify in retry_wait_handle can be triggered.
        // This simulates SessionTask handling ContinueExecution →
        // handle.skip_notify.notify_one() to wake the retry loop.
        let (core, _rx) = make_core_with_notify(false);
        let handle = core.retry_wait_handle.as_ref().unwrap();

        // notify_one is idempotent and non-blocking — just verify it
        // doesn't panic and the Notify is properly constructed.
        handle.skip_notify.notify_one();
        // If we got here without panic, the Notify is alive.
        // Just verify basic sanity: notify_one didn't panic.
        assert!(
            Arc::strong_count(&handle.skip_notify) >= 1,
            "Skip notify Arc should have at least 1 strong reference"
        );
    }

    /// Async test: verify that retry_wait_handle's skip_notify actually
    /// wakes a waiting task. Uses tokio::spawn + select to test the
    /// same pattern used by ReliableProvider::retry_sleep.
    #[tokio::test]
    async fn test_retry_ux_skip_notify_wakes_waiter() {
        let (core, _rx) = make_core_with_notify(false);
        let handle = core.retry_wait_handle.as_ref().unwrap();
        let skip = handle.skip_notify.clone();

        // Spawn a task that waits on skip_notify
        let wait_task = tokio::spawn(async move {
            skip.notified().await;
            "woken"
        });

        // Give the task time to start waiting, then notify
        tokio::task::yield_now().await;
        handle.skip_notify.notify_one();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_task,
        )
        .await
        .expect("Timeout: skip_notify did not wake the waiter")
        .expect("Wait task panicked");
        assert_eq!(result, "woken", "skip_notify must wake the waiting task");
    }

    // ── ADR-022 §9: Runtime role transition & flush tests ──────────────

    /// ADR-022 §9 test 3: Runtime role transition produces three
    /// single-role JSONL lines.
    ///
    /// Simulates the event sequence:
    ///   Content("我先看一下")          → assistant line
    ///   ReasoningContent("分析路径")    → flush assistant, thought line
    ///   Content("然后搜索")            → flush thought, assistant line
    ///   Finished                       → flush assistant
    ///
    /// Expected JSONL:
    ///   line 1: {"role":"assistant","content":"我先看一下"}
    ///   line 2: {"role":"thought","content":"分析路径"}
    ///   line 3: {"role":"assistant","content":"然后搜索"}
    #[test]
    fn test_adr022_role_transition_produces_single_role_lines() {
        let session_id = "adr022-transition";
        let (core, session, temp_dir) = make_session_core_with_session(session_id);

        // Content("我先看一下") — starts assistant streaming line
        core.flush_and_new_streaming_line("assistant", Some(&session));
        core.append_streaming_delta("assistant", "我先看一下");

        // ReasoningContent("分析路径") — role change, flush assistant, start thought
        core.flush_and_new_streaming_line("thought", Some(&session));
        core.append_streaming_delta("thought", "分析路径");

        // Content("然后搜索") — role change, flush thought, start assistant
        core.flush_and_new_streaming_line("assistant", Some(&session));
        core.append_streaming_delta("assistant", "然后搜索");

        // Finished — flush final assistant line
        core.flush_streaming_line(Some(&session));

        // Give writer thread time to process
        std::thread::sleep(std::time::Duration::from_millis(50));

        let entries = read_jsonl_entries(temp_dir.path(), session_id);
        assert_eq!(
            entries.len(),
            3,
            "Should have 3 single-role lines: assistant, thought, assistant"
        );
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].content, "我先看一下");
        assert_eq!(entries[1].role, "thought");
        assert_eq!(entries[1].content, "分析路径");
        assert_eq!(entries[2].role, "assistant");
        assert_eq!(entries[2].content, "然后搜索");
    }

    /// ADR-022 §9 test 4: assistant text + tool_call preserves order.
    ///
    /// Simulates:
    ///   Content("我来查一下")          → assistant line
    ///   ToolCallStart                  → flush assistant, then tool_call JSONL row
    ///
    /// Expected JSONL:
    ///   line 1: {"role":"assistant","content":"我来查一下"}
    ///   line 2: {"role":"tool_call",...}
    #[test]
    fn test_adr022_assistant_text_then_tool_call_preserves_order() {
        let session_id = "adr022-text-tool";
        let (core, session, temp_dir) = make_session_core_with_session(session_id);

        // Content("我来查一下") — assistant streaming line
        core.flush_and_new_streaming_line("assistant", Some(&session));
        core.append_streaming_delta("assistant", "我来查一下");

        // ToolCallStart arrives — flush assistant text first,
        // then write tool_call row.
        core.flush_streaming_line(Some(&session));

        // Simulate prepare_tool_calls writing the tool_call JSONL row
        session.append_message(
            "tool_call",
            r#"{"name":"grep","arguments":{"pattern":"foo"}}"#,
            None,
        );

        std::thread::sleep(std::time::Duration::from_millis(50));

        let entries = read_jsonl_entries(temp_dir.path(), session_id);
        assert_eq!(entries.len(), 2, "assistant text + tool_call = 2 lines");
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].content, "我来查一下");
        assert_eq!(entries[1].role, "tool_call");
    }

    /// ADR-022 §9 test 5: Finished event with tool_calls flushes text first.
    ///
    /// Some providers don't send ToolCallStart during streaming; they return
    /// complete tool_calls in the Finished response. Runtime must still flush
    /// any accumulated assistant text before writing tool_call rows.
    #[test]
    fn test_adr022_finished_with_tool_calls_flushes_text_first() {
        let session_id = "adr022-finished-tool";
        let (core, session, temp_dir) = make_session_core_with_session(session_id);

        // Content("开始搜索") — assistant streaming line accumulates
        core.flush_and_new_streaming_line("assistant", Some(&session));
        core.append_streaming_delta("assistant", "开始搜索");

        // Finished arrives with tool_calls — prepare_tool_calls path:
        // 1. flush_streaming_line (captures assistant text)
        // 2. append_message tool_call rows
        core.flush_streaming_line(Some(&session));
        session.append_message(
            "tool_call",
            r#"{"name":"search","arguments":{"q":"test"}}"#,
            None,
        );

        std::thread::sleep(std::time::Duration::from_millis(50));

        let entries = read_jsonl_entries(temp_dir.path(), session_id);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].content, "开始搜索");
        assert_eq!(entries[1].role, "tool_call");
    }

    /// ADR-022 invariant: ensure_streaming_line does NOT overwrite role.
    ///
    /// If a caller forgets to call flush_and_new_streaming_line and directly
    /// calls append_streaming_delta with a different role, the streaming line
    /// keeps its original role. In debug builds this triggers a debug_assert.
    #[test]
    #[should_panic(expected = "ADR-022 violation: append_streaming_delta called with role")]
    fn test_adr022_ensure_streaming_line_does_not_overwrite_role() {
        let (core, _session, _temp_dir) = make_session_core_with_session("adr022-no-overwrite");

        // Create an assistant streaming line
        core.append_streaming_delta("assistant", "hello");

        // Attempt to append thought content without flushing first.
        // This is a caller bug — debug_assert_eq! must fire.
        core.append_streaming_delta("thought", "should be thought but stays assistant");
    }
}
