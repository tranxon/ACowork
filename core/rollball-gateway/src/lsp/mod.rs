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
//! ## Configuration
//!
//! LSP server specifications are loaded from `lsp_servers.json` at startup.
//! The file is searched in multiple locations (see `build_config_candidates`).
//! If no file is found, built-in defaults are used as a fallback.
//! Language alias mapping (e.g. `js→typescript`) is kept in code (protocol logic).

pub mod pool;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
    Json,
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
    pub args: Vec<String>,
    /// One-line install hint shown to the user.
    pub install_hint: String,
    /// Name of the install script file (e.g. "rust" → assets/lsp_install/rust.sh).
    /// Null means no dedicated script; use `install_hint` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_script: Option<String>,
    /// Human-readable description.
    pub description: String,
}

/// Top-level structure for `lsp_servers.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServersConfig {
    /// Schema version (for future migration).
    pub version: u32,
    /// Language-keyed server entries (canonical language names only).
    pub servers: std::collections::HashMap<String, LspServerEntry>,
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
        load_lsp_servers_from_file()
            .unwrap_or_else(|| {
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
                Ok(content) => {
                    match serde_json::from_str::<LspServersConfig>(&content) {
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
                    }
                }
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

    // 0. CLI / env override: --lsp-config-dir / ROLLBALL_LSP_CONFIG_DIR
    //    This is the highest-priority path — Desktop App passes its Tauri
    //    resource_dir here. In remote mode (standalone Gateway), this is
    //    unset and Gateway falls back to exe_dir below.
    if let Ok(config_dir) = std::env::var("ROLLBALL_LSP_CONFIG_DIR") {
        let path = std::path::PathBuf::from(&config_dir).join("lsp_servers.json");
        if path.exists() {
            candidates.push(path);
        }
    }

    // 1. CARGO_MANIFEST_DIR ../../assets/ (dev and test via cargo)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let path = std::path::PathBuf::from(&manifest_dir)
            .join("..").join("..").join("assets").join("lsp_servers.json");
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

    servers.insert("rust".into(), LspServerEntry {
        candidates: vec!["rust-analyzer".into()],
        args: vec![],
        install_hint: "rustup component add rust-analyzer".into(),
        install_script: Some("rust".into()),
        description: "Rust language server (defaults to stdio, no --stdio flag)".into(),
    });
    servers.insert("python".into(), LspServerEntry {
        candidates: vec!["pylsp".into(), "pyright-langserver".into(), "python-lsp-server".into()],
        args: vec!["--stdio".into()],
        install_hint: "pip install python-lsp-server".into(),
        install_script: Some("python".into()),
        description: "Python language server".into(),
    });
    servers.insert("typescript".into(), LspServerEntry {
        candidates: vec!["typescript-language-server".into(), "typescript-language-server.cmd".into()],
        args: vec!["--stdio".into()],
        install_hint: "npm install -g typescript-language-server typescript".into(),
        install_script: Some("typescript".into()),
        description: "TypeScript/JavaScript language server".into(),
    });
    servers.insert("go".into(), LspServerEntry {
        candidates: vec!["gopls".into()],
        args: vec!["serve".into()],
        install_hint: "go install golang.org/x/tools/gopls@latest".into(),
        install_script: Some("go".into()),
        description: "Go language server (uses 'serve' subcommand)".into(),
    });
    servers.insert("c".into(), LspServerEntry {
        candidates: vec!["clangd".into()],
        args: vec![],
        install_hint: "Install clangd: https://clangd.llvm.org/installation".into(),
        install_script: Some("clangd".into()),
        description: "C/C++ language server (defaults to stdio)".into(),
    });
    servers.insert("json".into(), LspServerEntry {
        candidates: vec!["vscode-json-languageserver".into(), "json-languageserver".into()],
        args: vec!["--stdio".into()],
        install_hint: "npm install -g vscode-json-languageserver".into(),
        install_script: None,
        description: "JSON language server".into(),
    });
    servers.insert("yaml".into(), LspServerEntry {
        candidates: vec!["yaml-language-server".into()],
        args: vec!["--stdio".into()],
        install_hint: "npm install -g yaml-language-server".into(),
        install_script: None,
        description: "YAML language server".into(),
    });
    servers.insert("html".into(), LspServerEntry {
        candidates: vec!["vscode-html-languageserver".into(), "html-languageserver".into()],
        args: vec!["--stdio".into()],
        install_hint: "npm install -g vscode-html-languageserver".into(),
        install_script: None,
        description: "HTML language server".into(),
    });
    servers.insert("css".into(), LspServerEntry {
        candidates: vec!["vscode-css-languageserver".into(), "css-languageserver".into()],
        args: vec!["--stdio".into()],
        install_hint: "npm install -g vscode-css-languageserver".into(),
        install_script: None,
        description: "CSS/SCSS/Less language server".into(),
    });
    servers.insert("markdown".into(), LspServerEntry {
        candidates: vec!["marksman".into()],
        args: vec![],
        install_hint: "Install marksman: https://github.com/artempyanykh/marksman".into(),
        install_script: None,
        description: "Markdown language server (defaults to stdio)".into(),
    });

    LspServersConfig { version: 1, servers }
}

// ── Resolve LSP command ────────────────────────────────────────────────

/// Resolve the LSP server command and launch arguments for a given language.
///
/// Looks up the canonical language in `lsp_servers.json`, then tries each
/// candidate on PATH. Returns `LspServerSpec` with the found command,
/// the server-specific args, plus install hint/script for UI display.
/// Returns `None` if no candidate command is found on PATH.
fn resolve_lsp_command(language: &str) -> Option<LspServerSpec> {
    let lang_lower = language.to_lowercase();
    let canonical = canonical_language(&lang_lower);
    let cfg = lsp_servers_config();

    let entry = cfg.servers.get(canonical);
    if entry.is_none() {
        tracing::warn!("[LSP] No config entry for language '{}' (canonical: '{}')", language, canonical);
        return None;
    }
    let entry = entry.unwrap();

    // Find first candidate that exists on PATH.
    for cmd in &entry.candidates {
        if let Some(found) = find_on_path(cmd) {
            tracing::info!(
                "[LSP] Found LSP command for '{}' (canonical '{}'): {}, args: {:?}",
                language, canonical, found, entry.args
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
        language, canonical, entry.candidates
    );
    // Return spec with install_hint even if command not found, so the
    // handler can give a useful error message.
    None
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
pub async fn lsp_servers_list(
    State(_state): State<AppState>,
) -> Json<LspServersConfig> {
    let cfg = lsp_servers_config();
    Json(cfg.clone())
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

    let script_name = cfg.servers.get(canonical)
        .and_then(|e| e.install_script.as_ref());

    let script_name = match script_name {
        Some(name) => name,
        None => {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({
                "error": format!("No install script for language: {}", language),
                "code": 404
            }))).into_response();
        }
    };

    // Determine extension based on platform
    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let filename = format!("{}.{}", script_name, ext);

    // Search for the script file
    let content = load_install_script(&filename);
    match content {
        Some(script) => (StatusCode::OK, Json(serde_json::json!({
            "language": canonical,
            "filename": filename,
            "script": script,
            "platform": ext,
        }))).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": format!("Install script file '{}' not found", filename),
            "code": 404
        }))).into_response(),
    }
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

    // 0. CLI / env override: --lsp-config-dir / ROLLBALL_LSP_CONFIG_DIR
    //    Desktop App passes its Tauri resource_dir here.
    if let Ok(config_dir) = std::env::var("ROLLBALL_LSP_CONFIG_DIR") {
        let path = std::path::PathBuf::from(&config_dir).join("lsp_install").join(filename);
        if path.exists() {
            candidates.push(path);
        }
    }

    // 1. CARGO_MANIFEST_DIR ../../assets/lsp_install/ (dev)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let path = std::path::PathBuf::from(&manifest_dir)
            .join("..").join("..").join("assets").join("lsp_install").join(filename);
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
            tracing::info!(
                "[LSP] lsp_handler — workspace_root resolved: '{}'",
                root
            );
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
    let spec = match resolve_lsp_command(&lang_lower) {
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
            let install_hint = cfg.servers.get(canonical2)
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
        lang_lower, spec.command, spec.args, workspace_root
    );

    let pool = Arc::clone(&state.lsp_pool);
    ws.on_upgrade(move |socket| lsp_relay(socket, spec, workspace_root, pool))
}

/// Bidirectional LSP relay: WebSocket ↔ pooled LSP process
///
/// Uses the process pool to get/spawn an LSP process. When the WebSocket
/// disconnects, the LSP process stays alive for future reconnections.
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
    let entry = match pool.get_or_spawn(&spec.command, &spec.args, &workspace_root).await {
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
            tracing::error!("[LSP] relay — Failed to get/spawn '{}': {}", spec.command, err);
            return;
        }
    };

    let stdin_tx = entry.stdin_tx.clone();
    let mut stdout_rx = entry.stdout_tx.subscribe();

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Task: LSP stdout (broadcast) → WebSocket
    let cmd_for_send = spec.command.clone();
    let mut send_task = tokio::spawn(async move {
        tracing::info!("[LSP] relay — stdout→WS task started for '{}'", cmd_for_send);
        loop {
            match stdout_rx.recv().await {
                Ok(msg) => {
                    // Extract JSON-RPC method name for diagnostic logging
                    let method_hint = extract_method_hint(&msg);
                    tracing::debug!(
                        "[LSP] relay → WS: '{}' method='{}' len={}",
                        cmd_for_send,
                        method_hint,
                        msg.len()
                    );
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
                    // Continue — we lost some messages but can still relay future ones
                }
                Err(_) => {
                    // Channel closed — LSP process died
                    tracing::warn!("[LSP] relay — stdout channel closed for '{}', breaking", cmd_for_send);
                    break;
                }
            }
        }
        // Attempt to send close frame
        let _ = ws_tx.send(Message::Close(None)).await;
        tracing::info!("[LSP] relay — stdout→WS task ended for '{}'", cmd_for_send);
    });

    // Task: WebSocket → LSP stdin (via mpsc)
    let cmd_for_recv = spec.command.clone();
    let mut recv_task = tokio::spawn(async move {
        tracing::info!("[LSP] relay — WS→stdin task started for '{}'", cmd_for_recv);
        while let Some(msg) = ws_rx.next().await {
            let text: String = match msg {
                Ok(Message::Text(t)) => {
                    // Extract JSON-RPC method name for diagnostic logging
                    let method_hint = extract_method_hint(t.as_str());
                    tracing::debug!(
                        "[LSP] relay WS → stdin: '{}' method='{}' len={}",
                        cmd_for_recv,
                        method_hint,
                        t.len()
                    );
                    t.to_string()
                }
                Ok(Message::Binary(data)) => {
                    match String::from_utf8(data.to_vec()) {
                        Ok(s) => s,
                        Err(_) => continue,
                    }
                }
                Ok(Message::Close(_)) => break,
                _ => continue,
            };

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
    pool.client_disconnected(&spec.command, &spec.args, &workspace_root).await;
    tracing::info!(
        "[LSP] relay — WebSocket client disconnected for '{}' in '{}'",
        spec.command, workspace_root
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
        if let Some(open) = rest.find('"') {
            if let Some(close) = rest[open + 1..].find('"') {
                return rest[open + 1..open + 1 + close].to_string();
            }
        }
    }
    // Check if it's a response (has "id" but no "method")
    if msg.contains("\"id\":") && !msg.contains("\"method\":") {
        return "(response)".to_string();
    }
    "(no method)".to_string()
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
        (StatusCode::NOT_FOUND, "Agent not running — cannot resolve workspace".to_string())
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

        Err((StatusCode::NOT_FOUND, format!("Workspace directory not found: {}", ws_id)))
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

    #[test]
    fn test_resolve_lsp_command_known_languages() {
        // These may return None if the binary is not on PATH,
        // but should not panic.
        let _ = resolve_lsp_command("rust");
        let _ = resolve_lsp_command("python");
        let _ = resolve_lsp_command("go");
    }

    #[test]
    fn test_resolve_lsp_command_unknown_language() {
        assert!(resolve_lsp_command("brainfuck").is_none());
        assert!(resolve_lsp_command("").is_none());
    }

    #[test]
    fn test_resolve_lsp_command_case_insensitive() {
        // Both "Rust" and "rust" should resolve to the same canonical language
        let lower = resolve_lsp_command("rust");
        let upper = resolve_lsp_command("Rust");
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
}
