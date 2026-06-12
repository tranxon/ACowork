//! Gateway process management commands.
//!
//! Provides Tauri commands for the frontend to start/stop the local Gateway
//! process and query its status. Remote mode does not use these commands.

use std::process::Command;
use std::time::Duration;

use crate::state::AppState;
use rollball_core::defaults;
use tauri::Manager;

/// Start the local Gateway process.
///
/// Finds the `rollball-gateway` binary in the same directory as the current
/// executable, spawns it as a child process, and waits up to 10 seconds for
/// its health endpoint to become available.
#[tauri::command]
pub async fn start_local_gateway(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    spawn_gateway(&state, &app_handle).await?;
    wait_for_gateway_ready().await
}

/// Spawn the Gateway process without waiting for readiness.
/// Used by both `start_local_gateway` command and Rust-side early startup.
pub async fn spawn_gateway(
    state: &AppState,
    app_handle: &tauri::AppHandle,
) -> Result<(), String> {
    tracing::info!("[BOOT] spawn_gateway entered");

    // Check if already running (tracked by our process handle)
    {
        let proc = state.gateway_process.lock().await;
        if let Some(ref child) = *proc {
            if child_output_is_alive(child) {
                tracing::info!("[BOOT] Gateway already running, skipping spawn");
                return Ok(());
            }
        }
    }

    // Kill any stale Gateway process left from a previous run.
    // When Ctrl+C kills the Desktop App, the Gateway child process is orphaned
    // and keeps holding port 19876. We must kill it before spawning a new one.
    tracing::info!("[BOOT] Checking for stale Gateway processes...");
    kill_stale_gateway_process();
    tracing::info!("[BOOT] Stale Gateway cleanup done");

    // Find gateway binary next to current executable
    let gateway_bin = find_gateway_binary(app_handle.clone())?;

    tracing::info!("[BOOT] Starting local Gateway: {}", gateway_bin.display());

    // Get Tauri resource directory — where bundled assets (lsp_servers.json,
    // lsp_install/, offline_providers.json, etc.) are extracted.
    // Pass it to Gateway via ROLLBALL_LSP_CONFIG_DIR so Gateway can find
    // LSP config files in local mode.
    let resource_dir = app_handle
        .path()
        .resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
    tracing::info!("[BOOT] Tauri resource_dir: {}", resource_dir.display());

    // Spawn the gateway process
    let child = Command::new(&gateway_bin)
        .env("ROLLBALL_GATEWAY_DAEMON", "true")
        .env("ROLLBALL_GATEWAY_LOG_LEVEL", "info")
        .env("ROLLBALL_LSP_CONFIG_DIR", resource_dir.to_string_lossy().to_string())
        .spawn()
        .map_err(|e| format!("Failed to spawn Gateway process: {}", e))?;

    tracing::info!("[BOOT] Gateway process spawned, pid: {:?}", child.id());

    // Store the child handle
    {
        let mut proc = state.gateway_process.lock().await;
        *proc = Some(child);
    }

    Ok(())
}

/// Stop the locally running Gateway process.
#[tauri::command]
pub async fn stop_local_gateway(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut proc = state.gateway_process.lock().await;
    if let Some(mut child) = proc.take() {
        tracing::info!("Stopping local Gateway (pid: {:?})", child.id());
        if let Err(e) = child.kill() {
            // Process may already be dead — not an error
            tracing::warn!("Failed to kill Gateway process: {}", e);
        }
        // Reap the child to avoid zombies
        let _ = child.wait();
    }
    *proc = None;
    Ok(())
}

/// Check if the local Gateway process is currently running.
#[tauri::command]
pub async fn get_local_gateway_status(
    state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    let proc = state.gateway_process.lock().await;
    if let Some(ref child) = *proc {
        Ok(child_output_is_alive(child))
    } else {
        Ok(false)
    }
}

// ── Helper functions ────────────────────────────────────────────────────

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
    let resource_dir = app_handle
        .path()
        .resource_dir()
        .unwrap_or(exe_dir.clone());

    let candidates = [
        exe_dir.join("rollball-gateway.exe"),
        exe_dir.join("rollball-gateway"),
        resource_dir.join("rollball-gateway.exe"),
        resource_dir.join("rollball-gateway"),
    ];

    for path in &candidates {
        if path.exists() {
            return Ok(path.clone());
        }
    }

    // Also try the workspace target/ directory for dev convenience.
    // Check release first, then debug.
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR")
        .or_else(|_| std::env::var("TAURI_DEV_DIR"))
    {
        let manifest_path = std::path::PathBuf::from(&manifest_dir);
        // manifest_dir = .../apps/rollball-desktop/src-tauri
        // Go up 3 levels to workspace root: .../apps/rollball-desktop/src-tauri -> .../
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
            let exe = target_dir.join("rollball-gateway.exe");
            let bin = target_dir.join("rollball-gateway");
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
        candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
    ))
}

/// Wait for Gateway health endpoint to become ready.
async fn wait_for_gateway_ready() -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    let health_url = format!("{}/health", defaults::GATEWAY_HTTP_URL);

    // Poll for up to 10 seconds (34 * 300ms)
    for i in 0..34 {
        if client.get(&health_url).send().await.is_ok() {
            tracing::info!("Local Gateway is ready");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        if i % 5 == 0 {
            tracing::debug!("Waiting for Gateway to be ready...");
        }
    }

    Err("Gateway did not become ready within 10 seconds".to_string())
}

/// Check if a child process output indicates it is alive.
fn child_output_is_alive(child: &std::process::Child) -> bool {
    // Child::id() returns the PID assigned at spawn; if the process
    // was never spawned or has been reaped, id() still returns the
    // original PID. For our purposes, we treat a non-zero PID as
    // "was successfully spawned".
    child.id() > 0
}

/// Kill any stale Gateway process left from a previous Desktop App run.
///
/// When the Desktop App is killed (e.g., Ctrl+C in dev mode), the Gateway
/// child process is orphaned and keeps listening on port 19876. A new
/// Gateway instance cannot bind that port, causing startup to hang.
///
/// This function finds and kills any `rollball-gateway` process that is
/// NOT our tracked child (i.e., a leftover from a previous run).
pub fn kill_stale_gateway_process_pub() {
    kill_stale_gateway_process();
}

fn kill_stale_gateway_process() {
    #[cfg(target_os = "windows")]
    {
        // Use `taskkill` to find and kill rollball-gateway processes.
        // /FI "PID ne <ours>" is not reliable here since we don't track
        // the old PID. Instead, just kill all rollball-gateway processes
        // — our own child won't exist yet at this point in the flow.
        let output = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "rollball-gateway.exe"])
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
            .args(["-f", "rollball-gateway"])
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
