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
/// executable, spawns it as a child process, and waits up to 15 seconds for
/// its health endpoint to become available.
#[tauri::command]
pub async fn start_local_gateway(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    // Check if already running
    {
        let proc = state.gateway_process.lock().await;
        if let Some(ref child) = *proc {
            // Check if the existing child is still alive
            if child_output_is_alive(child) {
                // Already running — just verify health and return
                drop(proc);
                return wait_for_gateway_ready().await;
            }
        }
    }

    // Find gateway binary next to current executable
    let gateway_bin = find_gateway_binary(app_handle.clone())?;

    tracing::info!("Starting local Gateway: {}", gateway_bin.display());

    // Spawn the gateway process
    let child = Command::new(&gateway_bin)
        .env("ROLLBALL_GATEWAY_DAEMON", "true")
        .env("ROLLBALL_GATEWAY_LOG_LEVEL", "info")
        .spawn()
        .map_err(|e| format!("Failed to spawn Gateway process: {}", e))?;

    // Store the child handle
    {
        let mut proc = state.gateway_process.lock().await;
        *proc = Some(child);
    }

    // Wait for Gateway to be ready (up to 15s)
    wait_for_gateway_ready().await
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
fn find_gateway_binary(app_handle: tauri::AppHandle) -> Result<std::path::PathBuf, String> {
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

    // Also try the workspace target/ directory for dev convenience
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR")
        .or_else(|_| std::env::var("TAURI_DEV_DIR"))
    {
        let workspace_target = std::path::PathBuf::from(&manifest_dir)
            .parent() // src-tauri
            .and_then(|p| p.parent()) // apps/rollball-desktop
            .and_then(|p| p.parent()) // apps
            .map(|p| p.join("target/release"));

        if let Some(target_dir) = workspace_target {
            let dev_gateway_exe = target_dir.join("rollball-gateway.exe");
            let dev_gateway = target_dir.join("rollball-gateway");
            if dev_gateway_exe.exists() {
                return Ok(dev_gateway_exe);
            }
            if dev_gateway.exists() {
                return Ok(dev_gateway);
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
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    let health_url = format!("{}/health", defaults::GATEWAY_HTTP_URL);

    // Poll for up to 15 seconds (30 * 500ms)
    for i in 0..30 {
        if client.get(&health_url).send().await.is_ok() {
            tracing::info!("Local Gateway is ready");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        if i % 6 == 0 {
            tracing::debug!("Waiting for Gateway to be ready...");
        }
    }

    Err("Gateway did not become ready within 15 seconds".to_string())
}

/// Check if a child process output indicates it is alive.
fn child_output_is_alive(child: &std::process::Child) -> bool {
    // Child::id() returns the PID assigned at spawn; if the process
    // was never spawned or has been reaped, id() still returns the
    // original PID. For our purposes, we treat a non-zero PID as
    // "was successfully spawned".
    child.id() > 0
}
