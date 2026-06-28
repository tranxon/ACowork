//! LSP install script management.
//!
//! Handles loading and executing install scripts for LSP servers.
//! Uses idle-timeout monitoring via `acowork_core::process` — the script
//! is killed only if it produces no stdout/stderr output for 60 seconds,
//! allowing long-running installs (e.g. `npm install` with slow network)
//! to complete as long as they keep printing progress.

use std::path::PathBuf;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, extract::Path};

use acowork_core::process::run_command_with_idle_timeout;

use crate::config::{canonical_language, lsp_servers_config, refresh_path_from_profiles};

/// `GET /api/lsp/install/{language}` — return the install script content.
pub async fn lsp_install_script(Path(language): Path<String>) -> impl IntoResponse {
    let lang_lower = language.to_lowercase();
    let canonical = canonical_language(&lang_lower);
    let cfg = lsp_servers_config();

    let script_name = cfg
        .servers
        .get(canonical)
        .and_then(|e| e.install_script.as_ref());

    let script_name = match script_name {
        Some(name) => name,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("No install script for language: {}", language),
                    "code": 404
                })),
            )
                .into_response();
        }
    };

    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let filename = format!("{}.{}", script_name, ext);

    let config_dir = std::env::var("ACOWORK_LSP_CONFIG_DIR")
        .ok()
        .map(std::path::PathBuf::from);
    let content = load_install_script(&filename, config_dir.as_deref());
    match content {
        Some(script) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "language": canonical,
                "filename": filename,
                "script": script,
                "platform": ext,
            })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("Install script file '{}' not found", filename),
                "code": 404
            })),
        )
            .into_response(),
    }
}

/// `POST /api/lsp/install/{language}` — run the install script.
pub async fn lsp_install_run(Path(language): Path<String>) -> impl IntoResponse {
    let lang_lower = language.to_lowercase();
    let canonical = canonical_language(&lang_lower);
    let cfg = lsp_servers_config();

    let script_name = cfg
        .servers
        .get(canonical)
        .and_then(|e| e.install_script.as_ref());

    let script_name = match script_name {
        Some(name) => name,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("No install script for language: {}", language),
                    "code": 404
                })),
            )
                .into_response();
        }
    };

    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let filename = format!("{}.{}", script_name, ext);
    let config_dir = std::env::var("ACOWORK_LSP_CONFIG_DIR")
        .ok()
        .map(std::path::PathBuf::from);
    let script_path = match find_install_script_path(&filename, config_dir.as_deref()) {
        Some(path) => path,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("Install script file '{}' not found", filename),
                    "code": 404
                })),
            )
                .into_response();
        }
    };

    tracing::info!(
        "[LSP] Running install script for '{}': {}",
        canonical,
        script_path.display()
    );

    // Build the command — run_command_with_idle_timeout automatically
    // configures stdout/stderr as piped and sets kill_on_drop(true).
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("powershell");
        c.args(["-ExecutionPolicy", "Bypass", "-NoProfile", "-File"]);
        c.arg(&script_path);
        c
    } else {
        let mut c = tokio::process::Command::new("bash");
        c.arg(&script_path);
        c
    };

    // Idle timeout: 60 seconds with no stdout/stderr output → kill.
    // As long as the script keeps printing (e.g. download progress),
    // it will never time out. This is far better than an absolute timeout
    // which would kill a slow-but-progressing download.
    let idle_timeout = Duration::from_secs(60);

    match run_command_with_idle_timeout(&mut cmd, idle_timeout).await {
        Ok(output) => {
            let success = output.exit_code == Some(0);

            tracing::info!(
                "[LSP] Install script for '{}' completed — success: {}, exit_code: {:?}, stdout_len: {}, stderr_len: {}",
                canonical,
                success,
                output.exit_code,
                output.stdout.len(),
                output.stderr.len()
            );

            let code = if success {
                // After a successful install, refresh the PATH cache from
                // shell profile files. Install scripts write `export PATH=...`
                // to ~/.profile / ~/.zshrc / ~/.bashrc, but those changes
                // only take effect in new shell sessions. By parsing the
                // profile files here, we make the newly-installed binary
                // discoverable immediately without restarting the LSP Relay.
                refresh_path_from_profiles();
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (
                code,
                Json(serde_json::json!({
                    "language": canonical,
                    "success": success,
                    "exit_code": output.exit_code,
                    "stdout": output.stdout,
                    "stderr": output.stderr,
                })),
            )
                .into_response()
        }
        Err(e) => {
            // idle_secs == 0 indicates a spawn failure (not an actual timeout).
            if e.idle_secs == 0 {
                tracing::error!(
                    "[LSP] Failed to spawn install script for '{}': {}",
                    canonical,
                    e.stderr()
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("Failed to run install script: {}", e.stderr()),
                        "code": 500
                    })),
                )
                    .into_response()
            } else {
                tracing::warn!(
                    "[LSP] Install script for '{}' timed out (idle {}s with no output)",
                    canonical,
                    e.idle_secs
                );
                (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({
                        "error": format!(
                            "Install script for '{}' timed out after {}s with no output. \
                             The process may be stuck — please retry or install manually.",
                            canonical,
                            e.idle_secs
                        ),
                        "code": 504,
                        "stdout": e.stdout(),
                        "stderr": e.stderr(),
                    })),
                )
                    .into_response()
            }
        }
    }
}

fn find_install_script_path(filename: &str, config_dir: Option<&std::path::Path>) -> Option<PathBuf> {
    let candidates = build_install_script_candidates(filename, config_dir);
    candidates.into_iter().find(|p| p.exists())
}

fn load_install_script(filename: &str, config_dir: Option<&std::path::Path>) -> Option<String> {
    let candidates = build_install_script_candidates(filename, config_dir);
    for path in &candidates {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    tracing::info!("Loaded install script from: {}", path.display());
                    return Some(content);
                }
                Err(e) => {
                    tracing::warn!("Failed to read install script {}: {}", path.display(), e);
                }
            }
        }
    }
    None
}

fn build_install_script_candidates(filename: &str, config_dir: Option<&std::path::Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    // 0. Explicit config dir (CLI arg or env var, resolved by caller)
    if let Some(config_dir) = config_dir {
        let path = config_dir
            .join("lsp_install")
            .join(filename);
        if path.exists() {
            candidates.push(path);
        }
    }

    // 1. CARGO_MANIFEST_DIR ../../assets/lsp_install/ (dev)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let path = PathBuf::from(&manifest_dir)
            .join("..")
            .join("..")
            .join("assets")
            .join("lsp_install")
            .join(filename);
        if path.exists() {
            candidates.push(path);
        }
    }

    // 2. Same directory as executable / lsp_install/
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        candidates.push(exe_dir.join("lsp_install").join(filename));
    }

    // 3. Current working directory (dev convenience)
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("lsp_install").join(filename));
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::process::run_command_with_idle_timeout;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::time::Duration;
    use tower::ServiceExt;

    /// Build a minimal router with only install routes (no State required).
    fn install_router() -> axum::Router {
        axum::Router::new()
            .route(
                "/api/lsp/install/{language}",
                axum::routing::get(lsp_install_script),
            )
            .route(
                "/api/lsp/install/{language}",
                axum::routing::post(lsp_install_run),
            )
    }

    // ── HTTP handler: GET /api/lsp/install/{language} ────────────────────

    #[tokio::test]
    async fn get_install_script_unknown_language_returns_404() {
        let app = install_router();
        let req = Request::builder()
            .method("GET")
            .uri("/api/lsp/install/brainfuck")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_install_script_known_language_returns_200() {
        // "rust" is in the built-in defaults and assets/lsp_install/rust.sh
        // exists in the repo, so this should return 200.
        let app = install_router();
        let req = Request::builder()
            .method("GET")
            .uri("/api/lsp/install/rust")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["language"], "rust");
        assert!(!json["script"].as_str().unwrap().is_empty());
    }

    // ── HTTP handler: POST /api/lsp/install/{language} ───────────────────

    #[tokio::test]
    async fn post_install_unknown_language_returns_404() {
        let app = install_router();
        let req = Request::builder()
            .method("POST")
            .uri("/api/lsp/install/brainfuck")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── Idle timeout integration tests ───────────────────────────────────
    //
    // These tests verify that `run_command_with_idle_timeout` (used by
    // `lsp_install_run`) behaves correctly. We test the function directly
    // rather than through the HTTP handler to avoid running real install
    // scripts (e.g. `rustup component add rust-analyzer`).

    fn write_temp_script(body: &str) -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test_install.sh");
        std::fs::write(&path, format!("#!/usr/bin/env bash\n{body}\n")).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let path_clone = path.clone();
        std::mem::forget(dir); // keep temp dir alive for test duration
        path_clone
    }

    #[tokio::test]
    async fn idle_timeout_normal_completion() {
        let script = write_temp_script("echo 'installing...'\necho 'done'\nexit 0");
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg(&script);
        let output = run_command_with_idle_timeout(&mut cmd, Duration::from_secs(10))
            .await
            .expect("should complete normally");
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("installing..."));
        assert!(output.stdout.contains("done"));
    }

    #[tokio::test]
    async fn idle_timeout_script_failure() {
        let script = write_temp_script("echo 'error occurred'\nexit 1");
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg(&script);
        let output = run_command_with_idle_timeout(&mut cmd, Duration::from_secs(10))
            .await
            .expect("should complete (non-zero exit is not an error)");
        assert_eq!(output.exit_code, Some(1));
        assert!(output.stdout.contains("error occurred"));
    }

    #[tokio::test]
    async fn idle_timeout_triggers_on_silent_script() {
        // Script sleeps for 10s with no output — should be killed after 2s idle.
        let script = write_temp_script("sleep 10");
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg(&script);
        let result = run_command_with_idle_timeout(&mut cmd, Duration::from_secs(2)).await;
        assert!(result.is_err(), "should timeout — no output for 2s");
        let err = result.unwrap_err();
        assert_eq!(err.idle_secs, 2);
    }

    #[tokio::test]
    async fn idle_timeout_does_not_trigger_with_continuous_output() {
        // Script prints a line every 500ms for 5s — should NOT timeout
        // because the idle timer resets on each output line.
        let script = write_temp_script(
            "for i in $(seq 1 10); do echo \"progress $i\"; sleep 0.5; done\nexit 0",
        );
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg(&script);
        let output = run_command_with_idle_timeout(&mut cmd, Duration::from_secs(2))
            .await
            .expect("should complete — continuous output prevents timeout");
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("progress 10"));
    }

    #[tokio::test]
    async fn idle_timeout_nonexistent_command_returns_error() {
        // Using a nonexistent binary (not bash with a nonexistent script)
        // to test the actual spawn failure path. `bash /bad/path` would
        // spawn bash successfully — bash then exits with an error code,
        // which is a normal completion, not a spawn failure.
        let mut cmd = tokio::process::Command::new(
            "/nonexistent/binary/that/does/not/exist",
        );
        let result = run_command_with_idle_timeout(&mut cmd, Duration::from_secs(5)).await;
        assert!(result.is_err(), "should fail — command does not exist");
        let err = result.unwrap_err();
        assert_eq!(err.idle_secs, 0, "idle_secs=0 indicates spawn failure");
        assert!(err.stderr().contains("Failed to spawn"));
    }

    // ── Helper function tests ────────────────────────────────────────────

    #[test]
    fn test_build_install_script_candidates_with_env_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let install_dir = dir.path().join("lsp_install");
        std::fs::create_dir_all(&install_dir).expect("mkdir");
        let script_path = install_dir.join("test_lang.sh");
        std::fs::write(&script_path, "#!/bin/bash\necho test\n").expect("write script");

        // Pass the config dir explicitly — no env var mutation needed.
        let candidates = build_install_script_candidates("test_lang.sh", Some(dir.path()));

        let env_candidate = candidates
            .iter()
            .find(|p| p.to_string_lossy().contains("test_lang.sh"));
        assert!(
            env_candidate.is_some(),
            "env override candidate should be present"
        );
    }

    #[test]
    fn test_find_install_script_path_finds_existing() {
        // "rust.sh" exists in assets/lsp_install/ in the repo
        let path = find_install_script_path("rust.sh", None);
        assert!(
            path.is_some(),
            "should find rust.sh in assets or other search paths"
        );
    }

    #[test]
    fn test_find_install_script_path_returns_none_for_nonexistent() {
        let path = find_install_script_path("definitely_nonexistent_script_12345.sh", None);
        assert!(path.is_none());
    }

    #[test]
    fn test_load_install_script_returns_content() {
        // "rust.sh" exists in assets/lsp_install/
        let content = load_install_script("rust.sh", None);
        assert!(content.is_some(), "should load rust.sh content");
        let script = content.unwrap();
        assert!(!script.is_empty(), "script content should not be empty");
    }

    #[test]
    fn test_load_install_script_returns_none_for_nonexistent() {
        let content = load_install_script("definitely_nonexistent_script_12345.sh", None);
        assert!(content.is_none());
    }
}
