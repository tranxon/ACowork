//! Built-in tools module
//!
//! Phase 1: 13 built-in tools per design doc (12-tool-system.md)
//! Phase 4 (S4.4): +1 RAG tool (rag_query, conditional on manifest RAG declaration)
//!
//! | Tool | Permission |
//! |------|------------|
//! | memory_recall | memory:read |
//! | memory_store | memory:write |
//! | http_request | network:<url> |
//! | web_fetch | network:<url> |
//! | web_search | search:web |
//! | shell | filesystem:exec |
//! | file_read | filesystem:read:<path> |
//! | file_write | filesystem:write:<path> |
//! | file_edit | filesystem:write:<path> |
//! | doc_reader | filesystem:read:<path> |
//! | glob_search | filesystem:read:<path> |
//! | content_search | filesystem:read:<path> |
//! | intent_send | intent:send:<target> |
//! | rag_query | rag:query + network:<rag_url> (conditional) |
//! | ask_user_question | (no permission — LLM-initiated, always allowed) |

pub mod ask_user_question;
pub mod codebase;
pub mod content_search;
pub mod doc_reader;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob_search;
pub mod http_request;
pub mod intent_send;
pub mod mcp_install;
pub mod mcp_uninstall;
pub mod memory_recall;
pub mod memory_store;
pub mod rag_query;
pub mod search_backends;
pub mod shell;
pub mod todo_write;
pub mod web_fetch;
pub mod web_search;

use acowork_core::tools::traits::Tool;
use acowork_grafeo::grafeo::GrafeoStore;
use std::sync::Arc;
use std::time::Duration;

use crate::mcp_notify::McpNotifyRef;
use crate::tools::workspace_resolver::SharedResolver;
use search_backends::WebSearchEngine;

/// Create the standard built-in tools (without RAG).
///
/// Shell tools are registered dynamically based on platform detection:
/// - Windows: Git Bash (bash) + PowerShell, or just PowerShell if Git not found
/// - Linux/macOS: Single "shell" tool using system shell
///
/// # Arguments
/// * `resolver` - Workspace directory resolver (single source of truth)
/// * `agent_id` - Agent ID for memory isolation and identity management
/// * `tool_http_timeout_ms` - Default HTTP timeout in milliseconds for built-in tools
/// * `has_search_providers` - Whether at least one search provider is configured.
///   When false, the `web_search` tool is skipped to avoid wasting LLM calls on
///   a tool that always returns "Provider not configured".
/// * `grafeo_store` - Optional GrafeoStore for memory_store backend wiring.
/// * `memory_session` - Optional MemorySessionHandle for memory_recall session-aware retrieval.
/// * `mcp_notifier` - Optional McpConfigNotifier for mcp_install/mcp_uninstall event notification.
/// * `agent_home` - Agent home directory (from `config().work_dir`). Required by mcp_install/mcp_uninstall
///   for config persistence — MCP configs are per-agent, stored in `{agent_home}/config/agent_mcp.json`,
///   not per-project. No fallback: must always be set explicitly.
#[allow(clippy::too_many_arguments)]
pub fn all_builtin_tools(
    resolver: &SharedResolver,
    agent_id: &str,
    tool_http_timeout_ms: u64,
    has_search_providers: bool,
    grafeo_store: Option<Arc<GrafeoStore>>,
    memory_session: Option<Arc<crate::memory::MemorySessionHandle>>,
    mcp_notifier: McpNotifyRef,
    agent_home: String,
    lsp_relay_endpoint: Option<String>,
) -> Vec<Arc<dyn Tool>> {
    // Register shell tools based on platform detection
    let shell_tools: Vec<Arc<dyn Tool>> = crate::platform::detected_shells()
        .into_iter()
        .map(|s| {
            Arc::new(shell::ShellTool::new(
                &s.tool_name,
                &s.display_name,
                &s.binary,
                &s.path,
                &s.arg,
            )) as Arc<dyn Tool>
        })
        .collect();

    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(memory_recall::MemoryRecallTool::new(
            agent_id,
            memory_session,
        )),
        Arc::new(memory_store::MemoryStoreTool::new(agent_id, grafeo_store)),
        Arc::new(http_request::HttpRequestTool::new()),
        Arc::new(web_fetch::WebFetchTool::with_timeout(
            Duration::from_millis(tool_http_timeout_ms),
        )),
        Arc::new(file_read::FileReadTool::new()),
        Arc::new(file_write::FileWriteTool::new()),
        Arc::new(file_edit::FileEditTool::new()),
        Arc::new(doc_reader::DocReaderTool::new()),
        Arc::new(glob_search::GlobSearchTool::new(resolver)),
        Arc::new(content_search::ContentSearchTool::new(resolver)),
        Arc::new(intent_send::IntentSendTool::new()),
        Arc::new(ask_user_question::AskUserQuestionTool::new()),
        Arc::new(todo_write::TodoWriteTool::new()),
        Arc::new(mcp_install::McpInstallTool::new(
            mcp_notifier.clone(),
            agent_home.clone(),
        )),
        Arc::new(mcp_uninstall::McpUninstallTool::new(
            mcp_notifier.clone(),
            agent_home.clone(),
        )),
    ];

    // Only register web_search when at least one search provider is configured.
    // Without providers, the tool always fails with "Provider not configured",
    // wasting LLM inference tokens on doomed calls.
    if has_search_providers {
        // Build search engine from agent's configured backends.
        // Initially empty — backends are populated when search config arrives from Gateway.
        // The timeout is passed through so that build_backend() creates backends with the configured value.
        let search_engine =
            WebSearchEngine::new(Vec::new(), Duration::from_millis(tool_http_timeout_ms));
        tools.push(Arc::new(web_search::WebSearchTool::new(search_engine)));
    }

    // Only register codebase when the LSP Relay is available.
    // Without the relay, the tool always fails with "LSP Relay not available",
    // wasting LLM inference tokens on doomed calls.
    if let Some(endpoint) = lsp_relay_endpoint {
        tools.push(Arc::new(codebase::CodebaseTool::new(endpoint)));
    }

    // Append platform-specific shell tools
    tools.extend(shell_tools);
    tools
}
