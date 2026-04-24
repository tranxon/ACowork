//! Built-in tools module
//!
//! Phase 1: 13 built-in tools per design doc (12-tool-system.md)
//! Phase 2 (S3.4): +2 identity tools (identity_query, identity_observe)
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
//! | glob_search | filesystem:read:<path> |
//! | content_search | filesystem:read:<path> |
//! | intent_send | intent:send:<target> |
//! | identity_store | identity:write |
//! | identity_query | identity:read |
//! | identity_observe | identity:read |

pub mod memory_recall;
pub mod memory_store;
pub mod http_request;
pub mod web_fetch;
pub mod web_search;
pub mod shell;
pub mod file_read;
pub mod file_write;
pub mod file_edit;
pub mod glob_search;
pub mod content_search;
pub mod intent_send;
pub mod identity_store;
pub mod identity_query;
pub mod identity_observe;

use rollball_core::tools::traits::Tool;
use std::sync::Arc;

/// Create all 15 built-in tools (13 Phase 1 + 2 Phase 2 identity tools)
///
/// # Arguments
/// * `work_dir` - Working directory for filesystem/shell tools
/// * `agent_id` - Agent ID for memory isolation and identity management
pub fn all_builtin_tools(
    work_dir: &str,
    agent_id: &str,
) -> Vec<Arc<dyn Tool>> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(memory_recall::MemoryRecallTool::new(agent_id)),
        Arc::new(memory_store::MemoryStoreTool::new(agent_id)),
        Arc::new(http_request::HttpRequestTool::new()),
        Arc::new(web_fetch::WebFetchTool::new()),
        Arc::new(web_search::WebSearchTool::new()),
        Arc::new(shell::ShellTool::new(work_dir)),
        Arc::new(file_read::FileReadTool::new(work_dir)),
        Arc::new(file_write::FileWriteTool::new(work_dir)),
        Arc::new(file_edit::FileEditTool::new(work_dir)),
        Arc::new(glob_search::GlobSearchTool::new(work_dir)),
        Arc::new(content_search::ContentSearchTool::new(work_dir)),
        Arc::new(intent_send::IntentSendTool::new()),
        Arc::new(identity_store::IdentityStoreTool::new(agent_id)),
        Arc::new(identity_query::IdentityQueryTool::new(agent_id)),
        Arc::new(identity_observe::IdentityObserveTool::new(agent_id)),
    ];
    tools
}
