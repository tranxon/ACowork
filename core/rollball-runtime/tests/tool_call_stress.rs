//! Stress test suite for tool call execution
//!
//! Covers:
//! 1. Rapid sequential tool calls (write + read)
//! 2. Concurrent file operations (write + read)
//! 3. Alternating success and failure
//! 4. Glob search with many files
//! 5. Concurrent content search
//! 6. ToolSpec serialization roundtrip stress

use std::sync::Arc;

use futures::future::join_all;
use rollball_core::tools::traits::{Tool, ToolSpec};
use rollball_runtime::tools::builtin;
use rollball_runtime::tools::workspace_resolver::WorkspaceResolver;

// ── Test 1: Rapid sequential tool calls ─────────────────────────────────

#[tokio::test]
async fn stress_test_rapid_tool_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let write_tool = builtin::file_write::FileWriteTool::new(&work_dir);
    let read_tool = builtin::file_read::FileReadTool::new(&work_dir);

    let count = 50;

    // Phase 1: rapid sequential writes
    for i in 0..count {
        let result = write_tool
            .execute(serde_json::json!({
                "path": format!("file_{}.txt", i),
                "content": format!("Content for file {}", i)
            }))
            .await
            .unwrap();

        assert!(result.ok, "Write #{} should succeed: {:?}", i, result.error);
    }

    // Phase 2: rapid sequential reads
    for i in 0..count {
        let result = read_tool
            .execute(serde_json::json!({ "path": format!("file_{}.txt", i) }))
            .await
            .unwrap();

        assert!(result.ok, "Read #{} should succeed: {:?}", i, result.error);
        assert_eq!(
            result.content,
            format!("Content for file {}", i),
            "Read #{} content mismatch",
            i
        );
    }
}

// ── Test 2: Concurrent file operations ──────────────────────────────────

#[tokio::test]
async fn stress_test_concurrent_file_operations() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let write_tool = Arc::new(builtin::file_write::FileWriteTool::new(&work_dir));
    let read_tool = Arc::new(builtin::file_read::FileReadTool::new(&work_dir));

    let concurrency = 10;

    // Phase 1: concurrent writes
    let write_futures: Vec<_> = (0..concurrency)
        .map(|i| {
            let tool = Arc::clone(&write_tool);
            let path = format!("concurrent_{}.txt", i);
            let content = format!("Concurrent content {}", i);
            async move {
                tool
                    .execute(serde_json::json!({ "path": path, "content": content }))
                    .await
                    .unwrap()
            }
        })
        .collect();

    let write_results = join_all(write_futures).await;
    for (i, result) in write_results.iter().enumerate() {
        assert!(result.ok, "Concurrent write #{} failed: {:?}", i, result.error);
    }

    // Phase 2: concurrent reads
    let read_futures: Vec<_> = (0..concurrency)
        .map(|i| {
            let tool = Arc::clone(&read_tool);
            let path = format!("concurrent_{}.txt", i);
            async move {
                tool
                    .execute(serde_json::json!({ "path": path }))
                    .await
                    .unwrap()
            }
        })
        .collect();

    let read_results = join_all(read_futures).await;
    for (i, result) in read_results.iter().enumerate() {
        assert!(result.ok, "Concurrent read #{} failed: {:?}", i, result.error);
        assert_eq!(
            result.content,
            format!("Concurrent content {}", i),
            "Concurrent read #{} content mismatch",
            i
        );
    }
}

// ── Test 3: Alternating success and failure ─────────────────────────────

#[tokio::test]
async fn stress_test_alternating_success_and_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let write_tool = builtin::file_write::FileWriteTool::new(&work_dir);
    let read_tool = builtin::file_read::FileReadTool::new(&work_dir);

    let iterations = 20;

    // Pre-create files that should succeed
    for i in 0..iterations {
        let result = write_tool
            .execute(serde_json::json!({
                "path": format!("existing_{}.txt", i),
                "content": format!("Valid content {}", i)
            }))
            .await
            .unwrap();
        assert!(result.ok, "Setup write #{} failed", i);
    }

    // Alternating success / failure
    for i in 0..iterations {
        // Success: read existing file
        let success_result = read_tool
            .execute(serde_json::json!({ "path": format!("existing_{}.txt", i) }))
            .await
            .unwrap();

        assert!(
            success_result.ok,
            "Success read #{} should succeed: {:?}",
            i,
            success_result.error
        );
        assert_eq!(
            success_result.content,
            format!("Valid content {}", i),
            "Success read #{} content mismatch",
            i
        );

        // Failure: read nonexistent file
        let failure_result = read_tool
            .execute(serde_json::json!({ "path": format!("nonexistent_{}.txt", i) }))
            .await
            .unwrap();

        assert!(
            !failure_result.ok,
            "Failure read #{} should fail",
            i
        );
        assert!(
            failure_result.error.is_some(),
            "Failure read #{} should have error",
            i
        );
    }
}

// ── Test 4: Glob search with many files ─────────────────────────────────

#[tokio::test]
async fn stress_test_glob_search_many_files() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let write_tool = builtin::file_write::FileWriteTool::new(&work_dir);
    let glob_tool = builtin::glob_search::GlobSearchTool::new(&WorkspaceResolver::new(&work_dir));

    let file_count = 100;

    // Create 100 files
    for i in 0..file_count {
        let result = write_tool
            .execute(serde_json::json!({
                "path": format!("file_{:03}.txt", i),
                "content": format!("File content {}", i)
            }))
            .await
            .unwrap();
        assert!(result.ok, "Setup write #{} failed", i);
    }

    // Also create some non-txt files to ensure filtering works
    for i in 0..10 {
        let result = write_tool
            .execute(serde_json::json!({
                "path": format!("other_{}.rs", i),
                "content": "fn main() {}"
            }))
            .await
            .unwrap();
        assert!(result.ok, "Setup non-txt write #{} failed", i);
    }

    // Search for *.txt
    let result = glob_tool
        .execute(serde_json::json!({ "pattern": "*.txt" }))
        .await
        .unwrap();

    assert!(result.ok, "glob_search should succeed: {:?}", result.error);

    // Count how many txt files are listed in the result
    let mut found_count = 0;
    for i in 0..file_count {
        if result.content.contains(&format!("file_{:03}.txt", i)) {
            found_count += 1;
        }
    }

    assert_eq!(
        found_count, file_count,
        "Should find all {} txt files, found {}",
        file_count, found_count
    );
}

// ── Test 5: Concurrent content search ───────────────────────────────────

#[tokio::test]
async fn stress_test_concurrent_content_search() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let write_tool = builtin::file_write::FileWriteTool::new(&work_dir);
    let search_tool = Arc::new(builtin::content_search::ContentSearchTool::new(&WorkspaceResolver::new(&work_dir)));

    let file_count = 20;

    // Create 20 files with different searchable content
    for i in 0..file_count {
        let result = write_tool
            .execute(serde_json::json!({
                "path": format!("searchable_{}.txt", i),
                "content": format!("Unique keyword for file number {}", i)
            }))
            .await
            .unwrap();
        assert!(result.ok, "Setup write #{} failed", i);
    }

    // Concurrent searches for different keywords
    let concurrency = 10;
    let search_futures: Vec<_> = (0..concurrency)
        .map(|i| {
            let tool = Arc::clone(&search_tool);
            let pattern = format!("file number {}", i);
            async move {
                tool
                    .execute(serde_json::json!({ "pattern": pattern }))
                    .await
                    .unwrap()
            }
        })
        .collect();

    let search_results = join_all(search_futures).await;
    for (i, result) in search_results.iter().enumerate() {
        assert!(
            result.ok,
            "Concurrent search #{} failed: {:?}",
            i,
            result.error
        );
        // Each search should find at least one match
        assert!(
            result.content.contains(&format!("searchable_{}.txt", i)),
            "Search #{} should find file containing keyword",
            i
        );
    }
}

// ── Test 6: ToolSpec serialization roundtrip stress ─────────────────────

#[tokio::test]
async fn stress_test_tool_spec_serialization_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = tmp.path().to_string_lossy().to_string();

    let resolver = WorkspaceResolver::new(&work_dir);
    let tools = builtin::all_builtin_tools(&resolver, "com.test.stress");
    let rounds = 1000;

    for tool in &tools {
        let spec = tool.spec();

        for round in 0..rounds {
            // Serialize
            let json = serde_json::to_value(&spec)
                .unwrap_or_else(|_| panic!("Serialize failed for '{}' round {}", spec.name, round));

            // Deserialize
            let deserialized: ToolSpec = serde_json::from_value(json)
                .unwrap_or_else(|_| panic!("Deserialize failed for '{}' round {}", spec.name, round));

            // Verify fields match
            assert_eq!(
                deserialized.name, spec.name,
                "Name mismatch for '{}' round {}",
                spec.name, round
            );
            assert_eq!(
                deserialized.description, spec.description,
                "Description mismatch for '{}' round {}",
                spec.name, round
            );

            // Verify serialized JSON has "parameters" not "input_schema"
            let json_str = serde_json::to_string(&spec).unwrap();
            assert!(
                json_str.contains("\"parameters\""),
                "JSON should contain 'parameters' for '{}' round {}",
                spec.name, round
            );
            assert!(
                !json_str.contains("\"input_schema\""),
                "JSON should NOT contain 'input_schema' for '{}' round {}",
                spec.name, round
            );
        }
    }
}
