//! ADR-060 end-to-end test (crate-internal): prompt-cache-friendly context
//! block reorganisation across the full production chain.
//!
//! Drives the REAL chain in-process:
//!   SessionManager → SessionTask → AgentLoop → ContextBuilder::build()
//!   → ScriptedProvider (records every ChatRequest) → tool dispatch
//!   (`todo_write` intercepted in `loop_tools`) → `SessionState::update_todos`
//!   → `ConversationSession::set_todos` → meta.json on disk
//!
//! Then simulates a process restart (fresh SessionManager, fresh AgentCore,
//! resume from disk) and verifies Block C is rebuilt from the persisted
//! todos.
//!
//! Why crate-internal? `AgentCore::new` is `pub(crate)` — the production
//! startup path (`startup::session_init`) is not a public API, so an
//! integration test under `tests/` cannot assemble the loop. Same pattern
//! as `test_support.rs`: the module is compiled out of non-test builds.
//!
//! The ONLY simulated part is the LLM: `ScriptedProvider` pops scripted
//! `ChatResponse` fixtures (the same trust boundary as `memory_e2e.rs`).
//! Everything else — session lifecycle, message persistence, meta writes,
//! context building, tool execution, restart recovery — is production code.

#![cfg(test)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acowork_core::error::Result;
use acowork_core::manifest::AgentManifest;
use acowork_core::protocol::{ModelCapabilitiesInfo, ProviderListItem, ProviderModelEntry};
use acowork_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, FunctionCall, MessageRole, Provider, StreamEvent,
    ToolCall, UsageInfo,
};
use async_trait::async_trait;
use futures_core::Stream;

use crate::agent::agent_core::{AgentCore, BuiltinToolEntry};
use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};
use crate::agent::session::session_manager::{SessionManager, SessionManagerConfig};
use crate::agent::session::session_task::SessionMessage;
use crate::config::RuntimeConfig;
use crate::conversation::{ConversationSession, SessionConfig};
use crate::tools::builtin::todo_write::TodoWriteTool;
use crate::tools::workspace_resolver::WorkspaceResolver;

// ═══════════════════════════════════════════════════════════════════════
// ScriptedProvider — the only simulated component
// ═══════════════════════════════════════════════════════════════════════

/// One scripted LLM response step.
#[derive(Debug, Clone)]
struct ScriptedStep {
    content: String,
    tool_calls: Option<Vec<ToolCall>>,
}

fn text_step(content: &str) -> ScriptedStep {
    ScriptedStep {
        content: content.to_string(),
        tool_calls: None,
    }
}

/// A `todo_write` tool-call step: asks the loop to persist `(id, content)`
/// pairs, exactly like the production LLM would after reading Block C.
fn todo_write_step(items: &[(&str, &str)]) -> ScriptedStep {
    let todos: Vec<serde_json::Value> = items
        .iter()
        .map(|(id, content)| {
            serde_json::json!({
                "id": id,
                "content": content,
                "status": "pending",
            })
        })
        .collect();
    ScriptedStep {
        content: String::new(),
        tool_calls: Some(vec![ToolCall {
            id: "toolu_01".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "todo_write".to_string(),
                arguments: serde_json::json!({ "todos": todos }).to_string(),
            },
        }]),
    }
}

/// Deterministic LLM stand-in for the ADR-060 E2E.
///
/// Two script queues keep the fixture deterministic even though the
/// session-title generation runs on a background task racing the main
/// loop: title requests (`Generate a session title …` single-message
/// calls, routed via `provider.chat`) pop from `title_steps`, main chat
/// requests (the Block A/B/C/D pipeline, routed via `provider.chat_stream`)
/// pop from `main_steps`. The last step of each queue repeats so an
/// unexpected extra call fails loudly in assertions instead of returning
/// an empty response.
struct ScriptedProvider {
    main_steps: Mutex<VecDeque<ScriptedStep>>,
    title_steps: Mutex<VecDeque<ScriptedStep>>,
    /// Main chat requests in call order (Block A/B/C/D assertions).
    main_captured: Mutex<Vec<ChatRequest>>,
    /// Title-generation requests (asserted only for count sanity).
    title_captured: Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    fn new(main_steps: Vec<ScriptedStep>, title_steps: Vec<ScriptedStep>) -> Self {
        Self {
            main_steps: Mutex::new(main_steps.into()),
            title_steps: Mutex::new(title_steps.into()),
            main_captured: Mutex::new(Vec::new()),
            title_captured: Mutex::new(Vec::new()),
        }
    }

    /// Captured main-chat requests (excludes background title calls).
    fn captured(&self) -> Vec<ChatRequest> {
        self.main_captured.lock().unwrap().clone()
    }

    /// True for the session-title generation request: a single User
    /// message carrying the title prompt (spawned after the first message
    /// of each session).
    fn is_title_request(request: &ChatRequest) -> bool {
        request.messages.len() == 1
            && request.messages[0].role == MessageRole::User
            && request.messages[0].content.contains("Generate a session title")
    }

    fn pop_step(&self, title: bool) -> ChatResponse {
        let steps = if title {
            &self.title_steps
        } else {
            &self.main_steps
        };
        let mut steps = steps.lock().unwrap();
        // Pop while more than one remains; the last step repeats so an
        // unexpected extra LLM call fails loudly in assertions instead of
        // returning an empty response.
        let step = if steps.len() > 1 {
            steps.pop_front().expect("non-empty queue")
        } else {
            steps.front().expect("script always has a step").clone()
        };
        ChatResponse {
            content: step.content,
            tool_calls: step.tool_calls,
            usage: Some(UsageInfo {
                prompt_tokens: 50,
                completion_tokens: 25,
                total_tokens: 75,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted-e2e"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let title = Self::is_title_request(&request);
        if title {
            self.title_captured.lock().unwrap().push(request);
        } else {
            self.main_captured.lock().unwrap().push(request);
        }
        Ok(self.pop_step(title))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let title = Self::is_title_request(&request);
        if title {
            self.title_captured.lock().unwrap().push(request.clone());
        } else {
            self.main_captured.lock().unwrap().push(request.clone());
        }
        let resp = self.pop_step(title);
        let mut events = Vec::with_capacity(2);
        if !resp.content.is_empty() {
            events.push(StreamEvent::Content(resp.content.clone()));
        }
        events.push(StreamEvent::Finished(resp));
        Ok(Box::new(futures_util::stream::iter(events)))
    }

    async fn chat_token_count(&self, messages: &[ChatMessage]) -> Result<u64> {
        Ok(messages.iter().map(|m| m.content.len() as u64 / 4).sum())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Harness — real production assembly
// ═══════════════════════════════════════════════════════════════════════

/// Test agent identity shared by config + manifest.
const AGENT_ID: &str = "com.test.agent";
/// System prompt; Block A stability assertions depend on it being identical
/// across both harness generations.
const SYSTEM_PROMPT: &str = "You are the ACowork E2E test agent (ADR-060).";

fn test_config(work_dir: &Path) -> RuntimeConfig {
    RuntimeConfig {
        agent_id: AGENT_ID.to_string(),
        work_dir: work_dir.display().to_string(),
        ..Default::default()
    }
}

fn test_manifest() -> AgentManifest {
    // All optional manifest sections default via serde; only the identity
    // strings are required.
    AgentManifest::from_toml(
        r#"
        agent_id = "com.test.agent"
        version = "1.0.0"
        name = "Test Agent"
        description = "ADR-060 E2E test agent"
        author = "acowork"
        runtime_version = "1.0.0"
        "#,
    )
    .expect("minimal manifest parses")
}

struct E2eHarness {
    sid: String,
    sm: Arc<tokio::sync::Mutex<SessionManager>>,
    chunk_rx: tokio::sync::mpsc::Receiver<SessionChunkEvent>,
}

impl E2eHarness {
    /// Send one user message and block until the round emits `Done`.
    ///
    /// The session task is driven entirely by production code: context
    /// build, LLM call (scripted), tool execution, persistence.
    async fn send(&mut self, content: &str, message_id: &str) {
        {
            let mut guard = self.sm.lock().await;
            guard
                .send_to_session(
                    &self.sid,
                    SessionMessage::ChatMessage {
                        content: content.to_string(),
                        message_id: message_id.to_string(),
                        skill_instructions: None,
                        attached_items: None,
                        content_parts: None,
                    },
                )
                .expect("session task accepts the message");
        }
        // Wait for the round to finish (Done). Other control events
        // (ContextUsage, TodoListUpdated, …) are ignored.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let evt = tokio::time::timeout(remaining, self.chunk_rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("round for '{content}' did not finish within 60s")
                })
                .expect("chunk channel stays open");
            match evt.event {
                ChunkEvent::Done { .. } => return,
                ChunkEvent::Error { user_message, .. } => {
                    panic!("round for '{content}' failed: {user_message}")
                }
                _ => {}
            }
        }
    }
}

/// Assemble the REAL production chain for one "process generation":
/// AgentCore (shared template) + SessionManager + a ConversationSession
/// wired to disk. When `resume_sid` is `Some`, the conversation is resumed
/// from disk (restart simulation) instead of created fresh.
async fn spawn_harness(
    work_dir: &Path,
    provider: Arc<ScriptedProvider>,
    resume_sid: Option<&str>,
) -> E2eHarness {
    let core = Arc::new(AgentCore::new(
        test_config(work_dir),
        test_manifest(),
        provider,
        vec![BuiltinToolEntry {
            // Minimal tool set: only `todo_write` is needed for ADR-060.
            tool: Arc::new(TodoWriteTool::new()),
            enabled: true,
        }],
    ));

    // Register the test model with capabilities — in production this data
    // arrives via the Gateway `AgentHelloResult` push; the loop's
    // context-usage reporting requires it before emitting a response.
    core.global_provider_list.write().unwrap().push(ProviderListItem {
        id: "test-provider".to_string(),
        base_url: "http://127.0.0.1:0".to_string(),
        protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
        models: vec![ProviderModelEntry {
            id: "test-model".to_string(),
            capabilities: ModelCapabilitiesInfo {
                context_window: 128_000,
                max_output_tokens: 4096,
                max_input_tokens: None,
                supports_tool_calling: true,
                supports_reasoning: None,
                supports_attachment: None,
                supports_temperature: None,
                cost: None,
                modalities: None,
                name: Some("Test Model".to_string()),
                family: None,
                knowledge_cutoff: None,
                default_reasoning_effort: None,
                thinking_mode: None,
            },
            max_output_tokens_limit: 4096,
        }],
        compact_model: None,
        custom: false,
    });

    let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(64);
    let mut sm = SessionManager::new(
        core,
        SessionManagerConfig {
            system_prompt: SYSTEM_PROMPT.to_string(),
            chunk_tx: Some(chunk_tx),
            ..Default::default()
        },
    );
    sm.set_resolver(Arc::new(std::sync::RwLock::new(
        WorkspaceResolver::new_for_test(vec![]),
    )));

    let committed_lines = SessionManager::new_committed_lines();
    let (sid, conversation) = if let Some(resume_sid) = resume_sid {
        let (conv, _meta_rx, _state_rx) = ConversationSession::resume(
            work_dir,
            resume_sid,
            committed_lines.clone(),
        )
        .expect("resume conversation from disk");
        (resume_sid.to_string(), conv)
    } else {
        let sid = "test-sid-1".to_string();
        let (conv, _meta_rx, _state_rx) = ConversationSession::new(
            work_dir,
            &sid,
            SessionConfig {
                agent_id: AGENT_ID.to_string(),
                workspace_id: None,
                // Model MUST be a registered id (loop resolves capabilities
                // from `global_provider_list`). Provider id is deliberately
                // NOT registered: `SessionTask` then falls into the
                // `ReliableProvider::new(core.provider)` branch instead of
                // building a real network provider for `base_url`, keeping
                // the scripted provider in the call path.
                model: Some("test-model".to_string()),
                provider: Some("scripted-e2e".to_string()),
            },
            8,
            committed_lines.clone(),
        )
        .expect("create conversation on disk");
        (sid, conv)
    };
    sm.create_session_with_id_and_conversation(sid.clone(), Some(conversation), Some(committed_lines))
        .await
        .expect("session task starts");

    E2eHarness {
        sid,
        sm: Arc::new(tokio::sync::Mutex::new(sm)),
        chunk_rx,
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Assertion helpers
// ═══════════════════════════════════════════════════════════════════════

/// Short role names for layout assertions (A/B/C/D block ordering).
fn roles(msgs: &[ChatMessage]) -> Vec<&'static str> {
    msgs.iter()
        .map(|m| match m.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        })
        .collect()
}

/// Assert the classic prompt-cache layout of one request:
/// `[Block A system, …, Block C user, Block D user(current)]` with Block C
/// carrying the todo snapshot.
fn assert_block_c_layout(msgs: &[ChatMessage], current: &str, expected_todo: &str) {
    let n = msgs.len();
    assert!(n >= 3, "expected A + B + C + D, got {n} messages");
    // Block D — the current user message (explicitly passed, cloned copy).
    assert_eq!(msgs[n - 1].role, MessageRole::User, "Block D is user role");
    assert_eq!(msgs[n - 1].content, current, "Block D carries the current turn");
    // Block C — the todo snapshot as an independent User-role message.
    assert_eq!(msgs[n - 2].role, MessageRole::User, "Block C is user role");
    assert!(
        msgs[n - 2].content.contains("## Active Task List"),
        "Block C must contain the todo-list header"
    );
    assert!(
        msgs[n - 2].content.contains(expected_todo),
        "Block C must carry the expected todo item"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// The end-to-end test
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn todo_write_roundtrip_block_c_layout_and_restart_recovery() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    // ── Generation 1: fresh process ────────────────────────────────────
    // Main script: turn 1 text → turn 2 todo_write tool call → turn 2
    // follow-up text → turn 3 text. Title script: one response per
    // session (first message triggers a background title call).
    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("Turn 1 response"),
            todo_write_step(&[("t1", "Implement Block C")]),
            text_step("Todos saved"),
            text_step("Turn 3 response"),
        ],
        vec![text_step("Test session")],
    ));
    let mut h1 = spawn_harness(work_dir, provider.clone(), None).await;
    let sid = h1.sid.clone();

    // ── Turn 1: no todos yet → Block C absent, layout is A + B + D ──
    h1.send("First turn", "m1").await;
    let captured = provider.captured();
    assert_eq!(captured.len(), 1, "turn 1 must trigger exactly one LLM call");
    let msgs = &captured[0].messages;
    assert_eq!(roles(msgs), ["system", "user", "user"], "A + B(u1) + D(u1')");
    assert!(
        msgs[0].content.contains(SYSTEM_PROMPT),
        "Block A carries the system prompt"
    );
    assert!(
        !msgs[0].content.contains("## Active Task List"),
        "Block A must NOT contain the todo snapshot"
    );
    assert_eq!(msgs[1].content, "First turn", "Block B holds the original turn");
    assert_eq!(msgs[2].content, "First turn", "Block D is the cloned copy");

    // ── Turn 2: LLM returns a todo_write tool call ──
    h1.send("Second turn", "m2").await;
    let captured = provider.captured();
    // Call 2 = turn-2 request; call 3 = the post-tool iteration request.
    assert_eq!(captured.len(), 3, "turn 2 must trigger two LLM calls (tool iteration)");
    let msgs = &captured[1].messages;
    assert_eq!(msgs.last().unwrap().role, MessageRole::User);
    assert_eq!(msgs.last().unwrap().content, "Second turn");
    let msgs = &captured[2].messages;
    // Tool iteration (ADR-060 §5.5): no Block D — the request tail is
    // Block C (fresh todo snapshot, User role) right after the todo_write
    // tool result; the current user turn appears only once (Block B).
    assert_eq!(
        msgs.last().unwrap().role,
        MessageRole::User,
        "tool iteration: last message is Block C (todo snapshot), no Block D"
    );
    assert!(
        msgs.last().unwrap().content.contains("## Active Task List"),
        "Block C is the request tail after the todo change"
    );
    assert!(
        msgs.iter().any(|m| {
            m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some("toolu_01")
        }),
        "Block B must contain the todo_write tool result"
    );
    assert!(
        msgs.iter().any(|m| {
            m.role == MessageRole::User && m.content.contains("## Active Task List")
        }),
        "Block C appears immediately after todos change (tool iteration)"
    );
    // The current user turn appears exactly once (Block B only).
    assert_eq!(
        msgs.iter().filter(|m| m.content == "Second turn").count(),
        1,
        "no Block D in tool iteration — user turn appears once in Block B"
    );

    // ── Disk: todos must be persisted to meta.json immediately ──
    let meta = crate::conversation::read_session_meta(&work_dir.join("conversations"), &sid)
        .expect("session meta exists");
    let todos = meta.todos.expect("todo list persisted to meta.json");
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].id, "t1");
    assert_eq!(todos[0].content, "Implement Block C");

    // ── Turn 3: Block C now present between B and D ──
    h1.send("Third turn", "m3").await;
    let captured = provider.captured();
    assert_eq!(captured.len(), 4);
    assert_block_c_layout(&captured[3].messages, "Third turn", "Implement Block C");

    // ── Prefix stability: Block A byte-identical across every call ──
    // This is the core prompt-cache guarantee of ADR-060: the static
    // kernel (Block A) must never change once a session exists.
    for (i, req) in captured.iter().enumerate() {
        assert_eq!(
            req.messages[0].content,
            captured[0].messages[0].content,
            "Block A (system) must be byte-stable across calls (call {i})"
        );
        assert_eq!(req.messages[0].role, MessageRole::System);
    }
    // Block B is append-only: the turn-1 user message reappears verbatim.
    assert_eq!(
        captured[3].messages[1].content,
        captured[0].messages[1].content,
        "Block B history must be append-only (u1 verbatim in turn 3)"
    );

    // ── "Process restart": drop generation 1, rebuild from disk ──
    // Dropping the harness closes the session task's sender; the session
    // task terminates. Todos were already persisted by set_todos.
    drop(h1);

    let provider2 = Arc::new(ScriptedProvider::new(
        vec![text_step("Turn 4 response")],
        vec![text_step("Restarted session")],
    ));
    let mut h2 = spawn_harness(work_dir, provider2.clone(), Some(&sid)).await;
    h2.send("Fourth turn", "m4").await;

    let captured2 = provider2.captured();
    assert_eq!(captured2.len(), 1);
    let msgs = &captured2[0].messages;
    // Block C rebuilt from DISK (not from the old in-memory state).
    assert_block_c_layout(msgs, "Fourth turn", "Implement Block C");
    // Block A is byte-identical across the process restart.
    assert_eq!(
        msgs[0].content,
        captured[0].messages[0].content,
        "Block A must be byte-stable across the process restart"
    );
}
