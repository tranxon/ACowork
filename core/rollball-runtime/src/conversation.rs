//! Session lifecycle management and JSONL conversation file writing.
//!
//! Provides `ConversationSession` for managing a single session's JSONL file
//! and `ConversationWriter` for channel-based single-writer thread architecture.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::error::Result;

/// Format version for the JSONL conversation file.
const CONVERSATION_FORMAT_VERSION: u32 = 1;

/// A single line in the conversation JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntry {
    /// Unique message ID (UUID v4)
    pub id: String,
    /// ISO 8601 timestamp with millisecond precision
    pub ts: String,
    /// Message role: "user" | "assistant" | "thought" | "tool_call" | "tool_result" | "system"
    pub role: String,
    /// Full message content
    pub content: String,
    /// Optional metadata (e.g. tool_call_id, tool_name)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
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
}

impl ConversationWriter {
    /// Create a new writer.
    fn new(file: std::fs::File, path: PathBuf, receiver: mpsc::UnboundedReceiver<WriterCommand>) -> Self {
        Self { file, path, receiver }
    }

    /// Run the writer loop. Blocks until Shutdown is received.
    fn run(mut self) {
        while let Some(cmd) = self.receiver.blocking_recv() {
            match cmd {
                WriterCommand::AppendEntry(entry) => {
                    if let Err(e) = self.write_entry(&entry) {
                        tracing::error!("Failed to write conversation entry: {}", e);
                    }
                }
                WriterCommand::UpdateMetadata(meta) => {
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
    fn write_entry(&mut self, entry: &ConversationEntry) -> std::io::Result<()> {
        // Always seek to end for append; handles resume where file position may be at 0
        self.file.seek(std::io::SeekFrom::End(0))?;
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

        Ok(())
    }
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
    sender: mpsc::UnboundedSender<WriterCommand>,
    /// Path to the JSONL file (for session-level distillation on close).
    session_file_path: PathBuf,
}

impl ConversationSession {
    /// Create a new session.
    ///
    /// Creates `{work_dir}/conversations/{session_id}.jsonl`, writes the
    /// `SessionMetadata` header, and starts the background writer thread.
    pub fn new(work_dir: &Path, session_id: &str, agent_id: &str) -> Result<Self> {
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
            agent_id: agent_id.to_string(),
            title: None,
            updated_at: Some(now),
            message_count: Some(0),
            corrupted: false,
        };

        // Write metadata as the first line — build complete line then single write
        let mut line = serde_json::to_string(&metadata)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.sync_data()?;

        let (tx, rx) = mpsc::unbounded_channel::<WriterCommand>();
        let writer = ConversationWriter::new(file, file_path.clone(), rx);
        std::thread::spawn(move || writer.run());

        Ok(Self {
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: now_for_self,
            title_set: AtomicBool::new(false),
            current_title: std::sync::Mutex::new(None),
            sender: tx,
            session_file_path: file_path,
        })
    }

    /// Resume an existing session.
    ///
    /// Opens the existing JSONL file in append mode and starts the
    /// background writer thread.
    pub fn resume(work_dir: &Path, session_id: &str) -> Result<Self> {
        let conversations_dir = work_dir.join("conversations");
        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file_path)?;

        // Read existing metadata to get agent_id
        let meta = read_session_metadata(&file_path)?;

        let (tx, rx) = mpsc::unbounded_channel::<WriterCommand>();
        let writer = ConversationWriter::new(file, file_path.clone(), rx);
        std::thread::spawn(move || writer.run());

        Ok(Self {
            session_id: session_id.to_string(),
            agent_id: meta.agent_id,
            created_at: meta.created_at,
            title_set: AtomicBool::new(meta.title.is_some()),
            current_title: std::sync::Mutex::new(meta.title.clone()),
            sender: tx,
            session_file_path: file_path,
        })
    }

    /// Append a message to the conversation.
    ///
    /// This is non-blocking: the message is sent via channel to the
    /// background writer thread.
    pub fn append_message(
        &self,
        role: &str,
        content: &str,
        metadata: Option<serde_json::Value>,
    ) {
        let entry = ConversationEntry {
            id: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            role: role.to_string(),
            content: content.to_string(),
            metadata,
        };
        if let Err(e) = self.sender.send(WriterCommand::AppendEntry(entry)) {
            tracing::error!("Failed to send message to conversation writer: {}", e);
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
            tracing::error!("Failed to send metadata update to conversation writer: {}", e);
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
            updated_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            message_count: None,
            corrupted: false,
        };
        self.update_metadata(metadata);
        // Track current title for dedup
        if let Ok(mut current) = self.current_title.lock() {
            *current = Some(title);
        }
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
            updated_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            message_count: None,
            corrupted: false,
        };
        self.update_metadata(metadata);
        // Track current title for dedup
        if let Ok(mut current) = self.current_title.lock() {
            *current = Some(truncated.clone());
        }
        tracing::info!(session_id = %self.session_id, title = %truncated, "Session title force-updated via API");
        true
    }
}

// Safety: ConversationSession only contains String and UnboundedSender,
// both of which are Send + Sync.
unsafe impl Send for ConversationSession {}
unsafe impl Sync for ConversationSession {}

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

/// Chunk size for backward reading (8 KB).
const BACKWARD_READ_CHUNK: usize = 8 * 1024;

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
    cursor.strip_prefix("offset:").and_then(|s| s.parse::<u64>().ok())
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
pub fn find_latest_session(conversations_dir: &Path) -> Option<String> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(conversations_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        })
        .collect();

    if entries.is_empty() {
        return None;
    }

    // Sort descending by filename (timestamp prefix => newest first)
    entries.sort_by(|a, b| {
        b.file_name()
            .to_string_lossy()
            .cmp(&a.file_name().to_string_lossy())
    });

    entries.first().and_then(|e| {
        e.path()
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
    })
}

/// Asynchronously scan all sessions in the conversations directory.
///
/// Reads the first line of each `.jsonl` file to extract `SessionMetadata`
/// and builds a `Vec<SessionInfo>`. Results are sorted newest-first.
pub fn scan_sessions_async(
    conversations_dir: PathBuf,
    page: Option<u32>,
    size: Option<u32>,
) -> tokio::task::JoinHandle<(Vec<SessionInfo>, usize)> {
    tokio::task::spawn_blocking(move || {
        let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&conversations_dir) {
            Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
            Err(_) => return (Vec::new(), 0),
        };

        // Sort descending by filename
        entries.sort_by(|a, b| {
            b.file_name()
                .to_string_lossy()
                .cmp(&a.file_name().to_string_lossy())
        });

        let total = entries.len();
        let page = page.unwrap_or(1).max(1) as usize;
        let size = size.unwrap_or(20).max(1) as usize;
        let start = (page - 1) * size;
        let end = start + size;

        let entries_page = entries.into_iter().skip(start).take(end);

        let mut sessions = Vec::new();
        for entry in entries_page {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                && let Ok(meta) = read_session_metadata(&path)
            {
                sessions.push(SessionInfo {
                    session_id: meta.session_id,
                    created_at: meta.created_at,
                    message_count: meta.message_count.unwrap_or(0),
                    title: meta.title,
                    corrupted: meta.corrupted,
                });
            }
        }
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

    if direction == "forward" {
        read_messages_forward(&mut file, cursor, limit, meta_end, file_len)
    } else {
        read_messages_backward(&mut file, cursor, limit, meta_end, file_len)
    }
}

/// Backward pagination: read the most recent `limit` messages, or older
/// messages before the cursor offset.
fn read_messages_backward(
    file: &mut std::fs::File,
    cursor: Option<String>,
    limit: u32,
    meta_end: u64,
    file_len: u64,
) -> Result<PaginatedMessages> {
    let end_offset = cursor
        .as_deref()
        .and_then(parse_offset_cursor)
        .unwrap_or(file_len);

    let line_offsets = read_lines_backward(file, end_offset, limit as usize)?;

    // Parse lines into ConversationEntry, skipping invalid JSON
    let mut messages = Vec::new();
    let mut first_valid_offset = None;
    for lo in &line_offsets {
        match serde_json::from_str::<ConversationEntry>(&lo.content) {
            Ok(entry) => {
                if first_valid_offset.is_none() {
                    first_valid_offset = Some(lo.offset);
                }
                messages.push(entry);
            }
            Err(e) => {
                tracing::warn!("Skipping invalid JSONL line: {}", e);
            }
        }
    }

    let first_offset = first_valid_offset.unwrap_or(meta_end);

    // has_more: the first message's offset is still after metadata,
    // meaning there are older messages beyond this page
    let has_more = first_offset > meta_end;

    // next_cursor: byte offset of the first (oldest) message in this page
    let next_cursor = if has_more {
        Some(format!("offset:{}", first_offset))
    } else {
        None
    };

    Ok(PaginatedMessages {
        messages,
        cursor: next_cursor,
        has_more,
    })
}

/// Forward pagination: read `limit` messages starting from cursor offset.
fn read_messages_forward(
    file: &mut std::fs::File,
    cursor: Option<String>,
    limit: u32,
    meta_end: u64,
    file_len: u64,
) -> Result<PaginatedMessages> {
    let start_offset = cursor
        .as_deref()
        .and_then(parse_offset_cursor)
        .unwrap_or(meta_end);

    let line_offsets = read_lines_forward(file, start_offset, limit as usize)?;

    // Parse lines into ConversationEntry
    let mut messages = Vec::new();
    let mut last_line_end = start_offset;
    for lo in &line_offsets {
        match serde_json::from_str::<ConversationEntry>(&lo.content) {
            Ok(entry) => {
                // End of this line = offset + content length + newline
                last_line_end = lo.offset + lo.content.len() as u64 + 1u64;
                messages.push(entry);
            }
            Err(e) => {
                tracing::warn!("Skipping invalid JSONL line: {}", e);
            }
        }
    }

    let has_more = last_line_end < file_len;

    // next_cursor: byte offset at the end of the last message
    let next_cursor = if has_more {
        Some(format!("offset:{}", last_line_end))
    } else {
        None
    };

    Ok(PaginatedMessages {
        messages,
        cursor: next_cursor,
        has_more,
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
        assert_eq!(parts.len(), 3, "Session ID should have 3 parts separated by underscores");
        assert_eq!(parts[0].len(), 8, "Date part should be 8 chars (YYYYMMDD)");
        assert_eq!(parts[1].len(), 6, "Time part should be 6 chars (HHMMSS)");
        assert_eq!(parts[2].len(), 6, "Short UUID should be 6 chars");
        assert!(parts[0].chars().all(|c| c.is_ascii_digit()), "Date should be digits");
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()), "Time should be digits");
    }

    #[test]
    fn test_conversation_writer_basic() {
        let temp_dir = TempDir::new().unwrap();
        let work_dir = temp_dir.path();
        let session_id = generate_session_id();
        let agent_id = "com.test.agent";

        // Create session and write messages
        let session = ConversationSession::new(work_dir, &session_id, agent_id).unwrap();
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
        let file_path = work_dir.join("conversations").join(format!("{}.jsonl", session_id));
        let content = std::fs::read_to_string(&file_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4, "Should have 4 lines: metadata + 3 messages");

        // First line is metadata
        let meta: SessionMetadata = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(meta.version, 1);
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
        assert_eq!(entry.metadata, Some(serde_json::json!({"model": "test-model"})));

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

        // Create a few session files with different names
        let ids = vec![
            "20260503_100000_aaaaaa",
            "20260503_120000_bbbbbb",
            "20260503_110000_cccccc",
        ];
        for id in &ids {
            let path = conv_dir.join(format!("{}.jsonl", id));
            let meta = SessionMetadata {
                version: 1,
                session_id: id.to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
                agent_id: "com.test".to_string(),
                title: None,
                updated_at: None,
                message_count: Some(0),
                corrupted: false,
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
        assert!(cursor.starts_with("offset:"), "Cursor should be offset format, got: {}", cursor);

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
        assert!(!page3.has_more, "No more messages after reaching the beginning");
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
        let session = ConversationSession::new(work_dir, session_id, agent_id).unwrap();
        session.append_message("user", "First message", None);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            session.close().await.unwrap();
        });

        // Resume session
        let resumed = ConversationSession::resume(work_dir, session_id).unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(resumed.agent_id(), agent_id);

        resumed.append_message("assistant", "Resumed response", None);
        std::thread::sleep(std::time::Duration::from_millis(50));

        rt.block_on(async {
            resumed.close().await.unwrap();
        });

        // Verify file has both messages
        let file_path = work_dir.join("conversations").join(format!("{}.jsonl", session_id));
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
            };
            serde_json::to_writer(&mut file, &entry).unwrap();
            writeln!(file).unwrap();
        }

        // read_session_metadata should return degraded metadata instead of Err
        let meta = read_session_metadata(&file_path).unwrap();
        assert!(meta.corrupted, "corrupted flag should be true for degraded metadata");
        assert_eq!(meta.session_id, session_id, "session_id should be recovered from filename");
        assert_eq!(meta.title, Some("(corrupted session)".to_string()));
        assert!(meta.created_at.is_empty());
        assert!(meta.agent_id.is_empty());

        // read_messages_paginated should still work, skipping the corrupted header
        let page = read_messages_paginated(&file_path, None, 10, "backward").unwrap();
        assert_eq!(page.messages.len(), 1, "Should recover the valid message entry");
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
        };
        let mut file = std::fs::File::create(&file_path).unwrap();
        serde_json::to_writer(&mut file, &meta).unwrap();
        writeln!(file).unwrap();

        let read_meta = read_session_metadata(&file_path).unwrap();
        assert!(!read_meta.corrupted, "valid metadata should not be marked as corrupted");
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
        let (sessions, total) = rt.block_on(async {
            scan_sessions_async(conv_dir, None, None).await.unwrap()
        });

        assert_eq!(sessions.len(), 2, "Should find both valid and corrupted sessions");

        let valid_session = sessions.iter().find(|s| s.session_id == valid_id).unwrap();
        assert!(!valid_session.corrupted);

        let corrupt_session = sessions.iter().find(|s| s.session_id == corrupt_id).unwrap();
        assert!(corrupt_session.corrupted);
        assert_eq!(corrupt_session.title, Some("(corrupted session)".to_string()));
    }

    #[test]
    fn test_session_metadata_serde_backward_compatible() {
        // Ensure old JSON without "corrupted" field deserializes with corrupted=false
        let old_json = r#"{"version":1,"session_id":"test","created_at":"2026-01-01T00:00:00Z","agent_id":"com.test","title":null,"updated_at":null,"message_count":0}"#;
        let meta: SessionMetadata = serde_json::from_str(old_json).unwrap();
        assert!(!meta.corrupted, "Missing 'corrupted' field should default to false");
        assert_eq!(meta.session_id, "test");
    }
}
