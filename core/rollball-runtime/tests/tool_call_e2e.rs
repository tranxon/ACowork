//! End-to-end integration test suite for tool call full pipeline
//!
//! Covers:
//! A. Tool definition serialization — verify ToolSpec serializes with "parameters"
//! B. Tool argument validation — valid/invalid/empty JSON arguments
//! C. Built-in tool execution — direct execute() tests for each tool
//! D. Error recovery — mock provider errors, concurrent tool calls
//! E. convert_tools validation — parameter field handling in OpenAI format

use std::sync::Arc;

use rollball_core::providers::mock::{MockProvider, MockResponse};
use rollball_core::providers::traits::{FunctionCall, ToolCall};
use rollball_core::tools::traits::{Tool, ToolSpec};
use rollball_core::Budget;

use rollball_runtime::agent::context::ContextBuilder;
use rollball_runtime::agent::loop_::AgentLoop;
use rollball_runtime::config::RuntimeConfig;
use rollball_runtime::tools::builtin;
use rollball_runtime::tools::wrappers::{PathGuardedTool, WorkspaceAccess, WorkspaceDir};


use serde_json::Value;

// ── Test helpers ─────────────────────────────────────────────────────────

/// Create a manifest with all permissions and declared tools
fn full_manifest() -> rollball_core::AgentManifest {
    rollball_core::AgentManifest::from_toml(
        r#"
        agent_id = "com.test.e2e"
        version = "1.0.0"
        name = "E2E Test Agent"
        description = "Full tool call e2e test"
        author = "test"
        runtime_version = "0.1.0"

        [[permissions]]
        type = "Shell"

        [[permissions]]
        type = "Network"

        [[permissions]]
        type = "MemoryRead"

        [[permissions]]
        type = "MemoryWrite"

        [[permissions]]
        type = "FilesystemRead"

        [[permissions]]
        type = "FilesystemWrite"

        [[permissions]]
        type = "IntentSend"

        [[permissions]]
        type = "IdentityWrite"

        [[permissions]]
        type = "IdentityRead"

        [[tools]]
        name = "shell"

        [[tools]]
        name = "file_read"

        [[tools]]
        name = "file_write"

        [[tools]]
        name = "file_edit"

        [[tools]]
        name = "glob_search"

        [[tools]]
        name = "content_search"

        [[tools]]
        name = "memory_store"

        [[tools]]
        name = "memory_recall"

        [[tools]]
        name = "http_request"

        [[tools]]
        name = "intent_send"

        [[tools]]
        name = "identity_store"

        [[tools]]
        name = "identity_query"

        [llm]
        provider = "mock"
        model = "mock-model"
    "#,
    )
    .unwrap()
}

fn test_config() -> RuntimeConfig {
    RuntimeConfig::default()
}

fn test_budget() -> Budget {
    Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// A. Tool definition serialization validation
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_tool_definition_parameters_serialization() {
    // Verify all builtin tools' ToolSpec serialize with "parameters" field
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();
    let tools = builtin::all_builtin_tools(&work_dir, "com.test.e2e");

    for tool in &tools {
        let spec = tool.spec();
        let serialized = serde_json::to_value(&spec)
            .unwrap_or_else(|_| panic!("Failed to serialize ToolSpec for '{}'", spec.name));

        // Must have "parameters" key (from #[serde(rename = "parameters")])
        assert!(
            serialized.get("parameters").is_some(),
            "Tool '{}' should have 'parameters' field after serialization",
            spec.name
        );
        // Must NOT have "input_schema" key
        assert!(
            serialized.get("input_schema").is_none(),
            "Tool '{}' should NOT have 'input_schema' field after serialization",
            spec.name
        );
        // "parameters" must be a JSON object
        let params = serialized.get("parameters").unwrap();
        assert!(
            params.is_object(),
            "Tool '{}' parameters should be a JSON object, got: {:?}",
            spec.name,
            params
        );
        // "parameters" should have "type" and "properties"
        assert!(
            params.get("type").is_some(),
            "Tool '{}' parameters should have 'type' field",
            spec.name
        );
        assert!(
            params.get("properties").is_some(),
            "Tool '{}' parameters should have 'properties' field",
            spec.name
        );
    }
}

#[tokio::test]
async fn test_all_builtin_tools_have_unique_names() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();
    let tools = builtin::all_builtin_tools(&work_dir, "com.test.e2e");

    let names: Vec<String> = tools.iter().map(|t| t.name()).collect();
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();

    assert_eq!(
        names.len(),
        unique.len(),
        "All builtin tools must have unique names"
    );
}

#[tokio::test]
async fn test_all_builtin_tools_count() {
    // Design doc: 15 built-in tools (without RAG)
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();
    let tools = builtin::all_builtin_tools(&work_dir, "com.test.e2e");
    assert_eq!(tools.len(), 16, "Should have 16 builtin tools (14 fixed + 2 shell on Windows)");
}

// ═══════════════════════════════════════════════════════════════════════
// B. Tool argument validation
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_tool_call_with_valid_json_arguments() {
    // Use the agent loop with a mock provider to test valid tool call arguments
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Write a test file for the tool to read
    let test_file = tmp.path().join("test.txt");
    std::fs::write(&test_file, "Hello, tool call e2e!").unwrap();

    let provider = Arc::new(MockProvider::tool_call_then_text(
        "file_read",
        r#"{"path": "test.txt"}"#,
        "I read the file successfully.",
    ));

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(builtin::file_read::FileReadTool::new(&work_dir)),
    ];

    let manifest = full_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());

    let result = agent_loop.run("Read the file test.txt", &mut context_builder).await;
    assert!(result.is_ok(), "Valid tool call should succeed: {:?}", result);
}

#[tokio::test]
async fn test_tool_call_with_invalid_json_arguments() {
    // Test that invalid JSON arguments don't crash the runtime.
    // The execute_single_tool function in loop_.rs catches parse errors
    // and falls back to an empty object. We test this via AgentLoop.
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_bad_json".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "file_read".to_string(),
                    arguments: "not valid json {{{".to_string(),
                },
            }],
            content: String::new(),
        },
        // file_read with empty object will fail because path is empty,
        // then the LLM gets the error and responds with text
        MockResponse::Text {
            content: "I see the file read failed due to invalid parameters.".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(builtin::file_read::FileReadTool::new(&work_dir)),
    ];

    let manifest = full_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());

    let result = agent_loop.run("Read a file", &mut context_builder).await;
    // Should NOT panic — the invalid JSON is caught and produces a graceful error
    assert!(result.is_ok(), "Invalid JSON arguments should not crash: {:?}", result);
}

#[tokio::test]
async fn test_tool_call_with_empty_arguments() {
    // Test that empty string arguments fall back to empty object
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_empty".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "file_read".to_string(),
                    arguments: String::new(), // empty string
                },
            }],
            content: String::new(),
        },
        MockResponse::Text {
            content: "The tool call had no arguments, so it returned an error.".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(builtin::file_read::FileReadTool::new(&work_dir)),
    ];

    let manifest = full_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());

    let result = agent_loop.run("Read a file with no args", &mut context_builder).await;
    assert!(result.is_ok(), "Empty arguments should not crash: {:?}", result);
}

// ═══════════════════════════════════════════════════════════════════════
// C. Built-in tool end-to-end tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_file_read_valid_path() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Write a test file
    let test_file = tmp.path().join("hello.txt");
    std::fs::write(&test_file, "Hello, world!\nSecond line.").unwrap();

    let tool = builtin::file_read::FileReadTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "path": "hello.txt" }))
        .await
        .unwrap();

    assert!(result.ok, "file_read should succeed: {:?}", result.error);
    assert!(result.content.contains("Hello, world!"));
}

#[tokio::test]
async fn test_file_read_with_line_range() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let test_file = tmp.path().join("lines.txt");
    std::fs::write(&test_file, "line1\nline2\nline3\nline4\nline5").unwrap();

    let tool = builtin::file_read::FileReadTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "path": "lines.txt", "start_line": 2, "end_line": 3 }))
        .await
        .unwrap();

    assert!(result.ok);
    assert!(result.content.contains("line2"));
    assert!(result.content.contains("line3"));
    assert!(!result.content.contains("line1"));
    assert!(!result.content.contains("line4"));
}

#[tokio::test]
async fn test_file_read_nonexistent_path() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let tool = builtin::file_read::FileReadTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "path": "nonexistent.txt" }))
        .await
        .unwrap();

    assert!(!result.ok, "file_read of nonexistent file should fail");
    assert!(result.error.is_some());
    assert!(result.error.unwrap().contains("Failed to read file"));
}

#[tokio::test]
async fn test_file_read_empty_path_parameter() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let tool = builtin::file_read::FileReadTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "path": "" }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("Missing 'path'"));
}

#[tokio::test]
async fn test_file_read_path_traversal_blocked() {
    // Test that PathGuardedTool blocks path traversal attacks
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let inner = Arc::new(builtin::file_read::FileReadTool::new(&work_dir));
    let guarded = PathGuardedTool::new(inner, vec![WorkspaceDir {
        path: work_dir.clone(),
        access: WorkspaceAccess::ReadWrite,
    }]);

    // Attempt to read a file outside the workspace using ".."
    let result = guarded
        .execute(serde_json::json!({ "path": "../../etc/passwd" }))
        .await
        .unwrap();

    assert!(!result.ok, "Path traversal should be blocked");
    assert!(
        result.error.unwrap().contains("outside all allowed workspace directories"),
        "Error should mention path is outside workspace"
    );
}

#[tokio::test]
async fn test_file_write_and_read_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let write_tool = builtin::file_write::FileWriteTool::new(&work_dir);
    let read_tool = builtin::file_read::FileReadTool::new(&work_dir);

    // Write a file
    let write_result = write_tool
        .execute(serde_json::json!({
            "path": "roundtrip.txt",
            "content": "Round-trip test content!"
        }))
        .await
        .unwrap();

    assert!(write_result.ok, "file_write should succeed: {:?}", write_result.error);
    assert!(write_result.content.contains("roundtrip.txt"));

    // Read it back
    let read_result = read_tool
        .execute(serde_json::json!({ "path": "roundtrip.txt" }))
        .await
        .unwrap();

    assert!(read_result.ok, "file_read should succeed: {:?}", read_result.error);
    assert_eq!(read_result.content, "Round-trip test content!");
}

#[tokio::test]
async fn test_file_write_creates_subdirectory() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let write_tool = builtin::file_write::FileWriteTool::new(&work_dir);
    let result = write_tool
        .execute(serde_json::json!({
            "path": "sub/dir/deep.txt",
            "content": "Deep file content"
        }))
        .await
        .unwrap();

    assert!(result.ok, "file_write should create parent directories");
    assert!(tmp.path().join("sub/dir/deep.txt").exists());
}

#[tokio::test]
async fn test_file_write_empty_path() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let tool = builtin::file_write::FileWriteTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "path": "", "content": "data" }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("Missing 'path'"));
}

#[tokio::test]
async fn test_file_edit_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Create initial file
    let test_file = tmp.path().join("edit_me.txt");
    std::fs::write(&test_file, "Hello, world!").unwrap();

    let tool = builtin::file_edit::FileEditTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({
            "path": "edit_me.txt",
            "old_text": "world",
            "new_text": "RollBall"
        }))
        .await
        .unwrap();

    assert!(result.ok, "file_edit should succeed: {:?}", result.error);

    // Verify the content was actually changed
    let content = std::fs::read_to_string(&test_file).unwrap();
    assert_eq!(content, "Hello, RollBall!");
}

#[tokio::test]
async fn test_file_edit_old_text_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let test_file = tmp.path().join("edit_nf.txt");
    std::fs::write(&test_file, "Hello, world!").unwrap();

    let tool = builtin::file_edit::FileEditTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({
            "path": "edit_nf.txt",
            "old_text": "nonexistent",
            "new_text": "replacement"
        }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("old_text not found"));
}

#[tokio::test]
async fn test_file_edit_ambiguous_match() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let test_file = tmp.path().join("edit_amb.txt");
    std::fs::write(&test_file, "abc abc abc").unwrap();

    let tool = builtin::file_edit::FileEditTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({
            "path": "edit_amb.txt",
            "old_text": "abc",
            "new_text": "xyz"
        }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("found 3 times"));
}

#[tokio::test]
async fn test_file_edit_missing_required_params() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let tool = builtin::file_edit::FileEditTool::new(&work_dir);

    // Missing old_text
    let result = tool
        .execute(serde_json::json!({ "path": "any.txt", "new_text": "x" }))
        .await
        .unwrap();
    assert!(!result.ok);

    // Empty path
    let result = tool
        .execute(serde_json::json!({ "path": "", "old_text": "x", "new_text": "y" }))
        .await
        .unwrap();
    assert!(!result.ok);
}

#[tokio::test]
async fn test_file_edit_crlf_compatible() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Create a file with CRLF line endings
    let test_file = tmp.path().join("crlf_file.txt");
    std::fs::write(&test_file, "line1\r\nHello, world!\r\nline3").unwrap();

    let tool = builtin::file_edit::FileEditTool::new(&work_dir);
    // old_text uses LF (as LLM would generate), file has CRLF
    let result = tool
        .execute(serde_json::json!({
            "path": "crlf_file.txt",
            "old_text": "Hello, world!",
            "new_text": "Hello, RollBall!"
        }))
        .await
        .unwrap();

    assert!(result.ok, "file_edit should succeed on CRLF file: {:?}", result.error);

    // Verify the replacement was successful
    let content = std::fs::read_to_string(&test_file).unwrap();
    assert!(content.contains("Hello, RollBall!"), "Content should be replaced");

    // Verify line endings are preserved as CRLF
    assert!(
        content.contains("\r\n"),
        "CRLF line endings should be preserved, got: {:?}",
        content
    );
    assert!(
        !content.contains("\r\n\r\n"),
        "Should not have double CRLF, got: {:?}",
        content
    );
    // Ensure no bare LF without CR
    let normalized = content.replace("\r\n", "\n");
    assert!(
        !normalized.contains("\r"),
        "Should not have stray CR characters, got: {:?}",
        content
    );
}

#[tokio::test]
async fn test_glob_search_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Create some test files
    std::fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(tmp.path().join("lib.rs"), "pub fn lib() {}").unwrap();
    std::fs::write(tmp.path().join("readme.md"), "# Hello").unwrap();

    let tool = builtin::glob_search::GlobSearchTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "pattern": "*.rs" }))
        .await
        .unwrap();

    assert!(result.ok);
    assert!(result.content.contains("main.rs"));
    assert!(result.content.contains("lib.rs"));
    assert!(!result.content.contains("readme.md"));
}

#[tokio::test]
async fn test_glob_search_recursive_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Create nested structure
    let sub = tmp.path().join("src");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("app.rs"), "mod app;").unwrap();
    std::fs::write(tmp.path().join("root.rs"), "mod root;").unwrap();

    let tool = builtin::glob_search::GlobSearchTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "pattern": "**/*.rs" }))
        .await
        .unwrap();

    assert!(result.ok);
    assert!(result.content.contains("app.rs"));
    assert!(result.content.contains("root.rs"));
}

#[tokio::test]
async fn test_glob_search_no_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let tool = builtin::glob_search::GlobSearchTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "pattern": "*.xyz" }))
        .await
        .unwrap();

    assert!(result.ok);
    assert!(result.content.contains("No files matched"));
}

#[tokio::test]
async fn test_glob_search_empty_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let tool = builtin::glob_search::GlobSearchTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "pattern": "" }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("Missing 'pattern'"));
}

#[tokio::test]
async fn test_glob_search_windows_backslash_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Create nested structure
    let sub = tmp.path().join("src");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(sub.join("lib.rs"), "pub fn lib() {}").unwrap();
    std::fs::write(tmp.path().join("readme.md"), "# Hello").unwrap();

    let tool = builtin::glob_search::GlobSearchTool::new(&work_dir);
    // Use Windows-style backslash separator in the pattern
    let result = tool
        .execute(serde_json::json!({ "pattern": "src\\*.rs" }))
        .await
        .unwrap();

    assert!(result.ok, "glob_search with backslash pattern should succeed: {:?}", result.error);
    assert!(result.content.contains("src/main.rs"), "Output should use forward slashes: {}", result.content);
    assert!(result.content.contains("src/lib.rs"), "Output should use forward slashes: {}", result.content);
    assert!(!result.content.contains("readme.md"));
}

#[tokio::test]
async fn test_content_search_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Create files with searchable content
    std::fs::write(tmp.path().join("hello.txt"), "Hello, RollBall AI!").unwrap();
    std::fs::write(tmp.path().join("bye.txt"), "Goodbye, world!").unwrap();

    let tool = builtin::content_search::ContentSearchTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "pattern": "RollBall" }))
        .await
        .unwrap();

    assert!(result.ok);
    assert!(result.content.contains("hello.txt"));
    assert!(result.content.contains("RollBall"));
}

#[tokio::test]
async fn test_content_search_no_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    std::fs::write(tmp.path().join("data.txt"), "Nothing interesting here").unwrap();

    let tool = builtin::content_search::ContentSearchTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "pattern": "nonexistent_pattern_12345" }))
        .await
        .unwrap();

    assert!(result.ok);
    assert!(result.content.contains("No matches found"));
}

#[tokio::test]
async fn test_content_search_invalid_regex() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let tool = builtin::content_search::ContentSearchTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "pattern": "[invalid" }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("Invalid regex"));
}

#[tokio::test]
async fn test_content_search_empty_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let tool = builtin::content_search::ContentSearchTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "pattern": "" }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("Missing 'pattern'"));
}

#[tokio::test]
async fn test_content_search_path_format() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Create nested structure to test path formatting
    let sub = tmp.path().join("src");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("main.rs"), "fn main() { RollBall; }").unwrap();

    let tool = builtin::content_search::ContentSearchTool::new(&work_dir);
    let result = tool
        .execute(serde_json::json!({ "pattern": "RollBall" }))
        .await
        .unwrap();

    assert!(result.ok, "content_search should succeed: {:?}", result.error);
    assert!(
        result.content.contains("src/main.rs"),
        "Output path should use forward slashes, got: {}",
        result.content
    );
    assert!(
        !result.content.contains("\\"),
        "Output should not contain backslashes, got: {}",
        result.content
    );
}

#[tokio::test]
async fn test_shell_command_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let info = rollball_runtime::platform::detected_shell();
    let tool = builtin::shell::ShellTool::new(
        "test-shell", info.display_name, info.binary, info.binary, info.arg, &work_dir,
    );
    let result = tool
        .execute(serde_json::json!({ "command": "echo hello_world" }))
        .await
        .unwrap();

    assert!(result.ok, "shell echo should succeed: {:?}", result.error);
    assert!(result.content.contains("hello_world"));
}

#[tokio::test]
async fn test_shell_command_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let info = rollball_runtime::platform::detected_shell();
    let tool = builtin::shell::ShellTool::new(
        "test-shell", info.display_name, info.binary, info.binary, info.arg, &work_dir,
    );
    let result = tool
        .execute(serde_json::json!({ "command": "" }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("Missing 'command'"));
}

#[tokio::test]
async fn test_shell_command_missing_param() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let info = rollball_runtime::platform::detected_shell();
    let tool = builtin::shell::ShellTool::new(
        "test-shell", info.display_name, info.binary, info.binary, info.arg, &work_dir,
    );
    let result = tool
        .execute(serde_json::json!({}))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("Missing 'command'"));
}

#[tokio::test]
async fn test_memory_store_and_recall() {
    let store_tool = builtin::memory_store::MemoryStoreTool::new("com.test.e2e");
    let recall_tool = builtin::memory_recall::MemoryRecallTool::new("com.test.e2e");

    // Store a memory
    let store_result = store_tool
        .execute(serde_json::json!({
            "key": "favorite_lang",
            "content": "Rust",
            "category": "core"
        }))
        .await
        .unwrap();

    assert!(store_result.ok, "memory_store should succeed: {:?}", store_result.error);
    assert!(store_result.content.contains("favorite_lang"));
    assert!(store_result.content.contains("Rust"));
    assert!(store_result.content.contains("core"));

    // Recall the memory (Phase 1: returns placeholder, but should not error)
    let recall_result = recall_tool
        .execute(serde_json::json!({ "query": "favorite_lang" }))
        .await
        .unwrap();

    assert!(recall_result.ok, "memory_recall should succeed: {:?}", recall_result.error);
    assert!(recall_result.content.contains("favorite_lang"));
}

#[tokio::test]
async fn test_memory_store_default_category() {
    let tool = builtin::memory_store::MemoryStoreTool::new("com.test.e2e");
    let result = tool
        .execute(serde_json::json!({
            "key": "test_key",
            "content": "test_value"
        }))
        .await
        .unwrap();

    assert!(result.ok);
    assert!(result.content.contains("core"), "Default category should be 'core'");
}

#[tokio::test]
async fn test_memory_store_missing_key() {
    let tool = builtin::memory_store::MemoryStoreTool::new("com.test.e2e");
    let result = tool
        .execute(serde_json::json!({ "content": "test" }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("Missing required parameter 'key'"));
}

#[tokio::test]
async fn test_memory_recall_no_filters() {
    let tool = builtin::memory_recall::MemoryRecallTool::new("com.test.e2e");
    let result = tool
        .execute(serde_json::json!({}))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("at least"));
}

#[tokio::test]
async fn test_intent_send_valid() {
    let tool = builtin::intent_send::IntentSendTool::new();
    let result = tool
        .execute(serde_json::json!({
            "target": "com.example.calendar",
            "action": "schedule",
            "params": { "time": "10:00" }
        }))
        .await
        .unwrap();

    assert!(result.ok);
    assert!(result.content.contains("com.example.calendar"));
    assert!(result.content.contains("schedule"));
}

#[tokio::test]
async fn test_intent_send_invalid_target() {
    let tool = builtin::intent_send::IntentSendTool::new();
    let result = tool
        .execute(serde_json::json!({
            "target": "not-a-domain",
            "action": "ping"
        }))
        .await
        .unwrap();

    assert!(!result.ok);
    assert!(result.error.unwrap().contains("reverse-domain"));
}

#[tokio::test]
async fn test_identity_store_and_query() {
    let store_tool = builtin::identity_store::IdentityStoreTool::new("com.rollball.system");
    let query_tool = builtin::identity_query::IdentityQueryTool::new("com.test.e2e");

    // Store identity
    let store_result = store_tool
        .execute(serde_json::json!({
            "field": "language",
            "value": "zh-CN",
            "confidence": 0.9,
            "source": "user_input"
        }))
        .await
        .unwrap();

    assert!(store_result.ok);
    assert!(store_result.content.contains("language"));
    assert!(store_result.content.contains("zh-CN"));

    // Query identity
    let query_result = query_tool
        .execute(serde_json::json!({
            "fields": ["language"]
        }))
        .await
        .unwrap();

    assert!(query_result.ok);
    assert!(query_result.content.contains("language"));
}

// ═══════════════════════════════════════════════════════════════════════
// D. Error recovery tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_provider_error_does_not_crash_runtime() {
    // Simulate a provider error — the runtime should return an error, not panic
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Error {
            message: "API rate limit exceeded".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![];
    let manifest = full_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());

    let result = agent_loop.run("Hello", &mut context_builder).await;
    // Should return an error, not panic
    assert!(result.is_err(), "Provider error should propagate as Err");
}

#[tokio::test]
async fn test_multiple_concurrent_tool_calls() {
    // Test parallel tool execution with multiple tool calls in one response
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    // Create test files
    std::fs::write(tmp.path().join("a.txt"), "Content A").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "Content B").unwrap();

    let provider = Arc::new(MockProvider::new(vec![
        // First response: two tool calls simultaneously
        MockResponse::ToolCalls {
            tool_calls: vec![
                ToolCall {
                    id: "call_a".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "file_read".to_string(),
                        arguments: r#"{"path": "a.txt"}"#.to_string(),
                    },
                },
                ToolCall {
                    id: "call_b".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "file_read".to_string(),
                        arguments: r#"{"path": "b.txt"}"#.to_string(),
                    },
                },
            ],
            content: String::new(),
        },
        // Second response: text summary
        MockResponse::Text {
            content: "I read both files: A and B.".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(builtin::file_read::FileReadTool::new(&work_dir)),
    ];

    let manifest = full_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());

    let result = agent_loop.run("Read both files", &mut context_builder).await;
    assert!(result.is_ok(), "Concurrent tool calls should succeed: {:?}", result);

    // Verify history has the tool call messages
    let history = agent_loop.history();
    let messages = history.messages();
    // user + assistant(tool_calls) + 2x tool + assistant(text) = at least 5
    assert!(
        messages.len() >= 5,
        "History should have at least 5 messages for concurrent tool calls, got {}",
        messages.len()
    );
}

#[tokio::test]
async fn test_unknown_tool_returns_error_not_panic() {
    // Test that calling an unknown tool returns an error message, not a panic
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls {
            tool_calls: vec![ToolCall {
                id: "call_unknown".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "nonexistent_tool".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
            content: String::new(),
        },
        MockResponse::Text {
            content: "The tool was not found.".to_string(),
        },
    ]));

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(builtin::file_read::FileReadTool::new(&work_dir)),
    ];

    let manifest = full_manifest();
    let config = test_config();
    let budget = test_budget();

    let (mut agent_loop, _) = AgentLoop::new(config, manifest, provider, tools, budget, None, None);
    let mut context_builder = ContextBuilder::new("You are a test assistant.".to_string());

    let result = agent_loop.run("Use a nonexistent tool", &mut context_builder).await;
    assert!(result.is_ok(), "Unknown tool should not crash: {:?}", result);
}

// ═══════════════════════════════════════════════════════════════════════
// E. convert_tools validation tests
// ═══════════════════════════════════════════════════════════════════════
//
// convert_tools is a private function in openai.rs. We test the equivalent
// behavior by directly constructing the input/output format and verifying
// the ToolSpec → JSON → NativeToolSpec chain works correctly.

/// Replicate the convert_tools logic for testing purposes.
/// This mirrors the behavior in rollball_runtime::providers::openai::convert_tools
fn convert_tools_for_test(tools: Option<&[Value]>) -> Option<Vec<serde_json::Value>> {
    tools.map(|items| {
        items
            .iter()
            .map(|tool| {
                let name = tool["name"].as_str().unwrap_or("unknown").to_string();
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let parameters = match tool.get("parameters") {
                    Some(p) if p.is_object() => p.clone(),
                    Some(_) => serde_json::json!({"type": "object", "properties": {}}),
                    None => serde_json::json!({"type": "object", "properties": {}}),
                };

                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    }
                })
            })
            .collect()
    })
}

#[test]
fn test_convert_tools_with_valid_parameters() {
    let tool_json = serde_json::json!({
        "name": "shell",
        "description": "Execute shell commands",
        "parameters": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                }
            },
            "required": ["command"]
        }
    });

    let native = convert_tools_for_test(Some(&[tool_json])).unwrap();

    assert_eq!(native.len(), 1);
    let func = &native[0]["function"];
    assert_eq!(func["name"], "shell");
    assert_eq!(func["description"], "Execute shell commands");

    let params = &func["parameters"];
    assert!(params.get("properties").is_some());
    assert!(params["properties"].get("command").is_some());
}

#[test]
fn test_convert_tools_with_missing_parameters() {
    // Tool JSON without "parameters" field — should fallback to default schema
    let tool_json = serde_json::json!({
        "name": "no_params_tool",
        "description": "A tool without parameters"
    });

    let native = convert_tools_for_test(Some(&[tool_json])).unwrap();

    assert_eq!(native.len(), 1);
    let func = &native[0]["function"];
    assert_eq!(func["name"], "no_params_tool");

    // Should fallback to empty object schema
    let params = &func["parameters"];
    assert_eq!(
        *params,
        serde_json::json!({"type": "object", "properties": {}})
    );
}

#[test]
fn test_convert_tools_with_non_object_parameters() {
    // Tool JSON with parameters as a string instead of object — should fallback
    let tool_json = serde_json::json!({
        "name": "bad_params_tool",
        "description": "Tool with bad params",
        "parameters": "not an object"
    });

    let native = convert_tools_for_test(Some(&[tool_json])).unwrap();

    let params = &native[0]["function"]["parameters"];
    assert_eq!(
        *params,
        serde_json::json!({"type": "object", "properties": {}})
    );
}

#[test]
fn test_convert_tools_with_none_input() {
    // No tools provided — should return None
    let result = convert_tools_for_test(None);
    assert!(result.is_none());
}

#[test]
fn test_convert_tools_with_empty_array() {
    // Empty tools array — should return Some(empty)
    let result = convert_tools_for_test(Some(&[]));
    assert!(result.is_some());
    assert!(result.unwrap().is_empty());
}

#[test]
fn test_convert_tools_preserves_all_builtin_tools() {
    // Verify that all builtin tools can be serialized and converted
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();
    let tools = builtin::all_builtin_tools(&work_dir, "com.test.e2e");

    // Serialize each tool's spec
    let tool_jsons: Vec<Value> = tools
        .iter()
        .map(|t| serde_json::to_value(t.spec()).unwrap())
        .collect();

    // Convert through the pipeline
    let native = convert_tools_for_test(Some(&tool_jsons)).unwrap();

    assert_eq!(native.len(), tools.len());

    // Verify each converted tool has the expected structure
    for (i, nt) in native.iter().enumerate() {
        assert_eq!(nt["type"], "function", "Tool {} should have type='function'", i);
        assert!(
            nt["function"]["name"].is_string(),
            "Tool {} function.name should be a string",
            i
        );
        assert!(
            nt["function"]["parameters"].is_object(),
            "Tool {} function.parameters should be an object",
            i
        );
    }
}

#[test]
fn test_toolspec_serialization_roundtrip() {
    // Verify ToolSpec can be serialized and deserialized without data loss
    let spec = ToolSpec {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "input": { "type": "string" }
            },
            "required": ["input"]
        }),
    };

    let json = serde_json::to_value(&spec).unwrap();

    // Verify "parameters" field name (from serde rename)
    assert!(json.get("parameters").is_some());
    assert!(json.get("input_schema").is_none());

    // Deserialize back
    let deserialized: ToolSpec = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.name, "test_tool");
    assert_eq!(deserialized.description, "A test tool");
    assert!(deserialized.input_schema.get("properties").is_some());
}
