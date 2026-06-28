//! Gateway configuration and process management commands.
//!
//! Frontend is the source of truth for gateway mode + URL (persisted in
//! its settingsStore). On startup the frontend MUST call
//! [`set_gateway_config`] to push its persisted values into Rust so that
//! all HTTP commands use the correct base URL and the local spawn is
//! skipped in remote mode. See module docs of `crate::state` for details.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::state::{AppState, GatewayMode};
use acowork_core::defaults;
use serde::{Deserialize, Serialize};
use tauri::Manager;

/// Payload for [`set_gateway_config`].
#[derive(Debug, Deserialize)]
pub struct GatewayConfigInput {
    /// `"local"` or `"remote"` (anything else falls back to `local`)
    pub mode: String,
    /// User-configured URL. Only used in remote mode; ignored in local mode
    /// (local always listens on `acowork_core::defaults::GATEWAY_HTTP_URL`).
    #[serde(default)]
    pub url: String,
}

/// Returned by [`get_gateway_config`] so the frontend can re-sync after
/// reload / external mutation.
#[derive(Debug, Serialize)]
pub struct GatewayConfigOutput {
    pub mode: String,
    pub base_url: String,
}

/// Push the persisted gateway configuration from the frontend into Rust.
///
/// - Local mode: `base_url` is forced to `acowork_core::defaults::GATEWAY_HTTP_URL`
///   regardless of what the frontend sends. This guarantees the spawned
///   Gateway and the HTTP client always agree on the same address.
/// - Remote mode: `base_url` is taken from the input. Trust-on-save: no
///   health probe (the user sees connection errors in the UI if unreachable).
///
/// If the mode changes from local→remote while a local Gateway is running,
/// the running local process is stopped to avoid leaving an orphan on the
/// default port. The reverse (remote→local) does NOT auto-spawn; the
/// frontend must call [`init_local_gateway`] to start a new local instance.
#[tauri::command]
pub async fn set_gateway_config(
    state: tauri::State<'_, AppState>,
    config: GatewayConfigInput,
) -> Result<GatewayConfigOutput, String> {
    let mode = GatewayMode::from_str(&config.mode);
    tracing::info!(
        "[CFG] set_gateway_config: mode={:?}, url={:?}",
        mode,
        config.url
    );

    // Resolve base_url per mode policy
    let base_url = match mode {
        GatewayMode::Local => defaults::GATEWAY_HTTP_URL.to_string(),
        GatewayMode::Remote => {
            let trimmed = config.url.trim().trim_end_matches('/').to_string();
            if trimmed.is_empty() {
                return Err("Remote gateway URL cannot be empty".to_string());
            }
            trimmed
        }
    };

    // Update HTTP client base_url
    {
        let mut client = state.gateway.write().await;
        client.set_base_url(base_url.clone());
    }

    // Update mode
    {
        let mut m = state.gateway_mode.write().await;
        *m = mode;
    }

    // If switching to remote, stop any locally-spawned Gateway to free the port
    if mode == GatewayMode::Remote {
        let mut proc = state.gateway_process.lock().await;
        if let Some(mut child) = proc.take() {
            tracing::info!(
                "[CFG] Switching to remote: stopping local Gateway (pid: {:?})",
                child.id()
            );
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    Ok(GatewayConfigOutput {
        mode: mode.as_str().to_string(),
        base_url,
    })
}

/// Read back the current configuration from Rust. Used by the frontend to
/// detect drift (e.g. after a Rust-side default change).
#[tauri::command]
pub async fn get_gateway_config(
    state: tauri::State<'_, AppState>,
) -> Result<GatewayConfigOutput, String> {
    let mode = *state.gateway_mode.read().await;
    let client = state.gateway.read().await;
    Ok(GatewayConfigOutput {
        mode: mode.as_str().to_string(),
        base_url: client.base_url().to_string(),
    })
}

/// Spawn the local Gateway process and wait for it to become ready.
///
/// This is the ONLY place local Gateway is spawned. It is called from the
/// frontend (SplashScreen init) after `set_gateway_config`, so we know:
///   - The mode is `local` (otherwise this is an error)
///   - The `GatewayClient.base_url` points at the local default
///
/// Returns once `/health` responds on the configured base URL (max ~10s).
#[tauri::command]
pub async fn init_local_gateway(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<String, String> {
    {
        let m = state.gateway_mode.read().await;
        if *m != GatewayMode::Local {
            return Err(format!(
                "init_local_gateway called in {:?} mode; refusing to spawn local process",
                *m
            ));
        }
    }

    spawn_gateway(&state, &app_handle).await?;

    // Determine the URL to poll. In local mode this is always the
    // shared default constant, but we still read it from the client to
    // honour any future override.
    let base_url = state.gateway.read().await.base_url().to_string();
    wait_for_gateway_ready(&base_url).await?;
    Ok(base_url)
}

/// System Agent ID — always bundled with Desktop App.
pub const SYSTEM_AGENT_ID: &str = "com.acowork.system";

/// Auto-install the bundled System Agent if not already installed.
///
/// Called by the frontend after `init_local_gateway` (local mode) or
/// directly after `set_gateway_config` (remote mode, where the Gateway
/// is presumed already running). Uses `state.gateway.base_url` so it
/// targets whichever Gateway the user has configured.
#[tauri::command]
pub async fn ensure_system_agent(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    use tokio::time::{Duration, sleep};

    // Resolve URL from AppState (single source of truth)
    let gateway_url = state.gateway.read().await.base_url().to_string();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    // Wait for Gateway to be reachable (max ~15s)
    for i in 0..30 {
        if client
            .get(format!("{}/health", gateway_url))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        sleep(Duration::from_millis(500)).await;
        if i % 6 == 0 {
            tracing::debug!(
                "[SYS-AGENT] Waiting for Gateway at {} to be ready...",
                gateway_url
            );
        }
    }

    // Check if System Agent is already installed
    match client
        .get(format!("{}/api/agents/{}", gateway_url, SYSTEM_AGENT_ID))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!("[SYS-AGENT] Already installed, skipping");
            return Ok(());
        }
        _ => {}
    }

    // Locate the bundled System Agent on disk
    let resource_dir = app_handle
        .path()
        .resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
    let system_agent_package = resource_dir
        .join("agent-packages")
        .join("com.acowork.system.agent");

    if !system_agent_package.exists() {
        tracing::warn!(
            "[SYS-AGENT] Bundled package not found at {:?}",
            system_agent_package
        );
        return Ok(());
    }

    tracing::info!(
        "[SYS-AGENT] Installing bundled package from {:?}",
        system_agent_package
    );

    let package_bytes = std::fs::read(&system_agent_package)
        .map_err(|e| format!("Failed to read System Agent package: {}", e))?;
    let form = reqwest::multipart::Form::new()
        .part(
            "package",
            reqwest::multipart::Part::bytes(package_bytes)
                .file_name("com.acowork.system.agent")
                .mime_str("application/octet-stream")
                .map_err(|e| format!("Invalid package mime: {}", e))?,
        )
        .text("dev_mode", "true");

    match client
        .post(format!("{}/api/agents/install", gateway_url))
        .multipart(form)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                tracing::info!("[SYS-AGENT] Auto-install succeeded");
            } else {
                let error = resp.text().await.unwrap_or_default();
                tracing::warn!("[SYS-AGENT] Install failed: {}", error);
            }
        }
        Err(e) => {
            tracing::warn!("[SYS-AGENT] Install call failed: {}", e);
        }
    }

    Ok(())
}

/// Start the local Gateway process (used by the Settings page "Start" button).
///
/// Refuses in remote mode. Use [`init_local_gateway`] on first launch
/// instead — this entry point is only for manual restart.
#[tauri::command]
pub async fn start_local_gateway(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    {
        let m = state.gateway_mode.read().await;
        if *m != GatewayMode::Local {
            return Err(format!(
                "start_local_gateway called in {:?} mode; refusing to spawn",
                *m
            ));
        }
    }
    spawn_gateway(&state, &app_handle).await?;
    let base_url = state.gateway.read().await.base_url().to_string();
    wait_for_gateway_ready(&base_url).await
}

/// Spawn the Gateway process without waiting for readiness.
/// Used by both `start_local_gateway` command and Rust-side early startup.
///
/// # Concurrency
///
/// This function is safe to call concurrently from multiple Tauri commands.
/// The entire decision pipeline (`check in-process handle → probe port →
/// kill stale → spawn → store`) runs under a single `Mutex` critical
/// section, so React StrictMode's dev double-invocation, or any other
/// race, results in **at most one** child Gateway process. Late callers
/// see the freshly-spawned child stored in `state.gateway_process` and
/// return immediately. A port probe additionally detects Gateway
/// instances started outside of Tauri (e.g. a leftover from a crashed
/// previous run still holding the port) and skips spawning in that case
/// as well.
pub async fn spawn_gateway(state: &AppState, app_handle: &tauri::AppHandle) -> Result<(), String> {
    tracing::info!("[BOOT] spawn_gateway entered");

    // ── Single critical section: serialise check → spawn → store ─────
    // Holding the lock across the spawn guarantees that any concurrent
    // caller observes either `None` (and waits) or a valid `Child`
    // (and skips the work entirely).
    let mut proc_guard = state.gateway_process.lock().await;

    // 1) In-process handle check: did we already spawn one?
    if let Some(ref child) = *proc_guard {
        if child_output_is_alive(child) {
            tracing::info!("[BOOT] Gateway already running (in-process handle), skipping spawn");
            return Ok(());
        }
    }

    // 2) Port probe: is there already a Gateway listening on the default
    //    URL? Covers the case of a stale process from a previous run that
    //    is reachable but was never tracked in our handle.
    let base_url = defaults::GATEWAY_HTTP_URL;
    if is_gateway_reachable(base_url).await {
        tracing::info!(
            "[BOOT] Gateway already reachable at {} (external), skipping spawn",
            base_url
        );
        return Ok(());
    }

    // 3) Kill any stale local Gateway from a previous run.
    //    We only get here if no in-process handle AND port is free, so
    //    anything left holding the port is genuinely stale.
    tracing::info!("[BOOT] Checking for stale Gateway processes...");
    kill_stale_gateway_process();
    tracing::info!("[BOOT] Stale Gateway cleanup done");

    // 4) Find binary + spawn the new child.
    let gateway_bin = find_gateway_binary(app_handle.clone())?;
    tracing::info!("[BOOT] Starting local Gateway: {}", gateway_bin.display());

    // Get Tauri resource directory — where bundled assets (lsp_servers.json,
    // lsp_install/, offline_providers.json, etc.) are extracted.
    // Pass it to Gateway via ACOWORK_LSP_CONFIG_DIR env var. The Gateway
    // passes this to the LSP Relay subprocess, which uses it to locate
    // lsp_servers.json and install scripts.
    let resource_dir = app_handle
        .path()
        .resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
    tracing::info!("[BOOT] Tauri resource_dir: {}", resource_dir.display());

    let mut command = Command::new(&gateway_bin);
    command
        .current_dir(&resource_dir)
        .env("ACOWORK_GATEWAY_DAEMON", "true")
        .env("ACOWORK_GATEWAY_LOG_LEVEL", "debug")
        .env("RUST_LOG", "debug,h2=info,hyper=info,hyper_util=info,tower=info")
        .env(
            "ACOWORK_LSP_CONFIG_DIR",
            resource_dir.to_string_lossy().to_string(),
        );
    // Suppress the pop-up Windows Terminal / conhost window. The Gateway
    // is a console-subsystem binary, so when spawned from a non-console
    // parent (the Tauri WebView2 host), Windows otherwise allocates a new
    // console and the user's screen gets a flashing terminal showing the
    // gateway's startup banner — confusing in a desktop app. Logs are
    // already captured by the Gateway's own file logger
    // (data/logs/gateway.log), and debug output is forwarded to the
    // Tauri dev console in dev mode. CREATE_NO_WINDOW has no effect on
    // macOS / Linux; the cfg gate keeps it platform-correct.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    if let Some(ort_lib_dir) = locate_bundled_ort_lib_dir(&gateway_bin, &resource_dir) {
        inject_ort_env(&mut command, &ort_lib_dir);
        tracing::info!(ort_lib_dir = %ort_lib_dir.display(), "Injected bundled ORT for Gateway");
    }
    let child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn Gateway process: {}", e))?;

    tracing::info!("[BOOT] Gateway process spawned, pid: {:?}", child.id());

    // 5) Store the handle. The next concurrent caller will see this
    //    inside the same lock and return early at step 1.
    *proc_guard = Some(child);

    Ok(())
}

/// Stop the locally running Gateway process and all its children.
///
/// On Windows, uses `taskkill /T /F` to kill the entire process tree
/// (Gateway + Runtime + Embed) in one shot.
/// On Unix, sends SIGINT so the Gateway's signal handler cleans up
/// children before exiting.
#[tauri::command]
pub async fn stop_local_gateway(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut proc = state.gateway_process.lock().await;
    if let Some(child) = proc.take() {
        let pid = child.id();
        tracing::info!(pid = pid, "Stopping local Gateway process tree");
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
        }
        #[cfg(not(target_os = "windows"))]
        {
            // Send SIGINT so Gateway's signal handler cleans up children
            let _ = std::process::Command::new("kill")
                .args(["-INT", &pid.to_string()])
                .output();
        }
        // Reap the child to avoid zombies
        let mut child = child; // Child::wait needs &mut
        let _ = child.wait();
    }
    *proc = None;
    Ok(())
}

/// Check if the local Gateway process is currently running.
#[tauri::command]
pub async fn get_local_gateway_status(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let proc = state.gateway_process.lock().await;
    if let Some(ref child) = *proc {
        Ok(child_output_is_alive(child))
    } else {
        Ok(false)
    }
}

// ── Helper functions ────────────────────────────────────────────────────

fn locate_bundled_ort_lib_dir(gateway_bin: &Path, resource_dir: &Path) -> Option<PathBuf> {
    let gateway_dir = gateway_bin.parent();
    let candidates = [gateway_dir, Some(resource_dir)];
    for dir in candidates.into_iter().flatten() {
        if ort_lib_exists(dir) {
            return Some(dir.to_path_buf());
        }
    }
    locate_workspace_ort_lib_dir()
}

fn locate_workspace_ort_lib_dir() -> Option<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let mut base = PathBuf::from(manifest_dir);
    for _ in 0..3 {
        base = base.parent()?.to_path_buf();
    }
    let ort_base = base.join(".ort");
    let entries = std::fs::read_dir(ort_base).ok()?;
    for entry in entries.flatten() {
        let lib_dir = entry.path().join("lib");
        if ort_lib_exists(&lib_dir) {
            return Some(lib_dir);
        }
    }
    None
}

fn ort_lib_exists(dir: &Path) -> bool {
    let lib_path = if cfg!(windows) {
        dir.join("onnxruntime.dll")
    } else if cfg!(target_os = "macos") {
        dir.join("libonnxruntime.dylib")
    } else {
        dir.join("libonnxruntime.so")
    };
    lib_path.exists()
}

fn inject_ort_env(command: &mut Command, ort_lib_dir: &Path) {
    let dylib_path = if cfg!(windows) {
        ort_lib_dir.join("onnxruntime.dll")
    } else if cfg!(target_os = "macos") {
        ort_lib_dir.join("libonnxruntime.dylib")
    } else {
        ort_lib_dir.join("libonnxruntime.so")
    };
    command.env("ORT_LIB_LOCATION", ort_lib_dir);
    command.env("ORT_DYLIB_PATH", dylib_path);
    command.env("ORT_PREFER_DYNAMIC_LINK", "1");

    #[cfg(windows)]
    {
        let previous = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{};{}", ort_lib_dir.display(), previous));
    }
    #[cfg(target_os = "macos")]
    {
        let previous = std::env::var("DYLD_LIBRARY_PATH").unwrap_or_default();
        command.env(
            "DYLD_LIBRARY_PATH",
            format!("{}:{}", ort_lib_dir.display(), previous),
        );
    }
    #[cfg(target_os = "linux")]
    {
        let previous = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        command.env(
            "LD_LIBRARY_PATH",
            format!("{}:{}", ort_lib_dir.display(), previous),
        );
    }
}

/// Find the Gateway binary next to the current executable.
pub fn find_gateway_binary(app_handle: tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    // In development, Gateway binary lives next to current_exe in target/release/ or target/debug/.
    // In production (bundled), it's extracted next to the Desktop app.
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("Failed to get current exe path: {}", e))?
        .parent()
        .ok_or("Current exe has no parent directory")?
        .to_path_buf();

    // Also check the Tauri resource directory (for bundled builds)
    let resource_dir = app_handle.path().resource_dir().unwrap_or(exe_dir.clone());

    let candidates = [
        exe_dir.join("acowork-gateway.exe"),
        exe_dir.join("acowork-gateway"),
        resource_dir.join("acowork-gateway.exe"),
        resource_dir.join("acowork-gateway"),
    ];

    for path in &candidates {
        if path.exists() {
            return Ok(path.clone());
        }
    }

    // Also try the workspace target/ directory for dev convenience.
    // Check release first, then debug.
    if let Ok(manifest_dir) =
        std::env::var("CARGO_MANIFEST_DIR").or_else(|_| std::env::var("TAURI_DEV_DIR"))
    {
        let manifest_path = std::path::PathBuf::from(&manifest_dir);
        // manifest_dir = .../apps/acowork-desktop/src-tauri
        // Go up 3 levels to workspace root: .../apps/acowork-desktop/src-tauri -> .../
        let mut base = manifest_path.clone();
        for _ in 0..3 {
            if let Some(parent) = base.parent() {
                base = parent.to_path_buf();
            } else {
                break;
            }
        }

        for profile in &["release", "debug"] {
            let target_dir = base.join("target").join(profile);
            let exe = target_dir.join("acowork-gateway.exe");
            let bin = target_dir.join("acowork-gateway");
            if exe.exists() {
                return Ok(exe);
            }
            if bin.exists() {
                return Ok(bin);
            }
        }
    }

    Err(format!(
        "Gateway binary not found. Searched: {:?}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
    ))
}

/// Wait for Gateway health endpoint to become ready.
///
/// `base_url` must come from `AppState.gateway.base_url` so it matches
/// what HTTP commands will use (local default or remote URL).
async fn wait_for_gateway_ready(base_url: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    let health_url = format!("{}/health", base_url);

    // Poll for up to 10 seconds (34 * 300ms)
    for i in 0..34 {
        if client.get(&health_url).send().await.is_ok() {
            tracing::info!("Gateway is ready at {}", base_url);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        if i % 5 == 0 {
            tracing::debug!("Waiting for Gateway at {} to be ready...", base_url);
        }
    }

    Err(format!(
        "Gateway at {} did not become ready within 10 seconds",
        base_url
    ))
}

/// One-shot health probe used inside `spawn_gateway` to detect a Gateway
/// already listening on the default port (e.g. orphaned by a previous
/// crash). Returns `true` as soon as `/health` responds once; returns
/// `false` on any error or timeout. Intentionally short timeout to keep
/// the spawn path snappy.
async fn is_gateway_reachable(base_url: &str) -> bool {
    let health_url = format!("{}/health", base_url);
    let probe = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build();
    let Ok(client) = probe else {
        return false;
    };
    matches!(client.get(&health_url).send().await, Ok(resp) if resp.status().is_success())
}

/// Check if a child process output indicates it is alive.
fn child_output_is_alive(child: &std::process::Child) -> bool {
    // Child::id() returns the PID assigned at spawn; if the process
    // was never spawned or has been reaped, id() still returns the
    // original PID. For our purposes, we treat a non-zero PID as
    // "was successfully spawned".
    child.id() > 0
}

/// Kill any stale local Gateway process left from a previous Desktop App run.
///
/// This is only safe to call when we KNOW we're in local mode (and therefore
/// are about to spawn a new child Gateway that needs the default port).
/// All call sites go through [`spawn_gateway`], which is itself only reachable
/// from `init_local_gateway` / `start_local_gateway`, both of which check
/// `gateway_mode == Local` first.
fn kill_stale_gateway_process() {
    #[cfg(target_os = "windows")]
    {
        // Use `taskkill` to find and kill acowork-gateway processes.
        // /FI "PID ne <ours>" is not reliable here since we don't track
        // the old PID. Instead, just kill all acowork-gateway processes
        // — our own child won't exist yet at this point in the flow.
        let output = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "acowork-gateway.exe"])
            .output();
        match output {
            Ok(o) if o.status.success() => {
                tracing::info!("Killed stale Gateway process from previous run");
            }
            _ => {
                // No stale process found — this is the normal case
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let output = std::process::Command::new("pkill")
            .args(["-f", "acowork-gateway"])
            .output();
        match output {
            Ok(o) if o.status.success() => {
                tracing::info!("Killed stale Gateway process from previous run");
            }
            _ => {
                // No stale process found
            }
        }
    }
}
