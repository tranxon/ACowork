//! Session lifecycle management and JSONL conversation file writing.
//!
//! Provides `ConversationSession` for managing a single session's JSONL file
//! and `ConversationWriter` for channel-based single-writer thread architecture.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::error::Result;

/// Format version for the JSONL conversation file.
///
/// v2 (current): adds optional `kind` field to ConversationEntry.
///   `kind="compaction"` marks an LLM-driven compaction event whose `content`
///   is the summary text and whose `metadata` is a `CompactionEventMeta`.
///   When `kind` is absent or `"message"`, the entry is a regular
///   conversation message (role-based).
const CONVERSATION_FORMAT_VERSION: u32 = 2;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_tokens: Option<u64>,

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
    /// Last observed (input_tokens, output_tokens) from an LLM response.
    /// Persisted into meta file so the UI can restore the
    /// "context usage" indicator after a session resume.
    /// `None` means no LLM call has been made (or persisted) yet.
    last_tokens: std::sync::Mutex<Option<(u64, u64)>>,
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
        let (input_tokens, output_tokens) = self
            .last_tokens
            .lock()
            .ok()
            .and_then(|t| *t)
            .unwrap_or((0, 0));
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
            last_input_tokens: if input_tokens > 0 { Some(input_tokens) } else { None },
            last_output_tokens: if output_tokens > 0 { Some(output_tokens) } else { None },
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
            last_tokens: std::sync::Mutex::new(None),
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
            last_tokens: std::sync::Mutex::new(
                match (meta.last_input_tokens, meta.last_output_tokens) {
                    (Some(i), Some(o)) => Some((i, o)),
                    (Some(i), None) => Some((i, 0)),
                    (None, Some(o)) => Some((0, o)),
                    (None, None) => None,
                },
            ),
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

    /// Return the last persisted (input_tokens, output_tokens) pair, if any.
    ///
    /// Used on resume to seed the frontend "context usage" indicator with
    /// the same `prompt_tokens`/`completion_tokens` that the most recent LLM
    /// response reported. Window-derived fields (`context_window`,
    /// `usable_context`, `usage_percent`) are recomputed at resume time
    /// from the *current* model capabilities — this getter only returns the
    /// raw API-fact values.
    pub fn last_tokens(&self) -> Option<(u64, u64)> {
        self.last_tokens.lock().ok().and_then(|t| *t)
    }

    /// Persist the most recent LLM `usage` (input/output tokens) to meta
    /// file so the context-usage indicator survives a session resume.
    ///
    /// Called from the agent loop right after a `ContextUsage` chunk is
    /// emitted.
    pub fn update_last_tokens(&self, input_tokens: u64, output_tokens: u64) {
        if let Ok(mut t) = self.last_tokens.lock() {
            *t = Some((input_tokens, output_tokens));
        }
        self.write_meta();
    }
}

// Safety: ConversationSession only contains String and UnboundedSender,
// both of which are Send + Sync.
unsafe impl Send for ConversationSession {}
unsafe impl Sync for ConversationSession {}

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
        if let Err(e) = std::fs::remove_file(&meta_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "Failed to delete meta file during session pruning"
                );
            }
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
#[derive(Debug, Clone)]
pub struct PaginatedMessages {
    /// Messages in the current page
    pub messages: Vec<ConversationEntry>,
    /// Cursor for the next page (byte offset format: "offset:<bytes>")
    pub cursor: Option<String>,
    /// Whether more messages exist after this page
    pub has_more: bool,
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

/// Chunk size for backward reading (8 KB).
const BACKWARD_READ_CHUNK: usize = 8 * 1024;

/// Maximum raw entries to read per display-group page.
///
/// Frontend collapses consecutive `thought`/`tool_call`/`tool_result` entries
/// into a single visual "explore group".  Pagination should count these
/// display groups, not raw JSONL lines.  This cap ensures we read enough raw
/// lines to produce the requested number of display groups without
/// pathological I/O on malformed (intentionally huge) files.
const MAX_RAW_PER_DISPLAY_PAGE: usize = 500;

/// Count display groups in a chronological sequence of entries.
///
/// Consecutive entries with role `thought`, `tool_call`, or `tool_result`
/// are collapsed into a single display group (matching the frontend
/// `displayMessages` explore-group logic).
///
/// **Compaction marker special case**: an entry with `kind="compaction"`
/// always counts as its own group (1) and breaks any in-progress tool
/// sequence on either side, so it is rendered as a standalone summary card
/// in the UI without being merged into adjacent tool/explore blocks.
fn count_display_groups(entries: &[ConversationEntry]) -> usize {
    let mut groups = 0usize;
    let mut in_tool_sequence = false;
    for e in entries {
        if e.kind.as_deref() == Some(ENTRY_KIND_COMPACTION) {
            groups += 1;
            in_tool_sequence = false;
            continue;
        }
        match e.role.as_str() {
            "thought" | "tool_call" | "tool_result" => {
                if !in_tool_sequence {
                    groups += 1;
                    in_tool_sequence = true;
                }
            }
            _ => {
                groups += 1;
                in_tool_sequence = false;
            }
        }
    }
    groups
}

/// Trim entries from the **beginning** so that at most `max_groups` display
/// groups remain (counting from the newest end).
///
/// Entries must be in chronological order (oldest → newest).
/// Returns the split index: `entries[split_idx..]` contains exactly
/// `max_groups` display groups (or fewer if the total is already ≤ max).
///
/// Compaction markers (`kind="compaction"`) are treated as standalone groups
/// and never merged with adjacent tool sequences.
fn trim_oldest_display_groups(entries: &[ConversationEntry], max_groups: usize) -> usize {
    let total = count_display_groups(entries);
    if total <= max_groups {
        return 0;
    }

    // Walk from the newest end, counting groups backwards.
    let mut group_count = 0usize;
    let mut in_tool = false;
    for (i, e) in entries.iter().enumerate().rev() {
        let is_compaction = e.kind.as_deref() == Some(ENTRY_KIND_COMPACTION);
        let in_tool_seq = !is_compaction
            && matches!(e.role.as_str(), "thought" | "tool_call" | "tool_result");
        if is_compaction {
            group_count += 1;
            in_tool = false;
        } else if in_tool_seq {
            if !in_tool {
                group_count += 1;
                in_tool = true;
            }
        } else {
            group_count += 1;
            in_tool = false;
        }
        if group_count == max_groups {
            // If we landed inside a tool sequence, walk back toward older
            // entries to find the sequence start so the whole group is kept.
            // A compaction marker breaks the sequence, so stop at it.
            if in_tool {
                let mut first = i;
                while first > 0
                    && entries[first - 1].kind.as_deref() != Some(ENTRY_KIND_COMPACTION)
                    && matches!(
                        entries[first - 1].role.as_str(),
                        "thought" | "tool_call" | "tool_result"
                    )
                {
                    first -= 1;
                }
                return first;
            }
            return i;
        }
    }
    0
}

/// Trim entries from the **end** so that at most `max_groups` display
/// groups remain (counting from the oldest end).
///
/// Entries must be in chronological order (oldest → newest).
/// Returns the number of entries to keep: `entries[..keep_count]`.
///
/// Compaction markers (`kind="compaction"`) are treated as standalone groups
/// and never merged with adjacent tool sequences.
fn trim_newest_display_groups(entries: &[ConversationEntry], max_groups: usize) -> usize {
    let total = count_display_groups(entries);
    if total <= max_groups {
        return entries.len();
    }

    let mut group_count = 0usize;
    let mut in_tool = false;
    for (i, e) in entries.iter().enumerate() {
        let is_compaction = e.kind.as_deref() == Some(ENTRY_KIND_COMPACTION);
        let in_tool_seq = !is_compaction
            && matches!(e.role.as_str(), "thought" | "tool_call" | "tool_result");
        if is_compaction {
            group_count += 1;
            in_tool = false;
        } else if in_tool_seq {
            if !in_tool {
                group_count += 1;
                in_tool = true;
            }
        } else {
            group_count += 1;
            in_tool = false;
        }
        if group_count == max_groups {
            // Include trailing tool-sequence entries that form the same group
            // (but stop at a compaction marker, which is its own group).
            let mut keep = i + 1;
            while keep < entries.len()
                && entries[keep].kind.as_deref() != Some(ENTRY_KIND_COMPACTION)
                && matches!(entries[keep].role.as_str(), "thought" | "tool_call" | "tool_result")
            {
                keep += 1;
            }
            return keep;
        }
    }
    entries.len()
}

/// A parsed entry together with its file byte offset and raw line length.
struct ParsedLine {
    entry: ConversationEntry,
    offset: u64,
    /// Length of the raw (trimmed) line as it appears in the JSONL file.
    /// Needed for forward-pagination cursor calculation (byte offset after
    /// this line = offset + raw_line_len + 1 for the newline).
    raw_line_len: usize,
}

/// A line with its byte offset in the file.
#[derive(Clone)]
struct LineWithOffset {
    content: String,
    offset: u64,
}

/// Read `count` data lines backward from a file starting at `end_offset`.
///
/// Returns lines in chronological order (oldest → newest) with their byte
/// offsets. Skips the metadata line (first line of the file).
fn read_lines_backward(
    file: &mut std::fs::File,
    end_offset: u64,
    count: usize,
) -> std::io::Result<Vec<LineWithOffset>> {
    let file_len = file.metadata()?.len();
    let end = end_offset.min(file_len);

    if end == 0 || count == 0 {
        return Ok(Vec::new());
    }

    // Phase 1: Read chunks backward, accumulating raw bytes into one buffer.
    // Track the file offset where the accumulated buffer starts.
    let mut buf_start = end;
    let mut accumulated: Vec<u8> = Vec::new();
    let mut found_newlines = 0;

    while found_newlines < count + 1 && buf_start > 0 {
        let chunk_start = buf_start.saturating_sub(BACKWARD_READ_CHUNK as u64);
        let to_read = (buf_start - chunk_start) as usize;

        file.seek(SeekFrom::Start(chunk_start))?;
        let mut chunk = vec![0u8; to_read];
        file.read_exact(&mut chunk)?;

        // Count newlines in this chunk (plus those we already have)
        let newline_count = chunk.iter().filter(|&&b| b == b'\n').count()
            + accumulated.iter().filter(|&&b| b == b'\n').count();

        // Prepend chunk to accumulated buffer
        let mut new_buf = chunk;
        new_buf.extend_from_slice(&accumulated);
        accumulated = new_buf;
        buf_start = chunk_start;

        found_newlines = newline_count;
    }

    // Phase 2: Convert accumulated bytes to string, split into lines,
    // and compute exact byte offsets from buf_start.
    let text = String::from_utf8_lossy(&accumulated);
    let mut lines_with_offsets: Vec<LineWithOffset> = Vec::new();
    let mut byte_pos = buf_start;

    for line in text.split('\n') {
        let line_start = byte_pos;
        byte_pos += line.len() as u64;
        // The newline char itself (if present in the original file)
        // We track it for offset computation but skip adding for the last segment
        // which may not have a trailing newline
        byte_pos += 1u64; // account for the \n separator

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Skip metadata line (contains both "version" and "session_id")
        if trimmed.contains("\"version\"") && trimmed.contains("\"session_id\"") {
            continue;
        }

        lines_with_offsets.push(LineWithOffset {
            content: trimmed.to_string(),
            offset: line_start,
        });
    }

    // Take the last `count` lines (they are already in chronological order)
    let start = lines_with_offsets.len().saturating_sub(count);
    let result = lines_with_offsets[start..].to_vec();

    Ok(result)
}

/// Read `count` data lines forward from a file starting at `start_offset`.
///
/// Returns lines in chronological order with their byte offsets.
/// Skips the metadata line.
fn read_lines_forward(
    file: &mut std::fs::File,
    start_offset: u64,
    count: usize,
) -> std::io::Result<Vec<LineWithOffset>> {
    file.seek(SeekFrom::Start(start_offset))?;
    let reader = BufReader::new(file.try_clone()?);

    let mut lines = Vec::new();
    let mut byte_pos = start_offset;

    for line_result in reader.lines() {
        if lines.len() >= count {
            break;
        }
        let line = line_result?;
        let line_start = byte_pos;
        byte_pos += line.len() as u64 + 1; // +1 for '\n'

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip metadata line
        if trimmed.contains("\"version\"") && trimmed.contains("\"session_id\"") {
            continue;
        }

        lines.push(LineWithOffset {
            content: trimmed.to_string(),
            offset: line_start,
        });
    }

    Ok(lines)
}

/// Parse a cursor string in the format `"offset:<bytes>"`.
///
/// Returns the byte offset, or `None` if the cursor format is invalid.
fn parse_offset_cursor(cursor: &str) -> Option<u64> {
    cursor
        .strip_prefix("offset:")
        .and_then(|s| s.parse::<u64>().ok())
}

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
pub fn scan_sessions_async(
    conversations_dir: PathBuf,
    page: Option<u32>,
    size: Option<u32>,
) -> tokio::task::JoinHandle<(Vec<SessionInfo>, usize)> {
    tokio::task::spawn_blocking(move || {
        // ADR-024: scan per-session meta files instead of index.json.
        let sessions = scan_sessions_from_meta(&conversations_dir);

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

        (infos, total)
    })
}

/// Read messages from a JSONL file with pagination using byte-offset cursors.
///
/// - `cursor`: byte offset in `"offset:<bytes>"` format. If `None`, starts
///   from the most recent messages (backward) or oldest (forward).
/// - `limit`: maximum number of messages to return.
/// - `direction`: "backward" (older, default) or "forward" (newer).
///
/// Performance: backward reading only reads the tail of the file
/// (O(limit) instead of O(n) for full-file scan).
///
/// Returns messages in chronological order (oldest to newest within the page).
pub fn read_messages_paginated(
    path: &Path,
    cursor: Option<String>,
    limit: u32,
    direction: &str,
) -> Result<PaginatedMessages> {
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    // ADR-024: no metadata header line — data starts at byte 0.
    let meta_end = 0u64;

    if file_len == 0 {
        // No messages beyond metadata
        return Ok(PaginatedMessages {
            messages: Vec::new(),
            cursor: None,
            has_more: false,
        });
    }

    // Display path: show the full conversation history.
    //
    // The compaction boundary is only enforced on the **context path**
    // (`restore_history_from_jsonl`), which controls what enters the LLM
    // context window. The display path must show every entry so that
    // reopening a session restores the visual scene the user last saw —
    // including pre-compaction messages, with CompactionCard acting as a
    // visual separator.
    //
    // `meta_end` (the byte offset where the data section begins) is the
    // only lower bound we need: it skips the metadata header line.
    let data_start = meta_end;

    if direction == "forward" {
        read_messages_forward(&mut file, cursor, limit, data_start, file_len)
    } else {
        read_messages_backward(&mut file, cursor, limit, data_start, file_len)
    }
}

/// Backward pagination: read the most recent `limit` **display groups**,
/// or older groups before the cursor offset.
///
/// Consecutive `thought`/`tool_call`/`tool_result` entries count as one
/// group because the frontend collapses them into a single visual item.
///
/// `data_start` is the byte offset where the data section begins (i.e.
/// `meta_end`). Entries strictly before `data_start` are the metadata
/// header and are always skipped. The display path shows the full
/// conversation history — compaction boundary enforcement is only for
/// the context path (`restore_history_from_jsonl`).
fn read_messages_backward(
    file: &mut std::fs::File,
    cursor: Option<String>,
    limit: u32,
    data_start: u64,
    file_len: u64,
) -> Result<PaginatedMessages> {
    let raw_end = cursor
        .as_deref()
        .and_then(parse_offset_cursor)
        .unwrap_or(file_len);
    // Cursor below the data section start means we've reached the
    // beginning of the conversation history — no more pages.
    if raw_end <= data_start {
        return Ok(PaginatedMessages {
            messages: Vec::new(),
            cursor: None,
            has_more: false,
        });
    }
    let end_offset = raw_end;

    // Read enough raw lines to satisfy `limit` display groups.  Cap at
    // MAX_RAW_PER_DISPLAY_PAGE so we never scan the entire file on a huge
    // session just for one page.
    let raw_limit = std::cmp::min(limit as usize * 10, MAX_RAW_PER_DISPLAY_PAGE);
    let line_offsets = read_lines_backward(file, end_offset, raw_limit)?;

    // Parse lines into entries, keeping byte offsets for cursor tracking.
    // Drop any line whose offset falls below `data_start` — those belong
    // to the metadata header and must not be exposed.
    let mut parsed: Vec<ParsedLine> = Vec::new();
    for lo in &line_offsets {
        if lo.offset < data_start {
            continue;
        }
        match serde_json::from_str::<ConversationEntry>(&lo.content) {
            Ok(entry) => {
                parsed.push(ParsedLine {
                    entry,
                    offset: lo.offset,
                    raw_line_len: lo.content.len(),
                });
            }
            Err(e) => {
                tracing::warn!("Skipping invalid JSONL line: {}", e);
            }
        }
    }

    // Build a temporary slice of entries for grouping logic.
    let entries: Vec<ConversationEntry> = parsed.iter().map(|p| p.entry.clone()).collect();

    // Trim to `limit` display groups from the newest end.
    let kept_start = trim_oldest_display_groups(&entries, limit as usize);
    let kept = &parsed[kept_start..];

    // Cursor: byte offset of the oldest entry we kept.
    let page_start_offset = kept
        .first()
        .map(|p| p.offset)
        .unwrap_or(data_start);
    // `has_more` is true only if there is still room above `data_start`.
    // Once we reach the data section start (the metadata header boundary),
    // there is nothing older to offer.
    let has_more = page_start_offset > data_start;

    let messages: Vec<ConversationEntry> = kept.iter().map(|p| p.entry.clone()).collect();

    Ok(PaginatedMessages {
        messages,
        cursor: if has_more {
            Some(format!("offset:{}", page_start_offset))
        } else {
            None
        },
        has_more,
    })
}

/// Forward pagination: read `limit` **display groups** starting from cursor offset.
///
/// Consecutive `thought`/`tool_call`/`tool_result` entries count as one group.
///
/// `data_start` is the byte offset where the data section begins (i.e.
/// `meta_end`). Cursor values below it are clamped up to it so the
/// caller never reads the metadata header. The display path shows the
/// full conversation history — compaction boundary enforcement is only
/// for the context path (`restore_history_from_jsonl`).
fn read_messages_forward(
    file: &mut std::fs::File,
    cursor: Option<String>,
    limit: u32,
    data_start: u64,
    file_len: u64,
) -> Result<PaginatedMessages> {
    let raw_start = cursor
        .as_deref()
        .and_then(parse_offset_cursor)
        .unwrap_or(data_start);
    // Clamp cursor up to data section start to skip the metadata header.
    let start_offset = raw_start.max(data_start);

    // Read enough raw lines to satisfy `limit` display groups.
    let raw_limit = std::cmp::min(limit as usize * 10, MAX_RAW_PER_DISPLAY_PAGE);
    let line_offsets = read_lines_forward(file, start_offset, raw_limit)?;

    // Parse lines into entries with offsets.
    // Drop any line whose offset somehow falls below `data_start` (defensive).
    let mut parsed: Vec<ParsedLine> = Vec::new();
    for lo in &line_offsets {
        if lo.offset < data_start {
            continue;
        }
        match serde_json::from_str::<ConversationEntry>(&lo.content) {
            Ok(entry) => {
                parsed.push(ParsedLine {
                    entry,
                    offset: lo.offset,
                    raw_line_len: lo.content.len(),
                });
            }
            Err(e) => {
                tracing::warn!("Skipping invalid JSONL line: {}", e);
            }
        }
    }

    let entries: Vec<ConversationEntry> = parsed.iter().map(|p| p.entry.clone()).collect();

    // Trim to `limit` display groups from the oldest end.
    let kept_end = trim_newest_display_groups(&entries, limit as usize);
    let kept = &parsed[..kept_end];

    // Cursor: byte offset right after the last kept entry.
    let last_entry = kept.last();
    let last_line_end = last_entry.map_or(start_offset, |p| {
        p.offset + p.raw_line_len as u64 + 1u64
    });
    let has_more = last_line_end < file_len;

    let messages: Vec<ConversationEntry> = kept.iter().map(|p| p.entry.clone()).collect();

    Ok(PaginatedMessages {
        messages,
        cursor: if has_more {
            Some(format!("offset:{}", last_line_end))
        } else {
            None
        },
        has_more,
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
                last_input_tokens: None,
                last_output_tokens: None,
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

        // Read all messages (no cursor)
        let page = read_messages_paginated(&file_path, None, 10, "backward").unwrap();
        assert_eq!(page.messages.len(), 5);
        assert!(!page.has_more);

        // Read with limit 2, backward from end (latest 2)
        let page = read_messages_paginated(&file_path, None, 2, "backward").unwrap();
        assert_eq!(page.messages.len(), 2);
        assert!(page.has_more);
        assert_eq!(page.messages[0].content, "Message 3");
        assert_eq!(page.messages[1].content, "Message 4");

        // Verify cursor format is "offset:<bytes>"
        let cursor = page.cursor.unwrap();
        assert!(
            cursor.starts_with("offset:"),
            "Cursor should be offset format, got: {}",
            cursor
        );

        // Continue backward from cursor
        let page2 = read_messages_paginated(&file_path, Some(cursor), 2, "backward").unwrap();
        assert_eq!(page2.messages.len(), 2);
        assert!(page2.has_more);
        assert_eq!(page2.messages[0].content, "Message 1");
        assert_eq!(page2.messages[1].content, "Message 2");

        // Continue backward to the last page
        let cursor2 = page2.cursor.unwrap();
        assert!(cursor2.starts_with("offset:"));
        let page3 = read_messages_paginated(&file_path, Some(cursor2), 2, "backward").unwrap();
        assert_eq!(page3.messages.len(), 1);
        assert!(
            !page3.has_more,
            "No more messages after reaching the beginning"
        );
        assert_eq!(page3.messages[0].content, "Message 0");

        // Read forward from beginning (no cursor)
        let fwd = read_messages_paginated(&file_path, None, 3, "forward").unwrap();
        assert_eq!(fwd.messages.len(), 3);
        assert!(fwd.has_more);
        assert_eq!(fwd.messages[0].content, "Message 0");
        assert_eq!(fwd.messages[1].content, "Message 1");
        assert_eq!(fwd.messages[2].content, "Message 2");

        // Continue forward from cursor
        let fwd_cursor = fwd.cursor.unwrap();
        assert!(fwd_cursor.starts_with("offset:"));
        let fwd2 = read_messages_paginated(&file_path, Some(fwd_cursor), 10, "forward").unwrap();
        assert_eq!(fwd2.messages.len(), 2);
        assert!(!fwd2.has_more);
        assert_eq!(fwd2.messages[0].content, "Message 3");
        assert_eq!(fwd2.messages[1].content, "Message 4");
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
            last_input_tokens: None,
            last_output_tokens: None,
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
        let (sessions, _total) =
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
            last_input_tokens: Some(45_000),
            last_output_tokens: Some(1_200),
            last_compaction_offset: None,
            corrupted: false,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.last_input_tokens, Some(45_000));
        assert_eq!(parsed.last_output_tokens, Some(1_200));
        assert_eq!(parsed.last_compaction_offset, None);
        assert_eq!(parsed.version, CONVERSATION_FORMAT_VERSION);
        assert_eq!(parsed.model.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn test_session_meta_fields_default_to_none() {
        // ADR-024: old JSON without optional fields defaults correctly.
        let old_json = r#"{"version":2,"session_id":"old","agent_id":"com.test","created_at":"2026-01-01T00:00:00Z","last_active_at":"2026-01-01T00:00:00Z","message_count":0,"corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(old_json).unwrap();
        assert_eq!(meta.last_input_tokens, None, "should default to None");
        assert_eq!(meta.last_output_tokens, None, "should default to None");
        assert_eq!(meta.model, None);
        assert_eq!(meta.provider, None);
    }

    // ── display group pagination tests ────────────────────────────

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
    fn display_group_count_plain_messages() {
        // user, assistant, user, assistant, user → 5 groups
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "assistant", "a1"),
        ];
        assert_eq!(count_display_groups(&entries), 2);
    }

    #[test]
    fn display_group_collapses_tool_sequence() {
        // user, thought, tool_call, tool_result, assistant
        // → user, {tool sequence}, assistant = 3 groups
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "thought", "thinking…"),
            make_entry("3", "tool_call", "{…}"),
            make_entry("4", "tool_result", "result"),
            make_entry("5", "assistant", "done"),
        ];
        assert_eq!(count_display_groups(&entries), 3);
    }

    #[test]
    fn display_group_multiple_tool_bursts() {
        // user, thought, tc, tr, thought, tc, tr, assistant, user, thought, tc, tr, assistant
        // → u1, {t1,tc1,tr1,t2,tc2,tr2}, a1, u2, {t3,tc3,tr3}, a2 = 6 groups
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "thought", "t1"),
            make_entry("3", "tool_call", "tc1"),
            make_entry("4", "tool_result", "tr1"),
            make_entry("5", "thought", "t2"),
            make_entry("6", "tool_call", "tc2"),
            make_entry("7", "tool_result", "tr2"),
            make_entry("8", "assistant", "a1"),
            make_entry("9", "user", "u2"),
            make_entry("10", "thought", "t3"),
            make_entry("11", "tool_call", "tc3"),
            make_entry("12", "tool_result", "tr3"),
            make_entry("13", "assistant", "a2"),
        ];
        assert_eq!(count_display_groups(&entries), 6);
    }

    #[test]
    fn backward_limit_respects_display_groups() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "thought", "t1"),
            make_entry("3", "tool_call", "tc1"),
            make_entry("4", "tool_result", "tr1"),
            make_entry("5", "assistant", "a1"),
            make_entry("6", "user", "u2"),
            make_entry("7", "thought", "t2"),
            make_entry("8", "tool_call", "tc2"),
            make_entry("9", "tool_result", "tr2"),
            make_entry("10", "assistant", "a2"),
        ];
        // 6 display groups: u1, {t1,tc1,tr1}, a1, u2, {t2,tc2,tr2}, a2
        let path = write_test_jsonl(&dir, "sess-groups", &entries);

        // limit=6 groups → all 10 raw entries
        let page = read_messages_paginated(&path, None, 6, "backward").unwrap();
        assert_eq!(page.messages.len(), 10, "6 groups → all entries");
        assert!(!page.has_more);

        // limit=2 groups → keep newest 2: u2 + {t2,tc2,tr2} + a2 = wait...
        // Actually: limit=2 from NEWEST means we keep the LAST 2 groups.
        // Groups (oldest→newest): u1, G1, a1, u2, G2, a2
        // Last 2: G2 + a2 = 4 raw entries (t2, tc2, tr2, a2)
        let page = read_messages_paginated(&path, None, 2, "backward").unwrap();
        assert_eq!(page.messages.len(), 4, "2 groups → 4 entries");
        assert!(page.has_more);
        assert_eq!(page.messages[0].content, "t2");
        assert_eq!(page.messages[3].content, "a2");
    }

    #[test]
    fn user_message_visible_with_tool_heavy_conversation() {
        // Simulates the user's scenario: 1 user message + many tool calls + assistant.
        let dir = TempDir::new().unwrap();
        let mut entries = vec![make_entry("1", "user", "user-msg")];
        // 20 tool rounds (thought + tool_call + tool_result = 60 entries)
        for i in 0..20 {
            entries.push(make_entry(
                &format!("t{}", i * 3 + 2), "thought", &format!("think-{}", i),
            ));
            entries.push(make_entry(
                &format!("t{}", i * 3 + 3), "tool_call", &format!("call-{}", i),
            ));
            entries.push(make_entry(
                &format!("t{}", i * 3 + 4), "tool_result", &format!("result-{}", i),
            ));
        }
        entries.push(make_entry("last", "assistant", "final-reply"));
        // Total: 62 raw entries, 3 display groups (user, {tool seq}, assistant)

        let path = write_test_jsonl(&dir, "sess-heavy", &entries);

        // limit=50 display groups (frontend default) — more than the 3 groups we have
        let page = read_messages_paginated(&path, None, 50, "backward").unwrap();
        assert_eq!(page.messages.len(), 62, "all entries should be in one page");
        assert!(!page.has_more);
        // User message must be present
        assert!(
            page.messages.iter().any(|m| m.role == "user" && m.content == "user-msg"),
            "user message must be visible"
        );
    }

    #[test]
    fn trim_oldest_keeps_exact_groups() {
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "thought", "t1"),
            make_entry("3", "tool_call", "tc1"),
            make_entry("4", "tool_result", "tr1"),
            make_entry("5", "assistant", "a1"),
            make_entry("6", "user", "u2"),
        ];
        // 4 groups: u1, {t1,tc1,tr1}, a1, u2
        let split = trim_oldest_display_groups(&entries, 2);
        // Keep last 2 groups: a1, u2 → entries[4..]
        assert_eq!(split, 4);
        assert_eq!(entries[split].content, "a1");
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
    fn display_group_compaction_is_standalone() {
        // user, thought, tool_call, [COMPACTION], tool_result, assistant
        // The compaction marker BREAKS the tool sequence into two halves.
        // Groups: user, {thought,tool_call}, COMPACTION, {tool_result}, assistant = 5
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "thought", "t1"),
            make_entry("3", "tool_call", "tc1"),
            make_compaction_entry("4", "<summary>compacted u1..tc1</summary>"),
            make_entry("5", "tool_result", "tr1"),
            make_entry("6", "assistant", "a1"),
        ];
        assert_eq!(count_display_groups(&entries), 5);

        // Compaction adjacent to plain user/assistant is also its own group.
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "assistant", "a1"),
            make_compaction_entry("3", "<summary>...</summary>"),
            make_entry("4", "user", "u2"),
            make_entry("5", "assistant", "a2"),
        ];
        // Groups: u1, a1, COMPACTION, u2, a2 = 5
        assert_eq!(count_display_groups(&entries), 5);
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

        // limit large enough to span the entire file
        let page = read_messages_paginated(&path, None, 50, "backward").unwrap();
        // Expect: all 7 entries (4 pre-compaction + compaction + 2 post-compaction)
        assert_eq!(page.messages.len(), 7, "display path must show full history");
        assert!(!page.has_more, "no more pages — entire file consumed");

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
    fn pagination_has_more_true_at_compaction_boundary() {
        // Tight limit so the first page does NOT include the compaction marker;
        // the cursor returned must allow paging past the compaction boundary
        // to reach pre-compaction history (display path shows everything).
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

        // limit=2 groups → keep last 2 groups (new-u3, new-a3)
        let page1 = read_messages_paginated(&path, None, 2, "backward").unwrap();
        assert_eq!(page1.messages.len(), 2);
        assert_eq!(page1.messages[0].content, "new-u3");
        assert_eq!(page1.messages[1].content, "new-a3");
        assert!(page1.has_more, "more entries ahead (compaction + pre-compaction)");

        // Page 2 with the cursor should bring back everything before page1:
        // old-u1, old-a1, COMPACTION, new-u2, new-a2 (5 entries).
        let page2 = read_messages_paginated(
            &path,
            page1.cursor.clone(),
            50,
            "backward",
        )
        .unwrap();
        assert!(
            page2.messages.iter().any(|m| m.kind.as_deref() == Some(ENTRY_KIND_COMPACTION)),
            "page 2 must include the compaction marker"
        );
        assert!(
            page2.messages.iter().any(|m| m.content.starts_with("old-")),
            "page 2 must include pre-compaction history"
        );
        assert!(!page2.has_more, "no more pages — reached data section start");
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

        // Stale cursor pointing at offset 0 (below data section start).
        let stale_cursor = Some("offset:0".to_string());
        let page = read_messages_paginated(&path, stale_cursor, 50, "forward").unwrap();
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

        let page = read_messages_paginated(&path, None, 50, "backward").unwrap();
        assert_eq!(page.messages.len(), 4);
        assert!(!page.has_more);
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
