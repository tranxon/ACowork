//! Session resume: rebuild in-memory `HistoryManager` state from a JSONL file.
//!
//! Triggered on cold-start when an existing conversation is resumed
//! (see [`crate::startup::session_init::phase_b_init_session`]).
//!
//! ## Replay rules
//!
//! The JSONL is an append-only event log. Restoration walks it front-to-back
//! and translates each entry into protocol-level [`ChatMessage`]s that match
//! the in-memory state the previous session would have had at exit time.
//!
//! ### Filtering
//!
//! - `role="thought"` → dropped (frontend-only; never enters LLM context).
//! - `role="user" | "assistant" | "system"` → preserved as-is.
//! - `role="tool_call"` → merged onto the immediately preceding `Assistant`
//!   message as a `tool_calls` entry. If no preceding assistant exists, a new
//!   empty-content assistant is synthesized to host it.
//! - `role="tool_result"` → emitted as `MessageRole::Tool` with `tool_call_id`
//!   from metadata; orphaned results (no matching tool_call in the same
//!   contiguous block) are dropped.
//! - `kind="compaction"` → produces a `User{name="compaction_summary"}`
//!   marker. Only the **last** compaction event is honored: every entry
//!   strictly before the last compaction marker (except leading `system`
//!   messages) is discarded.
//!
//!   NOTE: The marker uses `User` role (not `Assistant`) in memory to
//!   avoid an `Assistant → Assistant{tool_calls}` adjacency in the
//!   rebuilt request, which glm-5.2 on Volcano Ark rejects with
//!   `400 InvalidParameter`.  Consumers identify the marker by
//!   `name == "compaction_summary"` regardless of role.
//!
//! ### Tool-call pairing
//!
//! After replay, any `Tool` message whose `tool_call_id` does not match a
//! preceding `Assistant.tool_calls[*].id` is dropped (defensive cleanup;
//! prevents provider-side sanitize errors).
//!
//! ## Failure handling
//!
//! - Corrupt JSONL line → skipped, counted as `skipped_entry_count`.
//! - I/O error opening the file → returns `Err(RestoreError::Io)`; caller
//!   should fall back to an empty history.

use std::path::Path;

use acowork_core::providers::traits::{ChatMessage, FunctionCall, MessageRole, ToolCall};

use crate::conversation::{ConversationEntry, ENTRY_KIND_COMPACTION};

/// Outcome of a successful restore call.
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    /// Messages ready to install into `HistoryManager` via `load_restored`.
    pub messages: Vec<ChatMessage>,
    /// Whether the JSONL contained at least one `kind="compaction"` event,
    /// i.e. replay was anchored at the most recent compaction summary.
    pub had_compaction: bool,
    /// Number of JSONL entries that contributed to the final message list
    /// (after merging tool_calls into assistants).
    pub replayed_entry_count: usize,
    /// Number of JSONL entries that were skipped (corrupt, orphaned tool
    /// results, pre-compaction noise, or `thought` filter).
    pub skipped_entry_count: usize,
}

/// Errors that abort restoration (caller should fall back to empty history).
#[derive(Debug)]
pub enum RestoreError {
    /// File could not be opened or read.
    Io(std::io::Error),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {}", e),
        }
    }
}

impl std::error::Error for RestoreError {}

impl From<std::io::Error> for RestoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Parse a JSONL conversation file into a replay-ready message sequence.
///
/// ADR-024: the file has no metadata header — parsing starts at line 0.
/// The caller provides `compaction_abs` (absolute byte offset of the last
/// compaction entry, read from `meta/{session_id}.json`) so we can skip
/// pre-compaction entries without an O(N) rposition scan.
///
/// See module docs for the full set of replay rules.
pub fn restore_history_from_jsonl(
    path: &Path,
    compaction_abs: Option<u64>,
) -> Result<RestoreOutcome, RestoreError> {
    use std::io::{BufRead, BufReader};

    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    // Pass 1: parse data lines.
    //
    // When `compaction_abs` is available we know the exact byte offset of
    // the compaction entry in the file.  This lets us skip storing pre-
    // compaction entries that will never enter the LLM context (user /
    // assistant / thought / tool entries before the last compaction),
    // saving both memory and the O(N) Pass-2 rposition scan.
    //
    // When `compaction_abs` is None (legacy session or session that has
    // never been compacted) we fall back to the original behaviour: store
    // every entry and locate the compaction via rposition.
    let mut entries: Vec<ConversationEntry> = Vec::new();
    let mut skipped = 0usize;
    // `passed_compaction` starts as `true` when there is no offset hint
    // (so we store *everything*).  It flips to `true` once we encounter
    // the compaction entry on the fast path.
    let mut passed_compaction = compaction_abs.is_none();

    for (line_idx, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(line_idx, error = %e, "Restore: I/O error reading line, skipping");
                skipped += 1;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        // Metadata line (line 0) was already consumed by read_line above.
        match serde_json::from_str::<ConversationEntry>(&line) {
            Ok(entry) => {
                let is_compaction =
                    entry.kind.as_deref() == Some(ENTRY_KIND_COMPACTION);

                if is_compaction {
                    passed_compaction = true;
                    entries.push(entry);
                } else if passed_compaction {
                    // After the compaction point: store everything.
                    entries.push(entry);
                } else if entry.role == "system" && entry.kind.is_none() {
                    // Before the compaction point: only keep system entries
                    // (identity context, workspace description, etc.).
                    entries.push(entry);
                }
                // else: pre-compaction non-system entry → skip (silently
                // dropped — it won't enter the LLM context).
            }
            Err(e) => {
                tracing::warn!(line_idx, error = %e, "Restore: malformed entry, skipping");
                skipped += 1;
            }
        }
    }

    // Pass 2: locate the most recent compaction marker (legacy path only;
    // the fast path above already tracked `had_compaction` and reordered
    // entries so the compaction entry is followed by post-compaction data).
    let last_compaction_idx = if compaction_abs.is_some() {
        // On the fast path entries are already [system, compaction, …]
        // so we can find the idx with a forward scan.
        entries.iter().position(|e| {
            e.kind.as_deref() == Some(ENTRY_KIND_COMPACTION)
        })
    } else {
        entries.iter().rposition(|e| {
            e.kind.as_deref() == Some(ENTRY_KIND_COMPACTION)
        })
    };

    // Pass 3: build the working entry slice based on whether a compaction
    // exists. With compaction: keep leading System entries + the compaction
    // entry itself (transformed into the marker) + all entries after it.
    // Without compaction: use the full entry list.
    let working: Vec<&ConversationEntry> = if let Some(comp_idx) = last_compaction_idx {
        let leading_system: Vec<&ConversationEntry> = entries[..comp_idx]
            .iter()
            .filter(|e| e.role == "system" && e.kind.is_none())
            .collect();
        let mut v = leading_system;
        v.push(&entries[comp_idx]);
        v.extend(entries[comp_idx + 1..].iter());
        v
    } else {
        entries.iter().collect()
    };

    // Pass 4: translate entries into ChatMessages, merging adjacent tool_call
    // rows onto their preceding assistant.
    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut replayed = 0usize;

    for entry in &working {
        // Compaction event → synthetic assistant marker (only honored once;
        // older compactions inside `working` shouldn't exist by construction,
        // but defensively skip them).
        if entry.kind.as_deref() == Some(ENTRY_KIND_COMPACTION) {
            // Compaction markers live at `User` role in memory (see
            // `HistoryManager::replace_middle_with_summary`).  Using
            // `Assistant` here would recreate the
            // `Assistant → Assistant{tool_calls}` adjacency that some
            // providers reject with 400 InvalidParameter.
            messages.push(ChatMessage {
                role: MessageRole::User,
                content: entry.content.clone(),
                name: Some("compaction_summary".to_string()),
                ..Default::default()
            });
            replayed += 1;
            continue;
        }

        match entry.role.as_str() {
            "thought" => {
                // Frontend-only; never enters LLM context.
                skipped += 1;
            }
            "system" => {
                messages.push(ChatMessage {
                    role: MessageRole::System,
                    content: entry.content.clone(),
                    ..Default::default()
                });
                replayed += 1;
            }
            "user" => {
                messages.push(ChatMessage::user(entry.content.clone()));
                replayed += 1;
            }
            "assistant" => {
                messages.push(ChatMessage::assistant(entry.content.clone()));
                replayed += 1;
            }
            "tool_call" => {
                let tool_call_id = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("tool_call_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_name = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if tool_call_id.is_empty() || tool_name.is_empty() {
                    tracing::warn!(
                        entry_id = %entry.id,
                        "Restore: tool_call missing tool_call_id or tool_name, dropping"
                    );
                    skipped += 1;
                    continue;
                }

                let new_call = ToolCall {
                    id: tool_call_id,
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: tool_name,
                        arguments: entry.content.clone(),
                    },
                };

                // Merge into the immediately preceding assistant if present;
                // otherwise synthesize an empty-content assistant.
                let merged = matches!(
                    messages.last(),
                    Some(m) if m.role == MessageRole::Assistant
                );
                if merged {
                    let last = messages.last_mut().unwrap();
                    last.tool_calls
                        .get_or_insert_with(Vec::new)
                        .push(new_call);
                } else {
                    messages.push(ChatMessage {
                        role: MessageRole::Assistant,
                        content: String::new(),
                        tool_calls: Some(vec![new_call]),
                        ..Default::default()
                    });
                }
                replayed += 1;
            }
            "tool_result" => {
                let tool_call_id = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("tool_call_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_name = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                if tool_call_id.is_empty() {
                    tracing::warn!(
                        entry_id = %entry.id,
                        "Restore: tool_result missing tool_call_id, dropping"
                    );
                    skipped += 1;
                    continue;
                }

                messages.push(ChatMessage {
                    role: MessageRole::Tool,
                    content: entry.content.clone(),
                    tool_call_id: Some(tool_call_id),
                    name: tool_name,
                    ..Default::default()
                });
                replayed += 1;
            }
            other => {
                tracing::warn!(role = other, "Restore: unknown role, dropping");
                skipped += 1;
            }
        }
    }

    // Pass 5: sanitize tool pairing — drop any Tool message whose tool_call_id
    // doesn't match a known preceding Assistant.tool_calls[*].id.
    let dropped = drop_orphan_tool_results(&mut messages);
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "Restore: dropped orphan tool_result messages with no matching tool_call"
        );
    }
    let skipped = skipped + dropped;

    Ok(RestoreOutcome {
        messages,
        had_compaction: last_compaction_idx.is_some(),
        replayed_entry_count: replayed,
        skipped_entry_count: skipped,
    })
}

/// Drop any `Tool` message whose `tool_call_id` cannot be matched to a
/// preceding `Assistant.tool_calls[*].id` in the same sequence.
///
/// Returns the number of messages removed.
fn drop_orphan_tool_results(messages: &mut Vec<ChatMessage>) -> usize {
    let mut known_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut keep_flags: Vec<bool> = Vec::with_capacity(messages.len());

    for msg in messages.iter() {
        match msg.role {
            MessageRole::Assistant => {
                if let Some(ref calls) = msg.tool_calls {
                    for c in calls {
                        known_ids.insert(c.id.clone());
                    }
                }
                keep_flags.push(true);
            }
            MessageRole::Tool => {
                let keep = msg
                    .tool_call_id
                    .as_ref()
                    .map(|id| known_ids.contains(id))
                    .unwrap_or(false);
                keep_flags.push(keep);
            }
            _ => keep_flags.push(true),
        }
    }

    let mut removed = 0usize;
    let mut idx = 0usize;
    messages.retain(|_| {
        let keep = keep_flags[idx];
        idx += 1;
        if !keep {
            removed += 1;
        }
        keep
    });
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{
        CompactionEventMeta, ConversationSession, SessionConfig,
    };
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    fn temp_workdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acowork-restorer-{}-{}",
            tag,
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn flush() {
        std::thread::sleep(std::time::Duration::from_millis(80));
    }

    #[test]
    fn restore_simple_user_assistant_roundtrip() {
        let work = temp_workdir("simple");
        let session_id = "sess-simple";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0, Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "hi", None);
        session.append_message("assistant", "hello", None);
        flush();

        let path = work.join("conversations").join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        assert!(!outcome.had_compaction);
        assert_eq!(outcome.messages.len(), 2);
        assert!(matches!(outcome.messages[0].role, MessageRole::User));
        assert_eq!(outcome.messages[0].content, "hi");
        assert!(matches!(outcome.messages[1].role, MessageRole::Assistant));
        assert_eq!(outcome.messages[1].content, "hello");
    }

    #[test]
    fn restore_drops_thought_lines() {
        let work = temp_workdir("thought");
        let session_id = "sess-thought";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0, Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "q", None);
        session.append_message("thought", "internal monologue", None);
        session.append_message("assistant", "a", None);
        flush();

        let path = work.join("conversations").join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        assert_eq!(outcome.messages.len(), 2, "thought should not enter context");
        assert!(outcome.messages.iter().all(|m| !matches!(m.role, MessageRole::System)));
        assert!(outcome.skipped_entry_count >= 1);
    }

    #[test]
    fn restore_merges_tool_calls_onto_assistant() {
        let work = temp_workdir("tools");
        let session_id = "sess-tools";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0, Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "list files", None);
        // Assistant text content + 2 tool_calls follow
        session.append_message("assistant", "", None);
        session.append_message(
            "tool_call",
            r#"{"path":"."}"#,
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_1"})),
        );
        session.append_message(
            "tool_call",
            r#"{"path":"./src"}"#,
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_2"})),
        );
        session.append_message(
            "tool_result",
            "a.rs\nb.rs",
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_1"})),
        );
        session.append_message(
            "tool_result",
            "main.rs",
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_2"})),
        );
        flush();

        let path = work.join("conversations").join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();

        // Expected: User, Assistant{tool_calls:[tc_1,tc_2]}, Tool(tc_1), Tool(tc_2)
        assert_eq!(outcome.messages.len(), 4);
        assert!(matches!(outcome.messages[0].role, MessageRole::User));
        assert!(matches!(outcome.messages[1].role, MessageRole::Assistant));
        let calls = outcome.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "tc_1");
        assert_eq!(calls[1].id, "tc_2");
        assert!(matches!(outcome.messages[2].role, MessageRole::Tool));
        assert_eq!(outcome.messages[2].tool_call_id.as_deref(), Some("tc_1"));
        assert!(matches!(outcome.messages[3].role, MessageRole::Tool));
        assert_eq!(outcome.messages[3].tool_call_id.as_deref(), Some("tc_2"));
    }

    #[test]
    fn restore_drops_orphan_tool_result() {
        let work = temp_workdir("orphan");
        let session_id = "sess-orphan";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0, Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        // tool_result with no preceding tool_call → orphan
        session.append_message("user", "q", None);
        session.append_message(
            "tool_result",
            "stale",
            Some(serde_json::json!({"tool_name":"x","tool_call_id":"missing"})),
        );
        session.append_message("assistant", "ok", None);
        flush();

        let path = work.join("conversations").join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        // user + assistant, orphan tool_result dropped
        assert_eq!(outcome.messages.len(), 2);
        assert!(outcome.skipped_entry_count >= 1);
    }

    #[test]
    fn restore_anchors_at_last_compaction() {
        let work = temp_workdir("compact");
        let session_id = "sess-compact";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0, Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        // Pre-compaction noise
        session.append_message("user", "u1", None);
        session.append_message("assistant", "a1", None);
        session.append_message("user", "u2", None);
        session.append_message("assistant", "a2", None);
        // Compaction event covering the above
        session.append_compaction_event(
            "<summary>compacted u1..a2</summary>",
            CompactionEventMeta {
                compacted_from_id: String::new(),
                compacted_to_id: String::new(),
                keep_last_rounds: 3,
                model: "test-model".into(),
                before_tokens: 1000,
                after_tokens: 100,
            },
        );
        // Post-compaction tail
        session.append_message("user", "u3", None);
        session.append_message("assistant", "a3", None);
        flush();

        let path = work.join("conversations").join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        assert!(outcome.had_compaction);
        // Expected: [compaction_summary marker, u3, a3]
        assert_eq!(outcome.messages.len(), 3);
        // Compaction marker lives at `User` role to avoid producing
        // Assistant→Assistant adjacency in the request payload (see
        // module-level docs).  Consumers identify the marker by name.
        assert!(matches!(outcome.messages[0].role, MessageRole::User));
        assert_eq!(
            outcome.messages[0].name.as_deref(),
            Some("compaction_summary")
        );
        assert!(outcome.messages[0].content.contains("compacted u1..a2"));
        assert!(matches!(outcome.messages[1].role, MessageRole::User));
        assert_eq!(outcome.messages[1].content, "u3");
        assert!(matches!(outcome.messages[2].role, MessageRole::Assistant));
        assert_eq!(outcome.messages[2].content, "a3");
    }

    #[test]
    fn restore_skips_corrupt_lines() {
        let work = temp_workdir("corrupt");
        let session_id = "sess-corrupt";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0, Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "ok1", None);
        flush();
        // Inject a bogus line directly into the file
        let path = work.join("conversations").join(format!("{}.jsonl", session_id));
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{{not valid json").unwrap();
        }
        session.append_message("user", "ok2", None);
        flush();

        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        assert_eq!(outcome.messages.len(), 2);
        assert!(outcome.skipped_entry_count >= 1);
    }
}
