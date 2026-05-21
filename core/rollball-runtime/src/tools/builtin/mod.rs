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

pub mod memory_recall;
pub mod memory_store;
pub mod http_request;
pub mod web_fetch;
pub mod web_search;
pub mod shell;
pub mod file_read;
pub mod file_write;
pub mod file_edit;
pub mod doc_reader;
pub mod glob_search;
pub mod content_search;
pub mod intent_send;
pub mod rag_query;
pub mod ask_user_question;

use rollball_core::tools::traits::Tool;
use std::sync::Arc;

use crate::tools::workspace_resolver::WorkspaceResolver;

/// Create the standard built-in tools (without RAG).
///
/// Shell tools are registered dynamically based on platform detection:
/// - Windows: Git Bash (bash) + PowerShell, or just PowerShell if Git not found
/// - Linux/macOS: Single "shell" tool using system shell
///
/// # Arguments
/// * `resolver` - Workspace directory resolver (single source of truth)
/// * `agent_id` - Agent ID for memory isolation and identity management
pub fn all_builtin_tools(
    resolver: &WorkspaceResolver,
    agent_id: &str,
) -> Vec<Arc<dyn Tool>> {
    let work_dir = resolver.agent_home();
    let current_dir = resolver.current_dir();

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
                current_dir,
            )) as Arc<dyn Tool>
        })
        .collect();

    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(memory_recall::MemoryRecallTool::new(agent_id)),
        Arc::new(memory_store::MemoryStoreTool::new(agent_id)),
        Arc::new(http_request::HttpRequestTool::new()),
        Arc::new(web_fetch::WebFetchTool::new()),
        Arc::new(web_search::WebSearchTool::new()),
        Arc::new(file_read::FileReadTool::new(current_dir)),
        Arc::new(file_write::FileWriteTool::new(current_dir)),
        Arc::new(file_edit::FileEditTool::new(current_dir)),
        Arc::new(doc_reader::DocReaderTool::new(current_dir)),
        Arc::new(glob_search::GlobSearchTool::new(resolver)),
        Arc::new(content_search::ContentSearchTool::new(resolver)),
        Arc::new(intent_send::IntentSendTool::new()),
        Arc::new(ask_user_question::AskUserQuestionTool::new()),
    ];

    // Append platform-specific shell tools
    tools.extend(shell_tools);
    tools
}
