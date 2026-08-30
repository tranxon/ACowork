//! Built-in tools module
//!
//! Phase 1: 13 built-in tools per design doc (12-tool-system.md)
//! Phase 4 (S4.4): +1 RAG tool (rag_query, registered in agent_init.rs when
//!                   manifest declares a RAG tool entry)
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
//! | rag_query | rag:query + network:<rag_url> (registered in agent_init.rs) |
//! | context_retrieve | context:read — retrieve original tool result content |
//! | context_abandon | context:write - replace tool result with placeholder |
//! | ask_user_question | (no permission — LLM-initiated, always allowed) |

pub mod ask_user_question;
pub mod codebase;
pub mod content_search;
pub mod context_abandon;
pub mod context_retrieve;
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
use std::sync::Arc;
use std::time::Duration;

use crate::agent::context_compression::RetrieveQueue;
use crate::mcp_notify::McpNotifyRef;
use crate::tools::workspace_resolver::SharedResolver;
use search_backends::WebSearchEngine;

/// Construct the `context_retrieve` platform-protected tool.
///
/// ADR-061 §10.2: `context_abandon` is no longer registered — LLM
/// autonomous tool compression is closed, and its in-place replacement
/// would invalidate the ADR-060 Block B prompt cache. `context_retrieve`
/// stays as the manual recall channel for compressed tool results.
///
/// Centralizing the construction here means there is exactly one place
/// to extend when a new platform tool is added (a single
/// [`crate::tools::registry::PLATFORM_PROTECTED_TOOLS`] match-arm
/// edit, plus one push below — same pattern as `all_builtin_tools`).
///
/// Reused by:
/// - [`crate::tools::builtin::all_builtin_tools`] at startup (always
///   registered — the `tool_compression_enabled` gate is removed)
///
/// The tool's internal queue (`retrieve_queue`) is an `Arc<Mutex<...>>`
/// clone of the per-`AgentCore` shared queue; the hot-reload path passes
/// the same `Arc` clone so any agent-loop side that drains the queue
/// keeps seeing the same backing storage regardless of how many times
/// the registry rebuilds.
pub fn build_platform_protected_tools(
    agent_home: &str,
    retrieve_queue: RetrieveQueue,
) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(context_retrieve::ContextRetrieveTool::new(
        agent_home,
        retrieve_queue,
    ))]
}

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
/// * `search_key_vault` - Shared search API key vault. The `web_search`
///   tool is registered when the provider list is non-empty at call time.
///   The engine reads from this vault (and `search_provider_list`) at
///   call time, so MQTT-driven key updates take effect immediately.
/// * `search_provider_list` - Shared list of configured search providers.
/// * `memory_session` - Optional MemorySessionHandle for memory_recall and memory_store late-binding store access.
/// * `mcp_notifier` - Optional McpConfigNotifier for mcp_install/mcp_uninstall event notification.
/// * `agent_home` - Agent home directory (from `config().work_dir`). Required by mcp_install/mcp_uninstall
///   for config persistence — MCP configs are per-agent, stored in `{agent_home}/config/agent_mcp.json`,
///   not per-project. No fallback: must always be set explicitly.
/// * `retrieve_queue` - Shared queue for context_retrieve tool (ADR-052). Injected into the tool
///   and the agent loop. ADR-061 §10.2: `context_retrieve` is **always** registered (manual recall
///   channel); the `tool_compression_enabled` gate and `context_abandon` are removed.
///
/// `#[allow(clippy::too_many_arguments)]` follows the project convention
/// for thin pass-through facades (cf. `AgentCore::new_with_observer`): the
/// arguments are independent assembly dependencies for the tool registry.
#[allow(clippy::too_many_arguments)]
pub fn all_builtin_tools(
    resolver: &SharedResolver,
    agent_id: &str,
    tool_http_timeout_ms: u64,
    search_key_vault: search_backends::SharedSearchKeyVault,
    search_provider_list: search_backends::SharedSearchProviderList,
    memory_session: Option<Arc<crate::memory::MemorySessionHandle>>,
    mcp_notifier: McpNotifyRef,
    agent_home: String,
    lsp_relay_endpoint: Option<String>,
    mqtt_slot: crate::http::server::SharedMqttClientSlot,
    retrieve_queue: RetrieveQueue,
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

    // Clone memory_session for MemoryStoreTool before the first use
    // (MemoryRecallTool takes ownership of the original).
    let memory_session_for_store = memory_session.clone();

    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(memory_recall::MemoryRecallTool::new(
            agent_id,
            memory_session,
        )),
        Arc::new(memory_store::MemoryStoreTool::new(agent_id, memory_session_for_store)),
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
        Arc::new(intent_send::IntentSendTool::new(agent_id.to_string(), mqtt_slot.clone())),
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

    // ADR-061 §10.2: `context_retrieve` is ALWAYS registered (manual
    // recall channel after compression). `context_abandon` is NOT
    // registered — LLM autonomous tool compression is closed; the
    // deprecated tool implementation stays in the crate for backward
    // compatibility but is never exposed to the LLM.
    tools.extend(build_platform_protected_tools(&agent_home, retrieve_queue));

    // Only register web_search when at least one search provider is configured
    // (checked from the shared provider list). Without providers, the tool
    // always fails with "Provider not configured", wasting LLM inference
    // tokens on doomed calls. The engine reads the vault/list dynamically at
    // search time, so MQTT-driven updates take effect without re-registration.
    let has_search_providers = !search_provider_list
        .read()
        .map(|l| l.is_empty())
        .unwrap_or(true);
    if has_search_providers {
        let search_engine = WebSearchEngine::new(
            search_key_vault,
            search_provider_list,
            Duration::from_millis(tool_http_timeout_ms),
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_notify::McpConfigNotifier;
    use crate::memory::MemorySessionHandle;
    use crate::tools::workspace_resolver::WorkspaceResolver;
    use std::collections::HashMap;

    /// Build a minimal set of dependencies for `all_builtin_tools` testing.
    /// Most dependencies are empty/default so the test focuses on the
    /// platform-tool registration (ADR-061 §10.2: retrieve always,
    /// abandon never).
    ///
    /// `#[allow(clippy::type_complexity)]`: the returned tuple mirrors
    /// `all_builtin_tools`'s assembly dependencies one-to-one; naming a
    /// struct for a single test helper would add indirection without value.
    #[allow(clippy::type_complexity)]
    fn make_test_deps() -> (
        SharedResolver,
        String,                                          // agent_id
        u64,                                             // tool_http_timeout_ms
        search_backends::SharedSearchKeyVault,
        search_backends::SharedSearchProviderList,
        Option<Arc<MemorySessionHandle>>,
        McpNotifyRef,
        String,                                          // agent_home
        Option<String>,                                  // lsp_relay_endpoint
        crate::http::server::SharedMqttClientSlot,
        RetrieveQueue,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let resolver = Arc::new(std::sync::RwLock::new(WorkspaceResolver::new(
            dir.path().to_str().unwrap(),
        )));
        let search_key_vault: search_backends::SharedSearchKeyVault =
            Arc::new(std::sync::RwLock::new(HashMap::new()));
        let search_provider_list: search_backends::SharedSearchProviderList =
            Arc::new(std::sync::RwLock::new(Vec::new()));
        let memory_session = Some(Arc::new(MemorySessionHandle::new(None)));
        let mcp_notifier: McpNotifyRef = Some(Arc::new(McpConfigNotifier::default()));
        let mqtt_slot: crate::http::server::SharedMqttClientSlot =
            Arc::new(tokio::sync::Mutex::new(None));
        let retrieve_queue: RetrieveQueue =
            Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));

        (
            resolver,
            "com.test.agent".to_string(),
            30_000,
            search_key_vault,
            search_provider_list,
            memory_session,
            mcp_notifier,
            "/tmp/test-agent".to_string(),
            None,
            mqtt_slot,
            retrieve_queue,
        )
    }

    /// Collect tool names from a `Vec<Arc<dyn Tool>>`.
    fn tool_names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
        let mut names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
        names.sort();
        names
    }

    #[test]
    fn test_all_builtin_tools_registers_context_retrieve_only() {
        // ADR-061 §10.2: context_retrieve is ALWAYS registered (manual
        // recall channel); context_abandon is NEVER registered (LLM
        // autonomous compression closed). The tool_compression_enabled
        // gate is removed.
        let (
            resolver,
            agent_id,
            timeout,
            search_kv,
            search_pl,
            mem,
            mcp,
            agent_home,
            lsp,
            mqtt,
            retrieve_q,
        ) = make_test_deps();

        let tools = all_builtin_tools(
            &resolver,
            &agent_id,
            timeout,
            search_kv,
            search_pl,
            mem,
            mcp,
            agent_home,
            lsp,
            mqtt,
            retrieve_q,
        );

        let names = tool_names(&tools);
        assert!(
            names.contains(&"context_retrieve".to_string()),
            "context_retrieve should always be registered; got: {:?}",
            names
        );
        assert!(
            !names.contains(&"context_abandon".to_string()),
            "context_abandon must NOT be registered (ADR-061 §10.2); got: {:?}",
            names
        );
    }

    #[test]
    fn test_all_builtin_tools_default_includes_core_tools() {
        // Regression: platform-tool changes only affect the compression
        // tools; all other core tools are always present.
        let (
            resolver,
            agent_id,
            timeout,
            search_kv,
            search_pl,
            mem,
            mcp,
            agent_home,
            lsp,
            mqtt,
            retrieve_q,
        ) = make_test_deps();

        let tools = all_builtin_tools(
            &resolver,
            &agent_id,
            timeout,
            search_kv,
            search_pl,
            mem,
            mcp,
            agent_home,
            lsp,
            mqtt,
            retrieve_q,
        );

        let names = tool_names(&tools);
        // Core tools that must always be present (sanity check)
        for required in [
            "memory_recall",
            "memory_store",
            "http_request",
            "web_fetch",
            "file_read",
            "file_write",
            "file_edit",
            "doc_reader",
            "glob_search",
            "content_search",
            "intent_send",
            "ask_user_question",
            "todo_write",
            "mcp_install",
            "mcp_uninstall",
        ] {
            assert!(
                names.contains(&required.to_string()),
                "Core tool {required} should always be registered; got: {:?}",
                names
            );
        }
    }
}
