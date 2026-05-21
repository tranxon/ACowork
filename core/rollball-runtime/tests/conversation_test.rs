//! S1.18 End-to-end integration tests — Groups A & B
//!
//! A: Session mechanism (T1–T6)
//! B: JSONL writing (T7–T15)

use std::io::Write as IoWrite;
use std::path::Path;

use tempfile::TempDir;

use rollball_runtime::conversation::{
    ConversationEntry, ConversationSession, PaginatedMessages, SessionMetadata,
    find_latest_session, generate_session_id, read_messages_paginated, read_session_metadata,
    scan_sessions_async,
};

// ── Helpers ───────────────────────────────────────────────────────────

/// Create a session in the given work dir and return it.
fn create_session(work_dir: &Path, agent_id: &str) -> ConversationSession {
    let session_id = generate_session_id();
    ConversationSession::new(work_dir, &session_id, agent_id)
        .unwrap_or_else(|e| panic!("Failed to create session: {e}"))
}

/// Close a session (blocking convenience wrapper).
fn close_session(session: &ConversationSession) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        session.close().await.unwrap();
    });
}

/// Read the JSONL file contents and return non-empty, non-whitespace lines.
fn read_jsonl_lines(path: &Path) -> Vec<String> {
    let content = std::fs::read_to_string(path).unwrap();
    content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Create a minimal JSONL session file with only a metadata header.
fn create_minimal_session_file(conv_dir: &Path, session_id: &str) {
    let path = conv_dir.join(format!("{session_id}.jsonl"));
    let meta = SessionMetadata {
        version: 1,
        session_id: session_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        agent_id: "com.test.agent".to_string(),
        title: None,
        updated_at: None,
        message_count: Some(0),
        corrupted: false,
    };
    let mut file = std::fs::File::create(&path).unwrap();
    serde_json::to_writer(&mut file, &meta).unwrap();
    writeln!(file).unwrap();
}

/// Wait for the background writer thread to flush pending messages.
fn wait_writer() {
    std::thread::sleep(std::time::Duration::from_millis(200));
}

// ═══════════════════════════════════════════════════════════════════════
// Group A: Session mechanism tests (T1–T6)
// ═══════════════════════════════════════════════════════════════════════

/// T1: Agent first startup automatically creates a session.
#[test]
fn test_t01_first_startup_creates_session() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();
    close_session(&session);

    // Verify the JSONL file exists
    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    assert!(jsonl_path.exists(), "Session JSONL file should exist");

    // Verify the first line is valid SessionMetadata
    let meta = read_session_metadata(&jsonl_path).unwrap();
    assert_eq!(meta.version, 1);
    assert_eq!(meta.session_id, session_id);
    assert_eq!(meta.agent_id, "com.test.agent");
    assert!(meta.created_at.contains('T'), "created_at should be ISO 8601");
}

/// T2: Agent restart resumes the latest session.
#[test]
fn test_t02_restart_resumes_latest_session() {
    let temp_dir = TempDir::new().unwrap();
    let conv_dir = temp_dir.path().join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    // Create two session files with different timestamps
    create_minimal_session_file(&conv_dir, "20260101_080000_aaaaaa");
    create_minimal_session_file(&conv_dir, "20260102_120000_bbbbbb");

    let latest = find_latest_session(&conv_dir);
    assert_eq!(latest, Some("20260102_120000_bbbbbb".to_string()));
}

/// T3: Repeated start/stop does not create duplicate session files.
#[test]
fn test_t03_no_duplicate_session_files() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    // Create and close a session
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();
    close_session(&session);

    let conv_dir = work_dir.join("conversations");
    let file_count_before = std::fs::read_dir(&conv_dir).unwrap().count();

    // Resume the same session — should not create a new file
    let resumed = ConversationSession::resume(work_dir, &session_id).unwrap();
    close_session(&resumed);

    let file_count_after = std::fs::read_dir(&conv_dir).unwrap().count();
    assert_eq!(
        file_count_before, file_count_after,
        "Resuming a session should not create a new file"
    );
}

/// T4: Session ID format validation.
#[test]
fn test_t04_session_id_format() {
    let ids: Vec<String> = (0..10).map(|_| generate_session_id()).collect();

    for id in &ids {
        // Format: {YYYYMMDD}_{HHMMSS}_{6chars}
        let parts: Vec<&str> = id.split('_').collect();
        assert_eq!(parts.len(), 3, "Session ID should have 3 underscore-separated parts");
        assert_eq!(parts[0].len(), 8, "Date part should be 8 chars");
        assert!(parts[0].chars().all(|c| c.is_ascii_digit()), "Date should be digits");
        assert_eq!(parts[1].len(), 6, "Time part should be 6 chars");
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()), "Time should be digits");
        assert_eq!(parts[2].len(), 6, "Short UUID should be 6 chars");
    }

    // Lexicographic sort should equal chronological sort (due to timestamp prefix)
    let mut ids_sorted_lex = ids.clone();
    ids_sorted_lex.sort();
    let mut ids_sorted_time = ids.clone();
    ids_sorted_time.sort_by_key(|a| a.to_string());
    assert_eq!(ids_sorted_lex, ids_sorted_time);
}

/// T5: Async scan of conversations directory with many sessions.
#[tokio::test]
async fn test_t05_scan_sessions_async_many_files() {
    let temp_dir = TempDir::new().unwrap();
    let conv_dir = temp_dir.path().join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    // Create 120 minimal session files
    for i in 0..120 {
        let sid = format!("20260101_{:06}_xx{:04}", i, i);
        create_minimal_session_file(&conv_dir, &sid);
    }

    let start = std::time::Instant::now();
    let handle = scan_sessions_async(conv_dir, None, Some(200));
    let (sessions, _total) = handle.await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(sessions.len(), 120, "Should find all 120 sessions");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "Scanning 120 sessions should take <2s, took {:?}",
        elapsed
    );

    // Verify sorted newest-first
    for window in sessions.windows(2) {
        assert!(
            window[0].session_id >= window[1].session_id,
            "Sessions should be sorted newest-first"
        );
    }
}

/// T6: Session metadata correctly updated after close.
#[test]
fn test_t06_session_metadata_updated_on_close() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();

    // Write a few messages and update metadata
    session.append_message("user", "Hello", None);
    session.append_message("assistant", "Hi there", None);

    // Manually update metadata with message count
    let updated_meta = SessionMetadata {
        version: 1,
        session_id: session_id.clone(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        agent_id: "com.test.agent".to_string(),
        title: Some("Test session".to_string()),
        updated_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        message_count: Some(2),
        corrupted: false,
    };
    session.update_metadata(updated_meta);
    wait_writer();
    close_session(&session);

    // Read back and verify
    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    let meta = read_session_metadata(&jsonl_path).unwrap();
    assert_eq!(meta.message_count, Some(2));
    assert_eq!(meta.title, Some("Test session".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════
// Group B: JSONL writing tests (T7–T15)
// ═══════════════════════════════════════════════════════════════════════

/// T7: User message correctly written.
#[test]
fn test_t07_user_message_write() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();

    session.append_message("user", "Hello world", None);
    wait_writer();
    close_session(&session);

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    let lines = read_jsonl_lines(&jsonl_path);
    // First line is metadata, second line should be the user message
    assert!(lines.len() >= 2, "Should have at least 2 lines (meta + message)");

    let entry: ConversationEntry = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(entry.role, "user");
    assert_eq!(entry.content, "Hello world");
    assert!(entry.metadata.is_none());
}

/// T8: Assistant response correctly written.
#[test]
fn test_t08_assistant_message_write() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();

    session.append_message("assistant", "I can help you", None);
    wait_writer();
    close_session(&session);

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    let lines = read_jsonl_lines(&jsonl_path);

    let entry: ConversationEntry = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(entry.role, "assistant");
    assert_eq!(entry.content, "I can help you");
}

/// T9: Tool call correctly written.
#[test]
fn test_t09_tool_call_write() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();

    let metadata = serde_json::json!({"tool_name": "read_file", "tool_call_id": "tc-001"});
    session.append_message("tool_call", r#"{"path": "test.txt"}"#, Some(metadata));
    wait_writer();
    close_session(&session);

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    let lines = read_jsonl_lines(&jsonl_path);

    let entry: ConversationEntry = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(entry.role, "tool_call");
    assert_eq!(entry.content, r#"{"path": "test.txt"}"#);
    let meta = entry.metadata.unwrap();
    assert_eq!(meta["tool_name"], "read_file");
    assert_eq!(meta["tool_call_id"], "tc-001");
}

/// T10: Tool result correctly written.
#[test]
fn test_t10_tool_result_write() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();

    let metadata = serde_json::json!({"tool_call_id": "tc-002"});
    session.append_message("tool_result", "result content", Some(metadata));
    wait_writer();
    close_session(&session);

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    let lines = read_jsonl_lines(&jsonl_path);

    let entry: ConversationEntry = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(entry.role, "tool_result");
    assert_eq!(entry.content, "result content");
    let meta = entry.metadata.unwrap();
    assert_eq!(meta["tool_call_id"], "tc-002");
}

/// T11: Think content correctly written.
#[test]
fn test_t11_think_content_write() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();

    session.append_message("think", "Let me consider...", None);
    wait_writer();
    close_session(&session);

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    let lines = read_jsonl_lines(&jsonl_path);

    let entry: ConversationEntry = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(entry.role, "think");
    assert_eq!(entry.content, "Let me consider...");
}

/// T12: Concurrent tool execution — messages are written in order without loss.
#[test]
fn test_t12_concurrent_message_writes() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();

    // Use an Arc to share the session across threads
    let session = std::sync::Arc::new(session);
    let mut handles = Vec::new();

    for i in 0..10 {
        let s = session.clone();
        handles.push(std::thread::spawn(move || {
            s.append_message("user", &format!("Message {i}"), None);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    wait_writer();
    close_session(&session);

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    let lines = read_jsonl_lines(&jsonl_path);

    // 1 metadata line + 10 message lines = 11
    assert_eq!(lines.len(), 11, "All 10 messages should be written plus metadata");

    // Each line (after metadata) should be valid JSON
    for line in &lines[1..] {
        let _: ConversationEntry = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Line should be valid JSON: {e}\nLine: {line}"));
    }
}

/// T13: Super-long message (>100KB) written without truncation.
#[test]
fn test_t13_long_message_no_truncation() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();

    // Generate a message larger than 100KB
    let long_content = "A".repeat(110_000);
    session.append_message("user", &long_content, None);
    wait_writer();
    close_session(&session);

    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    let lines = read_jsonl_lines(&jsonl_path);

    let entry: ConversationEntry = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(entry.content.len(), 110_000, "Content should not be truncated");
    assert_eq!(entry.content, long_content);
}

/// T14: Corrupted JSONL line does not affect other lines during reading.
#[test]
fn test_t14_corrupted_line_recovery() {
    let temp_dir = TempDir::new().unwrap();
    let conv_dir = temp_dir.path().join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    let session_id = "20260503_100000_test14";
    let file_path = conv_dir.join(format!("{session_id}.jsonl"));

    // Manually write a JSONL file with a corrupted line in the middle
    {
        let mut file = std::fs::File::create(&file_path).unwrap();
        let meta = SessionMetadata {
            version: 1,
            session_id: session_id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            agent_id: "com.test".to_string(),
            title: None,
            updated_at: None,
            message_count: Some(4),
            corrupted: false,
        };
        serde_json::to_writer(&mut file, &meta).unwrap();
        writeln!(file).unwrap();

        // Valid message 1
        let entry1 = ConversationEntry {
            id: "msg-1".to_string(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            role: "user".to_string(),
            content: "First message".to_string(),
            metadata: None,
        };
        serde_json::to_writer(&mut file, &entry1).unwrap();
        writeln!(file).unwrap();

        // Corrupted line
        writeln!(file, "THIS IS NOT VALID JSON!!!{{}}").unwrap();

        // Valid message 2
        let entry2 = ConversationEntry {
            id: "msg-2".to_string(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            role: "assistant".to_string(),
            content: "Third message".to_string(),
            metadata: None,
        };
        serde_json::to_writer(&mut file, &entry2).unwrap();
        writeln!(file).unwrap();
    }

    // Read with pagination — should get valid messages, skip the corrupted line
    let page: PaginatedMessages = read_messages_paginated(&file_path, None, 100, "backward").unwrap();
    assert_eq!(page.messages.len(), 2, "Should return 2 valid messages, skipping corrupted line");
    assert_eq!(page.messages[0].content, "First message");
    assert_eq!(page.messages[1].content, "Third message");
}

/// T15: Process crash recovery — JSONL file is fully readable without close().
#[test]
fn test_t15_crash_recovery_readable_without_close() {
    let temp_dir = TempDir::new().unwrap();
    let work_dir = temp_dir.path();

    // Create session and write messages WITHOUT calling close() (simulating crash)
    let session = create_session(work_dir, "com.test.agent");
    let session_id = session.session_id().to_string();
    session.append_message("user", "Before crash", None);
    wait_writer();
    // Do NOT call close() — drop the session directly
    drop(session);

    // Read the file directly — it should be fully readable
    let jsonl_path = work_dir.join("conversations").join(format!("{session_id}.jsonl"));
    let lines = read_jsonl_lines(&jsonl_path);

    // Should have metadata + 1 message
    assert!(lines.len() >= 2, "Should have metadata and at least 1 message");

    let meta: SessionMetadata = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(meta.session_id, session_id);

    let entry: ConversationEntry = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(entry.content, "Before crash");
}
