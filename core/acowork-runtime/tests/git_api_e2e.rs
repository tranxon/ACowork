//! End-to-end tests: ADR-078 — workspace Git status/diff/log HTTP API.
//!
//! Spins up a real `RuntimeHttpServer` (random loopback port) with a real
//! `RuntimeGitQueryService` backed by a real git repository created in the
//! test's temp dir, then exercises the three `/git/*` handlers exactly the
//! way the Desktop `gitStore` calls them (via the Gateway reverse proxy).
//! Covers the wire contract end-to-end: JSON shape, status codes,
//! workspace_id semantics, path traversal defence, rename/deleted/binary
//! diff variants, and log path/limit filtering.
//!
//! Mirrors the `prompts_api_e2e.rs` minimal-server pattern: one
//! `#[tokio::test]` per scenario, every test owns its own temp dir +
//! repo so tests can run in parallel.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

const AGENT_ID: &str = "com.test.git-e2e";

/// Test-only instance identity (ADR-073: must be a UUIDv4).
const INSTANCE_ID: &str = "0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d";

/// Ws id registered in `config/agent_workspaces.json` (workspace-subdir test).
const SUB_WS_ID: &str = "ws-sub";

// ── git helpers ──────────────────────────────────────────────────────────

/// Run a git command inside `dir`, panicking on failure (test setup).
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git should spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `git init -b main` + local identity + initial commit (`a.txt`).
fn init_repo(dir: &Path) {
    git_in(dir, &["init", "-b", "main"]);
    git_in(dir, &["config", "user.email", "e2e@test.local"]);
    git_in(dir, &["config", "user.name", "e2e"]);
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    git_in(dir, &["add", "a.txt"]);
    git_in(dir, &["commit", "-m", "init"]);
}

/// Write `config/agent_workspaces.json` registering `SUB_WS_ID` → `path`.
fn register_workspace(work_dir: &Path, ws_id: &str, path: &Path) {
    let cfg_dir = work_dir.join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg = format!(
        r#"{{"additional_dirs":[{{"id":"{ws_id}","path":"{}"}}]}}"#,
        path.display()
    );
    std::fs::write(cfg_dir.join("agent_workspaces.json"), cfg).unwrap();
}

// ── server + HTTP helpers ────────────────────────────────────────────────

async fn spawn_server(tag: &str) -> (u16, std::path::PathBuf) {
    let temp_dir = std::env::temp_dir().join(format!(
        "acowork-test-git-e2e-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Minimal stub slots — see `prompts_api_e2e.rs` for why each is None;
    // the /git/* routes only touch `git_query`.
    let snapshots = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let latest = Arc::new(std::sync::RwLock::new(None));
    let dispatch_tx = Arc::new(tokio::sync::Mutex::new(None));
    let embed_dim = Arc::new(std::sync::RwLock::new(0));
    let degraded_reasons = Arc::new(std::sync::RwLock::new(Vec::new()));
    let mqtt_client = Arc::new(tokio::sync::Mutex::new(None));
    let session_metadata = Arc::new(tokio::sync::Mutex::new(None));
    let memory_query = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_query = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_mutation = Arc::new(tokio::sync::Mutex::new(None));
    // REAL git service: work_dir = temp_dir, so `/git/*` without a
    // workspace_id resolves to the repo we create inside temp_dir.
    let git_query: Arc<
        tokio::sync::Mutex<Option<Arc<dyn acowork_runtime::usecases::GitQueryService>>>,
    > = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        acowork_runtime::usecases::git_query_impl::RuntimeGitQueryService::new(
            temp_dir.clone(),
            AGENT_ID.to_string(),
        ),
    ))));
    let agent_tools = Arc::new(tokio::sync::Mutex::new(None));
    let agent_config = Arc::new(tokio::sync::Mutex::new(None));
    let attachment = Arc::new(tokio::sync::Mutex::new(None));
    let session_config = Arc::new(tokio::sync::Mutex::new(None));
    let consolidation_timer: Arc<
        std::sync::RwLock<Option<Arc<acowork_runtime::memory::ConsolidationTimer>>>,
    > = Arc::new(std::sync::RwLock::new(None));
    let rag_provider: Arc<std::sync::RwLock<Option<Arc<dyn acowork_core::rag::RagProvider>>>> =
        Arc::new(std::sync::RwLock::new(None));
    let debug_service = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_resolver: Arc<
        std::sync::RwLock<acowork_runtime::tools::workspace_resolver::WorkspaceResolver>,
    > = Arc::new(std::sync::RwLock::new(
        acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
    ));
    let session_manager_slot: Arc<
        tokio::sync::RwLock<
            Option<Arc<tokio::sync::Mutex<acowork_runtime::agent::session::SessionManager>>>,
        >,
    > = Arc::new(tokio::sync::RwLock::new(None));

    let server = acowork_runtime::http::RuntimeHttpServer::start(
        temp_dir.clone(),
        temp_dir.clone(), // package_dir
        AGENT_ID.to_string(),
        INSTANCE_ID.to_string(),
        snapshots,
        latest,
        dispatch_tx,
        embed_dim,
        degraded_reasons,
        mqtt_client,
        session_metadata,
        memory_query,
        workspace_query,
        workspace_mutation,
        git_query,
        agent_tools,
        agent_config,
        attachment,
        session_config,
        consolidation_timer,
        rag_provider,
        debug_service,
        workspace_resolver,
        session_manager_slot,
        std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore
    )
    .await
    .expect("runtime http server should start");

    (server.port, temp_dir)
}

/// GET `/git/{endpoint}` with optional query pairs → `(status, json)`.
async fn get_git(
    port: u16,
    endpoint: &str,
    query: &[(&str, &str)],
) -> (u16, serde_json::Value) {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/git/{endpoint}");
    let resp = client.get(&url).query(query).send().await.unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

// ── /git/status ──────────────────────────────────────────────────────────

#[tokio::test]
async fn status_clean_repo_reports_main_branch() {
    let (port, dir) = spawn_server("status-clean").await;
    init_repo(&dir);

    let (status, body) = get_git(port, "status", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body["isRepo"], true);
    assert_eq!(body["branch"], "main");
    assert_eq!(body["error"], serde_json::Value::Null);
    assert_eq!(body["truncated"], false);
    assert_eq!(body["changes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn status_mixed_changes_fields_sort_and_rename_old_path() {
    let (port, dir) = spawn_server("status-mixed").await;
    init_repo(&dir);
    // a.txt modified (worktree), b.txt untracked, c.txt staged-added,
    // d.txt renamed → e.txt (staged), f.txt deleted (worktree).
    std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();
    std::fs::write(dir.join("b.txt"), "untracked\n").unwrap();
    std::fs::write(dir.join("c.txt"), "c\n").unwrap();
    std::fs::write(dir.join("d.txt"), "d\n").unwrap();
    std::fs::write(dir.join("f.txt"), "f\n").unwrap();
    git_in(&dir, &["add", "a.txt", "c.txt", "d.txt", "f.txt"]);
    git_in(&dir, &["commit", "-m", "second"]);
    std::fs::write(dir.join("a.txt"), "hello world modified\n").unwrap(); // worktree M
    git_in(&dir, &["mv", "d.txt", "e.txt"]); // staged R
    git_in(&dir, &["add", "-u"]); // stage f.txt deletion + a.txt? (a.txt re-modified after)
    std::fs::remove_file(dir.join("f.txt")).unwrap(); // f.txt: staged D
    // NOTE: after `add -u`, a.txt was re-written → worktree M on top of staged M.
    std::fs::write(dir.join("a.txt"), "final a\n").unwrap();

    let (status, body) = get_git(port, "status", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body["isRepo"], true);
    assert_eq!(body["error"], serde_json::Value::Null);

    let changes = body["changes"].as_array().unwrap();
    // staged entries sort first (ADR-078 decision 4): e.txt (R), f.txt (D),
    // a.txt (MM staged) — b.txt untracked last.
    let paths: Vec<&str> = changes.iter().map(|c| c["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["a.txt", "e.txt", "b.txt", "f.txt"], "sort order: staged M, staged R, untracked, worktree D");

    let rename = changes.iter().find(|c| c["path"] == "e.txt").unwrap();
    assert_eq!(rename["oldPath"], "d.txt", "rename must carry oldPath");
    assert_eq!(rename["index"], "renamed");
    assert_eq!(rename["staged"], true);

    let deleted = changes.iter().find(|c| c["path"] == "f.txt").unwrap();
    assert_eq!(deleted["index"], "unmodified");
    assert_eq!(deleted["worktree"], "deleted", "rm after add -u → worktree deletion");
    assert_eq!(deleted["staged"], false);

    let untracked = changes.iter().find(|c| c["path"] == "b.txt").unwrap();
    assert_eq!(untracked["worktree"], "untracked");
    assert_eq!(untracked["staged"], false);
}

#[tokio::test]
async fn status_non_repo_returns_is_repo_false() {
    let (port, _dir) = spawn_server("status-nonrepo").await;
    // No `git init` — plain temp dir.
    let (status, body) = get_git(port, "status", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body["isRepo"], false);
    assert_eq!(body["error"], "not_a_repo");
    assert_eq!(body["branch"], serde_json::Value::Null);
    assert_eq!(body["changes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn status_workspace_subdir_of_repo_filters_changes() {
    let (port, dir) = spawn_server("status-subdir").await;
    init_repo(&dir);
    // Repo lives at temp_dir; workspace is a subdir (also inside repo).
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("b.txt"), "b\n").unwrap();
    // A change OUTSIDE the workspace but inside the repo must be filtered.
    std::fs::write(dir.join("outside.txt"), "o\n").unwrap();
    register_workspace(&dir, SUB_WS_ID, &sub);

    let (status, body) = get_git(port, "status", &[("workspace_id", SUB_WS_ID)]).await;
    assert_eq!(status, 200);
    let changes = body["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "only the workspace-local change");
    assert_eq!(changes[0]["path"], "b.txt");
}

#[tokio::test]
async fn status_unknown_workspace_returns_404() {
    let (port, dir) = spawn_server("status-unknown-ws").await;
    init_repo(&dir);
    let (status, body) = get_git(port, "status", &[("workspace_id", "no-such-ws")]).await;
    assert_eq!(status, 404, "unknown workspace id → WorkspaceNotFound");
    assert!(body["error"].is_string());
}

// ── /git/diff ────────────────────────────────────────────────────────────

#[tokio::test]
async fn diff_modified_returns_head_and_worktree_sides() {
    let (port, dir) = spawn_server("diff-modified").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "a.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "modified");
    assert_eq!(body["original"], "hello\n");
    assert_eq!(body["modified"], "hello world\n");
}

#[tokio::test]
async fn diff_untracked_has_empty_original() {
    let (port, dir) = spawn_server("diff-untracked").await;
    init_repo(&dir);
    std::fs::write(dir.join("b.txt"), "brand new\n").unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "b.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "untracked");
    assert_eq!(body["original"], "");
    assert_eq!(body["modified"], "brand new\n");
}

#[tokio::test]
async fn diff_deleted_has_empty_modified() {
    let (port, dir) = spawn_server("diff-deleted").await;
    init_repo(&dir);
    std::fs::remove_file(dir.join("a.txt")).unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "a.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "deleted");
    assert_eq!(body["original"], "hello\n", "HEAD content preserved");
    assert_eq!(body["modified"], "");
}

#[tokio::test]
async fn diff_cached_reads_index_not_worktree() {
    let (port, dir) = spawn_server("diff-cached").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "index version\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    std::fs::write(dir.join("a.txt"), "worktree version\n").unwrap();

    // cached=1 → index vs HEAD
    let (status, body) = get_git(port, "diff", &[("path", "a.txt"), ("cached", "1")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "modified");
    assert_eq!(body["modified"], "index version\n");

    // cached=0 (default) → worktree vs HEAD
    let (status, body) = get_git(port, "diff", &[("path", "a.txt"), ("cached", "0")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["modified"], "worktree version\n");

    // cached > 1 → 400
    let (status, _) = get_git(port, "diff", &[("path", "a.txt"), ("cached", "2")]).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn diff_binary_returns_empty_sides() {
    let (port, dir) = spawn_server("diff-binary").await;
    init_repo(&dir);
    // Committed PNG, then replaced with different binary content.
    std::fs::write(dir.join("img.png"), b"\x89PNG\r\n\x1a\n\x00BINARY0").unwrap();
    git_in(&dir, &["add", "img.png"]);
    git_in(&dir, &["commit", "-m", "add png"]);
    std::fs::write(dir.join("img.png"), b"\x89PNG\r\n\x1a\n\x00BINARY1").unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "img.png")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "binary");
    assert_eq!(body["original"], "", "binary content never leaked (decision 4)");
    assert_eq!(body["modified"], "");
}

#[tokio::test]
async fn diff_rename_uses_old_path_for_head() {
    let (port, dir) = spawn_server("diff-rename").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "renamed body\n").unwrap();
    git_in(&dir, &["mv", "a.txt", "b.txt"]);

    // `git show HEAD:b.txt` fails (HEAD still holds a.txt) — the service
    // must fall back to the rename's old path for the original side.
    let (status, body) = get_git(port, "diff", &[("path", "b.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "modified");
    assert_eq!(body["original"], "hello\n", "HEAD content read from old path a.txt");
    assert_eq!(body["modified"], "renamed body\n", "worktree content from new path b.txt");
}

#[tokio::test]
async fn diff_path_traversal_rejected() {
    let (port, dir) = spawn_server("diff-traversal").await;
    init_repo(&dir);
    let (status, _) = get_git(port, "diff", &[("path", "../evil.txt")]).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn diff_missing_path_is_400() {
    let (port, dir) = spawn_server("diff-nopath").await;
    init_repo(&dir);
    let (status, _) = get_git(port, "diff", &[]).await;
    assert_eq!(status, 400, "path is required for /git/diff");
}

#[tokio::test]
async fn diff_non_repo_is_400() {
    let (port, _dir) = spawn_server("diff-nonrepo").await;
    // No repo in temp_dir.
    let (status, body) = get_git(port, "diff", &[("path", "a.txt")]).await;
    assert_eq!(status, 400);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn diff_utf8_path_roundtrips() {
    let (port, dir) = spawn_server("diff-utf8").await;
    init_repo(&dir);
    std::fs::write(dir.join("中文.txt"), "内容\n").unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "中文.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "untracked");
    assert_eq!(body["modified"], "内容\n");
}

// ── /git/log ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn log_repository_wide_returns_all_commits() {
    let (port, dir) = spawn_server("log-all").await;
    init_repo(&dir);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    git_in(&dir, &["commit", "-m", "add b"]);

    let (status, body) = get_git(port, "log", &[]).await;
    assert_eq!(status, 200);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 2);
    // Newest first.
    assert_eq!(commits[0]["subject"], "add b");
    assert_eq!(commits[1]["subject"], "init");
    // Wire-contract fields for the Desktop log tab.
    let c = &commits[0];
    for field in ["hash", "shortHash", "author", "date", "subject"] {
        assert!(c[field].is_string(), "commit must carry {field}");
    }
    assert_eq!(c["author"], "e2e");
    assert!(!c["hash"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn log_path_scoped_filters_commits() {
    let (port, dir) = spawn_server("log-path").await;
    init_repo(&dir);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    git_in(&dir, &["commit", "-m", "add b"]);
    std::fs::write(dir.join("a.txt"), "changed a\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "mod a"]);

    let (status, body) = get_git(port, "log", &[("path", "b.txt")]).await;
    assert_eq!(status, 200);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1, "only the commit touching b.txt");
    assert_eq!(commits[0]["subject"], "add b");
}

#[tokio::test]
async fn log_limit_respected() {
    let (port, dir) = spawn_server("log-limit").await;
    init_repo(&dir);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    git_in(&dir, &["commit", "-m", "add b"]);

    let (status, body) = get_git(port, "log", &[("limit", "1")]).await;
    assert_eq!(status, 200);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0]["subject"], "add b", "limit keeps newest");
}

#[tokio::test]
async fn log_non_repo_is_400() {
    let (port, _dir) = spawn_server("log-nonrepo").await;
    let (status, body) = get_git(port, "log", &[]).await;
    assert_eq!(status, 400);
    assert!(body["error"].is_string());
}
