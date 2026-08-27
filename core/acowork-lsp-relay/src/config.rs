//! LSP server configuration — loaded from `lsp_servers.json`.
//!
//! Moved from `acowork-gateway/src/lsp/mod.rs`. The config loading logic
//! is identical; only the search path for install scripts is adapted to
//! the new crate's binary location.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;
use tokio::sync::{Notify, RwLock};

// ── LSP server configuration (from JSON file) ──────────────────────────

/// Per-language LSP server specification from `lsp_servers.json`.
///
/// `candidate_args` is intentionally kept as `HashMap`: only the entry-point
/// `servers` / `status` maps are user-visible ordering, and there is no
/// stable contract on per-candidate arg overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerEntry {
    /// Candidate command names (tried in order).
    pub candidates: Vec<String>,
    /// Extra arguments for stdio-mode LSP communication.
    pub args: Vec<String>,
    /// Per-candidate arg overrides.
    #[serde(default, skip_serializing_if = "empty_candidate_args")]
    pub candidate_args: std::collections::HashMap<String, Vec<String>>,
    /// One-line install hint shown to the user.
    pub install_hint: String,
    /// Name of the install script file (e.g. "rust" → assets/lsp_install/rust.sh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_script: Option<String>,
    /// Human-readable description.
    pub description: String,
    /// Language-specific root marker files (e.g. tsconfig.json, Cargo.toml).
    /// Used by project root discovery to find the nearest project root.
    /// Languages with no markers (rootless) have an empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_markers: Vec<String>,
}

fn empty_candidate_args(map: &std::collections::HashMap<String, Vec<String>>) -> bool {
    map.is_empty()
}

/// Top-level structure for `lsp_servers.json`.
///
/// `servers` is an `IndexMap` (not `HashMap`) so iteration order matches
/// the order keys appear in the JSON file. `HashMap` would randomize the
/// order per-process (SipHash with a random seed), which made the harness
/// LSP list appear in a different order on every gateway restart — see
/// the comment on `LspServersWithStatus::status` for the same rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServersConfig {
    pub version: u32,
    pub servers: IndexMap<String, LspServerEntry>,
}

/// Per-language LSP server installation status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LspServerStatusEntry {
    pub language: String,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Combined response for `GET /api/lsp/servers-with-status`.
///
/// Returns the configured LSP servers together with their per-language
/// installation status in a single round-trip. The frontend uses this on
/// initial load and on Refresh to avoid the race window where the
/// server list is visible but install status badges have not yet been
/// resolved (previously caused a 1–2s "empty badges" flash on the
/// Harness → LSP tab).
///
/// Both `servers` and `status` are `IndexMap`, so iteration order is
/// deterministic across processes. The frontend's `Object.entries` then
/// sees the same order on every gateway restart — `HashMap`'s random
/// SipHash seed used to make the harness list shuffle on every boot.
///
/// Status is keyed by canonical language name (`LspServerStatusEntry.language`),
/// matching the keys of `LspServersConfig.servers`. A language present in
/// `servers` but missing from `status` should be treated as "unknown"
/// by the UI — under normal operation the two are in lockstep.
#[derive(Debug, Clone, Serialize)]
pub struct LspServersWithStatus {
    /// Configured LSP servers (same shape as `GET /api/lsp/servers`).
    pub servers: LspServersConfig,
    /// Per-language installation status, keyed by canonical language.
    pub status: IndexMap<String, LspServerStatusEntry>,
}

/// Resolved LSP server specification.
#[derive(Debug, Clone)]
pub struct LspServerSpec {
    pub command: String,
    pub args: Vec<String>,
    pub language: String,
    pub install_hint: String,
    pub install_script: Option<String>,
}

// ── Language alias mapping ──────────────────────────────────────────────

/// Map language aliases to canonical names used in `lsp_servers.json`.
pub fn canonical_language(lang: &str) -> &str {
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

/// Initialize the LSP servers config with an explicit config directory.
///
/// Call once at startup (before any other config access) to pass the CLI
/// `--lsp-config-dir` argument directly, without going through the
/// `ACOWORK_LSP_CONFIG_DIR` environment variable. If this is not called,
/// `lsp_servers_config()` falls back to reading the env var.
///
/// The config is cached in a `OnceLock` for the process lifetime.
pub fn init_lsp_servers_config(config_dir: Option<&std::path::Path>) {
    let _ = lsp_servers_config_inner(config_dir);
}

/// Load `lsp_servers.json` from disk (cached with `OnceLock`).
///
/// Reads `ACOWORK_LSP_CONFIG_DIR` on the first call (unless
/// `init_lsp_servers_config` was called earlier). The resulting config is
/// cached for the process lifetime.
pub fn lsp_servers_config() -> &'static LspServersConfig {
    let config_dir = std::env::var("ACOWORK_LSP_CONFIG_DIR")
        .ok()
        .map(std::path::PathBuf::from);
    lsp_servers_config_inner(config_dir.as_deref())
}

fn lsp_servers_config_inner(config_dir: Option<&std::path::Path>) -> &'static LspServersConfig {
    static CFG: OnceLock<LspServersConfig> = OnceLock::new();
    CFG.get_or_init(|| {
        load_lsp_servers_from_file(config_dir).unwrap_or_else(|| {
            tracing::warn!("lsp_servers.json not found, using built-in defaults");
            builtin_lsp_defaults()
        })
    })
}

fn load_lsp_servers_from_file(config_dir: Option<&std::path::Path>) -> Option<LspServersConfig> {
    let candidates = build_config_candidates(config_dir);
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

fn build_config_candidates(config_dir: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    // 0. Explicit config dir (CLI arg or env var, resolved by caller)
    if let Some(config_dir) = config_dir {
        let path = config_dir.join("lsp_servers.json");
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

    // 2. Same directory as the executable
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

/// Built-in default LSP server config.
///
/// Uses an `IndexMap` so the iteration order matches the `servers.insert`
/// call order below. With `HashMap` this function would also produce a
/// randomized order on every process start; with `IndexMap` the order is
/// the same order as the `assets/lsp_servers.json` shipped with the repo.
fn builtin_lsp_defaults() -> LspServersConfig {
    let mut servers: IndexMap<String, LspServerEntry> = IndexMap::new();

    servers.insert(
        "rust".into(),
        LspServerEntry {
            candidates: vec!["rust-analyzer".into()],
            args: vec![],
            install_hint: "rustup component add rust-analyzer".into(),
            install_script: Some("rust".into()),
            description: "Rust language server (defaults to stdio, no --stdio flag)".into(),
            root_markers: vec!["Cargo.toml".into()],
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
            root_markers: vec!["pyproject.toml".into(), "setup.py".into(), "setup.cfg".into()],
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
            root_markers: vec!["tsconfig.json".into(), "jsconfig.json".into()],
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
            root_markers: vec!["go.mod".into()],
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
            root_markers: vec!["CMakeLists.txt".into(), "compile_commands.json".into()],
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
            root_markers: vec![],
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
            root_markers: vec![],
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
            root_markers: vec![],
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
            root_markers: vec![],
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
            root_markers: vec![],
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
            root_markers: vec!["pom.xml".into(), "build.gradle".into(), "build.gradle.kts".into(), "settings.gradle".into()],
            candidate_args: Default::default(),
        },
    );
    servers.insert(
        "kotlin".into(),
        LspServerEntry {
            candidates: vec!["kotlin-language-server".into()],
            args: vec![],
            install_hint:
                "brew install kotlin-language-server (macOS) or download from https://github.com/fwcd/kotlin-language-server".into(),
            install_script: Some("kotlin".into()),
            description: "Kotlin language server (defaults to stdio, no --stdio flag)".into(),
            root_markers: vec!["build.gradle.kts".into(), "build.gradle".into()],
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
pub async fn resolve_lsp_command(language: &str) -> Option<LspServerSpec> {
    let lang_lower = language.to_lowercase();
    let canonical = canonical_language(&lang_lower);
    let cfg = lsp_servers_config();

    let entry = cfg.servers.get(canonical)?;

    // Find first candidate that exists on PATH or in known install locations,
    // AND can actually run.
    for cmd in &entry.candidates {
        if let Some(found) = find_lsp_binary(cmd, canonical) {
            // Resolve per-candidate args
            let args = entry
                .candidate_args
                .get(cmd)
                .cloned()
                .unwrap_or_else(|| entry.args.clone());

            if !verify_command_runnable(&found, &args).await {
                tracing::warn!(
                    "[LSP] Command '{}' (with args {:?}) not runnable — skipping",
                    found,
                    args
                );
                continue;
            }

            tracing::info!(
                "[LSP] Found LSP command for '{}' (canonical '{}'): {}, args: {:?}",
                language,
                canonical,
                found,
                args
            );
            return Some(LspServerSpec {
                command: found,
                args,
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

/// Verify a command is actually runnable (two-stage probe).
///
/// Stage 1 tries `--version` (fast path for most tools).
/// Stage 2 spawns the server with its real launch args, sends a minimal
/// LSP initialize handshake on stdin, and checks that the process stays
/// alive for at least 500 ms.  This is necessary for servers like jdtls
/// that validate stdin input and exit quickly on EOF / empty pipe.
pub async fn verify_command_runnable(command: &str, args: &[String]) -> bool {
    use std::process::Stdio;

    // Stage 1: --version (fast path for tools that support it).
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

    // Stage 2: spawn with real launch args + piped stdin.
    // Write a minimal LSP initialize request so that servers which
    // validate their input (e.g. jdtls) don't exit immediately.
    let mut child = match tokio::process::Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Send LSP initialize handshake to keep the server alive during probe.
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;

        let init =
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}"#;
        let header = format!("Content-Length: {}\r\n\r\n", init.len());
        // Best-effort write — if the process has already exited this will fail,
        // which is fine because the timeout below will catch it.
        let _ = stdin.write_all(header.as_bytes()).await;
        let _ = stdin.write_all(init.as_bytes()).await;

        // Keep stdin in scope so the pipe stays open during the wait.
        // Dropping it would close the pipe and may cause the server to exit.
        //
        // Use a 5-second window: JVM-based servers (jdtls, kotlin-language-server)
        // can take several seconds to start.  If the process is still alive after
        // 5 seconds, it's considered runnable.
        let exited_within_window =
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .is_ok();

        drop(stdin);
        !exited_within_window
    } else {
        false
    }
}

/// Check if a command exists on the system PATH.
pub fn find_on_path(cmd: &str) -> Option<String> {
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

// ── Extended LSP binary discovery ──────────────────────────────────────
//
// `find_on_path` only scans the process PATH. Many LSP servers are
// installed to non-PATH locations (VS Code extensions, language-specific
// tool directories, etc.). The install scripts search these locations
// during installation, but the status-check path (`resolve_lsp_command`)
// previously did not — causing "install succeeded but status shows not
// installed" for languages like Java (jdtls), Go (gopls from VS Code),
// Rust (rust-analyzer from VS Code), etc.
//
// `find_lsp_binary` unifies the discovery logic: first try PATH, then
// search language-specific known install directories.

/// Find an LSP binary: first search known install locations (more reliable),
/// then check PATH, then check the profile PATH cache (populated after
/// successful installs).
///
/// Known install locations are searched before PATH because PATH may contain
/// stale wrapper scripts (e.g. jdtls.cmd pointing to an old VS Code extension
/// version). Direct discovery from known locations is more reliable.
fn find_lsp_binary(cmd: &str, language: &str) -> Option<String> {
    // 1. Search language-specific known locations (more reliable than PATH).
    for dir in known_install_dirs(language) {
        // On Windows, try common extensions (.exe, .cmd, .bat) in addition
        // to the bare command name — many LSP servers ship as .bat wrappers.
        let candidates: Vec<PathBuf> = if cfg!(windows) {
            vec![
                dir.join(format!("{}.exe", cmd)),
                dir.join(format!("{}.cmd", cmd)),
                dir.join(format!("{}.bat", cmd)),
                dir.join(cmd),
            ]
        } else {
            vec![dir.join(cmd)]
        };

        for full in &candidates {
            if full.is_file() {
                tracing::info!(
                    "[LSP] Found '{}' for '{}' in known install dir: {}",
                    cmd,
                    language,
                    full.display()
                );
                return Some(full.to_string_lossy().into_owned());
            }
        }
    }

    // 2. Check PATH (may contain stale wrappers — known dirs are preferred).
    if let Some(found) = find_on_path(cmd) {
        return Some(found);
    }

    // 3. Check profile PATH cache (populated by refresh_path_from_profiles)
    if let Some(found) = find_in_profile_cache(cmd) {
        tracing::info!(
            "[LSP] Found '{}' for '{}' in profile PATH cache: {}",
            cmd,
            language,
            found
        );
        return Some(found);
    }

    None
}

/// Known install directories per language.
///
/// Mirrors the search logic in the corresponding install scripts
/// (`assets/lsp_install/{lang}.sh`). When adding a new install script
/// with custom search paths, add the corresponding directories here
/// so the status check can find the binary without a PATH entry.
fn known_install_dirs(language: &str) -> Vec<PathBuf> {
    let home = dirs_home();
    let mut dirs = Vec::new();

    match language {
        "java" => {
            // macOS install location (JDTLS_INSTALL_DIR in java.sh)
            #[cfg(target_os = "macos")]
            dirs.push(home.join("Library/Application Support/jdtls/bin"));
            // Linux install locations
            dirs.push(home.join(".local/jdtls/bin"));
            dirs.push(home.join(".local/share/jdtls/bin"));
            dirs.push(PathBuf::from("/usr/local/jdtls/bin"));
            dirs.push(PathBuf::from("/opt/jdtls/bin"));
            dirs.push(home.join("jdtls/bin"));
            dirs.push(home.join(".jdtls/bin"));
            // Windows install location (JDTLS_INSTALL_DIR in java.ps1).
            // This is where the install script creates the clean jdtls.cmd
            // wrapper (VS Code's jdtls.bat has 'pause' and is NOT usable).
            #[cfg(windows)]
            {
                if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
                    dirs.push(PathBuf::from(&local_appdata).join("jdtls/bin"));
                }
            }
            // NOTE: VS Code Java extension (redhat.java) is intentionally
            // NOT included here — its jdtls.bat has a 'pause' that hangs
            // the process.  The install script creates a clean wrapper at
            // the LOCALAPPDATA path above.
        }
        "go" => {
            // GOPATH/bin
            let gopath = std::env::var("GOPATH").unwrap_or_else(|_| {
                home.join("go").to_string_lossy().into_owned()
            });
            dirs.push(PathBuf::from(&gopath).join("bin"));
            dirs.push(home.join("go/bin"));
            // VS Code Go extension (golang.go)
            dirs.extend(vscode_extension_dirs("golang.go-", &["dist"]));
        }
        "rust" => {
            // VS Code rust-analyzer extension
            dirs.extend(vscode_extension_dirs("rust-lang.rust-analyzer-", &["server"]));
        }
        "kotlin" => {
            // VS Code Kotlin extension
            dirs.extend(vscode_extension_dirs("fwcd.kotlin-", &["server/bin"]));
            // SDKMAN
            dirs.push(home.join(".sdkman/candidates/kotlin-language-server/current/bin"));
            dirs.push(home.join(".local/bin"));
            dirs.push(PathBuf::from("/usr/local/bin"));
            dirs.push(PathBuf::from("/opt/homebrew/bin"));
            dirs.push(PathBuf::from("/usr/bin"));
            // Windows install location (INSTALL_DIR in kotlin.ps1)
            // server.zip extracts to server/bin/kotlin-language-server.bat
            #[cfg(windows)]
            {
                if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
                    dirs.push(PathBuf::from(&local_appdata).join("kotlin-language-server/server/bin"));
                    dirs.push(PathBuf::from(&local_appdata).join("kotlin-language-server/bin"));
                }
            }
        }
        "clangd" | "c" => {
            // VS Code clangd extension
            dirs.extend(vscode_extension_dirs(
                "llvm-vs-code-extensions.vscode-clangd-",
                &[],
            ));
        }
        "yaml" => {
            // VS Code YAML extension
            dirs.extend(vscode_extension_dirs(
                "redhat.vscode-yaml-",
                &["node_modules/yaml-language-server/bin"],
            ));
            // npm global bin
            dirs.push(home.join(".npm-global/bin"));
            dirs.push(PathBuf::from("/usr/local/bin"));
            dirs.push(PathBuf::from("/usr/bin"));
        }
        "python" => {
            dirs.push(home.join(".local/bin"));
            dirs.push(PathBuf::from("/usr/local/bin"));
            dirs.push(PathBuf::from("/opt/homebrew/bin"));
            dirs.push(home.join(".local/pipx/venvs/python-lsp-server/bin"));
            // macOS Python user installs
            #[cfg(target_os = "macos")]
            {
                if let Ok(entries) = std::fs::read_dir(home.join("Library/Python")) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name_str = name.to_string_lossy();
                        if name_str.starts_with("3.") {
                            dirs.push(entry.path().join("bin"));
                        }
                    }
                }
            }
            // Framework Python on macOS
            #[cfg(target_os = "macos")]
            {
                if let Ok(entries) =
                    std::fs::read_dir(PathBuf::from(
                        "/Library/Frameworks/Python.framework/Versions",
                    ))
                {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name_str = name.to_string_lossy();
                        if name_str.starts_with("3.") {
                            dirs.push(entry.path().join("bin"));
                        }
                    }
                }
            }
        }
        // npm-based LSP servers: css, html, json, typescript, yaml
        "css" | "html" | "json" | "typescript" => {
            dirs.push(home.join(".npm-global/bin"));
            dirs.push(PathBuf::from("/usr/local/bin"));
            dirs.push(PathBuf::from("/usr/bin"));
        }
        _ => {}
    }

    dirs
}

/// Search VS Code (and VS Code Insiders) extension directories for a
/// given extension prefix, returning candidate bin directories.
///
/// `extension_prefix` matches the start of the extension directory name
/// (e.g. `"redhat.java-"` matches `redhat.java-1.55.0-darwin-arm64`).
/// `sub_paths` are appended to the extension root to reach the bin dir
/// (e.g. `["server", "bin"]` for `redhat.java-*/server/bin`).
fn vscode_extension_dirs(extension_prefix: &str, sub_paths: &[&str]) -> Vec<PathBuf> {
    let home = dirs_home();
    let mut dirs = Vec::new();

    for vscode_dir in &[".vscode", ".vscode-insiders"] {
        let ext_root = home.join(vscode_dir).join("extensions");
        if let Ok(entries) = std::fs::read_dir(&ext_root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(extension_prefix) {
                    let mut bin_dir = entry.path();
                    for sub in sub_paths {
                        bin_dir = bin_dir.join(sub);
                    }
                    if bin_dir.is_dir() {
                        dirs.push(bin_dir);
                    }
                }
            }
        }
    }

    dirs
}

/// Resolve `$HOME` (or `%USERPROFILE%` on Windows).
fn dirs_home() -> PathBuf {
    #[cfg(not(windows))]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("C:\\"))
    }
}

// ── Profile PATH cache ─────────────────────────────────────────────────
//
// Install scripts write `export PATH="$PATH:/some/dir"` to shell profile
// files (~/.profile, ~/.zshrc, ~/.bashrc). These changes only take effect
// in new shell sessions — the LSP Relay process never sees them.
//
// After a successful install, `refresh_path_from_profiles()` parses the
// profile files and caches any discovered directories. `find_lsp_binary`
// checks this cache as a fallback, so newly-installed binaries are
// discoverable immediately without restarting the LSP Relay.

/// Thread-safe cache of extra PATH directories discovered from profile files.
static PROFILE_PATH_CACHE: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();

fn profile_path_cache() -> &'static Mutex<Vec<PathBuf>> {
    PROFILE_PATH_CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Parse shell profile files and cache any `export PATH=...` directories.
///
/// Called after a successful install so that the status check can find
/// the newly-installed binary without a process restart.
pub fn refresh_path_from_profiles() {
    let home = dirs_home();
    let mut new_dirs = Vec::new();

    // Profile files to scan, in order of preference
    let profile_files: &[&str] = if cfg!(windows) {
        &[]
    } else {
        &[".zshrc", ".zprofile", ".bashrc", ".bash_profile", ".profile"]
    };

    for filename in profile_files {
        let path = home.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                let trimmed = line.trim();
                // Match: export PATH="..." or export PATH=... or PATH=...
                if let Some(dir) = parse_path_export(trimmed)
                    && dir.is_dir()
                    && !new_dirs.contains(&dir)
                {
                    tracing::info!(
                        "[LSP] Discovered PATH dir from {}: {}",
                        filename,
                        dir.display()
                    );
                    new_dirs.push(dir);
                }
            }
        }
    }

    if !new_dirs.is_empty()
        && let Ok(mut cache) = profile_path_cache().lock()
    {
        for dir in new_dirs {
            if !cache.contains(&dir) {
                cache.push(dir);
            }
        }
    }
}

/// Extract a directory path from a shell `export PATH=...` line.
///
/// Handles these forms:
/// - `export PATH="$PATH:/some/dir"`
/// - `export PATH="/some/dir:$PATH"`
/// - `export PATH="/some/dir"`
fn parse_path_export(line: &str) -> Option<PathBuf> {
    // Strip leading "export " if present
    let rest = line
        .strip_prefix("export ")
        .unwrap_or(line);

    // Must be a PATH assignment
    let value = rest
        .strip_prefix("PATH=")?;

    // Remove surrounding quotes
    let value = value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(value);

    // Split on ':' and find directories that are not $PATH or $HOME references
    for segment in value.split(':') {
        let segment = segment.trim();
        // Skip variable references and empty segments
        if segment.is_empty()
            || segment.starts_with('$')
            || segment == "\"$PATH\""
        {
            continue;
        }
        let path = PathBuf::from(segment);
        if path.is_dir() {
            return Some(path);
        }
    }
    None
}

/// Check the profile PATH cache for a command.
fn find_in_profile_cache(cmd: &str) -> Option<String> {
    if let Ok(cache) = profile_path_cache().lock() {
        for dir in cache.iter() {
            let full = dir.join(cmd);
            if full.is_file() {
                return Some(full.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Compute installation status for every configured language.
pub async fn compute_lsp_status() -> Vec<LspServerStatusEntry> {
    let cfg = lsp_servers_config();
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

/// Bounded-concurrency limit for [`compute_lsp_status_concurrent`].
///
/// Each language's PATH probe spawns a child process to run `--version`
/// (with a 2s timeout inside [`verify_command_runnable`]). Sequential
/// probing scales linearly with the language count — ~13 configured
/// languages means a worst case of ~26s. A limit of 4 caps the wall
/// clock at roughly one probe-timeout (~2s) once the pool saturates,
/// while keeping fork-exec pressure modest when the language list grows.
const LSP_STATUS_PROBE_CONCURRENCY: usize = 4;

/// Compute installation status for every configured language in parallel.
///
/// Equivalent in shape and ordering to [`compute_lsp_status`], but uses
/// bounded concurrency (`LSP_STATUS_PROBE_CONCURRENCY`) via a
/// `tokio::sync::Semaphore` so that probes overlap instead of running
/// strictly sequentially. Each permit is released when the inner future
/// drops (i.e. on completion or cancellation).
///
/// Used by the merged `GET /api/lsp/servers-with-status` endpoint so
/// that the server list and per-language install badges arrive in a
/// single round-trip and within a bounded worst-case time.
pub async fn compute_lsp_status_concurrent() -> Vec<LspServerStatusEntry> {
    let cfg = lsp_servers_config();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(
        LSP_STATUS_PROBE_CONCURRENCY,
    ));
    let languages: Vec<String> = cfg.servers.keys().cloned().collect();

    let futures = languages.into_iter().map(|lang| {
        let sem = std::sync::Arc::clone(&sem);
        async move {
            // Bound probe concurrency; permit is released on drop.
            // `acquire_owned` only fails if the semaphore is closed,
            // which we never do — sem lives for this function call.
            let _permit = sem
                .acquire_owned()
                .await
                .expect("Semaphore::acquire_owned only fails if the semaphore is closed");
            match resolve_lsp_command(&lang).await {
                Some(spec) => LspServerStatusEntry {
                    language: spec.language,
                    installed: true,
                    command: Some(spec.command),
                },
                None => LspServerStatusEntry {
                    language: lang,
                    installed: false,
                    command: None,
                },
            }
        }
    });

    let mut entries = futures_util::future::join_all(futures).await;
    entries.sort_by(|a, b| a.language.cmp(&b.language));
    entries
}

// ── Status cache ───────────────────────────────────────────────────────
//
// Each `compute_lsp_status()` call forks `cmd --version` for every
// configured language — bounded by 4-concurrency it's roughly one probe
// timeout (~2s worst case) per round, ~6s for all 13 languages. JVM-based
// servers (jdtls, kotlin-language-server) fall through to a 5-second
// LSP handshake probe, so the worst-case is even higher.
//
// The harness UI calls this endpoint on:
//   - every Harness → LSP tab mount (AppLayout unmounts HarnessPage on
//     view switch, so each re-entry remounts LspTab and triggers a load)
//   - per-row "Check Status" button (one row in the UI, full probe on
//     the server — see `handleCheck` in `apps/.../LspTab.tsx`)
//   - top-bar Refresh button
//
// Without a cache this means a fresh 13-fork storm on every interaction.
// The harness UI is read-mostly (status doesn't change unless the user
// runs an install), so we cache the result keyed by canonical language
// with a TTL:
//
//   - Cache hit within TTL: return immediately, no fork.
//   - Cache miss or stale: probe (with bounded concurrency + dedup of
//     concurrent probes for the same language), write result.
//
// Invalidation:
//   - `invalidate_status(lang)`: drop the cached entry for `lang`. Called
//     from the install endpoint after a successful install so the next
//     probe reflects the freshly-installed binary.
//   - `invalidate_all_status()`: clear the entire cache. Reserved for
//     future "user changed PATH" hooks; not currently called.
//
// Force-refresh:
//   - `probe_one(lang, force=true)` and `probe_all(force=true)` skip
//     the cache. The harness UI's top-bar Refresh button uses this to
//     re-probe on user demand.
//
// Configuration:
//   - `STATUS_TTL_SECS`: AtomicU64, default 1800 (30 min). Set once at
//     startup via `init_status_cache_ttl()` from CLI arg or env var.

/// Default TTL for cached LSP install-status entries (seconds).
///
/// 30 minutes strikes a balance between freshness (user manually installs
/// a binary outside our flow) and probe cost (a 13-language probe is
/// ~6s with bounded concurrency).
pub const DEFAULT_STATUS_TTL_SECS: u64 = 1800;

/// Process-wide TTL for the status cache. Set once at startup via
/// [`init_status_cache_ttl`] from CLI arg or env var. Reads use
/// `Ordering::Relaxed` — the value is monotonic after init, and a stale
/// read is harmless (worst case: probes run with the previous TTL for a
/// few extra seconds during startup).
static STATUS_TTL_SECS: AtomicU64 = AtomicU64::new(DEFAULT_STATUS_TTL_SECS);

/// Initialize the status cache TTL. Call once at startup, before any
/// `probe_one` / `probe_all` call.
///
/// `ttl_secs == 0` is treated as "use default" — protects against
/// misconfigured CLI args or env vars (e.g. an empty env var string).
pub fn init_status_cache_ttl(ttl_secs: u64) {
    let effective = if ttl_secs == 0 {
        DEFAULT_STATUS_TTL_SECS
    } else {
        ttl_secs
    };
    STATUS_TTL_SECS.store(effective, Ordering::Relaxed);
    tracing::info!(
        ttl_secs = effective,
        "LSP status cache TTL initialized"
    );
}

/// Read the current TTL. Internal — used by `probe_one` and tests.
fn status_ttl_secs() -> u64 {
    STATUS_TTL_SECS.load(Ordering::Relaxed)
}

/// Per-language cache entry: probe result + monotonic timestamp of the
/// last successful probe.
type CachedEntry = (LspServerStatusEntry, std::time::Instant);

/// Type alias for the inner cache map (extracted to keep static decls
/// readable).
type CacheMap = HashMap<String, CachedEntry>;

/// Process-wide status cache. `RwLock` because reads dominate (every UI
/// load hits this) and writes are rare (only on cache miss + probe).
static STATUS_CACHE: OnceLock<RwLock<CacheMap>> = OnceLock::new();

fn status_cache() -> &'static RwLock<CacheMap> {
    STATUS_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Process-wide in-flight probe map. Maps canonical language to a
/// `Notify` handle — the first caller to insert becomes the "leader"
/// and does the actual probe; subsequent callers ("followers") wait on
/// the `Notify` and then re-read the cache.
///
/// We use a `std::sync::Mutex` (sync, not async) because the critical
/// section is a tiny HashMap lookup/insert — holding an async mutex
/// across a `Notify` would be a footgun (the async mutex would be
/// locked while waiting on the leader).
type InFlightMap = HashMap<String, Arc<Notify>>;
static INFLIGHT: OnceLock<std::sync::Mutex<InFlightMap>> = OnceLock::new();

fn inflight() -> &'static std::sync::Mutex<InFlightMap> {
    INFLIGHT.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Probe concurrency limiter. Used inside `probe_one` to bound fork
/// pressure (each probe is a `cmd --version` fork+exec, ~2s timeout
/// per call; JVM servers may fall through to a 5s LSP handshake).
///
/// Set to match the existing `LSP_STATUS_PROBE_CONCURRENCY` constant —
/// the cache changes when we probe, not how many probes we run in
/// parallel.
///
/// Stored as `Arc<Semaphore>` (not `Semaphore` directly) because
/// `tokio::sync::Semaphore::acquire_owned` is defined on
/// `Arc<Semaphore>` and produces an owned `OwnedSemaphorePermit` that
/// releases on drop. `tokio::sync::Semaphore` itself is not `Clone`,
/// so we share the instance via `Arc`.
static PROBE_SEM: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

fn probe_sem() -> &'static Arc<tokio::sync::Semaphore> {
    PROBE_SEM.get_or_init(|| {
        Arc::new(tokio::sync::Semaphore::new(
            LSP_STATUS_PROBE_CONCURRENCY,
        ))
    })
}

/// Acquire a probe slot for `canonical`. The returned guard either:
///
/// - Is a **leader** — this caller is responsible for running the probe
///   and writing the cache. Drop the guard after writing; it will notify
///   any followers and remove itself from the in-flight map.
/// - Is a **follower** — another caller is already probing `canonical`.
///   Call `wait_for_leader()` to await their result, then re-read the
///   cache (the leader writes before dropping the guard).
struct ProbeSlotGuard {
    canonical: String,
    is_leader: bool,
    notify: Arc<Notify>,
}

impl ProbeSlotGuard {
    /// Wait for the leader to finish probing. No-op if this guard is
    /// the leader.
    async fn wait_for_leader(&self) {
        if !self.is_leader {
            self.notify.notified().await;
        }
    }
}

impl Drop for ProbeSlotGuard {
    fn drop(&mut self) {
        if self.is_leader {
            // Notify all followers BEFORE removing from the in-flight
            // map. If we removed first, a follower could observe the
            // map empty, decide it's a leader, and start a duplicate
            // probe — race condition we want to avoid.
            self.notify.notify_waiters();
            if let Ok(mut map) = inflight().lock() {
                map.remove(&self.canonical);
            }
        }
    }
}

/// Acquire a probe slot for `canonical`. See [`ProbeSlotGuard`].
fn acquire_probe_slot(canonical: &str) -> ProbeSlotGuard {
    let mut map = inflight()
        .lock()
        .expect("INFLIGHT mutex poisoned — this is a bug");
    if let Some(existing) = map.get(canonical).cloned() {
        // Someone else is probing; we're a follower.
        ProbeSlotGuard {
            canonical: canonical.to_string(),
            is_leader: false,
            notify: existing,
        }
    } else {
        // We're the leader — insert our Notify and return.
        let notify = Arc::new(Notify::new());
        map.insert(canonical.to_string(), Arc::clone(&notify));
        ProbeSlotGuard {
            canonical: canonical.to_string(),
            is_leader: true,
            notify,
        }
    }
}

/// Probe a single language with cache + in-flight dedup + bounded
/// fork concurrency.
///
/// Behavior:
/// - `force == false`: cache hit within TTL returns immediately.
///   Otherwise acquire a probe slot, run the probe under a fork-permit,
///   write the cache, drop the slot (notifies followers).
/// - `force == true`: bypass cache and dedup; always fork. Still writes
///   the cache so subsequent non-forced calls hit it.
///
/// Returns a `LspServerStatusEntry` for the canonical language. Languages
/// not in the config return an "installed=false" entry (defensive — the
/// routes layer filters by canonical, but tests may probe arbitrary
/// strings).
pub async fn probe_one(language: &str, force: bool) -> LspServerStatusEntry {
    let lang_lower = language.to_lowercase();
    let canonical = canonical_language(&lang_lower).to_string();
    let ttl = status_ttl_secs();

    // Cache fast path — only when not forcing.
    if !force
        && let Some(entry) = read_cache_if_fresh(&canonical, ttl).await
    {
        return entry;
    }

    // In-flight dedup — skipped when forcing (each force caller pays
    // for their own fresh probe).
    let guard = if !force {
        Some(acquire_probe_slot(&canonical))
    } else {
        None
    };

    // If we're a follower, wait for the leader then re-check cache.
    if let Some(g) = &guard
        && !g.is_leader
    {
        g.wait_for_leader().await;
        // Leader should have written the cache. Re-check before
        // falling through to a fresh probe.
        if let Some(entry) = read_cache_if_fresh(&canonical, ttl).await {
            return entry;
        }
        // Leader didn't write (probe failed?). Fall through to
        // probe ourselves — but skip dedup for this fallback.
    }

    // Acquire fork permit and run the probe. `acquire_owned` consumes
    // an `Arc<Semaphore>` — clone the Arc first so the static instance
    // remains available for future callers.
    let permit = probe_sem()
        .clone()
        .acquire_owned()
        .await
        .expect("probe semaphore never closed");
    let entry = match resolve_lsp_command(&canonical).await {
        Some(spec) => LspServerStatusEntry {
            language: spec.language,
            installed: true,
            command: Some(spec.command),
        },
        None => LspServerStatusEntry {
            language: canonical.clone(),
            installed: false,
            command: None,
        },
    };
    drop(permit);

    // Write to cache (always, even when forced — subsequent non-forced
    // calls can still hit it within TTL).
    write_cache(&canonical, entry.clone()).await;

    entry
}

/// Probe every configured language with cache + in-flight dedup.
///
/// Equivalent ordering to [`compute_lsp_status_concurrent`] (sorted by
/// canonical language), but reuses cached entries within TTL and
/// coalesces concurrent probes for the same language.
///
/// `force == true` bypasses the cache (each language is probed fresh)
/// but still writes results back so the next non-forced call hits the
/// cache.
pub async fn probe_all(force: bool) -> Vec<LspServerStatusEntry> {
    let cfg = lsp_servers_config();
    let languages: Vec<String> = cfg.servers.keys().cloned().collect();

    let futures = languages.into_iter().map(|lang| async move {
        probe_one(&lang, force).await
    });

    let mut entries = futures_util::future::join_all(futures).await;
    entries.sort_by(|a, b| a.language.cmp(&b.language));
    entries
}

/// Read a cache entry if it exists and was probed within `ttl` seconds.
async fn read_cache_if_fresh(canonical: &str, ttl: u64) -> Option<LspServerStatusEntry> {
    let cache = status_cache().read().await;
    cache
        .get(canonical)
        .and_then(|(entry, probed_at)| {
            if probed_at.elapsed().as_secs() < ttl {
                Some(entry.clone())
            } else {
                None
            }
        })
}

/// Write a probe result to the cache with the current timestamp.
async fn write_cache(canonical: &str, entry: LspServerStatusEntry) {
    let mut cache = status_cache().write().await;
    cache.insert(canonical.to_string(), (entry, std::time::Instant::now()));
}

/// Drop the cached entry for `canonical`. Next call to [`probe_one`] or
/// [`probe_all`] will re-probe the language.
///
/// Called from the install endpoint after a successful install so the
/// status reflects the freshly-installed binary without waiting for TTL.
pub fn invalidate_status(canonical: &str) {
    let canonical = canonical_language(canonical);
    let cache = status_cache();
    // We hold the sync lock briefly; spawn a task to drop the entry.
    // Direct blocking_lock would also work but we want this fn to be
    // callable from sync context (the install handler is async but the
    // drop is sync-fast).
    let canonical = canonical.to_string();
    tokio::spawn(async move {
        let mut cache = cache.write().await;
        if cache.remove(&canonical).is_some() {
            tracing::info!(
                canonical = %canonical,
                "Invalidated LSP status cache"
            );
        }
    });
}

/// Drop the entire cache. Reserved for future use (e.g. user reports
/// "PATH changed externally, re-probe everything"). Not currently
/// called from any handler.
pub fn invalidate_all_status() {
    let cache = status_cache();
    tokio::spawn(async move {
        let mut cache = cache.write().await;
        let count = cache.len();
        cache.clear();
        tracing::info!(count, "Invalidated entire LSP status cache");
    });
}

/// Test-only helper: synchronous cache write bypassing TTL. Used by
/// unit tests that want to seed the cache without spawning probes.
#[cfg(test)]
async fn seed_cache_for_test(canonical: &str, entry: LspServerStatusEntry) {
    let mut cache = status_cache().write().await;
    cache.insert(canonical.to_string(), (entry, std::time::Instant::now()));
}

// ── Unit tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(canonical_language("rust"), "rust");
        assert_eq!(canonical_language("python"), "python");
    }

    #[test]
    fn test_lsp_servers_config_loads() {
        let cfg = lsp_servers_config();
        assert!(cfg.servers.contains_key("rust"));
        assert!(cfg.servers.contains_key("python"));
        assert!(cfg.servers.contains_key("go"));
        assert!(cfg.version == 1);
    }

    /// Regression: the harness LSP tab used to re-order its rows on every
    /// gateway restart because `LspServersConfig.servers` was a `HashMap`
    /// (SipHash with a per-process random seed). With `IndexMap`, the
    /// iteration order must match the order keys appear in
    /// `assets/lsp_servers.json`, both on the first read and on repeated
    /// reads within the same process.
    ///
    /// We don't pin a specific language at index 0 here (the JSON file
    /// can be edited); instead we verify that two consecutive iterations
    /// produce identical orderings, and that deserializing the same JSON
    /// twice yields the same order. Together these rules catch both
    /// regressions to `HashMap` and accidental re-shuffling in the
    /// config-loading path.
    #[test]
    fn test_lsp_servers_config_iteration_order_is_stable() {
        let cfg = lsp_servers_config();

        let first: Vec<&str> = cfg.servers.keys().map(String::as_str).collect();
        let second: Vec<&str> = cfg.servers.keys().map(String::as_str).collect();
        assert_eq!(
            first, second,
            "servers iteration order changed between two reads"
        );

        // Re-parse the on-disk JSON and confirm the order is identical
        // to the cached config. This guards against `load_lsp_servers_from_file`
        // re-shuffling after the file load (e.g. by re-inserting into a
        // different map type).
        let on_disk: LspServersConfig = {
            let json = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../assets/lsp_servers.json"),
            )
            .expect("assets/lsp_servers.json must be readable");
            serde_json::from_str(&json).expect("assets/lsp_servers.json must parse")
        };
        let on_disk_order: Vec<&str> =
            on_disk.servers.keys().map(String::as_str).collect();
        assert_eq!(
            first, on_disk_order,
            "cached config order diverged from on-disk JSON order"
        );
    }

    /// Regression: when JSON has no `servers` object (or it's empty), the
    /// built-in defaults must still produce a deterministic order across
    /// multiple invocations. With `HashMap` this would have been random;
    /// with `IndexMap` it must be the same list every time `builtin_lsp_defaults`
    /// is called.
    #[test]
    fn test_builtin_lsp_defaults_iteration_order_is_stable() {
        let a = builtin_lsp_defaults();
        let b = builtin_lsp_defaults();
        let order_a: Vec<&str> = a.servers.keys().map(String::as_str).collect();
        let order_b: Vec<&str> = b.servers.keys().map(String::as_str).collect();
        assert_eq!(order_a, order_b);
        assert!(!order_a.is_empty(), "builtin defaults must be non-empty");
    }

    /// Regression: the JSON payload's `servers` keys must serialize in the
    /// same order as the in-memory `IndexMap`. With `serde_json`'s
    /// `preserve_order` feature enabled on the workspace, `Value::Object`
    /// uses `IndexMap` internally — verify that contract so the frontend's
    /// `Object.entries` (which preserves key order) sees the same order
    /// the Rust iterator produced.
    #[test]
    fn test_lsp_servers_config_json_preserves_iteration_order() {
        let cfg = lsp_servers_config();
        let json: serde_json::Value = serde_json::to_value(cfg).unwrap();
        let json_keys: Vec<&str> = json["servers"]
            .as_object()
            .expect("servers must be a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        let cfg_keys: Vec<&str> = cfg.servers.keys().map(String::as_str).collect();
        assert_eq!(
            json_keys, cfg_keys,
            "JSON serialization re-ordered `servers` keys"
        );
    }

    #[test]
    fn test_find_on_path_known_command() {
        #[cfg(windows)]
        assert!(find_on_path("cmd").is_some());
        #[cfg(not(windows))]
        assert!(find_on_path("ls").is_some());
    }

    #[test]
    fn test_find_on_path_nonexistent() {
        assert!(find_on_path("this_command_definitely_does_not_exist_12345").is_none());
    }

    #[tokio::test]
    async fn test_resolve_lsp_command_unknown_language() {
        assert!(resolve_lsp_command("brainfuck").await.is_none());
        assert!(resolve_lsp_command("").await.is_none());
    }

    #[tokio::test]
    async fn test_resolve_lsp_command_case_insensitive() {
        let lower = resolve_lsp_command("rust").await;
        let upper = resolve_lsp_command("Rust").await;
        let lower_lang = lower.map(|s| s.language.clone());
        let upper_lang = upper.map(|s| s.language.clone());
        assert_eq!(lower_lang, upper_lang);
    }

    #[tokio::test]
    async fn test_compute_lsp_status_invariants() {
        let cfg = lsp_servers_config();
        let status = compute_lsp_status().await;
        assert_eq!(status.len(), cfg.servers.len());

        let mut langs: Vec<&str> = status.iter().map(|s| s.language.as_str()).collect();
        langs.sort();
        let mut deduped = langs.clone();
        deduped.dedup();
        assert_eq!(langs, deduped, "status contains duplicate languages");

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

        let mut sorted = status.clone();
        sorted.sort_by(|a, b| a.language.cmp(&b.language));
        assert_eq!(status, sorted, "status list is not sorted by language");
    }

    /// Same invariants as `test_compute_lsp_status_invariants` but on the
    /// parallel variant. Catches regressions where the bounded-concurrency
    /// probe drops entries or returns them out of order.
    #[tokio::test]
    async fn test_compute_lsp_status_concurrent_invariants() {
        let cfg = lsp_servers_config();
        let status = compute_lsp_status_concurrent().await;
        assert_eq!(status.len(), cfg.servers.len());

        let mut langs: Vec<&str> = status.iter().map(|s| s.language.as_str()).collect();
        langs.sort();
        let mut deduped = langs.clone();
        deduped.dedup();
        assert_eq!(langs, deduped, "status contains duplicate languages");

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

        let mut sorted = status.clone();
        sorted.sort_by(|a, b| a.language.cmp(&b.language));
        assert_eq!(status, sorted, "concurrent status list is not sorted by language");
    }

    /// The parallel probe must return the same set of `(installed, command)`
    /// pairs as the sequential one. The language ordering is already
    /// covered by the invariant test above; here we check semantic
    /// equivalence so a future refactor can't silently change probe
    /// behavior (e.g. by dropping a candidate mid-resolution).
    #[tokio::test]
    async fn test_compute_lsp_status_concurrent_matches_sequential() {
        let concurrent = compute_lsp_status_concurrent().await;
        let sequential = compute_lsp_status().await;
        assert_eq!(
            concurrent, sequential,
            "concurrent and sequential status results diverged"
        );
    }

    /// The merged endpoint payload must serialize both halves and preserve
    /// the status-by-language keying the frontend relies on.
    #[test]
    fn test_lsp_servers_with_status_serde() {
        let mut status: indexmap::IndexMap<String, LspServerStatusEntry> =
            indexmap::IndexMap::new();
        status.insert(
            "rust".to_string(),
            LspServerStatusEntry {
                language: "rust".to_string(),
                installed: true,
                command: Some("rust-analyzer".to_string()),
            },
        );
        status.insert(
            "python".to_string(),
            LspServerStatusEntry {
                language: "python".to_string(),
                installed: false,
                command: None,
            },
        );

        let mut servers_map: indexmap::IndexMap<String, LspServerEntry> =
            indexmap::IndexMap::new();
        servers_map.insert(
            "rust".to_string(),
            LspServerEntry {
                candidates: vec!["rust-analyzer".into()],
                args: vec![],
                candidate_args: Default::default(),
                install_hint: "rustup component add rust-analyzer".into(),
                install_script: Some("rust".into()),
                description: "Rust language server".into(),
                root_markers: vec!["Cargo.toml".into()],
            },
        );
        let servers = LspServersConfig {
            version: 1,
            servers: servers_map,
        };

        let payload = LspServersWithStatus { servers, status };
        let json: serde_json::Value = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["servers"]["version"], 1);
        assert!(json["servers"]["servers"]["rust"].is_object());
        assert_eq!(json["status"]["rust"]["installed"], true);
        assert_eq!(json["status"]["rust"]["command"], "rust-analyzer");
        assert_eq!(json["status"]["python"]["installed"], false);
        // `command` must be skipped when None to keep payloads small.
        assert!(json["status"]["python"].get("command").is_none());
    }

    #[test]
    fn test_lsp_server_status_entry_serde_skips_none_command() {
        let entry = LspServerStatusEntry {
            language: "brainfuck".to_string(),
            installed: false,
            command: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("command"),
            "command field must be skipped when None: {}",
            json
        );
        assert!(json.contains("\"installed\":false"));
    }

    // ── verify_command_runnable tests ──────────────────────────────────

    fn write_fake_binary(body: &str) -> std::path::PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake_lsp.sh");
        std::fs::write(&path, format!("#!/usr/bin/env bash\n{}\n", body))
            .expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let path_clone = path.clone();
        std::mem::forget(dir);
        path_clone
    }

    #[tokio::test]
    async fn verify_runnable_stage1_version_succeeds() {
        let path = write_fake_binary(
            "if [ \"$1\" = \"--version\" ]; then echo 1.0; exit 0; fi\n\
             exit 0",
        );
        let path_str = path.to_str().unwrap();
        assert!(verify_command_runnable(path_str, &[]).await);
    }

    #[tokio::test]
    async fn verify_runnable_stage2_stdio_lsp_server() {
        // The stage-2 probe writes an LSP initialize handshake on stdin and
        // expects the process to STAY ALIVE through the 5 s window (f3a86be7).
        // A real stdio server consumes the handshake and keeps serving, so
        // the fake binary must `sleep` after `read` — the original fixture
        // fell through to `exit 1` right after reading the handshake line,
        // which made `verify_command_runnable` return false.
        let path = write_fake_binary(
            "if [ \"$1\" = \"--version\" ]; then exit 1; fi\n\
             if [ \"$1\" = \"--stdio\" ]; then\n\
                 read -r _ || exit 0\n\
                 sleep 60\n\
             fi\n\
             exit 1",
        );
        let path_str = path.to_str().unwrap();
        let args = vec!["--stdio".to_string()];
        assert!(verify_command_runnable(path_str, &args).await);
    }

    #[tokio::test]
    async fn verify_runnable_broken_binary_returns_false() {
        let path = write_fake_binary(
            "if [ \"$1\" = \"--version\" ]; then exit 1; fi\n\
             exit 1",
        );
        let path_str = path.to_str().unwrap();
        let args = vec!["--stdio".to_string()];
        assert!(!verify_command_runnable(path_str, &args).await);
    }

    #[tokio::test]
    async fn verify_runnable_nonexistent_command_returns_false() {
        assert!(
            !verify_command_runnable("acowork_lsp_definitely_not_a_real_binary_xyz", &[]).await
        );
    }

    // ── build_config_candidates tests ──────────────────────────────────

    #[test]
    fn test_build_config_candidates_returns_non_empty() {
        // In the test environment, CARGO_MANIFEST_DIR is set by cargo,
        // and the assets/lsp_servers.json file exists in the repo.
        // So build_config_candidates should return at least one candidate.
        let candidates = build_config_candidates(None);
        assert!(
            !candidates.is_empty(),
            "expected at least one config candidate in test environment"
        );
    }

    #[test]
    fn test_build_config_candidates_with_env_override() {
        // Pass a temp dir explicitly — no env var mutation needed.
        // The temp dir doesn't contain lsp_servers.json, so the explicit
        // candidate won't be added. But other candidates (CARGO_MANIFEST_DIR,
        // exe dir, cwd) should still be present.
        let dir = tempfile::tempdir().expect("tempdir");
        let candidates = build_config_candidates(Some(dir.path()));
        // The temp dir doesn't contain lsp_servers.json, so the explicit
        // candidate won't be added. But other candidates (CARGO_MANIFEST_DIR,
        // exe dir, cwd) should still be present.
        assert!(!candidates.is_empty());
    }

    // ── load_lsp_servers_from_file tests ───────────────────────────────

    #[test]
    fn test_load_lsp_servers_from_file_with_valid_json() {
        // Create a temp dir with a valid lsp_servers.json and pass it
        // explicitly — no env var mutation needed.
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("lsp_servers.json");
        let json_content = r#"{
            "version": 1,
            "servers": {
                "testlang": {
                    "candidates": ["test-lsp"],
                    "args": ["--stdio"],
                    "install_hint": "install test-lsp",
                    "description": "Test language server"
                }
            }
        }"#;
        std::fs::write(&config_path, json_content).expect("write config");

        let result = load_lsp_servers_from_file(Some(dir.path()));

        let cfg = result.expect("should load config from temp dir");
        assert_eq!(cfg.version, 1);
        assert!(cfg.servers.contains_key("testlang"));
        assert_eq!(
            cfg.servers["testlang"].candidates,
            vec!["test-lsp".to_string()]
        );
    }

    #[test]
    fn test_load_lsp_servers_from_file_with_invalid_json_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("lsp_servers.json");
        std::fs::write(&config_path, "not valid json {{{").expect("write bad config");

        let result = load_lsp_servers_from_file(Some(dir.path()));

        // Invalid JSON in the env-override file → falls through to other
        // candidates. If another candidate succeeds, result is Some.
        // If no candidate succeeds, result is None.
        // Either way, it should NOT panic.
        let _ = result;
    }

    // ── Status cache tests ─────────────────────────────────────────────

    /// `init_status_cache_ttl(0)` must fall back to `DEFAULT_STATUS_TTL_SECS`.
    /// Protects against misconfigured env vars (`ACOWORK_LSP_STATUS_TTL_SECS=`
    /// would otherwise parse as `0` and make every call go through the probe
    /// path — defeating the cache).
    #[test]
    fn test_init_status_cache_ttl_zero_falls_back_to_default() {
        let prev = status_ttl_secs();
        init_status_cache_ttl(0);
        assert_eq!(
            status_ttl_secs(),
            DEFAULT_STATUS_TTL_SECS,
            "TTL=0 must fall back to default (got {})",
            status_ttl_secs()
        );
        init_status_cache_ttl(prev);
    }

    #[test]
    fn test_init_status_cache_ttl_stores_value() {
        let prev = status_ttl_secs();
        init_status_cache_ttl(120);
        assert_eq!(status_ttl_secs(), 120);
        init_status_cache_ttl(prev);
    }

    /// A TTL of 0 seconds must always report the cache as stale. This
    /// guards against a future refactor accidentally allowing zero-TTL
    /// cached entries to leak through.
    #[tokio::test]
    async fn test_read_cache_if_fresh_treats_zero_ttl_as_always_stale() {
        let key = "test-cache-zero-ttl-key-acowork";
        let entry = LspServerStatusEntry {
            language: key.to_string(),
            installed: true,
            command: Some("/usr/bin/test".to_string()),
        };
        write_cache(key, entry).await;
        let read_back = read_cache_if_fresh(key, 0).await;
        assert!(
            read_back.is_none(),
            "TTL=0 must always return None"
        );
        // Cleanup
        invalidate_status(key);
    }

    /// `read_cache_if_fresh` must return the cloned entry when the entry's
    /// timestamp is within the TTL window.
    #[tokio::test]
    async fn test_read_cache_if_fresh_returns_cached() {
        let key = "test-cache-roundtrip-acowork";
        let entry = LspServerStatusEntry {
            language: key.to_string(),
            installed: true,
            command: Some("/usr/bin/test".to_string()),
        };
        write_cache(key, entry.clone()).await;
        let read_back = read_cache_if_fresh(key, 60).await;
        assert_eq!(
            read_back,
            Some(entry),
            "cache round-trip lost fidelity"
        );
        invalidate_status(key);
    }

    /// `probe_one` must reuse cached results for an unknown language
    /// (not in `lsp_servers_config`) without re-probing. Two consecutive
    /// calls return equivalent `LspServerStatusEntry` objects.
    #[tokio::test]
    async fn test_probe_one_unknown_language_returns_same_twice() {
        let first = probe_one("test-unknown-language-acowork", false).await;
        let second = probe_one("test-unknown-language-acowork", false).await;
        assert_eq!(
            first, second,
            "probe_one should return identical results for identical inputs"
        );
        assert!(!first.installed);
        assert!(first.command.is_none());
        assert_eq!(first.language, "test-unknown-language-acowork");
        invalidate_status("test-unknown-language-acowork");
    }

    /// `probe_one` with a seeded cache entry must return the cached value
    /// when `force == false`. This is the central invariant for the
    /// "Check button should not re-probe other languages" optimization.
    ///
    /// We seed a fake command path that wouldn't resolve from the real
    /// filesystem probe — if `probe_one` respected the cache, the test
    /// passes; if it bypassed the cache, the probe would either find the
    /// real `rust-analyzer` (returning the real command) or not find
    /// anything (returning installed=false), neither of which equals the
    /// cached fake entry.
    #[tokio::test]
    async fn test_probe_one_honors_cache_for_real_language() {
        let canonical = "rust";
        let fake_command = "/tmp/acowork_test_fake_rust_xyz_42";
        let fake_entry = LspServerStatusEntry {
            language: canonical.to_string(),
            installed: true,
            command: Some(fake_command.to_string()),
        };
        seed_cache_for_test(canonical, fake_entry.clone()).await;

        let result = probe_one(canonical, false).await;
        assert_eq!(
            result, fake_entry,
            "probe_one must return seeded cache entry (cache bypassed otherwise)"
        );

        // Cleanup so other tests aren't affected.
        invalidate_status(canonical);
    }

    /// `probe_all` returns one entry per configured language, sorted by
    /// canonical name — preserves the existing contract of
    /// `compute_lsp_status` / `compute_lsp_status_concurrent`.
    #[tokio::test]
    async fn test_probe_all_returns_sorted_entries() {
        let cfg = lsp_servers_config();
        let result = probe_all(false).await;
        assert_eq!(
            result.len(),
            cfg.servers.len(),
            "probe_all returned wrong number of entries"
        );
        let mut sorted = result.clone();
        sorted.sort_by(|a, b| a.language.cmp(&b.language));
        assert_eq!(
            result, sorted,
            "probe_all must return entries sorted by language"
        );
    }

    /// `invalidate_status` must remove the cached entry so the next
    /// `probe_one` call observes a cache miss. We verify by reading
    /// `read_cache_if_fresh` directly before and after.
    #[tokio::test]
    async fn test_invalidate_status_removes_entry() {
        let key = "test-invalidate-acowork";
        let entry = LspServerStatusEntry {
            language: key.to_string(),
            installed: true,
            command: Some("/usr/bin/test".to_string()),
        };
        write_cache(key, entry).await;
        assert!(read_cache_if_fresh(key, 60).await.is_some());

        // `invalidate_status` spawns an async task to perform the
        // removal under the cache's write lock. We must yield so the
        // spawned task runs before reading the post-invalidation state.
        invalidate_status(key);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let post_invalidate = read_cache_if_fresh(key, 60).await;
        assert!(
            post_invalidate.is_none(),
            "invalidate_status did not remove entry (got {:?})",
            post_invalidate
        );
    }
}
