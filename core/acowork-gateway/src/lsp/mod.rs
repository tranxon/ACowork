//! LSP relay module
//!
//! LSP protocol relay: WebSocket ↔ stdin/stdout of a language server process.
//!
//! LSP over stdio uses the Base Protocol frame format:
//! ```text
//! Content-Length: <length>\r\n\r\n<JSON-RPC message>
//! ```
//! WebSocket side (vscode-ws-jsonrpc) sends/receives plain JSON-RPC text messages.
//!
//! The relay converts between these two formats:
//! - **WS → stdin**: receive JSON text, prepend `Content-Length` header, write to stdin
//! - **stdout → WS**: parse `Content-Length` header, extract JSON body, send as text
//!
//! Architecture:
//! ```text
//! Monaco (webview) ← WS (JSON text) → Gateway ← stdin/stdout (framed) → LSP Server
//! ```
//!
//! ## Process Pool
//!
//! LSP processes are pooled: their lifetime is bound to the Gateway process,
//! NOT individual WebSocket sessions. This avoids re-indexing (e.g. rust-analyzer)
//! every time the Desktop App reconnects.
//!
//! ## Install Status Probe
//!
//! `GET /api/lsp/status` reports whether each configured language has a
//! runnable LSP command on PATH. The probe is **two-stage**:
//!
//! 1. Try `{command} --version` (2 s timeout). Fast path for most servers
//!    (rust-analyzer, clangd, yaml-language-server, typescript-language-server,
//!    pylsp, pyright, marksman, ...). Also catches broken rustup proxies.
//! 2. Fall back to spawning the command with the language's configured
//!    launch args (`--stdio` for vscode-langservers-extracted servers
//!    and friends) and a **piped stdin**. If the process stays alive past
//!    a 500 ms grace window, it's a stdio-mode LSP server waiting for a
//!    client connection — report runnable. A piped stdin is required:
//!    `vscode-json-language-server` etc. throw `Connection input stream
//!    is not set` and exit immediately if stdin is `/dev/null`.
//!
//! `kill_on_drop(true)` ensures the probe process is reaped on every
//! exit path. See [`verify_command_runnable`].
//!
//! ## Configuration
//!
//! LSP server specifications are loaded from `lsp_servers.json` at startup.
//! The file is searched in multiple locations (see `build_config_candidates`).
//! If no file is found, built-in defaults are used as a fallback.
//! Language alias mapping (e.g. `js→typescript`) is kept in code (protocol logic).

pub mod pool;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::OnceLock;

use crate::http::routes::AppState;
pub use pool::LspPool;

// ── LSP server configuration (from JSON file) ──────────────────────────

/// Per-language LSP server specification from `lsp_servers.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerEntry {
    /// Candidate command names (tried in order).
    pub candidates: Vec<String>,
    /// Extra arguments for stdio-mode LSP communication.
    ///
    /// Different LSP servers have different stdio-mode requirements:
    /// - Some default to stdio and reject `--stdio` (rust-analyzer, clangd, marksman, pylsp)
    /// - Some require `--stdio` explicitly (pyright-langserver, typescript-language-server, etc.)
    /// - Some use a subcommand instead of a flag (gopls uses `serve`)
    ///
    /// When `candidate_args` is set, `args` is used as default and specific
    /// overrides are applied per candidate name.
    pub args: Vec<String>,
    /// Per-candidate arg overrides. Keys are candidate command names;
    /// values replace `args` entirely for that candidate.
    /// This allows mixing servers with different arg requirements
    /// (e.g. pylsp needs no args, pyright-langserver needs --stdio).
    #[serde(default, skip_serializing_if = "empty_candidate_args")]
    pub candidate_args: std::collections::HashMap<String, Vec<String>>,
    /// One-line install hint shown to the user.
    pub install_hint: String,
    /// Name of the install script file (e.g. "rust" → assets/lsp_install/rust.sh).
    /// Null means no dedicated script; use `install_hint` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_script: Option<String>,
    /// Human-readable description.
    pub description: String,
}

/// Helper for serde skip_serializing_if — returns true when HashMap is empty.
fn empty_candidate_args(map: &std::collections::HashMap<String, Vec<String>>) -> bool {
    map.is_empty()
}

/// Top-level structure for `lsp_servers.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServersConfig {
    /// Schema version (for future migration).
    pub version: u32,
    /// Language-keyed server entries (canonical language names only).
    pub servers: std::collections::HashMap<String, LspServerEntry>,
}

/// Per-language LSP server installation status (computed at request time).
///
/// Returned by `GET /api/lsp/status`. Unlike [`LspServerEntry`], which is a
/// static config record, this struct reflects whether the server's command
/// is actually reachable on `PATH` right now.
///
/// The check is identical to the one used by the LSP WebSocket handler
/// (`resolve_lsp_command` → `find_on_path`), so the UI's "installed" badge
/// always matches the editor's ability to actually launch the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LspServerStatusEntry {
    /// Canonical language name (e.g. "rust", "python").
    pub language: String,
    /// Whether any candidate command was found on `PATH`.
    pub installed: bool,
    /// Resolved command path or name (e.g. "rust-analyzer" or
    /// "C:\\Users\\...\\rust-analyzer.exe"). `None` when `installed` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Resolved LSP server specification: command name + launch arguments.
///
/// Different LSP servers have different stdio-mode requirements:
/// - Some default to stdio and reject `--stdio` (rust-analyzer, clangd, marksman)
/// - Some require `--stdio` explicitly (pylsp, typescript-language-server, etc.)
/// - Some use a subcommand instead of a flag (gopls uses `serve`)
///
/// This struct centralises per-server knowledge so `spawn_pooled` can
/// launch any LSP process correctly.
#[derive(Debug, Clone)]
pub struct LspServerSpec {
    /// Command found on PATH (e.g. "rust-analyzer.exe", "gopls")
    pub command: String,
    /// Extra arguments required for stdio-mode LSP communication.
    pub args: Vec<String>,
    /// Canonical language name (after alias resolution).
    pub language: String,
    /// Install hint from config file.
    pub install_hint: String,
    /// Install script name from config file (if available).
    pub install_script: Option<String>,
}

// ── Language alias mapping ──────────────────────────────────────────────

/// Map language aliases to canonical names used in `lsp_servers.json`.
/// This mapping stays in code because it's protocol logic, not configuration.
fn canonical_language(lang: &str) -> &str {
    match lang {
        "js" => "typescript",
        "javascript" => "typescript",
        "yml" => "yaml",
        "scss" => "css",
        "less" => "css",
        "cpp" | "c++" => "c",
        "md" => "markdown",
        other => other,
    }
}

// ── Config file loading ────────────────────────────────────────────────

/// Load `lsp_servers.json` from disk (cached with `OnceLock`).
///
/// Search order (matches the `offline_providers.json` pattern):
///   1. `$CARGO_MANIFEST_DIR/../../assets/lsp_servers.json` (dev / test via cargo)
///   2. `{exe_dir}/lsp_servers.json` (installer-provided)
///   3. `{cwd}/lsp_servers.json` (dev convenience)
///
/// If no file is found, built-in defaults are used.
fn lsp_servers_config() -> &'static LspServersConfig {
    static CFG: OnceLock<LspServersConfig> = OnceLock::new();
    CFG.get_or_init(|| {
        load_lsp_servers_from_file().unwrap_or_else(|| {
            tracing::warn!("lsp_servers.json not found, using built-in defaults");
            builtin_lsp_defaults()
        })
    })
}

fn load_lsp_servers_from_file() -> Option<LspServersConfig> {
    let candidates = build_config_candidates();
    for path in &candidates {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => match serde_json::from_str::<LspServersConfig>(&content) {
                    Ok(cfg) => {
                        tracing::info!(
                            path = %path.display(),
                            count = cfg.servers.len(),
                            "Loaded LSP servers config"
                        );
                        return Some(cfg);
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "Failed to parse lsp_servers.json"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to read lsp_servers.json"
                    );
                }
            }
        }
    }
    None
}

fn build_config_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    // 0. CLI / env override: --lsp-config-dir / ACOWORK_LSP_CONFIG_DIR
    //    This is the highest-priority path — Desktop App passes its Tauri
    //    resource_dir here. In remote mode (standalone Gateway), this is
    //    unset and Gateway falls back to exe_dir below.
    if let Ok(config_dir) = std::env::var("ACOWORK_LSP_CONFIG_DIR") {
        let path = std::path::PathBuf::from(&config_dir).join("lsp_servers.json");
        if path.exists() {
            candidates.push(path);
        }
    }

    // 1. CARGO_MANIFEST_DIR ../../assets/ (dev and test via cargo)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let path = std::path::PathBuf::from(&manifest_dir)
            .join("..")
            .join("..")
            .join("assets")
            .join("lsp_servers.json");
        if path.exists() {
            candidates.push(path);
        }
    }

    // 2. Same directory as the executable (installer-provided, read-only)
    //    In remote mode, LSP config files are co-installed with Gateway.
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        candidates.push(exe_dir.join("lsp_servers.json"));
    }

    // 3. Current working directory (dev convenience)
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("lsp_servers.json"));
    }

    candidates
}

/// Built-in default LSP server config (used when `lsp_servers.json` is absent).
fn builtin_lsp_defaults() -> LspServersConfig {
    let mut servers = std::collections::HashMap::new();

    servers.insert(
        "rust".into(),
        LspServerEntry {
            candidates: vec!["rust-analyzer".into()],
            args: vec![],
            install_hint: "rustup component add rust-analyzer".into(),
            install_script: Some("rust".into()),
            description: "Rust language server (defaults to stdio, no --stdio flag)".into(),
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "python".into(),
        LspServerEntry {
            candidates: vec![
                "pyright-langserver".into(),
                "pylsp".into(),
                "python-lsp-server".into(),
            ],
            args: vec!["--stdio".into()],
            install_hint: "pip install python-lsp-server".into(),
            install_script: Some("python".into()),
            description: "Python language server".into(),
            // pylsp defaults to stdio — no --stdio flag. Override to empty args.
            candidate_args: std::collections::HashMap::from([
                ("pylsp".into(), vec![]),
                ("python-lsp-server".into(), vec![]),
            ]),
        },
    );
    servers.insert(
        "typescript".into(),
        LspServerEntry {
            candidates: vec![
                "typescript-language-server".into(),
                "typescript-language-server.cmd".into(),
            ],
            args: vec!["--stdio".into()],
            install_hint: "npm install -g typescript-language-server typescript".into(),
            install_script: Some("typescript".into()),
            description: "TypeScript/JavaScript language server".into(),
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "go".into(),
        LspServerEntry {
            candidates: vec!["gopls".into()],
            args: vec!["serve".into()],
            install_hint: "go install golang.org/x/tools/gopls@latest".into(),
            install_script: Some("go".into()),
            description: "Go language server (uses 'serve' subcommand)".into(),
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "c".into(),
        LspServerEntry {
            candidates: vec!["clangd".into()],
            args: vec![],
            install_hint: "Install clangd: https://clangd.llvm.org/installation".into(),
            install_script: Some("clangd".into()),
            description: "C/C++ language server (defaults to stdio)".into(),
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "json".into(),
        LspServerEntry {
            candidates: vec![
                "vscode-json-language-server".into(),
                "vscode-json-languageserver".into(),
                "json-languageserver".into(),
            ],
            args: vec!["--stdio".into()],
            install_hint: "npm install -g vscode-langservers-extracted".into(),
            install_script: Some("json".into()),
            description: "JSON language server".into(),
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "yaml".into(),
        LspServerEntry {
            candidates: vec!["yaml-language-server".into()],
            args: vec!["--stdio".into()],
            install_hint: "npm install -g yaml-language-server".into(),
            install_script: Some("yaml".into()),
            description: "YAML language server".into(),
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "html".into(),
        LspServerEntry {
            candidates: vec![
                "vscode-html-language-server".into(),
                "vscode-html-languageserver".into(),
                "html-languageserver".into(),
            ],
            args: vec!["--stdio".into()],
            install_hint: "npm install -g vscode-langservers-extracted".into(),
            install_script: Some("html".into()),
            description: "HTML language server".into(),
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "css".into(),
        LspServerEntry {
            candidates: vec![
                "vscode-css-language-server".into(),
                "vscode-css-languageserver".into(),
                "css-languageserver".into(),
            ],
            args: vec!["--stdio".into()],
            install_hint: "npm install -g vscode-langservers-extracted".into(),
            install_script: Some("css".into()),
            description: "CSS/SCSS/Less language server".into(),
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "markdown".into(),
        LspServerEntry {
            candidates: vec!["marksman".into()],
            args: vec![],
            install_hint: "Install marksman: https://github.com/artempyanykh/marksman".into(),
            install_script: Some("markdown".into()),
            description: "Markdown language server (defaults to stdio)".into(),
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "java".into(),
        LspServerEntry {
            candidates: vec!["jdtls".into()],
            args: vec![],
            install_hint:
                "Download Eclipse JDT Language Server: https://download.eclipse.org/jdtls/".into(),
            install_script: Some("java".into()),
            description: "Eclipse JDT Language Server for Java".into(),
            candidate_args: Default::default(),
        },
    );

    LspServersConfig {
        version: 1,
        servers,
    }
}

// ── Resolve LSP command ────────────────────────────────────────────────

/// Resolve the LSP server command and launch arguments for a given language.
///
/// Looks up the canonical language in `lsp_servers.json`, then tries each
/// candidate on PATH. Each found candidate is verified by
/// [`verify_command_runnable`] (two-stage: `--version` then spawn-with-args).
///
/// Returns `LspServerSpec` with the found command, the server-specific args,
/// plus install hint/script for UI display.
/// Returns `None` if no runnable candidate command is found.
async fn resolve_lsp_command(language: &str) -> Option<LspServerSpec> {
    let lang_lower = language.to_lowercase();
    let canonical = canonical_language(&lang_lower);
    let cfg = lsp_servers_config();

    let entry = cfg.servers.get(canonical);
    if entry.is_none() {
        tracing::warn!(
            "[LSP] No config entry for language '{}' (canonical: '{}')",
            language,
            canonical
        );
        return None;
    }
    let entry = entry.unwrap();

    // Find first candidate that exists on PATH AND can actually run.
    for cmd in &entry.candidates {
        if let Some(found) = find_on_path(cmd) {
            // Verify the command is actually runnable (not just a broken proxy
            // or a stdio-mode LSP server that lacks `--version`). We pass
            // `entry.args` so the fallback spawn probe uses the real launch
            // arguments (e.g. `--stdio` for vscode-langservers-extracted).
            if !verify_command_runnable(&found, &entry.args).await {
                tracing::warn!(
                    "[LSP] Command '{}' (with args {:?}) not runnable — skipping",
                    found,
                    entry.args
                );
                continue;
            }

            tracing::info!(
                "[LSP] Found LSP command for '{}' (canonical '{}'): {}, args: {:?}",
                language,
                canonical,
                found,
                entry.args
            );
            return Some(LspServerSpec {
                command: found,
                args: entry.args.clone(),
                language: canonical.to_string(),
                install_hint: entry.install_hint.clone(),
                install_script: entry.install_script.clone(),
            });
        }
    }

    tracing::warn!(
        "[LSP] No LSP command found for '{}' (canonical '{}', checked: {:?})",
        language,
        canonical,
        entry.candidates
    );
    None
}

/// Verify a command is actually runnable before reporting it as a usable
/// LSP server on the `/api/lsp/status` endpoint.
///
/// Two-stage probe:
///
/// 1. **Fast path — `--version`**: most LSP servers (rust-analyzer, clangd,
///    marksman, yaml-language-server, typescript-language-server, pylsp,
///    pyright, …) implement `--version` and exit 0. This stage also
///    catches broken rustup proxies that exist on PATH but fail at
///    runtime because the active toolchain is missing the component.
///
/// 2. **Fallback — spawn with the language's actual launch args**: the
///    `vscode-langservers-extracted` package (json / css / html servers)
///    is a **stdio-mode LSP server with no `--version` flag**. Calling
///    `--version` on it throws `Connection input stream is not set` and
///    exits 1, so stage 1 always fails for it. As a fallback we spawn
///    the binary with the args configured in `lsp_servers.json` (e.g.
///    `--stdio`) and a **piped stdin**, then wait 500 ms:
///
///    - **still alive** → it's a stdio-mode LSP server waiting for a
///      client connection → runnable
///    - **already exited** → the binary can't run with the configured
///      args (wrong version, missing native lib, etc.) → not runnable
///
///    A piped stdin is **required**: when stdin is `/dev/null` these
///    servers immediately detect "no input stream" and exit. With a
///    real pipe they sit in their event loop awaiting LSP frames.
///
/// `kill_on_drop(true)` ensures the probe process is reaped on every
/// exit path (success, timeout, panic, cancellation) — no zombie.
async fn verify_command_runnable(command: &str, args: &[String]) -> bool {
    use std::process::Stdio;

    // Stage 1: --version
    if let Ok(Ok(output)) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::process::Command::new(command)
            .arg("--version")
            .output(),
    )
    .await
        && output.status.success()
    {
        return true;
    }

    // Stage 2: spawn with the real launch args + piped stdin.
    let mut child = match tokio::process::Command::new(command)
        .args(args)
        .stdin(Stdio::piped()) // <-- critical for vscode-langservers-extracted
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false, // command cannot be spawned at all
    };

    // CRITICAL: explicitly take the stdin handle and keep it alive for
    // the duration of the probe. If we let `child` own stdin, the pipe's
    // parent end would be dropped (either when the timeout future is
    // dropped, or earlier in some tokio edge cases), the child would
    // see EOF on stdin, and any stdio-mode LSP server would exit
    // immediately. Holding `_stdin` open keeps the pipe alive.
    let mut _stdin = child.stdin.take();

    // 500ms grace window: long enough for slow startup of native LSP
    // servers (jvm-based jdtls, etc. — those still go through stage 1
    // successfully), short enough that the status endpoint stays snappy.
    // On timeout the future is dropped; kill_on_drop reaps the child.
    let exited_within_window =
        tokio::time::timeout(std::time::Duration::from_millis(500), child.wait())
            .await
            .is_ok(); // Ok = exited within 500ms → broken or wrong args

    // Explicitly close stdin before returning so the child sees EOF
    // immediately (avoids leaving a zombie waiting for a parent that
    // will never write). If `child.wait()` already completed the process
    // is gone and this is a no-op.
    _stdin.take();
    drop(_stdin);

    !exited_within_window // still alive after 500ms → stdio LSP server, OK
}

/// Check if a command exists on the system PATH.
///
/// On Windows, also tries `.exe`, `.cmd`, `.bat` extensions.
/// Returns the actual filename found (with extension), which is critical
/// for `Command::new` to spawn successfully on Windows.
fn find_on_path(cmd: &str) -> Option<String> {
    // On Windows, also try with .exe / .cmd / .bat extensions
    let candidates: Vec<String> = if cfg!(windows) {
        vec![
            format!("{}.exe", cmd),
            format!("{}.cmd", cmd),
            format!("{}.bat", cmd),
            cmd.to_string(),
        ]
    } else {
        vec![cmd.to_string()]
    };

    // Get PATH from environment
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for name in &candidates {
            let full = dir.join(name);
            if full.is_file() {
                return Some(name.clone());
            }
        }
    }
    None
}

// ── Query parameters ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LspQuery {
    /// Agent ID to resolve workspace directory
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Optional workspace ID for additional workspace directories
    #[serde(default)]
    pub workspace_id: Option<String>,
}

// ── HTTP API: LSP server list & install scripts ─────────────────────────

/// `GET /api/lsp/servers` — list all configured LSP servers.
///
/// Returns the full `lsp_servers.json` content so the Desktop App
/// can display install hints, available languages, etc.
/// Works for both local and remote Gateway scenarios.
pub async fn lsp_servers_list(State(_state): State<AppState>) -> Json<LspServersConfig> {
    let cfg = lsp_servers_config();
    Json(cfg.clone())
}

/// `GET /api/lsp/status` — report per-language installation status.
///
/// Returns one [`LspServerStatusEntry`] for every language configured in
/// `lsp_servers.json`. The `installed` flag is computed by trying each
/// candidate command against the current `PATH`, so it reflects the same
/// source of truth as the LSP WebSocket handler.
///
/// This endpoint exists so the Harness UI can show "installed" / "not
/// installed" badges and disable the Install button for already-installed
/// servers, without having to spawn N WebSocket handshakes on mount.
pub async fn lsp_status_list(State(_state): State<AppState>) -> Json<Vec<LspServerStatusEntry>> {
    Json(compute_lsp_status().await)
}

/// Pure helper: compute installation status for every configured language.
///
/// Extracted from [`lsp_status_list`] so unit tests can validate the result
/// shape without constructing an [`AppState`].
async fn compute_lsp_status() -> Vec<LspServerStatusEntry> {
    let cfg = lsp_servers_config();
    // Iterate in deterministic (sorted) order so the response is stable
    // across calls — makes UI diffing and testing predictable.
    let mut entries: Vec<LspServerStatusEntry> = Vec::new();
    for lang in cfg.servers.keys() {
        let entry = match resolve_lsp_command(lang).await {
            Some(spec) => LspServerStatusEntry {
                language: spec.language,
                installed: true,
                command: Some(spec.command),
            },
            None => LspServerStatusEntry {
                language: lang.clone(),
                installed: false,
                command: None,
            },
        };
        entries.push(entry);
    }
    entries.sort_by(|a, b| a.language.cmp(&b.language));
    entries
}

/// `GET /api/lsp/install/{language}` — return the install script content.
///
/// Returns the script file for the given language.
/// On Windows returns `.ps1`, on other platforms `.sh`.
/// If no install script is configured for the language, returns 404.
pub async fn lsp_install_script(
    Path(language): Path<String>,
    State(_state): State<AppState>,
) -> impl IntoResponse {
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

    // Determine extension based on platform
    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let filename = format!("{}.{}", script_name, ext);

    // Search for the script file
    let content = load_install_script(&filename);
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
///
/// Finds the install script on disk and spawns it as a child process.
/// Returns the script's stdout and stderr when it completes.
pub async fn lsp_install_run(
    Path(language): Path<String>,
    State(_state): State<AppState>,
) -> impl IntoResponse {
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

    // Determine script path
    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let filename = format!("{}.{}", script_name, ext);
    let script_path = match find_install_script_path(&filename) {
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

    // Spawn the script: powershell on Windows, bash on Unix
    let result: std::io::Result<std::process::Output> = if cfg!(windows) {
        std::process::Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-NoProfile", "-File"])
            .arg(&script_path)
            .output()
    } else {
        std::process::Command::new("bash")
            .arg(&script_path)
            .output()
    };

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let success = output.status.success();

            tracing::info!(
                "[LSP] Install script for '{}' completed — success: {}, stdout_len: {}, stderr_len: {}",
                canonical,
                success,
                stdout.len(),
                stderr.len()
            );

            let code = if success {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (
                code,
                Json(serde_json::json!({
                    "language": canonical,
                    "success": success,
                    "exit_code": output.status.code(),
                    "stdout": stdout,
                    "stderr": stderr,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(
                "[LSP] Failed to run install script for '{}': {}",
                canonical,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to run install script: {}", e),
                    "code": 500
                })),
            )
                .into_response()
        }
    }
}

/// Find an install script file path on disk (without reading its contents).
/// Uses the same search order as `load_install_script`.
fn find_install_script_path(filename: &str) -> Option<std::path::PathBuf> {
    let candidates = build_install_script_candidates(filename);
    candidates.into_iter().find(|p| p.exists())
}

/// Load an install script file from the lsp_install directory.
///
/// Search order:
///   1. `$CARGO_MANIFEST_DIR/../../assets/lsp_install/{filename}` (dev)
///   2. `{exe_dir}/lsp_install/{filename}` (installer-provided)
///   3. `{cwd}/lsp_install/{filename}` (dev convenience)
fn load_install_script(filename: &str) -> Option<String> {
    let candidates = build_install_script_candidates(filename);
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

fn build_install_script_candidates(filename: &str) -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    // 0. CLI / env override: --lsp-config-dir / ACOWORK_LSP_CONFIG_DIR
    //    Desktop App passes its Tauri resource_dir here.
    if let Ok(config_dir) = std::env::var("ACOWORK_LSP_CONFIG_DIR") {
        let path = std::path::PathBuf::from(&config_dir)
            .join("lsp_install")
            .join(filename);
        if path.exists() {
            candidates.push(path);
        }
    }

    // 1. CARGO_MANIFEST_DIR ../../assets/lsp_install/ (dev)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let path = std::path::PathBuf::from(&manifest_dir)
            .join("..")
            .join("..")
            .join("assets")
            .join("lsp_install")
            .join(filename);
        if path.exists() {
            candidates.push(path);
        }
    }

    // 2. Same directory as executable / lsp_install/ (installer-provided)
    //    In remote mode, install scripts are co-installed with Gateway.
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

// ── WebSocket handler ──────────────────────────────────────────────────

/// `GET /lsp/{language}` — WebSocket upgrade for LSP relay
///
/// Opens a WebSocket connection, gets/spawns an LSP process from the pool,
/// and relays bytes bidirectionally.
pub async fn lsp_handler(
    ws: WebSocketUpgrade,
    Path(language): Path<String>,
    Query(query): Query<LspQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let lang_lower = language.to_lowercase();

    // Resolve workspace root
    tracing::info!(
        "[LSP] lsp_handler — language='{}', agent_id='{}', workspace_id='{}'",
        lang_lower,
        query.agent_id.as_deref().unwrap_or("(none)"),
        query.workspace_id.as_deref().unwrap_or("(none)")
    );
    let workspace_root = match resolve_workspace_root_for_lsp(&state, &query).await {
        Ok(root) => {
            tracing::info!("[LSP] lsp_handler — workspace_root resolved: '{}'", root);
            root
        }
        Err((status, msg)) => {
            tracing::error!(
                "[LSP] lsp_handler — workspace_root resolution FAILED: {} (status {})",
                msg,
                status.as_u16()
            );
            let err_json = serde_json::json!({ "error": msg, "code": status.as_u16() });
            return (status, axum::Json(err_json)).into_response();
        }
    };

    // Resolve LSP command
    let spec = match resolve_lsp_command(&lang_lower).await {
        Some(spec) => {
            tracing::info!(
                "[LSP] lsp_handler — LSP command resolved: '{}' args={:?} for language '{}'",
                spec.command,
                spec.args,
                lang_lower
            );
            spec
        }
        None => {
            tracing::warn!(
                "[LSP] lsp_handler — No LSP command found for language '{}'",
                lang_lower
            );
            // Get install_hint from config (even if command not on PATH)
            let lang_lower2 = lang_lower.clone();
            let canonical2 = canonical_language(&lang_lower2);
            let cfg = lsp_servers_config();
            let install_hint = cfg
                .servers
                .get(canonical2)
                .map(|e| e.install_hint.as_str())
                .unwrap_or("Install the LSP server and ensure it is on PATH");
            let msg = format!(
                "No LSP server found for language: {}. {}",
                language, install_hint
            );
            let err_json = serde_json::json!({ "error": msg, "code": 400u16 });
            return (StatusCode::BAD_REQUEST, axum::Json(err_json)).into_response();
        }
    };

    tracing::info!(
        "[LSP] Upgrading WebSocket for language='{}', cmd='{}' args={:?}, workspace='{}'",
        lang_lower,
        spec.command,
        spec.args,
        workspace_root
    );

    let pool = Arc::clone(&state.lsp_pool);
    ws.on_upgrade(move |socket| lsp_relay(socket, spec, workspace_root, pool))
}

/// Bidirectional LSP relay: WebSocket ↔ pooled LSP process
///
/// Uses the process pool to get/spawn an LSP process. When the WebSocket
/// disconnects, the LSP process stays alive for future reconnections.
///
/// ## Initialize handshake caching
///
/// LSP protocol only allows `initialize` once per server lifetime. When
/// a subsequent WebSocket client reconnects to an already-initialized
/// pooled process, the relay intercepts `initialize`/`initialized` messages:
/// - `initialize`: synthesises a response from the cached `InitializeResult`
///   (with the request `id` substituted) and sends it directly to the WS,
///   WITHOUT forwarding to the LSP process.
/// - `initialized`: suppressed (not forwarded to the already-initialized LSP).
///
/// All other messages pass through transparently.
async fn lsp_relay(
    socket: WebSocket,
    spec: LspServerSpec,
    workspace_root: String,
    pool: Arc<LspPool>,
) {
    tracing::info!(
        "[LSP] relay — entering lsp_relay for cmd='{}' args={:?}, workspace='{}'",
        spec.command,
        spec.args,
        workspace_root
    );

    // Get or spawn from pool
    let entry = match pool
        .get_or_spawn(&spec.command, &spec.args, &workspace_root)
        .await
    {
        Ok(e) => {
            tracing::info!(
                "[LSP] relay — pool entry obtained for '{}', PID={}, active_clients={}",
                spec.command,
                e.pid,
                e.active_clients.load(std::sync::atomic::Ordering::Relaxed)
            );
            e
        }
        Err(err) => {
            tracing::error!(
                "[LSP] relay — Failed to get/spawn '{}': {}",
                spec.command,
                err
            );
            return;
        }
    };

    let stdin_tx = entry.stdin_tx.clone();
    let mut stdout_rx = entry.stdout_tx.subscribe();
    let entry_for_send = Arc::clone(&entry);
    let entry_for_recv = Arc::clone(&entry);

    // Synthesis channel: recv_task sends synthesised init responses here,
    // send_task consumes them and delivers directly to the WebSocket.
    let (synth_tx, mut synth_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Task: LSP stdout (broadcast) + synthesised messages → WebSocket
    let cmd_for_send = spec.command.clone();
    let mut send_task = tokio::spawn(async move {
        tracing::info!(
            "[LSP] relay — stdout→WS task started for '{}'",
            cmd_for_send
        );
        loop {
            tokio::select! {
                result = stdout_rx.recv() => {
                    match result {
                        Ok(msg) => {
                            // Cache the first InitializeResult for future reconnects
                            if is_initialize_result(&msg) {
                                let mut cached = entry_for_send.init_result.lock().await;
                                if cached.is_none() {
                                    *cached = Some(msg.clone());
                                    tracing::info!(
                                        "[LSP] relay — cached InitializeResult for '{}' ({} bytes)",
                                        cmd_for_send,
                                        msg.len()
                                    );
                                }
                            }
                            let method_hint = extract_method_hint(&msg);
                            // Log workspace-level & registration messages at info level
                            if method_hint.starts_with("workspace/") || method_hint == "client/registerFeature" || method_hint == "client/unregisterFeature" || method_hint == "(response)" {
                                tracing::info!(
                                    "[LSP] relay → WS: '{}' method='{}' len={}",
                                    cmd_for_send,
                                    method_hint,
                                    msg.len()
                                );
                            } else {
                                tracing::debug!(
                                    "[LSP] relay → WS: '{}' method='{}' len={}",
                                    cmd_for_send,
                                    method_hint,
                                    msg.len()
                                );
                            }
                            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                                tracing::warn!("[LSP] relay → WS: send failed for '{}', breaking", cmd_for_send);
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                "[LSP] WebSocket client lagged {} messages for '{}'",
                                n, cmd_for_send
                            );
                        }
                        Err(_) => {
                            tracing::warn!("[LSP] relay — stdout channel closed for '{}', breaking", cmd_for_send);
                            break;
                        }
                    }
                }
                Some(synth_msg) = synth_rx.recv() => {
                    tracing::debug!(
                        "[LSP] relay → WS (synth): '{}' len={}",
                        cmd_for_send,
                        synth_msg.len()
                    );
                    if ws_tx.send(Message::Text(synth_msg.into())).await.is_err() {
                        tracing::warn!("[LSP] relay → WS (synth): send failed for '{}', breaking", cmd_for_send);
                        break;
                    }
                }
            }
        }
        // Attempt to send close frame
        let _ = ws_tx.send(Message::Close(None)).await;
        tracing::info!("[LSP] relay — stdout→WS task ended for '{}'", cmd_for_send);
    });

    // Task: WebSocket → LSP stdin (via mpsc)
    // Intercepts initialize/initialized when reconnecting to an already-initialized process.
    let cmd_for_recv = spec.command.clone();
    let mut recv_task = tokio::spawn(async move {
        tracing::info!("[LSP] relay — WS→stdin task started for '{}'", cmd_for_recv);
        // Tracks whether THIS connection's initialize was synthesised (i.e.
        // we reused a cached InitializeResult instead of forwarding to the
        // LSP process). Only when true do we suppress the `initialized`
        // notification — otherwise the LSP would never enter running state.
        let mut synthesized_init = false;
        while let Some(msg) = ws_rx.next().await {
            let text: String = match msg {
                Ok(Message::Text(t)) => {
                    let method_hint = extract_method_hint(t.as_str());
                    // Log workspace-level & registration messages at info level
                    // for diagnosing jdtls workspace/symbol issues.
                    if method_hint.starts_with("workspace/")
                        || method_hint == "client/registerFeature"
                        || method_hint == "client/unregisterFeature"
                    {
                        tracing::info!(
                            "[LSP] relay WS → stdin: '{}' method='{}' len={}",
                            cmd_for_recv,
                            method_hint,
                            t.len()
                        );
                    } else {
                        tracing::debug!(
                            "[LSP] relay WS → stdin: '{}' method='{}' len={}",
                            cmd_for_recv,
                            method_hint,
                            t.len()
                        );
                    }
                    t.to_string()
                }
                Ok(Message::Binary(data)) => match String::from_utf8(data.to_vec()) {
                    Ok(s) => s,
                    Err(_) => continue,
                },
                Ok(Message::Close(_)) => break,
                _ => continue,
            };

            // Intercept initialize request — if the LSP process is already
            // initialized, synthesise the response without forwarding.
            if is_initialize_request(&text) {
                let cached = entry_for_recv.init_result.lock().await;
                if let Some(ref cached_result) = *cached {
                    let req_id = extract_jsonrpc_id(&text);
                    let response = substitute_jsonrpc_id(cached_result, &req_id);
                    tracing::info!(
                        "[LSP] relay — synthesised init response for '{}' (reusing cached)",
                        cmd_for_recv
                    );
                    drop(cached);
                    synthesized_init = true;
                    let _ = synth_tx.send(response);
                    continue; // Do NOT forward initialize to the LSP process
                }
                drop(cached);
            }

            // Intercept initialized notification — suppress ONLY if we
            // synthesised the initialize response for this connection.
            // On the first connection (synthesized_init=false), the
            // initialized MUST reach the LSP process so it enters running state.
            if is_initialized_notification(&text) && synthesized_init {
                tracing::debug!(
                    "[LSP] relay — suppressed 'initialized' for '{}' (init was synthesised)",
                    cmd_for_recv
                );
                continue; // Do NOT forward initialized to the LSP process
            }

            if stdin_tx.send(text).is_err() {
                tracing::warn!("[LSP] stdin channel closed for '{}'", cmd_for_recv);
                break;
            }
        }
    });

    // Wait for either task to complete
    let cmd_for_log = spec.command.clone();
    tokio::select! {
        r = &mut send_task => tracing::info!("[LSP] relay — send_task completed first for '{}' (result: {:?})", cmd_for_log, r),
        r = &mut recv_task => tracing::info!("[LSP] relay — recv_task completed first for '{}' (result: {:?})", cmd_for_log, r),
    }

    // Client disconnected — mark in pool (process stays alive)
    pool.client_disconnected(&spec.command, &spec.args, &workspace_root)
        .await;
    tracing::info!(
        "[LSP] relay — WebSocket client disconnected for '{}' in '{}'",
        spec.command,
        workspace_root
    );
}

/// Extract the JSON-RPC "method" field from a message for diagnostic logging.
/// Returns "(no method)" if the field is absent or parsing fails.
fn extract_method_hint(msg: &str) -> String {
    // Quick substring search — avoid full JSON parse for logging only.
    // Look for `"method":"xxx"` pattern.
    if let Some(idx) = msg.find("\"method\":") {
        let rest = &msg[idx + 9..]; // skip past "method":" 
        // Find the quoted value
        if let Some(open) = rest.find('"')
            && let Some(close) = rest[open + 1..].find('"') {
                return rest[open + 1..open + 1 + close].to_string();
            }
    }
    // Check if it's a response (has "id" but no "method")
    if msg.contains("\"id\":") && !msg.contains("\"method\":") {
        return "(response)".to_string();
    }
    "(no method)".to_string()
}

// ── Initialize handshake helpers ──────────────────────────────────────

/// Check if a message is an LSP `initialize` request.
fn is_initialize_request(msg: &str) -> bool {
    msg.contains("\"method\":\"initialize\"")
}

/// Check if a message is an LSP `initialized` notification.
fn is_initialized_notification(msg: &str) -> bool {
    msg.contains("\"method\":\"initialized\"")
}

/// Check if a message is an LSP `InitializeResult` (response with capabilities).
/// Responses have no "method" field but contain "id" and "result".
fn is_initialize_result(msg: &str) -> bool {
    !msg.contains("\"method\":") && msg.contains("\"id\":") && msg.contains("\"capabilities\"")
}

/// Extract the JSON-RPC `id` field from a message.
/// Returns "0" as fallback if the field cannot be parsed.
fn extract_jsonrpc_id(msg: &str) -> String {
    // Find `"id":` followed by a number or quoted string
    if let Some(idx) = msg.find("\"id\":") {
        let rest = &msg[idx + 5..]; // skip past "id":
        let rest = rest.trim_start();
        if let Some(stripped) = rest.strip_prefix('"') {
            // String id: "id":"abc"
            if let Some(close) = stripped.find('"') {
                return rest[..close + 2].to_string();
            }
        } else {
            // Numeric id: "id":42
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            return rest[..end].to_string();
        }
    }
    "0".to_string()
}

/// Substitute the `id` field in a cached JSON-RPC response with a new id.
///
/// The cached InitializeResult has the `id` from the first handshake;
/// subsequent clients send `initialize` with a different `id`, so we
/// must patch the response to match.
fn substitute_jsonrpc_id(msg: &str, new_id: &str) -> String {
    if let Some(idx) = msg.find("\"id\":") {
        let before = &msg[..idx + 5]; // include "id":
        let rest = &msg[idx + 5..];
        let rest_trimmed = rest.trim_start();
        let whitespace = &rest[..rest.len() - rest_trimmed.len()];

        if let Some(stripped) = rest_trimmed.strip_prefix('"') {
            // String id — skip to closing quote
            if let Some(close) = stripped.find('"') {
                let after = &rest_trimmed[close + 2..]; // after closing quote
                return format!("{}{}{}{}", before, whitespace, new_id, after);
            }
        } else {
            // Numeric id — skip digits
            let end = rest_trimmed
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest_trimmed.len());
            let after = &rest_trimmed[end..];
            return format!("{}{}{}{}", before, whitespace, new_id, after);
        }
    }
    // Fallback: return unchanged
    msg.to_string()
}

/// Parse `Content-Length: N` from a header line.
pub fn parse_content_length(line: &str) -> Option<usize> {
    let line = line.trim();
    let prefix = "Content-Length:";
    if let Some(rest) = line.strip_prefix(prefix) {
        rest.trim().parse().ok()
    } else if let Some(rest) = line.strip_prefix("Content-length:") {
        // Some LSP servers use lowercase 'l'
        rest.trim().parse().ok()
    } else {
        None
    }
}

// spawn_lsp removed — spawning is now handled by LspPool::spawn_pooled

// ── Workspace root resolution ─────────────────────────────────────────

/// Resolve workspace root directory for LSP process.
///
/// If `agent_id` is provided, look up the agent's workspace from running agents.
/// Otherwise, the LSP process runs in the current directory (fallback).
async fn resolve_workspace_root_for_lsp(
    state: &AppState,
    query: &LspQuery,
) -> Result<String, (StatusCode, String)> {
    // If no agent_id, use current directory as fallback
    let Some(agent_id) = &query.agent_id else {
        return Ok(".".to_string());
    };

    let gw = state.gateway_state.read().await;
    let info = gw.running_agents.get(agent_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "Agent not running — cannot resolve workspace".to_string(),
        )
    })?;

    let ws_id = query.workspace_id.as_deref().unwrap_or("");
    if ws_id.is_empty() || ws_id == "__agent_home__" {
        Ok(info.workspace.clone())
    } else {
        // Look up in workspace_config_json
        if let Some(json) = &info.workspace_config_json {
            #[derive(Deserialize)]
            struct AdditionalDir {
                id: String,
                path: String,
            }
            #[derive(Deserialize)]
            struct WsConfig {
                #[serde(default)]
                additional_dirs: Vec<AdditionalDir>,
            }

            if let Ok(cfg) = serde_json::from_str::<WsConfig>(json) {
                for dir in &cfg.additional_dirs {
                    if dir.id == ws_id {
                        return Ok(dir.path.clone());
                    }
                }
            }
        }

        Err((
            StatusCode::NOT_FOUND,
            format!("Workspace directory not found: {}", ws_id),
        ))
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content_length_standard() {
        assert_eq!(parse_content_length("Content-Length: 42"), Some(42));
        assert_eq!(parse_content_length("Content-Length:0"), Some(0));
        assert_eq!(parse_content_length("Content-Length: 1234\r\n"), Some(1234));
    }

    #[test]
    fn test_parse_content_length_lowercase() {
        // Some LSP servers use lowercase 'l'
        assert_eq!(parse_content_length("Content-length: 99"), Some(99));
    }

    #[test]
    fn test_parse_content_length_invalid() {
        assert_eq!(parse_content_length("Content-Type: application/json"), None);
        assert_eq!(parse_content_length("X-Custom: 42"), None);
        assert_eq!(parse_content_length(""), None);
    }

    #[test]
    fn test_parse_content_length_not_a_number() {
        assert_eq!(parse_content_length("Content-Length: abc"), None);
    }

    #[tokio::test]
    async fn test_resolve_lsp_command_known_languages() {
        // These may return None if the binary is not on PATH,
        // but should not panic.
        let _ = resolve_lsp_command("rust").await;
        let _ = resolve_lsp_command("python").await;
        let _ = resolve_lsp_command("go").await;
    }

    #[tokio::test]
    async fn test_resolve_lsp_command_unknown_language() {
        assert!(resolve_lsp_command("brainfuck").await.is_none());
        assert!(resolve_lsp_command("").await.is_none());
    }

    #[tokio::test]
    async fn test_resolve_lsp_command_case_insensitive() {
        // Both "Rust" and "rust" should resolve to the same canonical language
        let lower = resolve_lsp_command("rust").await;
        let upper = resolve_lsp_command("Rust").await;
        // Compare the language field (canonical name) rather than full struct
        // since LspServerSpec doesn't derive PartialEq
        let lower_lang = lower.map(|s| s.language.clone());
        let upper_lang = upper.map(|s| s.language.clone());
        assert_eq!(lower_lang, upper_lang);
    }

    #[test]
    fn test_canonical_language_aliases() {
        assert_eq!(canonical_language("js"), "typescript");
        assert_eq!(canonical_language("javascript"), "typescript");
        assert_eq!(canonical_language("yml"), "yaml");
        assert_eq!(canonical_language("scss"), "css");
        assert_eq!(canonical_language("less"), "css");
        assert_eq!(canonical_language("cpp"), "c");
        assert_eq!(canonical_language("c++"), "c");
        assert_eq!(canonical_language("md"), "markdown");
        // Canonical names pass through unchanged
        assert_eq!(canonical_language("rust"), "rust");
        assert_eq!(canonical_language("python"), "python");
    }

    #[test]
    fn test_lsp_servers_config_loads() {
        // Verify the config is loadable (either from file or defaults)
        let cfg = lsp_servers_config();
        assert!(cfg.servers.contains_key("rust"));
        assert!(cfg.servers.contains_key("python"));
        assert!(cfg.servers.contains_key("go"));
        assert!(cfg.version == 1);
    }

    #[test]
    fn test_find_on_path_known_command() {
        // On Windows, `cmd` should always be on PATH
        #[cfg(windows)]
        assert!(find_on_path("cmd").is_some());
        // On Unix, `ls` should always be on PATH
        #[cfg(not(windows))]
        assert!(find_on_path("ls").is_some());
    }

    #[test]
    fn test_find_on_path_nonexistent() {
        assert!(find_on_path("this_command_definitely_does_not_exist_12345").is_none());
    }

    /// `compute_lsp_status` must return one entry per configured language.
    ///
    /// The exact `installed` flag depends on the host PATH, so we only
    /// assert structural invariants: count matches the config, languages
    /// are unique, and each entry's `installed`/`command` fields are
    /// consistent with each other.
    #[tokio::test]
    async fn test_compute_lsp_status_invariants() {
        let cfg = lsp_servers_config();
        let status = compute_lsp_status().await;

        // Count and content match the config.
        assert_eq!(status.len(), cfg.servers.len());

        // Languages are unique.
        let mut langs: Vec<&str> = status.iter().map(|s| s.language.as_str()).collect();
        langs.sort();
        let mut deduped = langs.clone();
        deduped.dedup();
        assert_eq!(langs, deduped, "status contains duplicate languages");

        // `command` is set iff `installed` is true.
        for entry in &status {
            assert_eq!(
                entry.installed,
                entry.command.is_some(),
                "language '{}': installed={} but command={:?}",
                entry.language,
                entry.installed,
                entry.command
            );
        }

        // Result is sorted by language (deterministic response).
        let mut sorted = status.clone();
        sorted.sort_by(|a, b| a.language.cmp(&b.language));
        assert_eq!(status, sorted, "status list is not sorted by language");
    }

    /// `LspServerStatusEntry` must serialize without the `command` field
    /// when it's `None`, keeping the JSON payload minimal.
    #[test]
    fn test_lsp_server_status_entry_serde_skips_none_command() {
        let entry = LspServerStatusEntry {
            language: "brainfuck".to_string(),
            installed: false,
            command: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("command"), "command field must be skipped when None: {}", json);
        assert!(json.contains("\"installed\":false"));

        let entry_installed = LspServerStatusEntry {
            language: "rust".to_string(),
            installed: true,
            command: Some("rust-analyzer".to_string()),
        };
        let json_installed = serde_json::to_string(&entry_installed).unwrap();
        assert!(json_installed.contains("\"command\":\"rust-analyzer\""));
        assert!(json_installed.contains("\"installed\":true"));
    }

    // ── verify_command_runnable two-stage probe ──────────────────────────
    //
    // We exercise the function against three synthetic shell scripts that
    // mimic the three real-world cases the probe has to handle:
    //
    //   1. A normal CLI tool that supports `--version` (rust-analyzer,
    //      clangd, yaml-language-server, typescript-language-server, ...).
    //   2. A stdio-mode LSP server with no `--version` flag
    //      (vscode-langservers-extracted: json / css / html) that
    //      sits in `read` when launched with `--stdio` + piped stdin.
    //   3. A broken binary that fails on every invocation.
    //
    // We also verify the spawn-failure path (command does not exist).

    /// Write a bash script to a temp file with the given body and make it
    /// executable. Returns the absolute path.
    fn write_fake_binary(body: &str) -> std::path::PathBuf {
        let dir = ::tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake_lsp.sh");
        std::fs::write(&path, format!("#!/usr/bin/env bash\n{}\n", body))
            .expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        // Leak the tempdir so the script outlives the test. Background
        // processes spawned by the test (case 2) need the file to remain
        // on disk; once the timeout drops the future, kill_on_drop
        // reaps the child and the leaked dir is freed at process exit.
        let path_clone = path.clone();
        std::mem::forget(dir);
        path_clone
    }

    #[tokio::test]
    async fn verify_runnable_stage1_version_succeeds() {
        // Case 1: --version exits 0 → runnable, regardless of what
        // stage 2 would do.
        let path = write_fake_binary(
            "if [ \"$1\" = \"--version\" ]; then echo 1.0; exit 0; fi\n\
             # If somehow reached without --version, also exit 0 (shouldn't happen).\n\
             exit 0",
        );
        let path_str = path.to_str().unwrap();
        assert!(
            verify_command_runnable(path_str, &[]).await,
            "a binary whose --version exits 0 must be reported runnable"
        );
    }

    #[tokio::test]
    async fn verify_runnable_stage2_stdio_lsp_server() {
        // Case 2: --version exits 1, but with --stdio + piped stdin the
        // process sits in `read` awaiting input — exactly the
        // vscode-langservers-extracted behaviour.
        let path = write_fake_binary(
            "if [ \"$1\" = \"--version\" ]; then exit 1; fi\n\
             if [ \"$1\" = \"--stdio\" ]; then\n\
                 # Block on stdin, mirroring how a real stdio LSP server\n\
                 # waits for LSP frames. The probe's piped stdin keeps the\n\
                 # pipe open, so this read never returns and the process\n\
                 # stays alive past the 500ms grace window.\n\
                 read -r _ || exit 0\n\
             fi\n\
             exit 1",
        );
        let path_str = path.to_str().unwrap();
        let args = vec!["--stdio".to_string()];
        assert!(
            verify_command_runnable(path_str, &args).await,
            "a stdio-mode LSP server (no --version, blocks on stdin) \
             must be reported runnable via the spawn fallback"
        );
    }

    #[tokio::test]
    async fn verify_runnable_broken_binary_returns_false() {
        // Case 3: --version fails AND with the configured args the
        // process exits within 500ms. Broken.
        let path = write_fake_binary(
            "if [ \"$1\" = \"--version\" ]; then exit 1; fi\n\
             # Stage 2 spawn: also exit 1 immediately.\n\
             exit 1",
        );
        let path_str = path.to_str().unwrap();
        let args = vec!["--stdio".to_string()];
        assert!(
            !verify_command_runnable(path_str, &args).await,
            "a binary that fails --version and also exits within 500ms \
             with the configured args must be reported not runnable"
        );
    }

    #[tokio::test]
    async fn verify_runnable_nonexistent_command_returns_false() {
        // Stage 1 fails (no such command, exec error), stage 2 spawn
        // also fails (the OS returns ENOENT). Must be false.
        assert!(
            !verify_command_runnable(
                "acowork_lsp_definitely_not_a_real_binary_xyz",
                &[]
            )
            .await,
            "a nonexistent command must be reported not runnable"
        );
    }
}
