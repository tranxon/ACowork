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

/// Session metadata written as the first line of each JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Format version, currently 1
    pub version: u32,
    /// Session identifier
    pub session_id: String,
    /// ISO 8601 creation timestamp
    pub created_at: String,
    /// Agent identifier
    pub agent_id: String,
    /// Optional session title
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional last update timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Optional message count
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<u32>,
    /// Whether the metadata was recovered from a corrupted first line.
    /// When true, other fields may contain degraded/default values.
    #[serde(default)]
    pub corrupted: bool,
    /// Per-session workspace selection.
    /// `None` or `"__agent_home__"` means the agent's home directory.
    /// Persisted so sessions restore their workspace on cold start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Per-session model selection (ADR-012).
    /// Persisted so sessions restore their model on cold start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Per-session provider selection (ADR-012).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Per-session reasoning effort override.
    /// Persisted so sessions restore the user's thinking-level preference on resume.
    /// `None` means not set (use provider capability default on resume).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Per-session temperature override.
    /// Persisted so sessions restore the temperature preference on resume.
    /// `None` means not set (use agent config or global default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Last reported `prompt_tokens` from an LLM API response.
    ///
    /// Persisted so the frontend can render the context-usage indicator
    /// immediately on session resume, before any new LLM call has provided
    /// fresh `usage`. `None` for sessions that have not yet completed an
    /// LLM round, or for sessions written before this field was added.
    ///
    /// Only the raw value is stored; `usable_context`, `usage_percent`, and
    /// `context_window` are *not* persisted because they are model-derived
    /// and become stale on model switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_tokens: Option<u64>,
    /// Last reported `completion_tokens` from an LLM API response.
    ///
    /// Persisted alongside `last_input_tokens` purely for UI continuity on
    /// resume (so the "output tokens" stat does not visually reset to 0).
    /// Not used in any window-budget decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_tokens: Option<u64>,
    /// Byte offset of the most recent compaction marker relative to the
    /// **start of the data section** (i.e. `absolute_offset - meta_end`,
    /// where `meta_end` = metadata line byte length + 1).
    ///
    /// This is a relative offset so that metadata rewrites (which shift
    /// every data line by the same Δ when the first line changes length)
    /// do not invalidate it.  Callers recover the absolute offset via
    /// `meta_end + last_compaction_offset`.
    ///
    /// `None` if no compaction has occurred (legacy session, or session
    /// written before this field was added).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_offset: Option<u64>,
}

/// Commands sent to the background writer thread.
pub enum WriterCommand {
    /// Append a conversation entry to the JSONL file.
    AppendEntry(ConversationEntry),
    /// Update the session metadata (rewrites first line).
    UpdateMetadata(SessionMetadata),
    /// Flush and shut down the writer.
    Shutdown(oneshot::Sender<()>),
}

/// Background writer that exclusively owns the JSONL file handle.
pub struct ConversationWriter {
    file: std::fs::File,
    /// Path to the JSONL file (needed for atomic rename in rewrite_metadata)
    path: PathBuf,
    receiver: mpsc::UnboundedReceiver<WriterCommand>,
    /// Byte offset where the data section starts (= metadata line length + 1
    /// for the newline).  Updated after every `rewrite_metadata` call so the
    /// writer can compute compaction offsets relative to the data start.
    meta_end: u64,
    /// Byte offset of the most recent compaction marker, relative to
    /// `meta_end` (i.e. `absolute_offset - meta_end`).  Persisted into
    /// metadata on the next `UpdateMetadata` command.  `None` if no
    /// compaction has been written during this writer's lifetime.
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
        path: PathBuf,
        receiver: mpsc::UnboundedReceiver<WriterCommand>,
        meta_end: u64,
        committed_lines: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            file,
            path,
            receiver,
            meta_end,
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
                            // Entry successfully written — record the relative offset.
                            self.last_compaction_offset = Some(abs - self.meta_end);
                            tracing::debug!(
                                abs_offset = abs,
                                meta_end = self.meta_end,
                                relative = abs - self.meta_end,
                                "Recorded compaction offset"
                            );
                        }
                    }
                }
                WriterCommand::UpdateMetadata(mut meta) => {
                    // Inject the current compaction offset so it gets persisted
                    // alongside whatever triggered this update.
                    meta.last_compaction_offset = self.last_compaction_offset;
                    if let Err(e) = self.rewrite_metadata(&meta) {
                        tracing::error!("Failed to rewrite session metadata: {}", e);
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

    /// Rewrite the first line with updated metadata.
    ///
    /// Uses write-to-temp + atomic rename to prevent data loss on crash.
    /// If the process dies during rewrite, the original file remains intact
    /// (the temp file is simply discarded).
    fn rewrite_metadata(&mut self, meta: &SessionMetadata) -> std::io::Result<()> {
        let original_path = self.path.clone();
        let temp_path = original_path.with_extension("jsonl.tmp");

        // Read existing content from current file
        let content = std::fs::read_to_string(&original_path)?;
        let mut lines: Vec<&str> = content.lines().collect();

        // Replace first line with new metadata
        let new_meta = serde_json::to_string(meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if lines.is_empty() {
            lines.push(&new_meta);
        } else {
            lines[0] = &new_meta;
        }

        // Write complete content to temp file
        let mut output = lines.join("\n");
        output.push('\n');
        std::fs::write(&temp_path, &output)?;

        // Atomic rename — on same filesystem, this is atomic on both Unix and Windows
        std::fs::rename(&temp_path, &original_path)?;

        // Reopen the file handle since the old handle points to a replaced file
        self.file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&original_path)?;

        // Update meta_end — the new metadata line may have a different byte
        // length, which shifts every data line by Δ (keeping stored relative
        // compaction offsets valid without recomputation).
        self.meta_end = new_meta.len() as u64 + 1;

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
    /// Currently persisted title, for deduplicating force-update calls.
    current_title: std::sync::Mutex<Option<String>>,
    /// Per-session workspace selection, persisted in JSONL metadata.
    /// `None` or `"__agent_home__"` means the agent's home directory.
    /// Wrapped in Mutex for interior mutability so that both file persistence
    /// and in-memory state are updated atomically on the API side.
    workspace_id: std::sync::Mutex<Option<String>>,
    /// Per-session model selection (ADR-012).
    model: std::sync::Mutex<Option<String>>,
    /// Per-session provider selection (ADR-012).
    provider: std::sync::Mutex<Option<String>>,
    /// Per-session reasoning effort override, persisted in JSONL metadata.
    reasoning_effort: std::sync::Mutex<Option<String>>,
    /// Per-session temperature override, persisted in JSONL metadata.
    temperature: std::sync::Mutex<Option<f32>>,
    /// Last observed (input_tokens, output_tokens) from an LLM response.
    /// Persisted into JSONL metadata so the UI can restore the
    /// "context usage" indicator after a session resume.
    /// `None` means no LLM call has been made (or persisted) yet.
    last_tokens: std::sync::Mutex<Option<(u64, u64)>>,
    /// Running message count, incremented on every `append_message`.
    /// Used by the session index for fast UI display without reading JSONL.
    message_count: AtomicU64,
    /// Last time `update_index_entry` was written to disk.
    /// Guards against excessive I/O from rapid `append_message` calls.
    last_index_update: std::sync::Mutex<Instant>,
    sender: mpsc::UnboundedSender<WriterCommand>,
    /// Path to the JSONL file (for session-level distillation on close).
    session_file_path: PathBuf,
    /// Path to the conversations directory (for index.json updates).
    conversations_dir: PathBuf,
}

/// Minimum interval between index.json writes triggered by `append_message`.
///
/// Metadata-only mutations (title, model, provider, workspace) always write
/// immediately.  Only the high-frequency `append_message` path respects this
/// cooldown, capping index write I/O regardless of conversation speed.
const INDEX_COOLDOWN_MS: u64 = 3000; // 3 seconds

impl ConversationSession {
    /// Create a new session with optional initial metadata.
    ///
    /// Creates `{work_dir}/conversations/{session_id}.jsonl`, writes the
    /// `SessionMetadata` header (including initial model/provider/workspace_id),
    /// and starts the background writer thread.
    pub fn new(work_dir: &Path, session_id: &str, config: SessionConfig, max_sessions: usize, committed_lines: Arc<AtomicUsize>) -> Result<Self> {
        let conversations_dir = work_dir.join("conversations");
        std::fs::create_dir_all(&conversations_dir)?;

        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&file_path)?;

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let now_for_self = now.clone();
        let metadata = SessionMetadata {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: session_id.to_string(),
            created_at: now.clone(),
            agent_id: config.agent_id.clone(),
            title: None,
            updated_at: Some(now),
            message_count: Some(0),
            corrupted: false,
            workspace_id: config.workspace_id.clone(),
            model: config.model.clone(),
            provider: config.provider.clone(),
            reasoning_effort: None,
            temperature: None,
            last_input_tokens: None,
            last_output_tokens: None,
            last_compaction_offset: None,
        };

        // Write metadata as the first line — build complete line then single write
        let mut line = serde_json::to_string(&metadata)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.sync_data()?;

        let meta_end = line.len() as u64;
        let (tx, rx) = mpsc::unbounded_channel::<WriterCommand>();
        let writer = ConversationWriter::new(file, file_path.clone(), rx, meta_end, committed_lines);
        std::thread::spawn(move || writer.run());

        let session = Self {
            session_id: session_id.to_string(),
            agent_id: config.agent_id,
            created_at: now_for_self,
            title_set: AtomicBool::new(false),
            current_title: std::sync::Mutex::new(None),
            workspace_id: std::sync::Mutex::new(config.workspace_id),
            model: std::sync::Mutex::new(config.model),
            provider: std::sync::Mutex::new(config.provider),
            reasoning_effort: std::sync::Mutex::new(None),
            temperature: std::sync::Mutex::new(None),
            last_tokens: std::sync::Mutex::new(None),
            message_count: AtomicU64::new(0),
            last_index_update: std::sync::Mutex::new(Instant::now()),
            sender: tx,
            session_file_path: file_path,
            conversations_dir: conversations_dir.clone(),
        };

        // Persist the new session to index.json immediately so it appears
        // in session lists without waiting for a directory scan.
        session.update_index_entry(true); // force: new session must appear immediately

        // Enforce max-sessions limit: prune the oldest sessions if the
        // index now exceeds the configured threshold.
        if max_sessions > 0 {
            prune_excess_sessions(&conversations_dir, max_sessions);
        }

        Ok(session)
    }

    /// Resume an existing session.
    ///
    /// Opens the existing JSONL file in append mode and starts the
    /// background writer thread.
    pub fn resume(work_dir: &Path, session_id: &str, committed_lines: Arc<AtomicUsize>) -> Result<Self> {
        let conversations_dir = work_dir.join("conversations");
        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file_path)?;

        // Read existing metadata to get agent_id
        let meta = read_session_metadata(&file_path)?;

        let meta_end = metadata_end_offset(&mut file)?;

        let (tx, rx) = mpsc::unbounded_channel::<WriterCommand>();
        let writer = ConversationWriter::new(file, file_path.clone(), rx, meta_end, committed_lines);
        std::thread::spawn(move || writer.run());

        let msg_count = meta.message_count.unwrap_or(0) as u64;

        Ok(Self {
            session_id: session_id.to_string(),
            agent_id: meta.agent_id,
            created_at: meta.created_at,
            title_set: AtomicBool::new(meta.title.is_some()),
            current_title: std::sync::Mutex::new(meta.title.clone()),
            workspace_id: std::sync::Mutex::new(meta.workspace_id.clone()),
            model: std::sync::Mutex::new(meta.model.clone()),
            provider: std::sync::Mutex::new(meta.provider.clone()),
            reasoning_effort: std::sync::Mutex::new(meta.reasoning_effort.clone()),
            temperature: std::sync::Mutex::new(meta.temperature),
            last_tokens: std::sync::Mutex::new(
                match (meta.last_input_tokens, meta.last_output_tokens) {
                    (Some(i), Some(o)) => Some((i, o)),
                    (Some(i), None) => Some((i, 0)),
                    (None, Some(o)) => Some((0, o)),
                    (None, None) => None,
                },
            ),
            message_count: AtomicU64::new(msg_count),
            last_index_update: std::sync::Mutex::new(Instant::now()),
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
        // Update message count and index so the frontend sees the latest
        // active-at timestamp without a directory scan.
        self.message_count.fetch_add(1, Ordering::Relaxed);
        self.update_index_entry(false); // throttled: high-frequency message path
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

    /// Update the session metadata (e.g. message_count).
    ///
    /// Non-blocking: sent via channel to the writer thread.
    pub fn update_metadata(&self, metadata: SessionMetadata) {
        if let Err(e) = self.sender.send(WriterCommand::UpdateMetadata(metadata)) {
            tracing::error!(
                "Failed to send metadata update to conversation writer: {}",
                e
            );
        }
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
        let metadata = SessionMetadata {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            created_at: self.created_at.clone(),
            agent_id: self.agent_id.clone(),
            title: Some(title.clone()),
            updated_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            message_count: None,
            corrupted: false,
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            reasoning_effort: self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            temperature: self.temperature.lock().ok().and_then(|t| *t),
            last_input_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(i, _)| i)),
            last_output_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(_, o)| o)),
            last_compaction_offset: None,
        };
        self.update_metadata(metadata);
        // Track current title for dedup
        if let Ok(mut current) = self.current_title.lock() {
            *current = Some(title);
        }
        self.update_index_entry(true); // force: title is a user-visible metadata change
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
        let metadata = SessionMetadata {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            created_at: self.created_at.clone(),
            agent_id: self.agent_id.clone(),
            title: Some(truncated.clone()),
            updated_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            message_count: None,
            corrupted: false,
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            reasoning_effort: self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            temperature: self.temperature.lock().ok().and_then(|t| *t),
            last_input_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(i, _)| i)),
            last_output_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(_, o)| o)),
            last_compaction_offset: None,
        };
        self.update_metadata(metadata);
        // Track current title for dedup
        if let Ok(mut current) = self.current_title.lock() {
            *current = Some(truncated.clone());
        }
        self.update_index_entry(true);
        tracing::info!(session_id = %self.session_id, title = %truncated, "Session title force-updated via API");
        true
    }

    /// Persist the per-session workspace selection to the JSONL metadata.
    ///
    /// Rewrites the first line of the JSONL file so the workspace binding
    /// survives cold restarts. The authoritative workspace_id is stored in
    /// [`SessionCore`]; this method only persists to disk.
    pub fn update_workspace_id(&self, workspace_id: &str) {
        // Update in-memory state FIRST so that subsequent metadata updates
        // (e.g. set_title via first user message) don't lose workspace_id.
        if let Ok(mut w) = self.workspace_id.lock() {
            *w = Some(workspace_id.to_string());
        }
        let metadata = SessionMetadata {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            created_at: self.created_at.clone(),
            agent_id: self.agent_id.clone(),
            title: self.current_title.lock().ok().and_then(|t| t.clone()),
            updated_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            message_count: None,
            corrupted: false,
            workspace_id: Some(workspace_id.to_string()),
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            reasoning_effort: self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            temperature: self.temperature.lock().ok().and_then(|t| *t),
            last_input_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(i, _)| i)),
            last_output_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(_, o)| o)),
            last_compaction_offset: None,
        };
        self.update_metadata(metadata);
        self.update_index_entry(true);
        tracing::info!(
            session_id = %self.session_id,
            workspace_id = %workspace_id,
            "Session workspace_id persisted to JSONL"
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

    /// Persist the per-session model and provider selection to JSONL metadata (ADR-012).
    ///
    /// Rewrites the first line of the JSONL file so the model binding
    /// survives cold restarts. Does NOT mutate the in-memory
    /// `SessionState` — the caller is responsible for keeping the two in sync.
    pub fn update_model_provider(&self, model: &str, provider: Option<&str>) {
        // Update in-memory state FIRST so that subsequent metadata updates
        // (e.g. set_title via first user message) don't lose model/provider.
        if let Ok(mut m) = self.model.lock() {
            *m = Some(model.to_string());
        }
        if let Ok(mut p) = self.provider.lock() {
            *p = provider.map(|s| s.to_string());
        }
        let metadata = SessionMetadata {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            created_at: self.created_at.clone(),
            agent_id: self.agent_id.clone(),
            title: self.current_title.lock().ok().and_then(|t| t.clone()),
            updated_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            message_count: None,
            corrupted: false,
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            model: Some(model.to_string()),
            provider: provider.map(|s| s.to_string()),
            reasoning_effort: self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            temperature: self.temperature.lock().ok().and_then(|t| *t),
            last_input_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(i, _)| i)),
            last_output_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(_, o)| o)),
            last_compaction_offset: None,
        };
        self.update_metadata(metadata);
        self.update_index_entry(true);
        tracing::info!(
            session_id = %self.session_id,
            model = %model,
            provider = ?provider,
            "Session model/provider persisted to JSONL"
        );
    }

    /// Return the persisted reasoning_effort string, if any.
    pub fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort.lock().ok().and_then(|r| r.clone())
    }

    /// Persist the per-session reasoning_effort override to JSONL metadata.
    ///
    /// Updates in-memory state and rewrites the JSONL first line.
    pub fn update_reasoning_effort(&self, effort: Option<String>) {
        if let Ok(mut r) = self.reasoning_effort.lock() {
            *r = effort.clone();
        }
        let metadata = SessionMetadata {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            created_at: self.created_at.clone(),
            agent_id: self.agent_id.clone(),
            title: self.current_title.lock().ok().and_then(|t| t.clone()),
            updated_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            message_count: None,
            corrupted: false,
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            reasoning_effort: effort,
            temperature: self.temperature.lock().ok().and_then(|t| *t),
            last_input_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(i, _)| i)),
            last_output_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(_, o)| o)),
            last_compaction_offset: None,
        };
        self.update_metadata(metadata);
        tracing::info!(
            session_id = %self.session_id,
            "Session reasoning_effort persisted to JSONL"
        );
    }

    /// Return the persisted temperature, if any.
    pub fn temperature(&self) -> Option<f32> {
        self.temperature.lock().ok().and_then(|t| *t)
    }

    /// Persist the per-session temperature override to JSONL metadata.
    pub fn update_temperature(&self, temperature: Option<f32>) {
        if let Ok(mut t) = self.temperature.lock() {
            *t = temperature;
        }
        let metadata = SessionMetadata {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            created_at: self.created_at.clone(),
            agent_id: self.agent_id.clone(),
            title: self.current_title.lock().ok().and_then(|t| t.clone()),
            updated_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            message_count: None,
            corrupted: false,
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            reasoning_effort: self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            temperature,
            last_input_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(i, _)| i)),
            last_output_tokens: self.last_tokens.lock().ok().and_then(|t| t.map(|(_, o)| o)),
            last_compaction_offset: None,
        };
        self.update_metadata(metadata);
        tracing::info!(
            session_id = %self.session_id,
            "Session temperature persisted to JSONL"
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

    /// Persist the most recent LLM `usage` (input/output tokens) to JSONL
    /// metadata so the context-usage indicator survives a session resume.
    ///
    /// Called from the agent loop right after a `ContextUsage` chunk is
    /// emitted. Cheap (single metadata rewrite, debounced by the writer
    /// thread) but still optional — failure is non-fatal.
    pub fn update_last_tokens(&self, input_tokens: u64, output_tokens: u64) {
        if let Ok(mut t) = self.last_tokens.lock() {
            *t = Some((input_tokens, output_tokens));
        }
        let metadata = SessionMetadata {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            created_at: self.created_at.clone(),
            agent_id: self.agent_id.clone(),
            title: self.current_title.lock().ok().and_then(|t| t.clone()),
            updated_at: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            message_count: None,
            corrupted: false,
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            reasoning_effort: self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            temperature: self.temperature.lock().ok().and_then(|t| *t),
            last_input_tokens: Some(input_tokens),
            last_output_tokens: Some(output_tokens),
            last_compaction_offset: None,
        };
        self.update_metadata(metadata);
    }

    // ── Session index helpers ───────────────────────────────────────────

    /// Update this session's entry in `conversations/index.json`.
    ///
    /// Performs a load-modify-write cycle.  Concurrent writes from
    /// different sessions can race (last writer wins), which is
    /// acceptable because the affected fields (`last_active_at`,
    /// `message_count`) are eventually consistent — the next write
    /// from this session will correct any stale values.
    ///
    /// When `force` is `true` the write always happens immediately.
    /// When `force` is `false` (used by `append_message`), writes are
    /// throttled to at most one per `INDEX_COOLDOWN_MS` per session.
    fn update_index_entry(&self, force: bool) {
        if !force
            && let Ok(last) = self.last_index_update.lock()
            && last.elapsed().as_millis() < INDEX_COOLDOWN_MS as u128
        {
            return; // skip — in-memory counters are already up to date
        }

        let (mut index, _rebuilt) = ensure_index(&self.conversations_dir);
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let entry = SessionIndexEntry {
            title: self.current_title.lock().ok().and_then(|t| t.clone()),
            created_at: self.created_at.clone(),
            last_active_at: now,
            message_count: self.message_count.load(Ordering::Relaxed),
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            corrupted: false,
        };
        index
            .sessions
            .insert(self.session_id.clone(), entry);
        if let Err(e) = write_index_atomic(&self.conversations_dir, &index) {
            tracing::warn!(
                session_id = %self.session_id,
                error = %e,
                "Failed to update session index"
            );
        }

        if let Ok(mut last) = self.last_index_update.lock() {
            *last = Instant::now();
        }
    }
}

// Safety: ConversationSession only contains String and UnboundedSender,
// both of which are Send + Sync.
unsafe impl Send for ConversationSession {}
unsafe impl Sync for ConversationSession {}

impl Drop for ConversationSession {
    fn drop(&mut self) {
        // Force-flush the final state to index.json so the frontend sees
        // the correct message_count and last_active_at even if the last
        // `append_message` fell within the cooldown window.
        self.update_index_entry(true);
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

// ── Session Index (fast O(1) lookup, avoids per-file metadata reads) ───────

/// Per-session entry in the conversations index file.
///
/// Stored in `conversations/index.json` as a fast lookup source,
/// eliminating the need for directory scans and per-file metadata reads
/// when listing sessions or finding the most recently active session.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionIndexEntry {
    /// Optional session title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// ISO 8601 creation timestamp.
    created_at: String,
    /// ISO 8601 timestamp of the most recent message append.
    /// Updated on every `append_message` call; used by `find_latest_session`
    /// to select the session the user was most recently active in.
    last_active_at: String,
    /// Number of messages appended to this session.
    message_count: u64,
    /// Per-session workspace selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    /// Per-session model name (ADR-012).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Per-session provider name (ADR-012).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    /// Whether this session's JSONL metadata was recovered from a corrupted
    /// first line.  Only ever `true` for entries produced by
    /// `rebuild_index_from_disk`; live sessions never set this.
    #[serde(default)]
    corrupted: bool,
}

/// In-memory representation of `conversations/index.json`.
///
/// A flat map from `session_id` → `SessionIndexEntry`, designed for
/// fast lookups without touching individual JSONL files.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionIndex {
    sessions: HashMap<String, SessionIndexEntry>,
}

impl SessionIndex {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }
}

/// Atomically write the session index to `{conversations_dir}/index.json`.
///
/// Uses write-to-temp + rename to prevent corruption on crash.
/// Returns the number of sessions written on success.
fn write_index_atomic(conversations_dir: &Path, index: &SessionIndex) -> std::io::Result<usize> {
    let index_path = conversations_dir.join("index.json");
    let temp_path = conversations_dir.join("index.json.tmp");
    let json = serde_json::to_string_pretty(index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let count = index.sessions.len();
    std::fs::write(&temp_path, json)?;
    std::fs::rename(&temp_path, &index_path)?;
    Ok(count)
}

/// Load the session index from disk.
///
/// Returns `None` if the index file does not exist or is corrupted.
fn load_index(conversations_dir: &Path) -> Option<SessionIndex> {
    let index_path = conversations_dir.join("index.json");
    let data = std::fs::read_to_string(&index_path).ok()?;
    match serde_json::from_str::<SessionIndex>(&data) {
        Ok(index) => Some(index),
        Err(e) => {
            tracing::warn!(
                path = %index_path.display(),
                error = %e,
                "Session index is corrupted, will rebuild from disk"
            );
            None
        }
    }
}

/// Rebuild the index from scratch by scanning all JSONL files.
///
/// Used as a fallback when `index.json` is missing or corrupted.
/// Reads the metadata line from every `.jsonl` file in the directory.
/// The rebuilt index is persisted to disk so subsequent calls use the fast path.
fn rebuild_index_from_disk(conversations_dir: &Path) -> SessionIndex {
    let mut index = SessionIndex::new();
    let Ok(rd) = std::fs::read_dir(conversations_dir) else {
        return index;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
            && let Some(sid) = path.file_stem().and_then(|s| s.to_str())
            && let Ok(meta) = read_session_metadata(&path)
        {
            // Reconstructed sessions use `updated_at` (set on metadata rewrites)
            // as a best-effort `last_active_at`.  Once the session is loaded
            // in memory and `append_message` is called, it will be corrected.
            let entry = SessionIndexEntry {
                title: meta.title,
                created_at: meta.created_at.clone(),
                last_active_at: meta.updated_at.unwrap_or(meta.created_at),
                message_count: meta.message_count.unwrap_or(0) as u64,
                workspace_id: meta.workspace_id,
                model: meta.model,
                provider: meta.provider,
                corrupted: meta.corrupted,
            };
            index.sessions.insert(sid.to_string(), entry);
        }
    }
    if let Err(e) = write_index_atomic(conversations_dir, &index) {
        tracing::warn!(
            "Failed to persist rebuilt session index ({} entries): {}",
            index.sessions.len(),
            e
        );
    } else {
        tracing::info!(
            count = index.sessions.len(),
            "Rebuilt and persisted session index from disk"
        );
    }
    index
}

/// Ensure the session index exists, loading or rebuilding as needed.
///
/// Returns the index and a boolean indicating whether it was freshly rebuilt
/// (useful for callers that want to log or react to a rebuild).
fn ensure_index(conversations_dir: &Path) -> (SessionIndex, bool) {
    if let Some(index) = load_index(conversations_dir) {
        (index, false)
    } else {
        (rebuild_index_from_disk(conversations_dir), true)
    }
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

    let mut index = match load_index(conversations_dir) {
        Some(idx) => idx,
        None => return 0, // index missing — nothing to prune
    };

    if index.sessions.len() <= max_sessions {
        return 0;
    }

    // Sort by last_active_at ascending (oldest first).
    let mut sorted: Vec<(String, String)> = index
        .sessions
        .iter()
        .map(|(sid, e)| (sid.clone(), e.last_active_at.clone()))
        .collect();
    sorted.sort_by(|a, b| a.1.cmp(&b.1));

    let to_remove = index.sessions.len() - max_sessions;
    let mut pruned = 0usize;

    for (session_id, _) in sorted.iter().take(to_remove) {
        // Delete the JSONL file.
        let path = conversations_dir.join(format!("{}.jsonl", session_id));
        match std::fs::remove_file(&path) {
            Ok(()) => {
                index.sessions.remove(session_id);
                pruned += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // File already gone — still remove from index.
                index.sessions.remove(session_id);
                pruned += 1;
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    path = %path.display(),
                    error = %e,
                    "Failed to delete JSONL file during session pruning"
                );
            }
        }
    }

    if pruned > 0 {
        if let Err(e) = write_index_atomic(conversations_dir, &index) {
            tracing::warn!(
                pruned,
                error = %e,
                "Failed to write pruned session index"
            );
        } else {
            tracing::info!(
                pruned,
                remaining = index.sessions.len(),
                "Pruned excess sessions"
            );
        }
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

/// Get the byte offset where message data begins (after metadata line).
fn metadata_end_offset(file: &mut std::fs::File) -> std::io::Result<u64> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file.try_clone()?);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;
    // read_line includes all bytes through the newline in the returned string;
    // first_line.len() is therefore the exact byte count of the first line,
    // which equals the file offset where the second line begins.
    Ok(first_line.len() as u64)
}

/// Find the latest session in the conversations directory.
///
/// Scans for `*.jsonl` files, sorts by filename descending (timestamp
/// prefix guarantees chronological order), and returns the session ID
/// without the `.jsonl` extension.
/// Find the most recently active session.
///
/// Reads `conversations/index.json` and returns the session with the
/// highest `last_active_at` value.  Falls back to a directory scan +
/// index rebuild if `index.json` is missing or corrupted.
pub fn find_latest_session(conversations_dir: &Path) -> Option<String> {
    let (index, _rebuilt) = ensure_index(conversations_dir);
    index
        .sessions
        .iter()
        .max_by(|(_, a), (_, b)| a.last_active_at.cmp(&b.last_active_at))
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
        let (index, rebuilt) = ensure_index(&conversations_dir);
        if rebuilt {
            tracing::info!("Session index was rebuilt from disk during scan");
        }

        let mut entries: Vec<(String, SessionIndexEntry)> = index.sessions.into_iter().collect();

        // Sort descending by last_active_at (newest first).
        entries.sort_by(|(_, a), (_, b)| b.last_active_at.cmp(&a.last_active_at));

        let total = entries.len();
        let page = page.unwrap_or(1).max(1) as usize;
        let size = size.unwrap_or(20).max(1) as usize;
        let start = (page - 1) * size;
        let end = (start + size).min(total);

        let sessions = entries[start..end]
            .iter()
            .map(|(sid, e)| SessionInfo {
                session_id: sid.clone(),
                created_at: e.created_at.clone(),
                message_count: e.message_count as u32,
                title: e.title.clone(),
                corrupted: e.corrupted,
                model: e.model.clone(),
                provider: e.provider.clone(),
                workspace_id: e.workspace_id.clone(),
            })
            .collect();

        (sessions, total)
    })
}

/// Read session metadata from the first line of a JSONL file.
///
/// If the first line is corrupted (invalid JSON), attempts recovery by
/// inferring `session_id` from the filename and filling remaining fields
/// with safe defaults. The returned `SessionMetadata` will have
/// `corrupted: true` to signal degraded data.
pub fn read_session_metadata(path: &Path) -> Result<SessionMetadata> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;

    match serde_json::from_str::<SessionMetadata>(first_line.trim()) {
        Ok(meta) => Ok(meta),
        Err(e) => {
            tracing::warn!(
                "Corrupted session metadata in {}: {}. Attempting recovery from filename.",
                path.display(),
                e
            );
            // Recover session_id from filename (e.g. "session_abc123.jsonl" -> "session_abc123")
            let filename = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            Ok(SessionMetadata {
                version: CONVERSATION_FORMAT_VERSION,
                session_id: filename.to_string(),
                created_at: String::new(),
                agent_id: String::new(),
                title: Some("(corrupted session)".to_string()),
                updated_at: None,
                message_count: None,
                corrupted: true,
                workspace_id: None,
                model: None,
                provider: None,
                reasoning_effort: None,
                temperature: None,
                last_input_tokens: None,
                last_output_tokens: None,
                last_compaction_offset: None,
            })
        }
    }
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
    let meta_end = metadata_end_offset(&mut file)?;

    if file_len <= meta_end {
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
/// - `messages`: new complete lines from JSONL after `line_number`
/// - `streaming`: delta of the in-progress streaming line (if any)
/// - `total_lines`: current total line count in the JSONL file
///
/// # Arguments
/// - `path`: Path to the session JSONL file.
/// - `line_number`: Number of complete lines already read by the frontend
///   (0-based; 0 = metadata). The function returns lines with index > line_number.
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

    // Read new complete lines from JSONL (lines with index > line_number)
    let mut messages: Vec<ConversationEntry> = Vec::new();
    if line_number < total_lines
        && let Ok(file) = std::fs::File::open(path)
    {
        let reader = BufReader::new(file);
        for (idx, line) in reader.lines().enumerate() {
            // Line 0 is metadata, skip it
            if idx == 0 {
                continue;
            }
            // Only include lines after the frontend's last known line
            if idx <= line_number {
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
        assert_eq!(lines.len(), 4, "Should have 4 lines: metadata + 3 messages");

        // First line is metadata
        let meta: SessionMetadata = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(meta.version, 2);
        assert_eq!(meta.session_id, session_id);
        assert_eq!(meta.agent_id, agent_id);

        // Second line is user message
        let entry: ConversationEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(entry.role, "user");
        assert_eq!(entry.content, "Hello");
        assert!(entry.metadata.is_none());

        // Third line is assistant message
        let entry: ConversationEntry = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(entry.role, "assistant");
        assert_eq!(entry.content, "Hi there!");
        assert_eq!(
            entry.metadata,
            Some(serde_json::json!({"model": "test-model"}))
        );

        // Fourth line is tool_call
        let entry: ConversationEntry = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(entry.role, "tool_call");
        assert_eq!(entry.content, r#"{"path": "test.txt"}"#);
    }

    #[test]
    fn test_find_latest_session() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        // Create a few session files with different names and timestamps.
        // `find_latest_session` now reads from index.json (auto-rebuilt if
        // missing) and sorts by `last_active_at` (= updated_at from JSONL
        // metadata during rebuild).  Give each session a distinct
        // `updated_at` so ordering is deterministic regardless of rebuild
        // sort stability.
        let base = chrono::Utc::now();
        let ids = vec![
            (
                "20260503_100000_aaaaaa",
                (base - chrono::Duration::hours(3)).to_rfc3339(),
            ),
            (
                "20260503_120000_bbbbbb",
                base.to_rfc3339(), // newest
            ),
            (
                "20260503_110000_cccccc",
                (base - chrono::Duration::hours(1)).to_rfc3339(),
            ),
        ];
        for (id, updated) in &ids {
            let path = conv_dir.join(format!("{}.jsonl", id));
            let meta = SessionMetadata {
                version: CONVERSATION_FORMAT_VERSION,
                session_id: id.to_string(),
                created_at: updated.clone(),
                agent_id: "com.test".to_string(),
                title: None,
                updated_at: Some(updated.clone()),
                message_count: Some(0),
                corrupted: false,
                workspace_id: None,
                model: None,
                provider: None,
                reasoning_effort: None,
                temperature: None,
                last_input_tokens: None,
                last_output_tokens: None,
                last_compaction_offset: None,
            };
            let mut file = std::fs::File::create(&path).unwrap();
            serde_json::to_writer(&mut file, &meta).unwrap();
            writeln!(file).unwrap();
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

        // Write metadata + 5 messages
        {
            let mut file = std::fs::File::create(&file_path).unwrap();
            let meta = SessionMetadata {
                version: 1,
                session_id: session_id.to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
                agent_id: "com.test".to_string(),
                title: None,
                updated_at: None,
                message_count: Some(5),
                corrupted: false,
                workspace_id: None,
                model: None,
                provider: None,
                reasoning_effort: None,
                temperature: None,
                last_input_tokens: None,
                last_output_tokens: None,
                last_compaction_offset: None,
            };
            serde_json::to_writer(&mut file, &meta).unwrap();
            writeln!(file).unwrap();

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

        // Verify file has both messages
        let file_path = work_dir
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let content = std::fs::read_to_string(&file_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "Should have metadata + 2 messages");

        let entry1: ConversationEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(entry1.content, "First message");

        let entry2: ConversationEntry = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(entry2.content, "Resumed response");
    }

    #[test]
    fn test_read_session_metadata_corrupted_recovery() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        let session_id = "20260503_100000_corrupt1";
        let file_path = conv_dir.join(format!("{}.jsonl", session_id));

        // Write a file with corrupted first line (not valid JSON)
        {
            let mut file = std::fs::File::create(&file_path).unwrap();
            writeln!(file, "THIS IS NOT VALID JSON!!!").unwrap();
            // Write valid message entries after corrupted header
            let entry = ConversationEntry {
                id: "msg-1".to_string(),
                ts: chrono::Utc::now().to_rfc3339(),
                role: "user".to_string(),
                content: "Hello".to_string(),
                metadata: None,
                kind: None,
            };
            serde_json::to_writer(&mut file, &entry).unwrap();
            writeln!(file).unwrap();
        }

        // read_session_metadata should return degraded metadata instead of Err
        let meta = read_session_metadata(&file_path).unwrap();
        assert!(
            meta.corrupted,
            "corrupted flag should be true for degraded metadata"
        );
        assert_eq!(
            meta.session_id, session_id,
            "session_id should be recovered from filename"
        );
        assert_eq!(meta.title, Some("(corrupted session)".to_string()));
        assert!(meta.created_at.is_empty());
        assert!(meta.agent_id.is_empty());

        // read_messages_paginated should still work, skipping the corrupted header
        let page = read_messages_paginated(&file_path, None, 10, "backward").unwrap();
        assert_eq!(
            page.messages.len(),
            1,
            "Should recover the valid message entry"
        );
        assert_eq!(page.messages[0].content, "Hello");
    }

    #[test]
    fn test_read_session_metadata_valid_not_corrupted() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        let session_id = "20260503_100000_valid01";
        let file_path = conv_dir.join(format!("{}.jsonl", session_id));

        // Write a valid metadata header
        let meta = SessionMetadata {
            version: 1,
            session_id: session_id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            agent_id: "com.test".to_string(),
            title: Some("Valid session".to_string()),
            updated_at: None,
            message_count: Some(3),
            corrupted: false,
            workspace_id: None,
            model: None,
            provider: None,
            reasoning_effort: None,
            temperature: None,
            last_input_tokens: None,
            last_output_tokens: None,
            last_compaction_offset: None,
        };
        let mut file = std::fs::File::create(&file_path).unwrap();
        serde_json::to_writer(&mut file, &meta).unwrap();
        writeln!(file).unwrap();

        let read_meta = read_session_metadata(&file_path).unwrap();
        assert!(
            !read_meta.corrupted,
            "valid metadata should not be marked as corrupted"
        );
        assert_eq!(read_meta.session_id, session_id);
        assert_eq!(read_meta.title, Some("Valid session".to_string()));
    }

    #[test]
    fn test_scan_sessions_includes_corrupted() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        // Create a valid session
        let valid_id = "20260503_100000_valid";
        let valid_path = conv_dir.join(format!("{}.jsonl", valid_id));
        let valid_meta = SessionMetadata {
            version: 1,
            session_id: valid_id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            agent_id: "com.test".to_string(),
            title: Some("Valid".to_string()),
            updated_at: None,
            message_count: Some(0),
            corrupted: false,
            workspace_id: None,
            model: None,
            provider: None,
            reasoning_effort: None,
            temperature: None,
            last_input_tokens: None,
            last_output_tokens: None,
            last_compaction_offset: None,
        };
        let mut file = std::fs::File::create(&valid_path).unwrap();
        serde_json::to_writer(&mut file, &valid_meta).unwrap();
        writeln!(file).unwrap();

        // Create a corrupted session
        let corrupt_id = "20260503_110000_corrupt";
        let corrupt_path = conv_dir.join(format!("{}.jsonl", corrupt_id));
        let mut file = std::fs::File::create(&corrupt_path).unwrap();
        writeln!(file, "BROKEN METADATA LINE").unwrap();

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
        assert_eq!(
            corrupt_session.title,
            Some("(corrupted session)".to_string())
        );
    }

    #[test]
    fn test_session_metadata_serde_backward_compatible() {
        // Ensure old JSON without "corrupted" field deserializes with corrupted=false
        let old_json = r#"{"version":1,"session_id":"test","created_at":"2026-01-01T00:00:00Z","agent_id":"com.test","title":null,"updated_at":null,"message_count":0}"#;
        let meta: SessionMetadata = serde_json::from_str(old_json).unwrap();
        assert!(
            !meta.corrupted,
            "Missing 'corrupted' field should default to false"
        );
        assert_eq!(meta.session_id, "test");
    }

    #[test]
    fn test_session_metadata_last_tokens_roundtrip() {
        // Full round-trip: serialize with last tokens, deserialize, verify
        let meta = SessionMetadata {
            version: 2,
            session_id: "roundtrip_test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            agent_id: "com.test".to_string(),
            title: Some("Test session".to_string()),
            updated_at: None,
            message_count: Some(5),
            corrupted: false,
            workspace_id: None,
            model: Some("gpt-4".to_string()),
            provider: Some("openai".to_string()),
            reasoning_effort: None,
            temperature: Some(0.7),
            last_input_tokens: Some(45_000),
            last_output_tokens: Some(1_200),
            last_compaction_offset: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SessionMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.last_input_tokens, Some(45_000));
        assert_eq!(parsed.last_output_tokens, Some(1_200));
        assert_eq!(parsed.last_compaction_offset, None, "field should default to None");
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.model.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn test_session_metadata_last_tokens_missing_defaults_to_none() {
        // Old JSON without last_input_tokens / last_output_tokens.
        // These fields have serde(default), so they must deserialize to None.
        let old_json = r#"{"version":1,"session_id":"old","created_at":"2026-01-01T00:00:00Z","agent_id":"com.test","title":null,"updated_at":null,"message_count":0}"#;
        let meta: SessionMetadata = serde_json::from_str(old_json).unwrap();
        assert_eq!(meta.last_input_tokens, None, "should default to None");
        assert_eq!(meta.last_output_tokens, None, "should default to None");
    }

    // ── display group pagination tests ────────────────────────────

    /// Helper: write a JSONL file with metadata + given entries, return the path.
    fn write_test_jsonl(dir: &TempDir, session_id: &str, entries: &[ConversationEntry]) -> PathBuf {
        let conv_dir = dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();
        let file_path = conv_dir.join(format!("{}.jsonl", session_id));
        let mut file = std::fs::File::create(&file_path).unwrap();
        let meta = SessionMetadata {
            version: 2,
            session_id: session_id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            agent_id: "com.test".to_string(),
            title: None,
            updated_at: None,
            message_count: Some(entries.len() as u32),
            corrupted: false,
            workspace_id: None,
            model: None,
            provider: None,
            reasoning_effort: None,
            temperature: None,
            last_input_tokens: None,
            last_output_tokens: None,
            last_compaction_offset: None,
        };
        serde_json::to_writer(&mut file, &meta).unwrap();
        writeln!(file).unwrap();
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
        // JSONL now has: line 0 = metadata, line 1 = user. total_lines = 2.
        // Streaming line will become line 2. Frontend already saw line 2 chars 0..3.
        let map = make_streaming_map("sess-same-line", 2, "assistant", "hello world");

        // Frontend line_number=2 (caught up), offset=3 → expect "lo world".
        let result = read_messages_since(&path, 2, 3, &map, "sess-same-line", 0).unwrap();
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 2);
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
        // Frontend last saw up to line 2 (an assistant line just flushed).
        let entries = vec![
            make_entry("1", "user", "hi"),
            make_entry("2", "assistant", "previous assistant text"),
        ];
        let path = write_test_jsonl(&dir, "sess-new-line", &entries);
        // New streaming line is a thought that will become line 3.
        let map = make_streaming_map("sess-new-line", 3, "thought", "reasoning...");

        // Frontend sends line_number=2 (has NOT yet seen line 3) with a stale
        // offset of 10 that belonged to the old assistant line. The new thought
        // line must be returned in full, not skipped by 10 chars.
        let result = read_messages_since(&path, 2, 10, &map, "sess-new-line", 0).unwrap();
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 3);
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
        // New streaming line is short (only 5 chars).
        let map = make_streaming_map("sess-stale-exceeds", 3, "assistant", "Hello");

        // Frontend sends line_number=3 (matches streaming line) with a stale
        // offset of 100 that belonged to the previous (longer) assistant line.
        // The new line only has 5 chars, so offset 100 is clearly stale.
        let result =
            read_messages_since(&path, 3, 100, &map, "sess-stale-exceeds", 0).unwrap();
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 3);
        assert_eq!(
            streaming.content, "Hello",
            "stale offset exceeding current content length must return full content"
        );
        assert_eq!(streaming.char_offset, "Hello".chars().count());
    }
}
