//! Session lifecycle management and JSONL conversation file writing.
//!
//! Provides `ConversationSession` for managing a single session's JSONL file
//! and `ConversationWriter` for channel-based single-writer thread architecture.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::error::Result;
use acowork_core::providers::traits::UsageInfo;

/// Format version for the JSONL conversation file.
///
/// v3 (current): replaces `last_input_tokens` / `last_output_tokens` with a
///   structured `tokens` field on `SessionMeta`. See ADR-027.
const CONVERSATION_FORMAT_VERSION: u32 = 3;

/// Snapshot + cumulative token counts for a session.
///
/// Persisted in `SessionMeta.tokens` so the frontend can restore the
/// "context usage" indicator after a session resume, and so future rounds
/// have an authoritative cost record.
///
/// All four fields are **raw Provider-reported values** (or
/// `UsageInfo::default()` zeros when the Provider didn't return usage).
/// No local-tokenizer estimates are stored here — see ADR-027 for the
/// "宁可 miss 也不估计" policy.
///
/// - `last_input` / `last_output` — usage from the most recent LLM call.
///   Raw values are recorded verbatim, including zero, so the snapshot
///   faithfully reflects what the Provider returned (e.g. when
///   `prompt_tokens_reliable == false`).
/// - `total_input` / `total_output` — saturated sums across every LLM call
///   in this session. Only **reliable** calls (where the Provider returned
///   a positive count) are accumulated; calls with `prompt_tokens == 0`
///   are skipped on the input side so a Provider fallback does not
///   silently overwrite an accumulated cost.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SessionTokens {
    pub last_input: u64,
    pub last_output: u64,
    pub total_input: u64,
    pub total_output: u64,
}

/// Entry kind discriminator for `ConversationEntry.kind`.
pub const ENTRY_KIND_COMPACTION: &str = "compaction";

/// A single line in the conversation JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntry {
    /// Unique message ID (UUID v4)
    pub id: String,
    /// ISO 8601 timestamp with millisecond precision
    pub ts: String,
    /// For regular messages: "user" | "assistant" | "thought" | "tool_call" | "tool_result" | "system".
    /// For compaction events: still set to "system" so legacy readers degrade gracefully,
    /// but `kind` should be checked first.
    pub role: String,
    /// Full message content. For `kind="compaction"`, this carries the summary text.
    pub content: String,
    /// Optional metadata (e.g. tool_call_id, tool_name, or `CompactionEventMeta`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Entry kind. `None` or `"message"` denotes a regular message (default).
    /// `"compaction"` denotes an LLM-driven compaction event.
    /// Added in JSONL v2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Structured metadata payload for `kind="compaction"` entries.
///
/// Stored in `ConversationEntry.metadata` as a JSON object so legacy
/// readers can still parse the entry as opaque metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionEventMeta {
    /// First entry id covered by the summary (inclusive).
    /// May be empty if the compaction occurred before any message id was
    /// recorded (e.g. forced manual trigger on an empty session — pathological).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub compacted_from_id: String,
    /// Last entry id covered by the summary (inclusive).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub compacted_to_id: String,
    /// Number of trailing rounds preserved in memory after compaction.
    /// Used by the restorer to validate the replay window.
    pub keep_last_rounds: usize,
    /// Compaction model used (diagnostic only).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// History token estimate before compaction (diagnostic only).
    pub before_tokens: u64,
    /// History token estimate after compaction (diagnostic only).
    pub after_tokens: u64,
}

/// Per-session metadata stored in `conversations/meta/{session_id}.json`.
///
/// ADR-024: each session writes only its own meta file — no cross-session
/// contention, no index.json, no JSONL header line.
///
/// Field `last_compaction_offset` is an absolute byte offset (there is no
/// header in the JSONL, so the offset is always absolute).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    // ── Immutable fields ──
    pub version: u32,
    pub session_id: String,
    pub agent_id: String,
    pub created_at: String,

    // ── User/API mutable fields ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    // ── Runtime statistics (updated by AgentLoop) ──
    pub message_count: u64,
    pub last_active_at: String,
    /// ADR-027: snapshot + cumulative token counts (raw Provider values).
    /// `None` until the first LLM call has been recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<SessionTokens>,

    // ── Compaction ──
    /// Absolute byte offset of the most recent compaction marker.
    /// `None` if no compaction has occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_offset: Option<u64>,

    // ── Recovery flag ──
    #[serde(default)]
    pub corrupted: bool,
}

/// Commands sent to the background writer thread.
pub enum WriterCommand {
    /// Append a conversation entry to the JSONL file.
    AppendEntry(ConversationEntry),
    /// Flush and shut down the writer.
    Shutdown(oneshot::Sender<()>),
}

/// Background writer that exclusively owns the JSONL file handle.
///
/// ADR-024: the writer no longer manages a metadata header line.
/// Metadata is stored in a separate per-session file (`meta/{session_id}.json`).
pub struct ConversationWriter {
    file: std::fs::File,
    receiver: mpsc::UnboundedReceiver<WriterCommand>,
    /// Absolute byte offset of the most recent compaction marker.
    /// `None` if no compaction has been written during this writer's lifetime.
    last_compaction_offset: Option<u64>,
    /// ADR-022: Committed line count — incremented after each successful
    /// disk write so `read_messages_since` never sees a count ahead of
    /// the actual file.
    committed_lines: Arc<AtomicUsize>,
}

impl ConversationWriter {
    /// Create a new writer.
    fn new(
        file: std::fs::File,
        receiver: mpsc::UnboundedReceiver<WriterCommand>,
        committed_lines: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            file,
            receiver,
            last_compaction_offset: None,
            committed_lines,
        }
    }

    /// Run the writer loop. Blocks until Shutdown is received.
    fn run(mut self) {
        while let Some(cmd) = self.receiver.blocking_recv() {
            match cmd {
                WriterCommand::AppendEntry(entry) => {
                    let is_compaction = entry.kind.as_deref() == Some(ENTRY_KIND_COMPACTION);
                    // Capture absolute offset before writing (seek(End(0))
                    // positions us at the byte where the entry will land).
                    let abs_offset = if is_compaction {
                        match self.file.seek(std::io::SeekFrom::End(0)) {
                            Ok(pos) => Some(pos),
                            Err(e) => {
                                tracing::error!("Failed to seek for compaction entry: {}", e);
                                None
                            }
                        }
                    } else {
                        None
                    };
                    if let Err(e) = self.write_entry(&entry, abs_offset.is_some())
                    {
                        tracing::error!("Failed to write conversation entry: {}", e);
                    } else {
                        // ADR-022: Increment committed_lines AFTER the entry
                        // is physically written to disk.  This guarantees
                        // that read_messages_since never sees a line count
                        // ahead of the actual file contents.
                        self.committed_lines.fetch_add(1, Ordering::Relaxed);
                        if let Some(abs) = abs_offset {
                            // ADR-024: absolute offset (no header to subtract from).
                            self.last_compaction_offset = Some(abs);
                            tracing::debug!(
                                abs_offset = abs,
                                "Recorded compaction offset"
                            );
                        }
                    }
                }
                WriterCommand::Shutdown(tx) => {
                    if let Err(e) = self.file.flush() {
                        tracing::error!("Failed to flush conversation file: {}", e);
                    }
                    let _ = tx.send(());
                    break;
                }
            }
        }
    }

    /// Write a single entry as a JSON line.
    ///
    /// Builds the complete line in memory first, then issues a single
    /// `write_all` call so the OS can apply atomicity for small writes.
    /// Follows up with `sync_data` to flush to disk.
    ///
    /// If `already_positioned` is `true`, the file cursor is already at the
    /// end (set by the caller who captured the pre-write absolute offset).
    fn write_entry(
        &mut self,
        entry: &ConversationEntry,
        already_positioned: bool,
    ) -> std::io::Result<()> {
        if !already_positioned {
            // Seek to end for append; handles resume where file position may be at 0
            self.file.seek(std::io::SeekFrom::End(0))?;
        }
        // Build the complete line in memory first to ensure atomic write
        let mut line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        // Single write_all call — OS-level atomicity for small writes
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()?;
        Ok(())
    }
}

/// Initial configuration for creating a new `ConversationSession`.
///
/// Replaces a long positional parameter list with named fields, making call
/// sites self-documenting and trivial to extend.
pub struct SessionConfig {
    pub agent_id: String,
    pub workspace_id: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
}

/// Manages a single conversation session's JSONL file.
///
/// `ConversationSession` is `Send + Sync` so it can be held by `AgentLoop`
/// in async contexts.
pub struct ConversationSession {
    session_id: String,
    agent_id: String,
    created_at: String,
    /// Whether the session title has been set (first user message).
    title_set: AtomicBool,
    /// Currently persisted title, for deduplicating force-update calls
    /// and serving the `title()` getter without disk I/O.
    current_title: std::sync::Mutex<Option<String>>,
    /// Per-session workspace selection, persisted in meta file.
    /// `None` or `"__agent_home__"` means the agent's home directory.
    /// Wrapped in Mutex for interior mutability so that both file persistence
    /// and in-memory state are updated atomically on the API side.
    workspace_id: std::sync::Mutex<Option<String>>,
    /// Per-session model selection (ADR-012).
    model: std::sync::Mutex<Option<String>>,
    /// Per-session provider selection (ADR-012).
    provider: std::sync::Mutex<Option<String>>,
    /// Per-session reasoning effort override, persisted in meta file.
    reasoning_effort: std::sync::Mutex<Option<String>>,
    /// Per-session temperature override, persisted in meta file.
    temperature: std::sync::Mutex<Option<f32>>,
    /// ADR-027: snapshot + cumulative token counts (raw Provider values).
    /// `None` means no LLM call has been recorded yet.
    tokens: std::sync::Mutex<Option<SessionTokens>>,
    /// Running message count, incremented on every `append_message`.
    message_count: AtomicU64,
    /// Last time the meta file was written from `append_message`.
    /// Guards against excessive I/O from rapid `append_message` calls.
    last_meta_write: std::sync::Mutex<Instant>,
    sender: mpsc::UnboundedSender<WriterCommand>,
    /// Path to the JSONL file (for session-level distillation on close).
    session_file_path: PathBuf,
    /// Path to the conversations directory (for meta file writes).
    conversations_dir: PathBuf,
}

/// Minimum interval between meta file writes triggered by `append_message`.
///
/// Metadata-only mutations (title, model, provider, workspace) always write
/// immediately.  Only the high-frequency `append_message` path respects this
/// cooldown, capping meta write I/O regardless of conversation speed.
const META_WRITE_COOLDOWN_MS: u64 = 3000;

impl ConversationSession {
    /// Build a complete `SessionMeta` from the current in-memory state.
    fn build_meta(&self) -> SessionMeta {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let tokens = self.tokens.lock().ok().and_then(|t| t.clone());
        SessionMeta {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            created_at: self.created_at.clone(),
            title: self.current_title.lock().ok().and_then(|t| t.clone()),
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            reasoning_effort: self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            temperature: self.temperature.lock().ok().and_then(|t| *t),
            message_count: self.message_count.load(Ordering::Relaxed),
            last_active_at: now,
            tokens,
            last_compaction_offset: None,
            corrupted: false,
        }
    }

    /// Write the current in-memory state to the per-session meta file.
    ///
    /// Also updates `last_meta_write` timestamp for cooldown tracking.
    fn write_meta(&self) {
        let meta = self.build_meta();
        if let Err(e) = write_session_meta(&self.conversations_dir, &meta) {
            tracing::warn!(
                session_id = %self.session_id,
                error = %e,
                "Failed to write session meta file"
            );
        }
        if let Ok(mut last) = self.last_meta_write.lock() {
            *last = Instant::now();
        }
    }

    /// Create a new session with optional initial metadata.
    ///
    /// Creates a pure JSONL file (no metadata header — see ADR-024) and
    /// writes session metadata to `conversations/meta/{session_id}.json`.
    pub fn new(work_dir: &Path, session_id: &str, config: SessionConfig, max_sessions: usize, committed_lines: Arc<AtomicUsize>) -> Result<Self> {
        let conversations_dir = work_dir.join("conversations");
        std::fs::create_dir_all(&conversations_dir)?;

        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&file_path)?;

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        // ADR-024: no JSONL header — file starts at line 0.
        let (tx, rx) = mpsc::unbounded_channel::<WriterCommand>();
        let writer = ConversationWriter::new(file, rx, committed_lines);
        std::thread::spawn(move || writer.run());

        let session = Self {
            session_id: session_id.to_string(),
            agent_id: config.agent_id,
            created_at: now.clone(),
            title_set: AtomicBool::new(false),
            current_title: std::sync::Mutex::new(None),
            workspace_id: std::sync::Mutex::new(config.workspace_id),
            model: std::sync::Mutex::new(config.model),
            provider: std::sync::Mutex::new(config.provider),
            reasoning_effort: std::sync::Mutex::new(None),
            temperature: std::sync::Mutex::new(None),
            tokens: std::sync::Mutex::new(None),
            message_count: AtomicU64::new(0),
            last_meta_write: std::sync::Mutex::new(Instant::now()),
            sender: tx,
            session_file_path: file_path,
            conversations_dir: conversations_dir.clone(),
        };

        // ADR-024: write per-session meta file (replaces index.json update).
        session.write_meta();

        // Enforce max-sessions limit: prune the oldest sessions if the
        // limit now exceeds the configured threshold.
        if max_sessions > 0 {
            prune_excess_sessions(&conversations_dir, max_sessions);
        }

        Ok(session)
    }

    /// Resume an existing session.
    ///
    /// Opens the existing JSONL file in append mode, reads metadata from
    /// `conversations/meta/{session_id}.json`, and starts the background
    /// writer thread.
    pub fn resume(work_dir: &Path, session_id: &str, committed_lines: Arc<AtomicUsize>) -> Result<Self> {
        let conversations_dir = work_dir.join("conversations");
        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file_path)?;

        // ADR-024: read metadata from per-session meta file.
        let meta = read_session_meta(&conversations_dir, session_id)
            .map_err(|e| std::io::Error::new(e.kind(), format!("Failed to read session meta: {}", e)))?;

        // ADR-024: no JSONL header, meta_end is always 0.

        // Initialize committed_lines to the actual number of existing lines in
        // the JSONL file. Without this, the counter stays at 0 (the value
        // passed by callers like ensure_session_in_memory), which causes the
        // delivery cursor (ADR-025) to be reset to {0, 0} during full-load —
        // and the first incremental poll re-delivers all historical messages.
        let existing_lines = count_jsonl_lines(&file_path).unwrap_or(0);
        committed_lines.store(existing_lines, Ordering::Relaxed);

        let (tx, rx) = mpsc::unbounded_channel::<WriterCommand>();
        let writer = ConversationWriter::new(file, rx, committed_lines);
        std::thread::spawn(move || writer.run());

        Ok(Self {
            session_id: session_id.to_string(),
            agent_id: meta.agent_id,
            created_at: meta.created_at,
            title_set: AtomicBool::new(meta.title.is_some()),
            current_title: std::sync::Mutex::new(meta.title.clone()),
            workspace_id: std::sync::Mutex::new(meta.workspace_id),
            model: std::sync::Mutex::new(meta.model),
            provider: std::sync::Mutex::new(meta.provider),
            reasoning_effort: std::sync::Mutex::new(meta.reasoning_effort),
            temperature: std::sync::Mutex::new(meta.temperature),
            tokens: std::sync::Mutex::new(meta.tokens.clone()),
            message_count: AtomicU64::new(meta.message_count),
            last_meta_write: std::sync::Mutex::new(Instant::now()),
            sender: tx,
            session_file_path: file_path,
            conversations_dir,
        })
    }

    /// Append a message to the conversation.
    ///
    /// This is non-blocking: the message is sent via channel to the
    /// background writer thread.
    pub fn append_message(&self, role: &str, content: &str, metadata: Option<serde_json::Value>) {
        self.append_message_with_id(role, content, metadata, None);
    }

    /// Append a message with an explicit ID.
    ///
    /// When `id` is `Some`, the entry is stored with that ID instead of
    /// generating a new UUID. This is used for user messages where the
    /// frontend generates a deterministic ID (`msg-{uuid}`) and sends it
    /// via `message_id` — the backend stores it as-is so the frontend
    /// can deduplicate by ID when polling session messages.
    pub fn append_message_with_id(
        &self,
        role: &str,
        content: &str,
        metadata: Option<serde_json::Value>,
        id: Option<String>,
    ) {
        let entry = ConversationEntry {
            id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            role: role.to_string(),
            content: content.to_string(),
            metadata,
            kind: None,
        };
        if let Err(e) = self.sender.send(WriterCommand::AppendEntry(entry)) {
            tracing::error!("Failed to send message to conversation writer: {}", e);
        }
        // Update message count and write meta so the frontend sees the latest
        // active-at timestamp without a directory scan.
        self.message_count.fetch_add(1, Ordering::Relaxed);
        // ADR-024: throttle meta writes on the high-frequency append path.
        if let Ok(last) = self.last_meta_write.lock()
            && last.elapsed().as_millis() < META_WRITE_COOLDOWN_MS as u128
        {
            return; // skip — in-memory counters are already up to date
        }
        self.write_meta();
    }

    /// Append a compaction event to the JSONL.
    ///
    /// Used by [`AgentLoop::compact_history_if_needed`] after a successful
    /// LLM-driven compaction to mark the boundary between compacted and
    /// surviving messages. The session restorer uses the most recent such
    /// event to determine the replay window.
    ///
    /// The entry's `role` is set to `"system"` so legacy v1 readers (and any
    /// frontend that ignores `kind`) treat it as a benign system note.
    pub fn append_compaction_event(&self, summary: &str, meta: CompactionEventMeta) {
        let metadata_value = serde_json::to_value(&meta).ok();
        let entry = ConversationEntry {
            id: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            role: "system".to_string(),
            content: summary.to_string(),
            metadata: metadata_value,
            kind: Some(ENTRY_KIND_COMPACTION.to_string()),
        };
        if let Err(e) = self.sender.send(WriterCommand::AppendEntry(entry)) {
            tracing::error!("Failed to send compaction event to conversation writer: {}", e);
        }
    }

    /// Return the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Return the agent ID.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Return the current persisted session title, if any.
    pub fn title(&self) -> Option<String> {
        self.current_title.lock().ok().and_then(|t| t.clone())
    }

    /// Return the path to the JSONL session file.
    ///
    /// Used by session-level episode distillation on close.
    pub fn session_path(&self) -> &Path {
        &self.session_file_path
    }

    /// Close the session.
    ///
    /// Sends a Shutdown command to the writer thread and waits for
    /// it to flush and finish.
    pub async fn close(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel::<()>();
        if let Err(e) = self.sender.send(WriterCommand::Shutdown(tx)) {
            tracing::error!("Failed to send shutdown to conversation writer: {}", e);
            return Err(crate::error::RuntimeError::Io(std::io::Error::other(
                format!("shutdown send failed: {}", e),
            )));
        }
        let _ = rx.await;
        Ok(())
    }

    /// Set the session title from the first user message.
    ///
    /// Truncates to 30 characters. Only sets title once —
    /// subsequent calls are no-ops.
    pub fn set_title(&self, content: &str) {
        if self.title_set.swap(true, Ordering::Relaxed) {
            return;
        }
        let title = {
            let chars: Vec<char> = content.chars().collect();
            if chars.len() <= 30 {
                content.to_string()
            } else {
                // Find the last natural break point within first 30 chars
                let break_chars = [',', '，', '.', '。', '!', '！', '?', '？', ';', '；', '\n'];
                if let Some(pos) = chars[..30].iter().rposition(|c| break_chars.contains(c)) {
                    let truncated: String = chars[..=pos].iter().collect();
                    if pos < 29 {
                        truncated
                    } else {
                        format!("{}...", truncated)
                    }
                } else {
                    let truncated: String = chars[..30].iter().collect();
                    format!("{}...", truncated)
                }
            }
        };
        // Track current title for dedup
        if let Ok(mut current) = self.current_title.lock() {
            *current = Some(title);
        }
        // ADR-024: write entire meta file instead of rewrite_metadata + update_index_entry
        self.write_meta();
        tracing::info!(session_id = %self.session_id, "Session title set");
    }

    /// Force-update the session title (used by API, not first-message auto-set).
    ///
    /// Unlike `set_title`, this always writes the title even if one was
    /// already set. Used by the `update_session_title` action from Gateway.
    /// Returns `true` if the title was actually written (was different from current).
    pub fn update_title_force(&self, title: &str) -> bool {
        // No-op if the title hasn't changed
        if let Ok(current) = self.current_title.lock()
            && current.as_deref() == Some(title)
        {
            return false;
        }
        let truncated = {
            let chars: Vec<char> = title.chars().collect();
            if chars.len() <= 30 {
                title.to_string()
            } else {
                format!("{}...", chars[..30].iter().collect::<String>())
            }
        };
        self.title_set.store(true, Ordering::Relaxed);
        // Track current title for dedup
        if let Ok(mut current) = self.current_title.lock() {
            *current = Some(truncated.clone());
        }
        // ADR-024: write entire meta file instead of rewrite_metadata + update_index_entry
        self.write_meta();
        tracing::info!(session_id = %self.session_id, title = %truncated, "Session title force-updated via API");
        true
    }

    /// Persist the per-session workspace selection to the meta file.
    ///
    /// The authoritative workspace_id is stored in [`SessionCore`];
    /// this method only persists to disk.
    pub fn update_workspace_id(&self, workspace_id: &str) {
        // Update in-memory state FIRST so that subsequent metadata updates
        // (e.g. set_title via first user message) don't lose workspace_id.
        if let Ok(mut w) = self.workspace_id.lock() {
            *w = Some(workspace_id.to_string());
        }
        self.write_meta();
        tracing::info!(
            session_id = %self.session_id,
            workspace_id = %workspace_id,
            "Session workspace_id persisted to meta file"
        );
    }

    /// Return the persisted workspace_id, if any.
    pub fn workspace_id(&self) -> Option<String> {
        self.workspace_id.lock().ok().and_then(|w| w.clone())
    }

    /// Return the persisted model, if any (ADR-012).
    pub fn model(&self) -> Option<String> {
        self.model.lock().ok().and_then(|m| m.clone())
    }

    /// Return the persisted provider, if any (ADR-012).
    pub fn provider(&self) -> Option<String> {
        self.provider.lock().ok().and_then(|p| p.clone())
    }

    /// Persist the per-session model and provider selection to meta file (ADR-012).
    ///
    /// Does NOT mutate the in-memory `SessionState` — the caller is
    /// responsible for keeping the two in sync.
    pub fn update_model_provider(&self, model: &str, provider: Option<&str>) {
        // Update in-memory state FIRST so that subsequent metadata updates
        // (e.g. set_title via first user message) don't lose model/provider.
        if let Ok(mut m) = self.model.lock() {
            *m = Some(model.to_string());
        }
        if let Ok(mut p) = self.provider.lock() {
            *p = provider.map(|s| s.to_string());
        }
        self.write_meta();
        tracing::info!(
            session_id = %self.session_id,
            model = %model,
            provider = ?provider,
            "Session model/provider persisted to meta file"
        );
    }

    /// Return the persisted reasoning_effort string, if any.
    pub fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort.lock().ok().and_then(|r| r.clone())
    }

    /// Persist the per-session reasoning_effort override to meta file.
    ///
    /// Updates in-memory state and writes the meta file.
    pub fn update_reasoning_effort(&self, effort: Option<String>) {
        if let Ok(mut r) = self.reasoning_effort.lock() {
            *r = effort;
        }
        self.write_meta();
        tracing::info!(
            session_id = %self.session_id,
            "Session reasoning_effort persisted to meta file"
        );
    }

    /// Return the persisted temperature, if any.
    pub fn temperature(&self) -> Option<f32> {
        self.temperature.lock().ok().and_then(|t| *t)
    }

    /// Persist the per-session temperature override to meta file.
    pub fn update_temperature(&self, temperature: Option<f32>) {
        if let Ok(mut t) = self.temperature.lock() {
            *t = temperature;
        }
        self.write_meta();
        tracing::info!(
            session_id = %self.session_id,
            "Session temperature persisted to meta file"
        );
    }

    /// Return the full [`SessionTokens`] (last + totals), if any LLM call
    /// has been recorded yet.
    ///
    /// Used on resume to seed the frontend "context usage" indicator with
    /// the same raw `prompt_tokens`/`completion_tokens` that the most
    /// recent LLM response reported. Window-derived fields
    /// (`context_window`, `usable_context`, `usage_percent`) are
    /// recomputed at resume time from the *current* model capabilities —
    /// this getter only returns the raw API-fact values.
    pub fn tokens(&self) -> Option<SessionTokens> {
        self.tokens.lock().ok().and_then(|t| t.clone())
    }

    /// Persist a single LLM call's raw `usage` into the session token
    /// accumulator (ADR-027).
    ///
    /// Semantics:
    /// - `last_input` / `last_output` are **always** overwritten with the
    ///   raw Provider values, including zero. This makes the snapshot
    ///   honestly reflect what the Provider returned (e.g. when
    ///   `prompt_tokens_reliable == false` and the local tokenizer
    ///   fallback would have produced a different number).
    /// - `total_input` only accumulates when `usage.prompt_tokens > 0`;
    ///   a Provider fallback does not silently pollute the running sum.
    /// - `total_output` always accumulates (Providers report completion
    ///   counts more reliably than prompt counts, and a zero here is
    ///   usually a true zero — e.g. an aborted streaming response).
    ///
    /// `last_active_at` is bumped and the meta file is rewritten on every
    /// call. Callers should invoke this once per LLM round-trip, not on
    /// every streaming event.
    pub fn accumulate_llm_usage(&self, usage: &UsageInfo) {
        if let Ok(mut guard) = self.tokens.lock() {
            let total_input = if usage.prompt_tokens > 0 {
                guard
                    .as_ref()
                    .map(|t| t.total_input)
                    .unwrap_or(0)
                    .saturating_add(usage.prompt_tokens)
            } else {
                guard.as_ref().map(|t| t.total_input).unwrap_or(0)
            };
            let total_output = guard
                .as_ref()
                .map(|t| t.total_output)
                .unwrap_or(0)
                .saturating_add(usage.completion_tokens);
            *guard = Some(SessionTokens {
                last_input: usage.prompt_tokens,
                last_output: usage.completion_tokens,
                total_input,
                total_output,
            });
        }
        self.write_meta();
    }
}

// Send + Sync are auto-derived: all fields (String, Mutex<X: Send>,
// AtomicBool/AtomicU64, Instant, UnboundedSender, PathBuf) are Send + Sync.

/// Manual `Clone` so the struct can be cheaply captured into a spawned task
/// (e.g. session-end distillation that needs to call
/// [`Self::accumulate_llm_usage`] after the parent has already `.close()`d).
///
/// Each clone gets fresh `Mutex` / `Atomic` guards holding the same inner
/// value at the moment of cloning; later mutations on one clone are not
/// observed by another clone (the clone is a logical snapshot for read-only
/// purposes — callers must not mutate shared state from two clones
/// concurrently). The writer-thread channel sender is shared so any clone
/// can still append JSONL entries until the writer thread receives
/// `Shutdown`.
impl Clone for ConversationSession {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            created_at: self.created_at.clone(),
            title_set: AtomicBool::new(self.title_set.load(Ordering::Relaxed)),
            current_title: std::sync::Mutex::new(
                self.current_title.lock().ok().and_then(|t| t.clone()),
            ),
            workspace_id: std::sync::Mutex::new(
                self.workspace_id.lock().ok().and_then(|w| w.clone()),
            ),
            model: std::sync::Mutex::new(self.model.lock().ok().and_then(|m| m.clone())),
            provider: std::sync::Mutex::new(
                self.provider.lock().ok().and_then(|p| p.clone()),
            ),
            reasoning_effort: std::sync::Mutex::new(
                self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            ),
            temperature: std::sync::Mutex::new(self.temperature.lock().ok().and_then(|t| *t)),
            tokens: std::sync::Mutex::new(self.tokens.lock().ok().and_then(|t| t.clone())),
            message_count: AtomicU64::new(self.message_count.load(Ordering::Relaxed)),
            last_meta_write: std::sync::Mutex::new(
                self.last_meta_write
                    .lock()
                    .ok()
                    .map(|i| *i)
                    .unwrap_or_else(Instant::now),
            ),
            sender: self.sender.clone(),
            session_file_path: self.session_file_path.clone(),
            conversations_dir: self.conversations_dir.clone(),
        }
    }
}

impl Drop for ConversationSession {
    fn drop(&mut self) {
        // Force-flush the final state to the meta file so the frontend sees
        // the correct message_count and last_active_at even if the last
        // `append_message` fell within the cooldown window.
        self.write_meta();
    }
}

/// Generate a new session ID.
///
/// Format: `{YYYYMMDD_HHMMSS}_{6-char short UUID}`
/// Example: `20260503_143022_a1b2c3`
pub fn generate_session_id() -> String {
    let now = chrono::Local::now();
    let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
    let short_uuid = uuid::Uuid::new_v4().to_string();
    let short_uuid = &short_uuid[..6];
    format!("{}_{}", timestamp, short_uuid)
}

/// Information about a scanned session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Session identifier
    pub session_id: String,
    /// ISO 8601 creation timestamp
    pub created_at: String,
    /// Number of messages in the session
    pub message_count: u32,
    /// Optional session title
    pub title: Option<String>,
    /// Whether the session metadata was recovered from a corrupted first line
    pub corrupted: bool,
    /// Per-session model selection (ADR-012), from JSONL metadata
    pub model: Option<String>,
    /// Per-session provider selection (ADR-012), from JSONL metadata
    pub provider: Option<String>,
    /// Per-session workspace selection, from JSONL metadata
    pub workspace_id: Option<String>,
}

// ── Session Index (fast O(1) lookup) ───────────────────────────────────────
//
// ADR-024: the index.json + SessionIndexEntry + SessionIndex system has
// been superseded by per-session meta files (`conversations/meta/*.json`).
// Use `scan_sessions_from_meta()` for listing and `read_session_meta()` for
// single-session lookup.

// ── Per-session meta file I/O (ADR-024) ───────────────────────────────────

// ── Per-session meta file I/O (ADR-024) ───────────────────────────────────

/// Subdirectory where per-session meta files live.
const META_DIR: &str = "meta";

/// Build the path to `conversations/meta/{session_id}.json`.
fn meta_path(conversations_dir: &Path, session_id: &str) -> PathBuf {
    conversations_dir
        .join(META_DIR)
        .join(format!("{}.json", session_id))
}

/// Atomically write session metadata to `conversations/meta/{session_id}.json`.
///
/// Uses write-to-temp + rename to prevent corruption on crash.
pub fn write_session_meta(conversations_dir: &Path, meta: &SessionMeta) -> std::io::Result<()> {
    let meta_dir = conversations_dir.join(META_DIR);
    std::fs::create_dir_all(&meta_dir)?;

    let target = meta_path(conversations_dir, &meta.session_id);
    let temp = meta_dir.join(format!("{}.json.tmp", meta.session_id));

    let json = serde_json::to_string_pretty(meta)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&temp, json)?;
    std::fs::rename(&temp, &target)?;
    Ok(())
}

/// Read session metadata from `conversations/meta/{session_id}.json`.
pub fn read_session_meta(
    conversations_dir: &Path,
    session_id: &str,
) -> std::io::Result<SessionMeta> {
    let path = meta_path(conversations_dir, session_id);
    let data = std::fs::read_to_string(&path)?;
    serde_json::from_str(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Scan all session meta files and return them sorted by `last_active_at` descending.
///
/// Reads every `.json` file in `conversations/meta/`.  Files that fail to parse
/// are silently skipped (the caller can detect missing sessions via the returned
/// `Vec` length vs. the `.jsonl` file count).
pub fn scan_sessions_from_meta(conversations_dir: &Path) -> Vec<(String, SessionMeta)> {
    let meta_dir = conversations_dir.join(META_DIR);
    let Ok(rd) = std::fs::read_dir(&meta_dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<(String, SessionMeta)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|e| {
            let data = std::fs::read_to_string(e.path()).ok()?;
            let meta: SessionMeta = serde_json::from_str(&data).ok()?;
            Some((meta.session_id.clone(), meta))
        })
        .collect();
    // Sort descending by last_active_at (newest first).
    sessions.sort_by(|(_, a), (_, b)| b.last_active_at.cmp(&a.last_active_at));
    sessions
}

/// Prune excess sessions when the index exceeds `max_sessions`.
///
/// Sessions are ordered by `last_active_at` (oldest first) and removed
/// until the count is within the limit.  Each pruned session has its JSONL
/// file permanently deleted.
///
/// Returns the number of sessions pruned.
///
/// # Safety
///
/// This function only looks at the index file; it does NOT interact with
/// `SessionManager`.  By design it can only prune sessions that have been
/// evicted from memory (idle timeout), because active sessions constantly
/// update their `last_active_at` and will never be the oldest.
pub(crate) fn prune_excess_sessions(
    conversations_dir: &Path,
    max_sessions: usize,
) -> usize {
    if max_sessions == 0 {
        return 0;
    }

    // ADR-024: scan per-session meta files instead of index.json.
    let sessions = scan_sessions_from_meta(conversations_dir);

    if sessions.len() <= max_sessions {
        return 0;
    }

    // Sort by last_active_at ascending (oldest first).
    // `scan_sessions_from_meta` returns (session_id, meta) tuples sorted
    // newest-first; we need oldest-first for pruning.
    let mut sorted: Vec<_> = sessions
        .iter()
        .map(|(sid, meta)| (sid.as_str(), meta.last_active_at.as_str()))
        .collect();
    sorted.sort_by(|a, b| a.1.cmp(b.1));

    let to_remove = sessions.len() - max_sessions;
    let mut pruned = 0usize;

    for (session_id, _) in sorted.iter().take(to_remove) {
        let jsonl_path = conversations_dir.join(format!("{}.jsonl", session_id));
        let archive_path = conversations_dir.join(format!("{}.jsonl.archive", session_id));
        let meta_path = conversations_dir
            .join("meta")
            .join(format!("{}.json", session_id));

        // ADR-024: archive the JSONL file (rename) instead of deleting.
        match std::fs::rename(&jsonl_path, &archive_path) {
            Ok(()) => {
                tracing::debug!(
                    session_id = %session_id,
                    archive = %archive_path.display(),
                    "Archived excess session JSONL"
                );
                pruned += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // JSONL already gone — still count as pruned.
                pruned += 1;
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    path = %jsonl_path.display(),
                    error = %e,
                    "Failed to archive JSONL file during session pruning"
                );
                continue;
            }
        }

        // Delete the per-session meta file.
        if let Err(e) = std::fs::remove_file(&meta_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "Failed to delete meta file during session pruning"
            );
        }
    }

    if pruned > 0 {
        tracing::info!(
            pruned,
            remaining = sessions.len() - pruned,
            "Archived excess sessions"
        );
    }

    pruned
}

/// Paginated message result.
///
/// Pagination uses an `offset` model — direction is **derived from the
/// offset itself, not passed as a request parameter**:
///
/// - `offset = 0`  → the most recent `limit` raw entries.
/// - `offset = K`  → the `limit` raw entries that are K positions behind the
///   most recent (i.e. entries `[total-K-limit, total-K)`).
///
/// To scroll **older** (toward smaller offsets / older messages):
///   `next_offset = offset + returned_limit`.
/// To scroll **newer** (toward the bottom of the conversation):
///   `next_offset = max(0, offset - returned_limit)`.
/// `offset == 0` means the cache window already touches the newest
/// message (nothing newer to load from the bottom of the conversation).
#[derive(Debug, Clone)]
pub struct PaginatedMessages {
    /// Messages in the current page, in chronological order
    /// (oldest → newest within the page).
    pub messages: Vec<ConversationEntry>,
    /// Echo of the requested offset.
    pub offset: u64,
    /// Number of messages actually returned (≤ requested limit).
    pub limit: u32,
    /// Total number of message entries in the session JSONL
    /// (metadata header lines are not counted).
    pub total: u64,
}

// ── ADR-021: StreamingStateMap types ───────────────────────────────────────

/// An incomplete line: a message currently being streamed by the LLM
/// but not yet flushed to the JSONL file.
///
/// The frontend reads this via `read_messages_since()` to get the
/// in-progress content without waiting for a natural flush boundary.
#[derive(Debug, Clone)]
pub struct StreamingLine {
    /// The line number this will become in JSONL (0-based; 0 = metadata).
    pub line_number: usize,
    /// Role: "assistant" | "thought".
    pub role: String,
    /// ADR-035 M2: stable message id assigned at streaming line creation.
    /// Carried by every `stream_delta` push and the final `record_complete`
    /// event so the frontend can match the active buffer to the finalized
    /// record. Also written to JSONL via `append_message_with_id` so the
    /// persisted entry shares the same id.
    pub message_id: String,
    /// Current accumulated content (grows with each Delta).
    pub accumulated_content: String,
    /// ISO 8601 timestamp when streaming started.
    pub started_at: String,
    /// Unix epoch milliseconds when streaming started (for timing metadata
    /// in metadata: {startTime, endTime} written to JSONL on flush).
    pub started_at_ms: i64,
}

/// Delta portion of a streaming line returned to the frontend.
///
/// Only carries the *new* content since `char_offset`, not the full
/// accumulated content. This keeps poll responses small.
#[derive(Debug, Clone, Serialize)]
pub struct StreamingLineDelta {
    /// The line number this streaming line will become in JSONL.
    pub line: usize,
    /// Role: "assistant" | "thought".
    pub role: String,
    /// Only the new content since the requested `char_offset`.
    pub content: String,
    /// Current total character length of the accumulated content.
    /// The frontend uses this as the next `line_char_offset`.
    pub char_offset: usize,
}

/// Result of `read_messages_since()`.
#[derive(Debug, Clone)]
pub struct ReadMessagesSinceResult {
    /// New complete lines from JSONL (after `line_number`).
    pub messages: Vec<ConversationEntry>,
    /// Incomplete streaming line delta, if one exists for this session.
    pub streaming: Option<StreamingLineDelta>,
    /// Total lines in the JSONL file (including metadata line 0).
    pub total_lines: usize,
}

// ── Backend-managed delivery cursor (ADR-025) ───────────────────────────

/// Per-session delivery cursor — the backend tracks how much data has been
/// delivered to the frontend.  The frontend never sends coordinates; it
/// simply polls with `incremental=true` and the backend uses this cursor
/// to determine what to return.
///
/// * `line_number` — number of complete JSONL lines already delivered.
/// * `char_offset` — number of characters already delivered from the
///   current in-progress streaming line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeliveryCursor {
    pub line_number: usize,
    pub char_offset: usize,
}

/// Result of `read_messages_since_cursor()`.
#[derive(Debug, Clone)]
pub struct ReadMessagesSinceCursorResult {
    /// New complete lines from JSONL (batch-limited by `limit`).
    pub messages: Vec<ConversationEntry>,
    /// Incomplete streaming line delta, if one exists for this session.
    pub streaming: Option<StreamingLineDelta>,
    /// Total lines in the JSONL file.
    pub total_lines: usize,
    /// Whether more undelivered complete lines remain (batch catch-up signal).
    pub has_more: bool,
    /// The updated cursor after this read — caller should persist this.
    pub new_cursor: DeliveryCursor,
}

/// Shared map from SessionId to the current incomplete streaming line.
///
/// Written by AgentLoop on each Delta, read by the HTTP handler on poll.
/// Wrapped in `Arc<RwLock>` for concurrent access across tokio tasks.
pub type StreamingStateMap = Arc<RwLock<HashMap<String, StreamingLine>>>;

/// Find the most recently active session.
///
/// ADR-024: scans per-session meta files instead of index.json.
pub fn find_latest_session(conversations_dir: &Path) -> Option<String> {
    // ADR-024: scan per-session meta files.
    scan_sessions_from_meta(conversations_dir)
        .first()
        .map(|(sid, _)| sid.clone())
}

/// Asynchronously scan all sessions from the index file.
///
/// Reads `conversations/index.json` and returns a paginated list of
/// `SessionInfo` sorted by `last_active_at` descending (newest first).
/// Falls back to a full directory scan + index rebuild if the index
/// file is missing or corrupted.
///
/// ADR-028: in addition to the page slice, the join handle now also
/// returns `(agent_total_input, agent_total_output)` summed across every
/// session on disk (full-scan totals, not just the current page). The
/// caller uses these to bootstrap [`AgentCore`]'s in-process counters
/// via atomic-max merge, and to forward them to the Desktop App as a
/// fallback data source for the agent-total token display.
pub fn scan_sessions_async(
    conversations_dir: PathBuf,
    page: Option<u32>,
    size: Option<u32>,
) -> tokio::task::JoinHandle<(Vec<SessionInfo>, usize, (u64, u64))> {
    tokio::task::spawn_blocking(move || {
        // ADR-024: scan per-session meta files instead of index.json.
        let sessions = scan_sessions_from_meta(&conversations_dir);

        // ADR-028: full-scan aggregate. Walk every meta file (not just
        // the current page) so a single scan can rebuild the baseline
        // even when `size` is small (e.g. title-only fetches). `None`
        // sessions and sessions with `prompt_tokens == 0` (Provider
        // fallback) are skipped on the input side; the output side is
        // always accumulated (matches `ConversationSession::accumulate_llm_usage`).
        let (agent_total_input, agent_total_output) = sessions
            .iter()
            .fold((0u64, 0u64), |(acc_in, acc_out), (_, meta)| {
                let meta_in = meta
                    .tokens
                    .as_ref()
                    .map(|t| t.total_input)
                    .unwrap_or(0);
                let meta_out = meta
                    .tokens
                    .as_ref()
                    .map(|t| t.total_output)
                    .unwrap_or(0);
                (acc_in.saturating_add(meta_in), acc_out.saturating_add(meta_out))
            });

        let total = sessions.len();
        let page = page.unwrap_or(1).max(1) as usize;
        let size = size.unwrap_or(20).max(1) as usize;
        let start = (page - 1) * size;
        let end = (start + size).min(total);

        let infos = sessions[start..end]
            .iter()
            .map(|(sid, meta)| SessionInfo {
                session_id: sid.clone(),
                created_at: meta.created_at.clone(),
                message_count: meta.message_count as u32,
                title: meta.title.clone(),
                corrupted: meta.corrupted,
                model: meta.model.clone(),
                provider: meta.provider.clone(),
                workspace_id: meta.workspace_id.clone(),
            })
            .collect();

        (infos, total, (agent_total_input, agent_total_output))
    })
}

/// Read messages from a JSONL file with offset-based pagination.
///
/// - `offset`: how many of the most recent raw entries to skip over.
///   `0` returns the latest `limit` entries; larger values return
///   older windows. The caller (frontend) moves this number back and
///   forth to scroll — there is **no `direction` parameter**, the
///   direction is encoded entirely in the offset arithmetic.
/// - `limit`: maximum number of **raw entries** to return. A raw entry
///   is one JSONL line (one thought / tool_call / tool_result /
///   user / assistant row). A single displayed "explore group" may
///   count as multiple raw entries depending on how many
///   thoughts/tool_calls/tool_results it contains. `display group`
///   collapsing (think + tool_call + tool_result → 1 chip) is a
///   **pure-frontend UI abstraction** — see `displayMessages` in
///   `ChatPanel.tsx`. The backend never reasons about groups.
///
/// # Why raw entries and not groups?
///
/// `offset` and `limit` are both measured in raw entries so they share
/// a single dimension and arithmetic `next_offset = prev_offset +
/// prev_limit` is always correct. Group-aware pagination (slicing by
/// display groups) would require every protocol hop to recompute group
/// boundaries — a hidden coupling that reintroduces the partial-cut
/// bug family (a mega-explore group straddling a window boundary).
///
/// Implementation: read the whole JSONL once into memory in one pass.
/// Sessions are expected to stay in the low-megabyte range; this
/// trades raw-IO efficiency for a one-pass, branch-free paging
/// implementation that needs no direction-aware codepath and no
/// byte-offset cursors.
///
/// Returns messages in chronological order (oldest → newest within
/// the page).
pub fn read_messages_paginated(path: &Path, offset: u64, limit: u32) -> Result<PaginatedMessages> {
    // ADR-024: no metadata header line on disk; data lives in a sidecar
    // file (see `<sid>.json`).  We still defensively skip any header
    // line that contains both `"version"` and `"session_id"` so legacy
    // files (or hand-edited JSONLs) cannot leak metadata into message
    // lists.
    let file_len = std::fs::metadata(path)?.len();
    if file_len == 0 {
        return Ok(PaginatedMessages {
            messages: Vec::new(),
            offset,
            limit: 0,
            total: 0,
        });
    }

    let content = std::fs::read_to_string(path)?;
    let mut raw_lines: Vec<String> = Vec::new();
    for raw in content.split('\n') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains("\"version\"") && trimmed.contains("\"session_id\"") {
            continue;
        }
        raw_lines.push(trimmed.to_owned());
    }
    drop(content); // release the big string as soon as possible

    let total = raw_lines.len() as u64;

    let mut entries: Vec<ConversationEntry> = Vec::with_capacity(raw_lines.len());
    for s in &raw_lines {
        match serde_json::from_str::<ConversationEntry>(s) {
            Ok(e) => entries.push(e),
            Err(e) => tracing::warn!("Skipping invalid JSONL line: {}", e),
        }
    }
    drop(raw_lines); // release the per-line Strings after parsing

    // If `offset` skips past the end, the page is empty (harmless boundary).
    if offset >= total {
        return Ok(PaginatedMessages {
            messages: Vec::new(),
            offset,
            limit: 0,
            total,
        });
    }

    // Slice the raw entries into a [start_idx, end_idx) window.
    //   end_idx   = total - offset                 (exclusive, from oldest)
    //   start_idx = max(0, end_idx - limit)
    //
    // The next page's offset is then simply `prev_offset + limit` —
    // both sides in raw entries, no cross-dimensional arithmetic.
    let end_idx = total - offset;
    let start_idx = end_idx.saturating_sub(limit as u64);
    let messages = entries[start_idx as usize..end_idx as usize].to_vec();
    let actual_limit = messages.len() as u32;

    Ok(PaginatedMessages {
        messages,
        offset,
        limit: actual_limit,
        total,
    })
}

// ── ADR-021: Incremental read with line-number coordinates ─────────────────

/// Count the total number of lines in a JSONL file.
///
/// Line 0 is the metadata header. Returns 0 for empty/non-existent files.
pub fn count_jsonl_lines(path: &Path) -> std::io::Result<usize> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let reader = BufReader::new(file);
    let mut count = 0usize;
    for line in reader.lines() {
        if line.is_ok() {
            count += 1;
        }
    }
    Ok(count)
}

/// Read messages from a JSONL file since a given line-number coordinate.
///
/// ADR-021: This is the Runtime-side handler for incremental poll requests.
/// It returns:
/// - `messages`: new complete lines from JSONL with index >= `line_number`
/// - `streaming`: delta of the in-progress streaming line (if any)
/// - `total_lines`: current total line count in the JSONL file
///
/// # Arguments
/// - `path`: Path to the session JSONL file.
/// - `line_number`: Number of complete lines already read by the frontend
///   (a COUNT, not an index). The function returns lines with index >= line_number.
/// - `line_char_offset`: Number of characters already read from the streaming
///   line. The function returns only new characters after this offset.
/// - `streaming_lines`: Shared StreamingStateMap for in-progress lines.
/// - `session_id`: Session ID to look up in `streaming_lines`.
///
/// # Clamping
/// If `line_number` exceeds `total_lines` (e.g., JSONL was externally
/// truncated), it is clamped to `total_lines`. Similarly, if the streaming
/// line's `char_offset` is less than the requested `line_char_offset`
/// (should not happen in normal operation), the full content is returned.
pub fn read_messages_since(
    path: &Path,
    line_number: usize,
    line_char_offset: usize,
    streaming_lines: &StreamingStateMap,
    session_id: &str,
    cached_total_lines: usize,
) -> Result<ReadMessagesSinceResult> {
    // ADR-022: Use `cached_total_lines` (committed_lines from the writer
    // thread) as the authoritative count.  Unlike the old `total_lines`
    // which was incremented before the async disk write, `committed_lines`
    // is only incremented AFTER the write — so it never lies about what's
    // actually on disk.
    let total_lines = if cached_total_lines > 0 {
        cached_total_lines
    } else {
        count_jsonl_lines(path).unwrap_or(0)
    };

    // Clamp line_number to total_lines (defensive against external truncation)
    let line_number = line_number.min(total_lines);

    // Read new complete lines from JSONL (lines with index >= line_number).
    // line_number is a COUNT (number of lines already read by the frontend),
    // so the first unread line is at index line_number (0-based).
    let mut messages: Vec<ConversationEntry> = Vec::new();
    if line_number < total_lines
        && let Ok(file) = std::fs::File::open(path)
    {
        let reader = BufReader::new(file);
        for (idx, line) in reader.lines().enumerate() {
            // Skip lines the frontend has already read (indices 0..line_number-1)
            if idx < line_number {
                continue;
            }
            if let Ok(content) = line
                && let Ok(entry) = serde_json::from_str::<ConversationEntry>(&content)
            {
                messages.push(entry);
            }
        }
    }

    // Read streaming line delta — clone under read lock, compute delta outside lock
    // to minimize write-lock contention from concurrent Delta appends.
    let streaming = {
        let map = streaming_lines.read().unwrap();
        map.get(session_id).map(|sl| StreamingLine {
            line_number: sl.line_number,
            role: sl.role.clone(),
            message_id: sl.message_id.clone(),
            accumulated_content: sl.accumulated_content.clone(),
            started_at: sl.started_at.clone(),
            started_at_ms: sl.started_at_ms,
        })
    };
    let streaming = streaming.map(|sl| {
        let current_len = sl.accumulated_content.chars().count();
        // ADR-022: Detect stale `line_char_offset` from a previous streaming
        // line that has since been flushed to JSONL (role transition triggers
        // flush + new line). Applying a stale offset here would skip the
        // beginning of this fresh line and permanently drop its opening
        // characters.
        //
        // Case 1: `line_number < sl.line_number` — the frontend's line number
        //   is behind, so the offset belongs to a previous (now flushed) line.
        // Case 2: `line_char_offset > current_len` — the offset exceeds the
        //   current content length, which is impossible for this line (the
        //   offset is always set to `current_len` in our response). It must
        //   be from a previous (longer) line that was flushed.
        // In both cases, reset to 0 to return the full content of this line.
        let effective_offset = if line_number < sl.line_number || line_char_offset > current_len {
            0
        } else {
            line_char_offset
        };
        let delta_content: String = sl.accumulated_content.chars().skip(effective_offset).collect();
        StreamingLineDelta {
            line: sl.line_number,
            role: sl.role,
            content: delta_content,
            char_offset: current_len,
        }
    });

    Ok(ReadMessagesSinceResult {
        messages,
        streaming,
        total_lines,
    })
}

/// Backend-cursor variant of [`read_messages_since`].
///
/// Instead of receiving `line_number` / `line_char_offset` from the frontend,
/// this function takes a [`DeliveryCursor`] that the backend maintains
/// per-session.  It returns at most `limit` complete JSONL lines, plus the
/// streaming delta, plus a `has_more` flag for batch catch-up.
///
/// ## Batch delivery
///
/// When the cursor lags far behind `total_lines` (e.g. the session was in
/// the background for minutes), this function returns at most `limit` lines
/// and sets `has_more = true`.  The caller should immediately poll again
/// until `has_more` is false.
///
/// ## Streaming delta
///
/// The streaming delta is always returned (even during batch catch-up) so
/// the frontend can update the streaming placeholder incrementally.
///
/// ## Stale char_offset detection
///
/// If `cursor.line_number < streaming.line_number`, the `char_offset` belongs
/// to a previous (now flushed) streaming line and is reset to 0 — same logic
/// as [`read_messages_since`].
pub fn read_messages_since_cursor(
    path: &Path,
    cursor: DeliveryCursor,
    limit: usize,
    streaming_lines: &StreamingStateMap,
    session_id: &str,
    cached_total_lines: usize,
) -> Result<ReadMessagesSinceCursorResult> {
    let total_lines = if cached_total_lines > 0 {
        cached_total_lines
    } else {
        count_jsonl_lines(path).unwrap_or(0)
    };

    // Clamp cursor.line_number to total_lines (defensive against truncation).
    let start = cursor.line_number.min(total_lines);
    let end = (start + limit).min(total_lines);

    // Read complete lines [start..end).
    let mut messages: Vec<ConversationEntry> = Vec::new();
    if start < end
        && let Ok(file) = std::fs::File::open(path)
    {
        let reader = BufReader::new(file);
        for (idx, line) in reader.lines().enumerate() {
            if idx < start {
                continue;
            }
            if idx >= end {
                break;
            }
            if let Ok(content) = line
                && let Ok(entry) = serde_json::from_str::<ConversationEntry>(&content)
            {
                messages.push(entry);
            }
        }
    }

    let new_line_number = end;
    let has_more = new_line_number < total_lines;

    // Read streaming line delta.
    //
    // Only return streaming delta when the cursor has caught up to the
    // streaming line (new_line_number >= streaming.line_number).  During
    // batch catch-up (cursor behind streaming line), the delta is not
    // useful — the frontend is processing complete lines and the streaming
    // content will be delivered in full once the cursor catches up.
    let streaming = {
        let map = streaming_lines.read().unwrap();
        map.get(session_id).map(|sl| StreamingLine {
            line_number: sl.line_number,
            role: sl.role.clone(),
            message_id: sl.message_id.clone(),
            accumulated_content: sl.accumulated_content.clone(),
            started_at: sl.started_at.clone(),
            started_at_ms: sl.started_at_ms,
        })
    };
    let streaming = streaming.and_then(|sl| {
        // Only return delta if cursor has caught up to this streaming line.
        if new_line_number < sl.line_number {
            return None;
        }
        let current_len = sl.accumulated_content.chars().count();
        // If cursor was behind the streaming line before reading complete
        // lines, the char_offset belongs to a previous (flushed) streaming
        // line and must be reset to 0.
        let effective_offset = if cursor.line_number < sl.line_number || cursor.char_offset > current_len {
            0
        } else {
            cursor.char_offset
        };
        let delta_content: String = sl.accumulated_content.chars().skip(effective_offset).collect();
        Some(StreamingLineDelta {
            line: sl.line_number,
            role: sl.role,
            content: delta_content,
            char_offset: current_len,
        })
    });

    let new_char_offset = streaming.as_ref().map(|s| s.char_offset).unwrap_or(0);

    Ok(ReadMessagesSinceCursorResult {
        messages,
        streaming,
        total_lines,
        has_more,
        new_cursor: DeliveryCursor {
            line_number: new_line_number,
            char_offset: new_char_offset,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_generate_session_id() {
        let id = generate_session_id();
        // Format: YYYYMMDD_HHMMSS_xxxxxx (6-char short UUID)
        let parts: Vec<&str> = id.split('_').collect();
        assert_eq!(
            parts.len(),
            3,
            "Session ID should have 3 parts separated by underscores"
        );
        assert_eq!(parts[0].len(), 8, "Date part should be 8 chars (YYYYMMDD)");
        assert_eq!(parts[1].len(), 6, "Time part should be 6 chars (HHMMSS)");
        assert_eq!(parts[2].len(), 6, "Short UUID should be 6 chars");
        assert!(
            parts[0].chars().all(|c| c.is_ascii_digit()),
            "Date should be digits"
        );
        assert!(
            parts[1].chars().all(|c| c.is_ascii_digit()),
            "Time should be digits"
        );
    }

    #[test]
    fn test_conversation_writer_basic() {
        let temp_dir = TempDir::new().unwrap();
        let work_dir = temp_dir.path();
        let session_id = generate_session_id();
        let agent_id = "com.test.agent";

        // Create session and write messages
        let session = ConversationSession::new(
            work_dir,
            &session_id,
            SessionConfig {
                agent_id: agent_id.to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0, Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "Hello", None);
        session.append_message(
            "assistant",
            "Hi there!",
            Some(serde_json::json!({"model": "test-model"})),
        );
        session.append_message("tool_call", r#"{"path": "test.txt"}"#, None);

        // Give writer thread time to process
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Close session
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            session.close().await.unwrap();
        });

        // Verify file contents
        let file_path = work_dir
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let content = std::fs::read_to_string(&file_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "ADR-024: 3 data lines, no metadata header");

        // First line is user message
        let entry: ConversationEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry.role, "user");
        assert_eq!(entry.content, "Hello");
        assert!(entry.metadata.is_none());

        // Second line is assistant message
        let entry: ConversationEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(entry.role, "assistant");
        assert_eq!(entry.content, "Hi there!");
        assert_eq!(
            entry.metadata,
            Some(serde_json::json!({"model": "test-model"}))
        );

        // Third line is tool_call
        let entry: ConversationEntry = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(entry.role, "tool_call");
        assert_eq!(entry.content, r#"{"path": "test.txt"}"#);

        // Verify meta file exists
        let meta_path = work_dir
            .join("conversations")
            .join("meta")
            .join(format!("{}.json", session_id));
        assert!(meta_path.exists(), "Per-session meta file must exist");
        let meta: SessionMeta = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert_eq!(meta.version, CONVERSATION_FORMAT_VERSION);
        assert_eq!(meta.session_id, session_id);
        assert_eq!(meta.agent_id, agent_id);
    }

    #[test]
    fn test_find_latest_session() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        // ADR-024: find_latest_session scans per-session meta files.
        let meta_dir = conv_dir.join("meta");
        std::fs::create_dir_all(&meta_dir).unwrap();

        let base = chrono::Utc::now();
        let ids = vec![
            (
                "20260503_100000_aaaaaa",
                (base - chrono::Duration::hours(3)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            (
                "20260503_120000_bbbbbb",
                base.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), // newest
            ),
            (
                "20260503_110000_cccccc",
                (base - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
        ];
        for (id, ts) in &ids {
            let meta = SessionMeta {
                version: CONVERSATION_FORMAT_VERSION,
                session_id: id.to_string(),
                agent_id: "com.test".to_string(),
                created_at: ts.clone(),
                title: None,
                workspace_id: None,
                model: None,
                provider: None,
                reasoning_effort: None,
                temperature: None,
                message_count: 0,
                last_active_at: ts.clone(),
                tokens: None,
                last_compaction_offset: None,
                corrupted: false,
            };
            let meta_path = meta_dir.join(format!("{}.json", id));
            std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
        }

        let latest = find_latest_session(&conv_dir);
        assert_eq!(latest, Some("20260503_120000_bbbbbb".to_string()));
    }

    #[test]
    fn test_read_messages_paginated() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        let session_id = "20260503_100000_test01";
        let file_path = conv_dir.join(format!("{}.jsonl", session_id));

        // ADR-024: write 5 messages directly (no metadata header).
        {
            let mut file = std::fs::File::create(&file_path).unwrap();

            for i in 0..5 {
                let entry = ConversationEntry {
                    id: format!("msg-{}", i),
                    ts: chrono::Utc::now().to_rfc3339(),
                    role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                    content: format!("Message {}", i),
                    metadata: None,
                    kind: None,
                };
                serde_json::to_writer(&mut file, &entry).unwrap();
                writeln!(file).unwrap();
            }
        }

        // Page 1: offset=0, limit=10 → full conversation, total=5.
        let page = read_messages_paginated(&file_path, 0, 10).unwrap();
        assert_eq!(page.offset, 0);
        assert_eq!(page.limit, 5);
        assert_eq!(page.total, 5);
        assert_eq!(page.messages.len(), 5);
        assert_eq!(page.messages[0].content, "Message 0");
        assert_eq!(page.messages[4].content, "Message 4");

        // Page 2: offset=0, limit=2 → latest 2 messages (Message 3, 4).
        let page = read_messages_paginated(&file_path, 0, 2).unwrap();
        assert_eq!(page.offset, 0);
        assert_eq!(page.limit, 2);
        assert_eq!(page.total, 5);
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.messages[0].content, "Message 3");
        assert_eq!(page.messages[1].content, "Message 4");

        // Page 3: offset=2, limit=2 → next 2 older (Message 1, 2).
        let page2 = read_messages_paginated(&file_path, 2, 2).unwrap();
        assert_eq!(page2.offset, 2);
        assert_eq!(page2.limit, 2);
        assert_eq!(page2.total, 5);
        assert_eq!(page2.messages.len(), 2);
        assert_eq!(page2.messages[0].content, "Message 1");
        assert_eq!(page2.messages[1].content, "Message 2");

        // Page 4: offset=4, limit=2 → only Message 0 remains.
        let page3 = read_messages_paginated(&file_path, 4, 2).unwrap();
        assert_eq!(page3.offset, 4);
        assert_eq!(page3.limit, 1);
        assert_eq!(page3.total, 5);
        assert_eq!(page3.messages.len(), 1);
        assert_eq!(page3.messages[0].content, "Message 0");

        // Page 5: offset past the end → empty page, offset/total still meaningful.
        let out_of_range = read_messages_paginated(&file_path, 10, 2).unwrap();
        assert_eq!(out_of_range.offset, 10);
        assert_eq!(out_of_range.limit, 0);
        assert_eq!(out_of_range.total, 5);
        assert!(out_of_range.messages.is_empty());

        // Page 6: offset=0, limit=3 → use this as the anchor for scroll-newer.
        // The frontend arrives at a page by knowing its own offset+limit; here
        // the boundary conditions are exercised directly, no cursors involved.
        let latest3 = read_messages_paginated(&file_path, 0, 3).unwrap();
        assert_eq!(latest3.offset, 0);
        assert_eq!(latest3.limit, 3);
        assert_eq!(latest3.total, 5);
        assert_eq!(latest3.messages[0].content, "Message 2");
        assert_eq!(latest3.messages[1].content, "Message 3");
        assert_eq!(latest3.messages[2].content, "Message 4");

        // Symmetric scroll-newer step: from offset=2 (got Message 1,2), ask
        // for offset=2-2=0 to confirm we land on the same slice the first
        // page produced.
        let slide_to_bottom = read_messages_paginated(&file_path, 2, 2).unwrap();
        assert_eq!(slide_to_bottom.messages[0].content, "Message 1");
        assert_eq!(slide_to_bottom.messages[1].content, "Message 2");

        // Empty file → empty page with zeroed totals.
        std::fs::write(&file_path, "").unwrap();
        let empty = read_messages_paginated(&file_path, 0, 10).unwrap();
        assert_eq!(empty.offset, 0);
        assert_eq!(empty.limit, 0);
        assert_eq!(empty.total, 0);
        assert!(empty.messages.is_empty());
    }

    #[test]
    fn test_session_resume() {
        let temp_dir = TempDir::new().unwrap();
        let work_dir = temp_dir.path();
        let session_id = "20260503_100000_resume";
        let agent_id = "com.test.resume";

        // Create initial session
        let session = ConversationSession::new(
            work_dir,
            session_id,
            SessionConfig {
                agent_id: agent_id.to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0, Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "First message", None);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            session.close().await.unwrap();
        });

        // Resume session
        let resumed = ConversationSession::resume(work_dir, session_id, Arc::new(AtomicUsize::new(0))).unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(resumed.agent_id(), agent_id);

        resumed.append_message("assistant", "Resumed response", None);
        std::thread::sleep(std::time::Duration::from_millis(50));

        rt.block_on(async {
            resumed.close().await.unwrap();
        });

        // Verify file has both messages (ADR-024: no metadata header)
        let file_path = work_dir
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let content = std::fs::read_to_string(&file_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "ADR-024: 2 data lines, no metadata header");

        let entry1: ConversationEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry1.content, "First message");

        let entry2: ConversationEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(entry2.content, "Resumed response");
    }

    #[test]
    fn test_scan_sessions_includes_corrupted() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        // ADR-024: create per-session meta files instead of JSONL headers.
        // Create a valid session with meta file.
        let valid_id = "20260503_100000_valid";
        let meta_dir = conv_dir.join("meta");
        std::fs::create_dir_all(&meta_dir).unwrap();
        let meta_path = meta_dir.join(format!("{}.json", valid_id));
        let valid_meta = SessionMeta {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: valid_id.to_string(),
            agent_id: "com.test".to_string(),
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            title: Some("Valid".to_string()),
            workspace_id: None,
            model: None,
            provider: None,
            reasoning_effort: None,
            temperature: None,
            message_count: 0,
            last_active_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            tokens: None,
            last_compaction_offset: None,
            corrupted: false,
        };
        std::fs::write(&meta_path, serde_json::to_string(&valid_meta).unwrap()).unwrap();

        // Create a corrupted session with meta file (corrupted flag set).
        let corrupt_id = "20260503_110000_corrupt";
        let meta_path2 = meta_dir.join(format!("{}.json", corrupt_id));
        let corrupt_meta = SessionMeta {
            session_id: corrupt_id.to_string(),
            corrupted: true,
            title: None,
            ..valid_meta.clone()
        };
        std::fs::write(&meta_path2, serde_json::to_string(&corrupt_meta).unwrap()).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (sessions, _total, _agent_totals) =
            rt.block_on(async { scan_sessions_async(conv_dir, None, None).await.unwrap() });

        assert_eq!(
            sessions.len(),
            2,
            "Should find both valid and corrupted sessions"
        );

        let valid_session = sessions.iter().find(|s| s.session_id == valid_id).unwrap();
        assert!(!valid_session.corrupted);

        let corrupt_session = sessions
            .iter()
            .find(|s| s.session_id == corrupt_id)
            .unwrap();
        assert!(corrupt_session.corrupted);
        // ADR-024: corrupted sessions report whatever the meta file says;
        // title may be None (no recovery inference from JSONL header).
        assert_eq!(corrupt_session.title, None);
    }

    #[test]
    fn test_session_meta_serde_backward_compatible() {
        // ADR-024: SessionMeta uses `last_active_at` (new field) instead of
        // `updated_at`.  Old fields like `message_count` use `u64` with
        // `#[serde(default)]`.  Verify missing fields deserialize correctly.
        let old_json = r#"{"version":2,"session_id":"test","agent_id":"com.test","created_at":"2026-01-01T00:00:00Z","last_active_at":"2026-01-01T00:00:00Z","message_count":0,"corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(old_json).unwrap();
        assert!(!meta.corrupted);
        assert_eq!(meta.session_id, "test");
    }

    // ── ADR-027: SessionTokens accumulation tests ─────────────────

    #[test]
    fn test_accumulate_llm_usage_basic() {
        // Two reliable calls accumulate correctly.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let session =
                ConversationSession::new(&dir, "tok_acc_basic", cfg, 0, committed).unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1_000,
                completion_tokens: 200,
                ..Default::default()
            });
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 3_500,
                completion_tokens: 450,
                ..Default::default()
            });

            let tokens = session.tokens().expect("tokens should be set");
            assert_eq!(tokens.last_input, 3_500, "last_input is the most recent raw value");
            assert_eq!(tokens.last_output, 450);
            assert_eq!(tokens.total_input, 4_500);
            assert_eq!(tokens.total_output, 650);
        });
    }

    #[test]
    fn test_accumulate_llm_usage_skips_input_when_zero() {
        // Provider fallback (prompt_tokens == 0) must NOT pollute total_input
        // but MUST still record last_input as raw zero (ADR-027 honesty).
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let session =
                ConversationSession::new(&dir, "tok_acc_zero", cfg, 0, committed).unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1_000,
                completion_tokens: 100,
                ..Default::default()
            });
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 0, // Provider fallback
                completion_tokens: 50,
                ..Default::default()
            });
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 2_000,
                completion_tokens: 80,
                ..Default::default()
            });

            let tokens = session.tokens().expect("tokens should be set");
            assert_eq!(
                tokens.last_input, 2_000,
                "last_input reflects the most recent raw Provider value"
            );
            assert_eq!(
                tokens.total_input, 3_000,
                "total_input skipped the prompt_tokens=0 call (1000 + 2000)"
            );
            assert_eq!(
                tokens.total_output, 230,
                "total_output accumulates every call (100 + 50 + 80)"
            );
        });
    }

    #[test]
    fn test_accumulate_llm_usage_saturating_overflow() {
        // saturating_add guards against pathological Provider counters.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let session =
                ConversationSession::new(&dir, "tok_acc_overflow", cfg, 0, committed).unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: u64::MAX,
                completion_tokens: u64::MAX,
                ..Default::default()
            });
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1,
                completion_tokens: 1,
                ..Default::default()
            });

            let tokens = session.tokens().expect("tokens should be set");
            assert_eq!(
                tokens.last_input, 1,
                "last_input is the most recent raw value, not the sum"
            );
            assert_eq!(
                tokens.total_input, u64::MAX,
                "saturating_add caps total_input at u64::MAX"
            );
            assert_eq!(tokens.total_output, u64::MAX);
        });
    }

    #[test]
    fn test_session_tokens_serde_roundtrip() {
        let t = SessionTokens {
            last_input: 100,
            last_output: 20,
            total_input: 500,
            total_output: 80,
        };
        let json = serde_json::to_string(&t).unwrap();
        let parsed: SessionTokens = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, t);
    }

    #[test]
    fn test_session_tokens_default_is_all_zero() {
        let t = SessionTokens::default();
        assert_eq!(t.last_input, 0);
        assert_eq!(t.last_output, 0);
        assert_eq!(t.total_input, 0);
        assert_eq!(t.total_output, 0);
    }

    #[test]
    fn test_session_meta_tokens_field_default_to_none() {
        // ADR-027: backward compatibility — pre-ADR-027 meta files have no
        // `tokens` field; they should deserialize with `tokens: None`.
        let old_json = r#"{"version":2,"session_id":"pre027","agent_id":"com.test","created_at":"2026-01-01T00:00:00Z","last_active_at":"2026-01-01T00:00:00Z","message_count":0,"corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(old_json).unwrap();
        assert_eq!(meta.tokens, None);
    }

    #[test]
    fn test_session_meta_serde_roundtrip() {
        // ADR-024: full round-trip with SessionMeta fields.
        let meta = SessionMeta {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: "roundtrip_test".to_string(),
            agent_id: "com.test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            title: Some("Test session".to_string()),
            workspace_id: None,
            model: Some("gpt-4".to_string()),
            provider: Some("openai".to_string()),
            reasoning_effort: None,
            temperature: Some(0.7),
            message_count: 5,
            last_active_at: "2026-01-01T00:00:00Z".to_string(),
            tokens: Some(SessionTokens {
                last_input: 45_000,
                last_output: 1_200,
                total_input: 120_000,
                total_output: 3_400,
            }),
            last_compaction_offset: None,
            corrupted: false,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.tokens,
            Some(SessionTokens {
                last_input: 45_000,
                last_output: 1_200,
                total_input: 120_000,
                total_output: 3_400,
            })
        );
        assert_eq!(parsed.last_compaction_offset, None);
        assert_eq!(parsed.version, CONVERSATION_FORMAT_VERSION);
        assert_eq!(parsed.model.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn test_session_meta_fields_default_to_none() {
        // ADR-024: old JSON without optional fields defaults correctly.
        let old_json = r#"{"version":2,"session_id":"old","agent_id":"com.test","created_at":"2026-01-01T00:00:00Z","last_active_at":"2026-01-01T00:00:00Z","message_count":0,"corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(old_json).unwrap();
        assert_eq!(meta.tokens, None, "old JSON without tokens field should default to None");
        assert_eq!(meta.model, None);
        assert_eq!(meta.provider, None);
    }

    // ── raw-entry pagination tests ───────────────────────────────
    //
    // Both `offset` and `limit` are in raw-entry units (one JSONL line
    // each).  Display-group collapsing (think + tool_call + tool_result
    // → 1 chip) is a **frontend UI abstraction** and is not tested
    // here.  See `displayMessages` in `ChatPanel.tsx`.

    /// Helper: write a JSONL file with entries (ADR-024: no metadata header).
    fn write_test_jsonl(dir: &TempDir, session_id: &str, entries: &[ConversationEntry]) -> PathBuf {
        let conv_dir = dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();
        let file_path = conv_dir.join(format!("{}.jsonl", session_id));
        let mut file = std::fs::File::create(&file_path).unwrap();
        // ADR-024: no metadata header — entries start at line 0.
        for e in entries {
            serde_json::to_writer(&mut file, e).unwrap();
            writeln!(file).unwrap();
        }
        file_path
    }

    fn make_entry(id: &str, role: &str, content: &str) -> ConversationEntry {
        ConversationEntry {
            id: id.to_string(),
            ts: chrono::Utc::now().to_rfc3339(),
            role: role.to_string(),
            content: content.to_string(),
            metadata: None,
            kind: None,
        }
    }

    #[test]
    fn raw_limit_returns_last_n_raw_entries() {
        // 10 raw entries; limit=3 → last 3 entries (chronological order).
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (1..=10)
            .map(|i| make_entry(&i.to_string(), if i % 2 == 0 { "assistant" } else { "user" }, &format!("m{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "sess-raw-limit", &entries);

        let page = read_messages_paginated(&path, 0, 3).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.total, 10);
        // Last 3 entries: m8, m9, m10.
        assert_eq!(page.messages[0].content, "m8");
        assert_eq!(page.messages[1].content, "m9");
        assert_eq!(page.messages[2].content, "m10");
    }

    #[test]
    fn raw_offset_pagination_arithmetic() {
        // Both `offset` and `limit` are raw entries, so a single
        // arithmetic identity must hold across multiple pages:
        //   page_n = read(offset = n * limit, limit)
        //          = entries[total - (n+1)*limit .. total - n*limit]
        // Every page exposes exactly `limit` raw entries (no group
        // boundary alignment), and the union of all pages covers the
        // whole file.
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (1..=12)
            .map(|i| make_entry(&i.to_string(), "user", &format!("e{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "sess-paginate", &entries);

        let p0 = read_messages_paginated(&path, 0, 5).unwrap();
        assert_eq!(p0.messages.len(), 5);
        assert_eq!(p0.messages.first().unwrap().content, "e8");
        assert_eq!(p0.messages.last().unwrap().content, "e12");

        let p1 = read_messages_paginated(&path, 5, 5).unwrap();
        assert_eq!(p1.messages.len(), 5);
        assert_eq!(p1.messages.first().unwrap().content, "e3");
        assert_eq!(p1.messages.last().unwrap().content, "e7");

        let p2 = read_messages_paginated(&path, 10, 5).unwrap();
        assert_eq!(p2.messages.len(), 2, "tail page only has 2 entries left");
        assert_eq!(p2.messages.first().unwrap().content, "e1");
        assert_eq!(p2.messages.last().unwrap().content, "e2");

        // Union of pages = whole file. Sort with a numeric-aware comparator
        // so the assertion reads the natural order (`e1..e12`), not the
        // lexicographic order (`e1, e10, e11, e12, e2, ...`).
        let mut all: Vec<&str> = p2.messages.iter().chain(&p1.messages).chain(&p0.messages)
            .map(|e| e.content.as_str()).collect();
        all.sort_by_key(|s| s.trim_start_matches('e').parse::<u32>().unwrap());
        let expected: Vec<String> = (1..=12).map(|i| format!("e{}", i)).collect();
        assert_eq!(all, expected.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    }

    #[test]
    fn raw_offset_past_total_returns_empty() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "assistant", "a1"),
        ];
        let path = write_test_jsonl(&dir, "sess-empty-page", &entries);

        let page = read_messages_paginated(&path, 100, 5).unwrap();
        assert_eq!(page.messages.len(), 0);
        assert_eq!(page.total, 2);
        assert_eq!(page.limit, 0);
    }

    #[test]
    fn raw_limit_exceeds_total_returns_all() {
        // limit=50 on a 4-entry file must return all 4 entries —
        // the frontend default of 50 should always comfortably cover
        // any new session.
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "assistant", "a1"),
            make_entry("3", "user", "u2"),
            make_entry("4", "assistant", "a2"),
        ];
        let path = write_test_jsonl(&dir, "sess-under-limit", &entries);

        let page = read_messages_paginated(&path, 0, 50).unwrap();
        assert_eq!(page.messages.len(), 4);
        assert_eq!(page.total, 4);
        assert_eq!(page.limit, 4);
    }

    #[test]
    fn raw_pagination_does_not_hide_user_messages_in_tool_heavy_session() {
        // Regression: with `limit=50` raw entries (frontend default),
        // a session of 1 user + 60 tool rows + 1 assistant = 62 raw
        // entries must surface the user message (which lives at idx 0,
        // way outside the default page).  We verify two regimes:
        //   (a) limit=50: only the tail (idx 12..62) comes back — no user msg.
        //       This is fine: the frontend detects offset+limit == total
        //       after the first page and loads the previous page.
        //   (b) Paging back with offset=50, limit=50 returns idx 0..12,
        //       including the user message and the head of the tool run.
        let dir = TempDir::new().unwrap();
        let mut entries = vec![make_entry("1", "user", "user-msg")];
        for i in 0..20 {
            entries.push(make_entry(&format!("t{}", i * 3 + 2), "thought", &format!("think-{}", i)));
            entries.push(make_entry(&format!("c{}", i * 3 + 3), "tool_call", &format!("call-{}", i)));
            entries.push(make_entry(&format!("r{}", i * 3 + 4), "tool_result", &format!("result-{}", i)));
        }
        entries.push(make_entry("last", "assistant", "final-reply"));
        assert_eq!(entries.len(), 62);
        let path = write_test_jsonl(&dir, "sess-tool-heavy", &entries);

        // Page 1: latest 50 raw entries.
        let page1 = read_messages_paginated(&path, 0, 50).unwrap();
        assert_eq!(page1.messages.len(), 50);
        assert_eq!(page1.total, 62);
        assert!(
            !page1.messages.iter().any(|m| m.content == "user-msg"),
            "page 1 (offset=0, limit=50) must NOT yet include the user message"
        );

        // Page 2: paginate back by limit → entries[0..12], including user.
        let page2 = read_messages_paginated(&path, 50, 50).unwrap();
        assert_eq!(page2.messages.len(), 12);
        assert!(
            page2.messages.iter().any(|m| m.role == "user" && m.content == "user-msg"),
            "page 2 must include the user message"
        );
        // The slice ends on entry idx 11 which, per the construction loop above,
        // is `call-3` (a tool_call entry; the matching result-3 sits at idx 12
        // and belongs to page 1).
        assert_eq!(
            page2.messages.last().unwrap().content, "call-3",
            "page 2 ends on the 4th tool_call entry"
        );
    }

    fn make_compaction_entry(id: &str, summary: &str) -> ConversationEntry {
        ConversationEntry {
            id: id.to_string(),
            ts: chrono::Utc::now().to_rfc3339(),
            role: "system".to_string(),
            content: summary.to_string(),
            metadata: Some(serde_json::json!({
                "compacted_from_id": "first-id",
                "compacted_to_id": "last-id",
                "keep_last_rounds": 3,
                "model": "test-model",
                "before_tokens": 1000u64,
                "after_tokens": 200u64,
            })),
            kind: Some(ENTRY_KIND_COMPACTION.to_string()),
        }
    }

    #[test]
    fn pagination_shows_full_history_with_compaction() {
        // 4 pre-compaction entries + compaction marker + 2 post-compaction entries.
        // Display path must show ALL entries — compaction boundary is only
        // enforced on the context path (restorer), not the display path.
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "old-u1"),
            make_entry("2", "assistant", "old-a1"),
            make_entry("3", "user", "old-u2"),
            make_entry("4", "assistant", "old-a2"),
            make_compaction_entry("5", "<summary>compacted old-u1..old-a2</summary>"),
            make_entry("6", "user", "new-u3"),
            make_entry("7", "assistant", "new-a3"),
        ];
        let path = write_test_jsonl(&dir, "sess-compaction", &entries);

        // limit large enough to span the entire file in raw entries
        let page = read_messages_paginated(&path, 0, 50).unwrap();
        // Expect: all 7 entries (4 pre-compaction + compaction + 2 post-compaction)
        assert_eq!(page.messages.len(), 7, "display path must show full history");
        assert_eq!(page.total, 7);

        // Pre-compaction content must appear.
        assert!(
            page.messages.iter().any(|m| m.content == "old-u1"),
            "pre-compaction history must be visible in display path"
        );

        // Compaction marker must appear at the correct position (index 4).
        assert_eq!(
            page.messages[4].kind.as_deref(),
            Some(ENTRY_KIND_COMPACTION),
            "compaction marker must be at index 4"
        );

        // Post-compaction content must appear.
        assert_eq!(page.messages[5].content, "new-u3");
        assert_eq!(page.messages[6].content, "new-a3");
    }

    #[test]
    fn pagination_cursor_walks_back_through_compaction_boundary() {
        // Tight limit so the first page does NOT include the compaction marker;
        // paging back with `offset += limit` must still reach the pre-compaction
        // history (display path shows everything).
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "old-u1"),
            make_entry("2", "assistant", "old-a1"),
            make_compaction_entry("3", "<summary>...</summary>"),
            make_entry("4", "user", "new-u2"),
            make_entry("5", "assistant", "new-a2"),
            make_entry("6", "user", "new-u3"),
            make_entry("7", "assistant", "new-a3"),
        ];
        let path = write_test_jsonl(&dir, "sess-cap-boundary", &entries);

        // Page 1: latest 2 raw entries (new-u3, new-a3).
        let page1 = read_messages_paginated(&path, 0, 2).unwrap();
        assert_eq!(page1.messages.len(), 2);
        assert_eq!(page1.messages[0].content, "new-u3");
        assert_eq!(page1.messages[1].content, "new-a3");

        // Page 2: skip the 2 newest; returns 5 entries (old-u1..new-a2),
        // crossing the compaction boundary.
        let page2 = read_messages_paginated(&path, 2, 50).unwrap();
        assert_eq!(page2.messages.len(), 5, "5 entries before the 2 newest");
        assert!(
            page2.messages.iter().any(|m| m.kind.as_deref() == Some(ENTRY_KIND_COMPACTION)),
            "page 2 must include the compaction marker"
        );
        assert!(
            page2.messages.iter().any(|m| m.content.starts_with("old-")),
            "page 2 must include pre-compaction history"
        );
        // page2 starts at offset=0, so offset+limit covers the full file:
        // every entry is on this page, nothing else to fetch.
        assert_eq!(page2.total, 7);
        assert_eq!(page2.offset + page2.limit as u64, page2.total);
    }

    #[test]
    fn forward_pagination_with_stale_cursor() {
        // Forward pagination with a stale cursor pointing at offset 0
        // (below the data section start). The cursor should be clamped
        // up to `data_start` (meta_end), and all entries including
        // pre-compaction history should be returned.
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "old-u1"),
            make_entry("2", "assistant", "old-a1"),
            make_compaction_entry("3", "<summary>...</summary>"),
            make_entry("4", "user", "new-u2"),
            make_entry("5", "assistant", "new-a2"),
        ];
        let path = write_test_jsonl(&dir, "sess-forward-clamp", &entries);

        // offset below the data section start behaves like offset=0
        // (defensive: any offset value entered past the end or below the
        // data section just returns the latest window, never the metadata).
        let page = read_messages_paginated(&path, 0, 50).unwrap();
        // Expect: all 5 entries (old-u1, old-a1, compaction, new-u2, new-a2)
        assert_eq!(page.messages.len(), 5, "forward pagination must show full history");
        assert!(
            page.messages.iter().any(|m| m.kind.as_deref() == Some(ENTRY_KIND_COMPACTION)),
            "compaction marker must appear"
        );
        assert!(
            page.messages.iter().any(|m| m.content.starts_with("old-")),
            "pre-compaction history must be visible in forward pagination"
        );
    }

    #[test]
    fn pagination_without_compaction_is_unchanged() {
        // Regression: existing behavior (no compaction in file) must be preserved.
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "assistant", "a1"),
            make_entry("3", "user", "u2"),
            make_entry("4", "assistant", "a2"),
        ];
        let path = write_test_jsonl(&dir, "sess-no-compaction", &entries);

        let page = read_messages_paginated(&path, 0, 50).unwrap();
        assert_eq!(page.messages.len(), 4);
        assert_eq!(page.total, 4);
    }

    /// Build a StreamingStateMap with a single streaming line for `session_id`.
    fn make_streaming_map(
        session_id: &str,
        line_number: usize,
        role: &str,
        content: &str,
    ) -> StreamingStateMap {
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        map.write().unwrap().insert(
            session_id.to_string(),
            StreamingLine {
                line_number,
                role: role.to_string(),
                message_id: uuid::Uuid::new_v4().to_string(),
                accumulated_content: content.to_string(),
                started_at: chrono::Utc::now().to_rfc3339(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
            },
        );
        map
    }

    /// ADR-022: streaming delta uses the requested `line_char_offset` when the
    /// frontend is already caught up to this streaming line's `line_number`.
    #[test]
    fn test_read_since_streaming_same_line_uses_offset() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-same-line", &entries);
        // ADR-024: no metadata header — line 0 = user. total_lines = 1.
        // Streaming line will become line 1. Frontend already saw line 1 chars 0..3.
        let map = make_streaming_map("sess-same-line", 1, "assistant", "hello world");

        // Frontend line_number=1 (caught up), offset=3 → expect "lo world".
        let result = read_messages_since(&path, 1, 3, &map, "sess-same-line", 0).unwrap();
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 1);
        assert_eq!(streaming.content, "lo world");
        assert_eq!(streaming.char_offset, "hello world".chars().count());
    }

    /// ADR-022 regression: when a role transition flushed the previous
    /// streaming line to JSONL and started a NEW streaming line at a higher
    /// line_number, the frontend's stale `line_char_offset` (from the old
    /// line) must NOT be applied to the new line — otherwise the opening
    /// characters of the new line are silently dropped.
    #[test]
    fn test_read_since_streaming_new_line_ignores_stale_offset() {
        let dir = TempDir::new().unwrap();
        // ADR-024: line 0 = user, line 1 = assistant. Frontend last saw up to line 1.
        let entries = vec![
            make_entry("1", "user", "hi"),
            make_entry("2", "assistant", "previous assistant text"),
        ];
        let path = write_test_jsonl(&dir, "sess-new-line", &entries);
        // New streaming line is a thought that will become line 2.
        let map = make_streaming_map("sess-new-line", 2, "thought", "reasoning...");

        // Frontend sends line_number=1 (has NOT yet seen line 2) with a stale
        // offset of 10 that belonged to the old assistant line. The new thought
        // line must be returned in full, not skipped by 10 chars.
        let result = read_messages_since(&path, 1, 10, &map, "sess-new-line", 0).unwrap();
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 2);
        assert_eq!(
            streaming.content, "reasoning...",
            "stale offset from a flushed line must not truncate the new streaming line"
        );
        assert_eq!(streaming.char_offset, "reasoning...".chars().count());
    }

    /// ADR-022 regression: when the frontend's `line_number` matches the
    /// streaming line's `line_number` but `line_char_offset` exceeds the
    /// current content length (stale offset from a previous, longer streaming
    /// line that was flushed), the offset must be reset to 0 — not clamped
    /// to `current_len` (which would return an empty delta and lose content).
    #[test]
    fn test_read_since_streaming_stale_offset_exceeds_content() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hi"),
            make_entry("2", "assistant", "previous long assistant text"),
        ];
        let path = write_test_jsonl(&dir, "sess-stale-exceeds", &entries);
        // ADR-024: line 0 = user, line 1 = assistant. Streaming line = 2.
        let map = make_streaming_map("sess-stale-exceeds", 2, "assistant", "Hello");

        // Frontend sends line_number=2 (matches streaming line) with a stale
        // offset of 100 that belonged to the previous (longer) assistant line.
        let result =
            read_messages_since(&path, 2, 100, &map, "sess-stale-exceeds", 0).unwrap();
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 2);
        assert_eq!(
            streaming.content, "Hello",
            "stale offset exceeding current content length must return full content"
        );
        assert_eq!(streaming.char_offset, "Hello".chars().count());
    }

    // ── read_messages_since_cursor tests (ADR-025) ──────────────────

    #[test]
    fn test_cursor_basic_no_streaming() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hello"),
            make_entry("2", "assistant", "world"),
            make_entry("3", "user", "again"),
        ];
        let path = write_test_jsonl(&dir, "sess-cursor-basic", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Cursor at 0, limit 50 → all 3 messages, has_more=false
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 0, char_offset: 0 },
            50,
            &map,
            "sess-cursor-basic",
            0,
        ).unwrap();
        assert_eq!(result.messages.len(), 3);
        assert!(!result.has_more);
        assert_eq!(result.new_cursor.line_number, 3);
        assert_eq!(result.new_cursor.char_offset, 0);
        assert!(result.streaming.is_none());
    }

    #[test]
    fn test_cursor_batch_limit() {
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (0..10)
            .map(|i| make_entry(&format!("m{}", i), "user", &format!("msg{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "sess-cursor-batch", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Cursor at 0, limit 3 → first 3 messages, has_more=true
        let r1 = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 0, char_offset: 0 },
            3, &map, "sess-cursor-batch", 0,
        ).unwrap();
        assert_eq!(r1.messages.len(), 3);
        assert!(r1.has_more);
        assert_eq!(r1.new_cursor.line_number, 3);
        assert_eq!(r1.messages[0].content, "msg0");
        assert_eq!(r1.messages[2].content, "msg2");

        // Cursor at 3, limit 3 → next 3 messages, has_more=true
        let r2 = read_messages_since_cursor(
            &path, r1.new_cursor, 3, &map, "sess-cursor-batch", 0,
        ).unwrap();
        assert_eq!(r2.messages.len(), 3);
        assert!(r2.has_more);
        assert_eq!(r2.new_cursor.line_number, 6);
        assert_eq!(r2.messages[0].content, "msg3");

        // Cursor at 6, limit 3 → next 3, has_more=true
        let r3 = read_messages_since_cursor(
            &path, r2.new_cursor, 3, &map, "sess-cursor-batch", 0,
        ).unwrap();
        assert_eq!(r3.messages.len(), 3);
        assert!(r3.has_more);
        assert_eq!(r3.new_cursor.line_number, 9);

        // Cursor at 9, limit 3 → last 1 message, has_more=false
        let r4 = read_messages_since_cursor(
            &path, r3.new_cursor, 3, &map, "sess-cursor-batch", 0,
        ).unwrap();
        assert_eq!(r4.messages.len(), 1);
        assert!(!r4.has_more);
        assert_eq!(r4.new_cursor.line_number, 10);
        assert_eq!(r4.messages[0].content, "msg9");
    }

    #[test]
    fn test_cursor_streaming_same_line_uses_offset() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-cursor-same", &entries);
        // total_lines=1, streaming line at 1 with "hello world"
        let map = make_streaming_map("sess-cursor-same", 1, "assistant", "hello world");

        // Cursor at line 1 (caught up), char_offset 3 → delta "lo world"
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 1, char_offset: 3 },
            50, &map, "sess-cursor-same", 0,
        ).unwrap();
        assert_eq!(result.messages.len(), 0); // no new complete lines
        assert!(!result.has_more);
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 1);
        assert_eq!(streaming.content, "lo world");
        assert_eq!(streaming.char_offset, "hello world".chars().count());
        // Cursor advances char_offset to full length
        assert_eq!(result.new_cursor.line_number, 1);
        assert_eq!(result.new_cursor.char_offset, "hello world".chars().count());
    }

    #[test]
    fn test_cursor_streaming_new_line_resets_offset() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hi"),
            make_entry("2", "assistant", "previous text"),
        ];
        let path = write_test_jsonl(&dir, "sess-cursor-newline", &entries);
        // Streaming line at 2 (new), content "reasoning..."
        let map = make_streaming_map("sess-cursor-newline", 2, "thought", "reasoning...");

        // Cursor at line 1 (behind streaming line 2), stale char_offset 10
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 1, char_offset: 10 },
            50, &map, "sess-cursor-newline", 0,
        ).unwrap();
        // Should return line 1 (the complete assistant message)
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].content, "previous text");
        // Streaming delta should be full "reasoning..." (stale offset reset)
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.content, "reasoning...");
        // Cursor advances to line 2, char_offset = full streaming content length
        assert_eq!(result.new_cursor.line_number, 2);
        assert_eq!(result.new_cursor.char_offset, "reasoning...".chars().count());
    }

    #[test]
    fn test_cursor_streaming_stale_offset_exceeds_content() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hi"),
            make_entry("2", "assistant", "previous long text"),
        ];
        let path = write_test_jsonl(&dir, "sess-cursor-stale", &entries);
        // Streaming line at 2, short content "Hello"
        let map = make_streaming_map("sess-cursor-stale", 2, "assistant", "Hello");

        // Cursor at line 2 (matches streaming line), stale char_offset 100
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 2, char_offset: 100 },
            50, &map, "sess-cursor-stale", 0,
        ).unwrap();
        assert_eq!(result.messages.len(), 0); // no new complete lines
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.content, "Hello");
        assert_eq!(streaming.char_offset, "Hello".chars().count());
    }

    #[test]
    fn test_cursor_batch_with_streaming_delta() {
        // Background session scenario: 100 complete lines pending + streaming.
        // Streaming delta is NOT returned during batch catch-up (cursor behind
        // streaming line). It's only returned in the final batch when the
        // cursor catches up to the streaming line.
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (0..100)
            .map(|i| make_entry(&format!("m{}", i), "user", &format!("msg{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "sess-cursor-bg", &entries);
        // Streaming line at 100, content "streaming content"
        let map = make_streaming_map("sess-cursor-bg", 100, "assistant", "streaming content");

        // Cursor at 0 (way behind), limit 50 — batch 1
        let r1 = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 0, char_offset: 0 },
            50, &map, "sess-cursor-bg", 0,
        ).unwrap();
        assert_eq!(r1.messages.len(), 50);
        assert!(r1.has_more);
        assert_eq!(r1.new_cursor.line_number, 50);
        // Streaming delta NOT returned (cursor at 50 < streaming at 100)
        assert!(r1.streaming.is_none(), "no streaming delta during batch catch-up");
        assert_eq!(r1.new_cursor.char_offset, 0);

        // Batch 2: cursor at 50, limit 50 — catches up to line 100
        let r2 = read_messages_since_cursor(
            &path, r1.new_cursor, 50, &map, "sess-cursor-bg", 0,
        ).unwrap();
        assert_eq!(r2.messages.len(), 50);
        assert!(!r2.has_more);
        assert_eq!(r2.new_cursor.line_number, 100);
        // Streaming delta IS returned now (cursor at 100 == streaming at 100)
        let s2 = r2.streaming.as_ref().expect("streaming should be returned when caught up");
        assert_eq!(s2.content, "streaming content", "full content on first caught-up poll");
        assert_eq!(s2.char_offset, "streaming content".chars().count());
        assert_eq!(r2.new_cursor.char_offset, "streaming content".chars().count());

        // Batch 3: cursor at 100, no new complete lines, streaming delta only
        let r3 = read_messages_since_cursor(
            &path, r2.new_cursor, 50, &map, "sess-cursor-bg", 0,
        ).unwrap();
        assert_eq!(r3.messages.len(), 0);
        assert!(!r3.has_more);
        let s3 = r3.streaming.as_ref().expect("streaming delta on caught-up poll");
        assert_eq!(s3.content, "", "no new streaming chars");
    }

    #[test]
    fn test_cursor_at_total_lines_no_messages() {
        // Cursor already caught up, no streaming → empty result
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-cursor-caughtup", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 1, char_offset: 0 },
            50, &map, "sess-cursor-caughtup", 0,
        ).unwrap();
        assert_eq!(result.messages.len(), 0);
        assert!(!result.has_more);
        assert_eq!(result.new_cursor.line_number, 1);
        assert!(result.streaming.is_none());
    }

    #[test]
    fn test_cursor_clamped_to_total_lines() {
        // Cursor exceeds total_lines (defensive: JSONL was truncated)
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-cursor-clamp", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 999, char_offset: 0 },
            50, &map, "sess-cursor-clamp", 0,
        ).unwrap();
        assert_eq!(result.messages.len(), 0);
        assert!(!result.has_more);
        // Cursor is clamped to total_lines (1)
        assert_eq!(result.new_cursor.line_number, 1);
    }

    #[test]
    fn test_cursor_uses_cached_total_lines() {
        // When cached_total_lines > 0, it takes precedence over file scan.
        // In production, committed_lines is always accurate (ADR-022: incremented
        // AFTER write).  This test verifies the precedence logic only.
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (0..3)
            .map(|i| make_entry(&format!("m{}", i), "user", &format!("msg{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "sess-cursor-cached", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // File has 3 lines, cache says 3 — should return all 3.
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 0, char_offset: 0 },
            50, &map, "sess-cursor-cached", 3,
        ).unwrap();
        assert_eq!(result.messages.len(), 3);
        assert!(!result.has_more);
        assert_eq!(result.new_cursor.line_number, 3);
        assert_eq!(result.total_lines, 3);

        // File has 3 lines, cache says 0 — should fall back to file scan.
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 0, char_offset: 0 },
            50, &map, "sess-cursor-cached", 0,
        ).unwrap();
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.total_lines, 3);
    }

    #[test]
    fn test_cursor_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("conversations").join("empty.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 0, char_offset: 0 },
            50, &map, "empty", 0,
        ).unwrap();
        assert_eq!(result.messages.len(), 0);
        assert!(!result.has_more);
        assert_eq!(result.total_lines, 0);
        assert_eq!(result.new_cursor.line_number, 0);
    }

    #[test]
    fn test_cursor_limit_zero() {
        // Edge case: limit=0 → no messages, but has_more may still be true
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-cursor-lim0", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor { line_number: 0, char_offset: 0 },
            0, &map, "sess-cursor-lim0", 0,
        ).unwrap();
        assert_eq!(result.messages.len(), 0);
        assert!(result.has_more); // 0 < 1
        assert_eq!(result.new_cursor.line_number, 0);
    }

    // ── End-to-end delivery flow tests (ADR-025) ─────────────────────
    //
    // These tests simulate the full delivery flow that SessionManager +
    // cli.rs perform: full load (reset cursor) → incremental poll →
    // batch catch-up → streaming flush → final poll.  They exercise
    // read_messages_since_cursor with realistic cursor management,
    // covering all branches and edge cases.

    /// Helper: append entries to an existing JSONL file (simulates writer
    /// thread flushing new lines while session is in background).
    fn append_test_jsonl(path: &Path, entries: &[ConversationEntry]) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap();
        for e in entries {
            serde_json::to_writer(&mut file, e).unwrap();
            writeln!(file).unwrap();
        }
    }

    /// E2E: Full lifecycle — full load resets cursor, then incremental
    /// polls deliver new data.
    #[test]
    fn e2e_full_load_then_incremental() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hello"),
            make_entry("2", "assistant", "world"),
        ];
        let path = write_test_jsonl(&dir, "e2e-lifecycle", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        let sid = "e2e-lifecycle";

        // Step 1: Full load — read all messages via pagination.
        // SessionManager.reset_delivery_cursor(sid, total_lines=2)
        let mut cursor = DeliveryCursor { line_number: 2, char_offset: 0 };

        // Step 2: Incremental poll — no new data, cursor already at end.
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);
        cursor = r.new_cursor;
        assert_eq!(cursor.line_number, 2);

        // Step 3: Writer appends a new line (simulates flush).
        append_test_jsonl(&path, &[make_entry("3", "user", "new message")]);

        // Step 4: Incremental poll — should return the new line.
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].content, "new message");
        assert!(!r.has_more);
        cursor = r.new_cursor;
        assert_eq!(cursor.line_number, 3);
    }

    /// E2E: Background session — 100 lines accumulate while "away",
    /// then batch catch-up delivers them in batches.
    #[test]
    fn e2e_background_session_batch_catchup() {
        let dir = TempDir::new().unwrap();
        // Initial 2 lines (already delivered via full load)
        let entries = vec![
            make_entry("1", "user", "initial"),
            make_entry("2", "assistant", "response"),
        ];
        let path = write_test_jsonl(&dir, "e2e-bg", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        let sid = "e2e-bg";

        // Full load done — cursor at 2
        let mut cursor = DeliveryCursor { line_number: 2, char_offset: 0 };

        // Session goes to background. Writer appends 100 more lines.
        let new_entries: Vec<ConversationEntry> = (0..100)
            .map(|i| make_entry(&format!("bg{}", i), "user", &format!("bg-msg{}", i)))
            .collect();
        append_test_jsonl(&path, &new_entries);

        // User returns — incremental poll with limit=50
        let r1 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r1.messages.len(), 50);
        assert!(r1.has_more);
        assert_eq!(r1.messages[0].content, "bg-msg0");
        assert_eq!(r1.messages[49].content, "bg-msg49");
        cursor = r1.new_cursor;
        assert_eq!(cursor.line_number, 52);

        // Immediate re-poll (batch catch-up)
        let r2 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r2.messages.len(), 50);
        assert!(!r2.has_more); // 102 == 102, all complete lines delivered
        assert_eq!(r2.messages[0].content, "bg-msg50");
        assert_eq!(r2.messages[49].content, "bg-msg99");
        cursor = r2.new_cursor;
        assert_eq!(cursor.line_number, 102);

        // Final poll — no more complete lines
        let r3 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r3.messages.len(), 0);
        assert!(!r3.has_more);
        assert_eq!(r3.new_cursor.line_number, 102);
    }

    /// E2E: Streaming line lifecycle — delta delivery, flush, new line.
    #[test]
    fn e2e_streaming_flush_lifecycle() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "e2e-stream", &entries);
        let sid = "e2e-stream";
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Full load — cursor at 1
        let mut cursor = DeliveryCursor { line_number: 1, char_offset: 0 };

        // Streaming line starts at line 1, content "Hello"
        {
            let mut m = map.write().unwrap();
            m.insert(sid.to_string(), StreamingLine {
                line_number: 1,
                role: "assistant".to_string(),
                message_id: uuid::Uuid::new_v4().to_string(),
                accumulated_content: "Hello".to_string(),
                started_at: chrono::Utc::now().to_rfc3339(),
                started_at_ms: 0,
            });
        }

        // Poll 1: streaming delta "Hello" (full content, cursor caught up)
        let r1 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r1.messages.len(), 0); // no new complete lines
        let s1 = r1.streaming.as_ref().expect("streaming delta");
        assert_eq!(s1.content, "Hello");
        assert_eq!(s1.char_offset, 5);
        cursor = r1.new_cursor;
        assert_eq!(cursor.line_number, 1);
        assert_eq!(cursor.char_offset, 5);

        // More content arrives: " world" appended
        {
            let mut m = map.write().unwrap();
            m.get_mut(sid).unwrap().accumulated_content = "Hello world".to_string();
        }

        // Poll 2: streaming delta " world" (incremental)
        let r2 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r2.messages.len(), 0);
        let s2 = r2.streaming.as_ref().expect("streaming delta");
        assert_eq!(s2.content, " world");
        assert_eq!(s2.char_offset, 11);
        cursor = r2.new_cursor;
        assert_eq!(cursor.char_offset, 11);

        // Flush: write "Hello world" to JSONL, remove streaming line
        append_test_jsonl(&path, &[make_entry("2", "assistant", "Hello world")]);
        map.write().unwrap().remove(sid);

        // New streaming line starts at line 2, content "Next"
        {
            let mut m = map.write().unwrap();
            m.insert(sid.to_string(), StreamingLine {
                line_number: 2,
                role: "thought".to_string(),
                message_id: uuid::Uuid::new_v4().to_string(),
                accumulated_content: "Next".to_string(),
                started_at: chrono::Utc::now().to_rfc3339(),
                started_at_ms: 0,
            });
        }

        // Poll 3: flushed line + new streaming delta
        let r3 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r3.messages.len(), 1);
        assert_eq!(r3.messages[0].content, "Hello world");
        let s3 = r3.streaming.as_ref().expect("new streaming delta");
        assert_eq!(s3.content, "Next", "full content of new streaming line");
        assert_eq!(s3.line, 2);
        cursor = r3.new_cursor;
        assert_eq!(cursor.line_number, 2); // advanced past flushed line
        assert_eq!(cursor.char_offset, 4); // "Next" = 4 chars
    }

    /// E2E: Full load resets cursor — incremental poll after full load
    /// returns nothing (all existing lines already delivered).
    #[test]
    fn e2e_full_load_resets_cursor() {
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (0..10)
            .map(|i| make_entry(&format!("m{}", i), "user", &format!("msg{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "e2e-reset", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        let sid = "e2e-reset";

        // Simulate: cursor was at 3 (some incremental polls happened)
        let cursor_before = DeliveryCursor { line_number: 3, char_offset: 0 };

        // Full load happens — reset_delivery_cursor(sid, total_lines=10)
        let cursor = DeliveryCursor { line_number: 10, char_offset: 0 };

        // Incremental poll — should return nothing (cursor at end)
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);

        // Verify the reset actually happened (cursor was 3, now 10)
        assert_ne!(cursor_before.line_number, cursor.line_number);
        assert_eq!(cursor.line_number, 10);
    }

    /// E2E: Batch catch-up with streaming — streaming delta is NOT
    /// returned during catch-up, only when cursor catches up.
    #[test]
    fn e2e_batch_catchup_with_streaming() {
        let dir = TempDir::new().unwrap();
        // 5 initial lines (delivered via full load)
        let entries: Vec<ConversationEntry> = (0..5)
            .map(|i| make_entry(&format!("i{}", i), "user", &format!("init{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "e2e-batch-stream", &entries);
        let sid = "e2e-batch-stream";

        // 10 more lines accumulate while "away"
        let new_entries: Vec<ConversationEntry> = (0..10)
            .map(|i| make_entry(&format!("n{}", i), "user", &format!("new{}", i)))
            .collect();
        append_test_jsonl(&path, &new_entries);

        // Streaming line at 15 (after all complete lines)
        let map = make_streaming_map(sid, 15, "assistant", "streaming content");

        // Cursor at 5 (after initial full load), limit=3
        let mut cursor = DeliveryCursor { line_number: 5, char_offset: 0 };

        // Batch 1: lines 5-7, has_more=true, NO streaming delta
        let r1 = read_messages_since_cursor(&path, cursor, 3, &map, sid, 0).unwrap();
        assert_eq!(r1.messages.len(), 3);
        assert!(r1.has_more);
        assert!(r1.streaming.is_none(), "no streaming during batch catch-up");
        cursor = r1.new_cursor;

        // Batch 2: lines 8-10, has_more=true, NO streaming delta
        let r2 = read_messages_since_cursor(&path, cursor, 3, &map, sid, 0).unwrap();
        assert_eq!(r2.messages.len(), 3);
        assert!(r2.has_more);
        assert!(r2.streaming.is_none());
        cursor = r2.new_cursor;

        // Batch 3: lines 11-13, has_more=true, NO streaming delta
        let r3 = read_messages_since_cursor(&path, cursor, 3, &map, sid, 0).unwrap();
        assert_eq!(r3.messages.len(), 3);
        assert!(r3.has_more);
        assert!(r3.streaming.is_none());
        cursor = r3.new_cursor;

        // Batch 4: lines 14, has_more=false (14 < 15), cursor at 14 < 15
        // Wait: 5 initial + 10 new = 15 total. cursor was 5, batches: 5→8→11→14→15
        // Batch 4: lines 14, has_more = 14 < 15 = true? No, 14+3=15=min(15,17)=15
        // So batch 4: lines[14..15) = 1 line, cursor→15, has_more=15<15=false
        let r4 = read_messages_since_cursor(&path, cursor, 3, &map, sid, 0).unwrap();
        assert_eq!(r4.messages.len(), 1);
        assert!(!r4.has_more);
        // Cursor at 15 == streaming.line_number(15) → streaming delta returned!
        let s4 = r4.streaming.as_ref().expect("streaming delta when caught up");
        assert_eq!(s4.content, "streaming content");
        assert_eq!(s4.line, 15);
        cursor = r4.new_cursor;
        assert_eq!(cursor.line_number, 15);
        assert_eq!(cursor.char_offset, "streaming content".chars().count());
    }

    /// E2E: Multiple incremental polls with growing streaming content.
    /// Verifies char_offset advances correctly across polls.
    #[test]
    fn e2e_multiple_polls_streaming_growth() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "start")];
        let path = write_test_jsonl(&dir, "e2e-growth", &entries);
        let sid = "e2e-growth";
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Full load — cursor at 1
        let mut cursor = DeliveryCursor { line_number: 1, char_offset: 0 };

        // Streaming line at 1, initially empty
        {
            map.write().unwrap().insert(sid.to_string(), StreamingLine {
                line_number: 1,
                role: "assistant".to_string(),
                message_id: uuid::Uuid::new_v4().to_string(),
                accumulated_content: String::new(),
                started_at: chrono::Utc::now().to_rfc3339(),
                started_at_ms: 0,
            });
        }

        // Poll 1: streaming exists but empty → delta is "", char_offset=0
        let r1 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        let s1 = r1.streaming.as_ref().expect("streaming");
        assert_eq!(s1.content, "");
        assert_eq!(s1.char_offset, 0);
        cursor = r1.new_cursor;
        assert_eq!(cursor.char_offset, 0);

        // Content grows to "abc"
        map.write().unwrap().get_mut(sid).unwrap().accumulated_content = "abc".to_string();

        // Poll 2: delta = "abc" (from offset 0)
        let r2 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        let s2 = r2.streaming.as_ref().expect("streaming");
        assert_eq!(s2.content, "abc");
        assert_eq!(s2.char_offset, 3);
        cursor = r2.new_cursor;
        assert_eq!(cursor.char_offset, 3);

        // Content grows to "abcdef"
        map.write().unwrap().get_mut(sid).unwrap().accumulated_content = "abcdef".to_string();

        // Poll 3: delta = "def" (from offset 3)
        let r3 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        let s3 = r3.streaming.as_ref().expect("streaming");
        assert_eq!(s3.content, "def");
        assert_eq!(s3.char_offset, 6);
        cursor = r3.new_cursor;
        assert_eq!(cursor.char_offset, 6);
    }

    /// E2E: done event final poll — after all streaming is flushed,
    /// the final poll delivers the last flushed line.
    #[test]
    fn e2e_done_event_final_poll() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "question"),
            make_entry("2", "assistant", "partial"),
        ];
        let path = write_test_jsonl(&dir, "e2e-done", &entries);
        let sid = "e2e-done";
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Full load — cursor at 2
        let mut cursor = DeliveryCursor { line_number: 2, char_offset: 0 };

        // Streaming line at 2, content "final answer"
        {
            map.write().unwrap().insert(sid.to_string(), StreamingLine {
                line_number: 2,
                role: "assistant".to_string(),
                message_id: uuid::Uuid::new_v4().to_string(),
                accumulated_content: "final answer".to_string(),
                started_at: chrono::Utc::now().to_rfc3339(),
                started_at_ms: 0,
            });
        }

        // Poll 1: streaming delta "final answer"
        let r1 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        let s1 = r1.streaming.as_ref().expect("streaming");
        assert_eq!(s1.content, "final answer");
        cursor = r1.new_cursor;

        // done event: flush streaming, remove from map
        append_test_jsonl(&path, &[make_entry("3", "assistant", "final answer")]);
        map.write().unwrap().remove(sid);

        // Final poll (done handler): should deliver the flushed line
        let r2 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r2.messages.len(), 1);
        assert_eq!(r2.messages[0].content, "final answer");
        assert!(!r2.has_more);
        assert!(r2.streaming.is_none(), "no streaming after flush");
        cursor = r2.new_cursor;
        assert_eq!(cursor.line_number, 3);

        // Another poll — nothing left
        let r3 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r3.messages.len(), 0);
        assert!(!r3.has_more);
        assert!(r3.streaming.is_none());
    }

    /// E2E: compacting_ended one-shot poll — compaction record written
    /// to JSONL, one-shot incremental poll delivers it.
    #[test]
    fn e2e_compacting_ended_oneshot_poll() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "msg1"),
            make_entry("2", "assistant", "reply1"),
        ];
        let path = write_test_jsonl(&dir, "e2e-compact", &entries);
        let sid = "e2e-compact";
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Full load — cursor at 2
        let mut cursor = DeliveryCursor { line_number: 2, char_offset: 0 };

        // Compaction happens: compaction record written to JSONL
        append_test_jsonl(&path, &[make_compaction_entry("3", "<summary>compacted</summary>")]);

        // compacting_ended: one-shot incremental poll
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].kind.as_deref(), Some(ENTRY_KIND_COMPACTION));
        assert!(!r.has_more);
        cursor = r.new_cursor;
        assert_eq!(cursor.line_number, 3);
    }

    /// E2E: Cursor survives multiple write-then-poll cycles.
    /// Verifies cursor advancement is cumulative and correct.
    #[test]
    fn e2e_cumulative_cursor_advancement() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("0", "user", "base")];
        let path = write_test_jsonl(&dir, "e2e-cumulative", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        let sid = "e2e-cumulative";

        // Full load — cursor at 1
        let mut cursor = DeliveryCursor { line_number: 1, char_offset: 0 };
        let mut total_delivered = 0;

        // 5 rounds of: write 3 lines → poll → verify
        for round in 0..5 {
            let round_entries: Vec<ConversationEntry> = (0..3)
                .map(|i| make_entry(
                    &format!("r{}c{}", round, i),
                    "user",
                    &format!("round{}-msg{}", round, i),
                ))
                .collect();
            append_test_jsonl(&path, &round_entries);

            let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
            assert_eq!(r.messages.len(), 3, "round {} should deliver 3 messages", round);
            assert!(!r.has_more);
            total_delivered += r.messages.len();
            cursor = r.new_cursor;
        }

        assert_eq!(total_delivered, 15);
        // 1 initial + 15 new = 16 total lines
        assert_eq!(cursor.line_number, 16);

        // Final poll — nothing left
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);
    }
}
