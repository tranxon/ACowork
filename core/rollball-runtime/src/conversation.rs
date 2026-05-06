//! Session lifecycle management and JSONL conversation file writing.
//!
//! Provides `ConversationSession` for managing a single session's JSONL file
//! and `ConversationWriter` for channel-based single-writer thread architecture.

use std::io::{BufRead, BufReader, Read, Seek, Write};
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
    /// Message role: "user" | "assistant" | "think" | "tool_call" | "tool_result" | "system"
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
    receiver: mpsc::UnboundedReceiver<WriterCommand>,
}

impl ConversationWriter {
    /// Create a new writer.
    fn new(file: std::fs::File, receiver: mpsc::UnboundedReceiver<WriterCommand>) -> Self {
        Self { file, receiver }
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
    fn write_entry(&mut self, entry: &ConversationEntry) -> std::io::Result<()> {
        // Always seek to end for append; handles resume where file position may be at 0
        self.file.seek(std::io::SeekFrom::End(0))?;
        let mut buf_writer = std::io::BufWriter::new(&self.file);
        serde_json::to_writer(&mut buf_writer, entry)?;
        writeln!(buf_writer)?;
        buf_writer.flush()?;
        Ok(())
    }

    /// Rewrite the first line with updated metadata.
    ///
    /// Reads all content after the first line, truncates the file to 0,
    /// writes the new metadata, then restores the remaining content.
    /// This is more reliable than seek-and-overwrite which leaves leftover
    /// bytes when the new line is shorter than the old one.
    fn rewrite_metadata(&mut self, meta: &SessionMetadata) -> std::io::Result<()> {
        // Read all existing content after the first line
        let mut reader = std::io::BufReader::new(&self.file);
        let mut first_line = String::new();
        reader.read_line(&mut first_line)?; // skip and discard the first line
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest)?;
        drop(reader);

        // Truncate the file to 0 and rewrite from the start
        self.file.set_len(0)?;
        self.file.seek(std::io::SeekFrom::Start(0))?;

        let new_line = serde_json::to_string(meta)?;
        writeln!(self.file, "{}", new_line)?;
        self.file.write_all(&rest)?;
        self.file.flush()?;

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
        let file = std::fs::OpenOptions::new()
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
        };

        // Write metadata as the first line
        let mut buf_writer = std::io::BufWriter::new(&file);
        serde_json::to_writer(&mut buf_writer, &metadata)?;
        writeln!(buf_writer)?;
        buf_writer.flush()?;
        drop(buf_writer);

        let (tx, rx) = mpsc::unbounded_channel::<WriterCommand>();
        let writer = ConversationWriter::new(file, rx);
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
        let writer = ConversationWriter::new(file, rx);
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
        if let Ok(current) = self.current_title.lock() {
            if current.as_deref() == Some(title) {
                return false;
            }
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
}

/// Paginated message result.
#[derive(Debug, Clone)]
pub struct PaginatedMessages {
    /// Messages in the current page
    pub messages: Vec<ConversationEntry>,
    /// Cursor for the next page (message ID)
    pub cursor: Option<String>,
    /// Whether more messages exist after this page
    pub has_more: bool,
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
) -> tokio::task::JoinHandle<Vec<SessionInfo>> {
    tokio::task::spawn_blocking(move || {
        let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&conversations_dir) {
            Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
            Err(_) => return Vec::new(),
        };

        // Sort descending by filename
        entries.sort_by(|a, b| {
            b.file_name()
                .to_string_lossy()
                .cmp(&a.file_name().to_string_lossy())
        });

        let mut sessions = Vec::new();
        for entry in entries {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                && let Ok(meta) = read_session_metadata(&path)
            {
                sessions.push(SessionInfo {
                    session_id: meta.session_id,
                    created_at: meta.created_at,
                    message_count: meta.message_count.unwrap_or(0),
                    title: meta.title,
                });
            }
        }
        sessions
    })
}

/// Read session metadata from the first line of a JSONL file.
pub fn read_session_metadata(path: &Path) -> Result<SessionMetadata> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;
    let meta: SessionMetadata = serde_json::from_str(first_line.trim())?;
    Ok(meta)
}

/// Read messages from a JSONL file with pagination.
///
/// - `cursor`: message ID of the last message from the previous page.
///   If `None`, starts from the most recent messages.
/// - `limit`: maximum number of messages to return.
/// - `direction`: "backward" (older) or "forward" (newer).
///
/// Returns messages in chronological order (oldest to newest within the page).
pub fn read_messages_paginated(
    path: &Path,
    cursor: Option<String>,
    limit: u32,
    direction: &str,
) -> Result<PaginatedMessages> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let mut all_messages: Vec<ConversationEntry> = Vec::new();
    let mut is_first_line = true;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Skip first line (session metadata)
        if is_first_line {
            is_first_line = false;
            continue;
        }

        match serde_json::from_str::<ConversationEntry>(line) {
            Ok(entry) => all_messages.push(entry),
            Err(e) => {
                tracing::warn!("Skipping invalid JSONL line in {}: {}", path.display(), e);
            }
        }
    }

    // Pagination logic
    let limit = limit as usize;
    let mut start_idx = all_messages.len();

    if let Some(cursor_id) = cursor
        && let Some(pos) = all_messages.iter().position(|m| m.id == cursor_id)
    {
        if direction == "forward" {
            start_idx = pos + 1;
        } else {
            // backward: read messages before cursor
            start_idx = pos;
        }
    }

    let page_messages: Vec<ConversationEntry>;
    let has_more: bool;
    let next_cursor: Option<String>;

    if direction == "forward" {
        let end_idx = (start_idx + limit).min(all_messages.len());
        page_messages = all_messages[start_idx..end_idx].to_vec();
        has_more = end_idx < all_messages.len();
        next_cursor = page_messages.last().map(|m| m.id.clone());
    } else {
        // backward (default): read most recent messages, or older messages before cursor
        let end_idx = start_idx;
        let actual_start = end_idx.saturating_sub(limit);
        page_messages = all_messages[actual_start..end_idx].to_vec();
        has_more = actual_start > 0;
        next_cursor = page_messages.first().map(|m| m.id.clone());
    }

    Ok(PaginatedMessages {
        messages: page_messages,
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
        std::thread::sleep(std::time::Duration::from_millis(200));

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

        // Continue backward from cursor
        let cursor = page.cursor.unwrap();
        let page2 = read_messages_paginated(&file_path, Some(cursor), 2, "backward").unwrap();
        assert_eq!(page2.messages.len(), 2);
        assert!(page2.has_more);
        assert_eq!(page2.messages[0].content, "Message 1");
        assert_eq!(page2.messages[1].content, "Message 2");

        // Read forward from msg-1
        let page3 = read_messages_paginated(&file_path, Some("msg-1".to_string()), 10, "forward").unwrap();
        assert_eq!(page3.messages.len(), 3);
        assert!(!page3.has_more);
        assert_eq!(page3.messages[0].content, "Message 2");
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
        std::thread::sleep(std::time::Duration::from_millis(100));

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            session.close().await.unwrap();
        });

        // Resume session
        let resumed = ConversationSession::resume(work_dir, session_id).unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(resumed.agent_id(), agent_id);

        resumed.append_message("assistant", "Resumed response", None);
        std::thread::sleep(std::time::Duration::from_millis(100));

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
}
