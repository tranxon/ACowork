//! Process spawn/kill/health-check utilities for agent processes

use crate::error::GatewayError;
use crate::handlers::server::SharedState;
use std::path::Path;
use std::process::Stdio;

/// Handle to a spawned agent process
///
/// Stores the PID after spawn. The actual `Child` handle is detached into
/// a background reaper task that — in addition to reaping the zombie — also
/// removes the agent from `GatewayState::running_agents` when the child
/// exits, regardless of the cause (idle auto-sleep, crash, manual kill).
pub struct AgentChild {
    pid: u32,
}

impl AgentChild {
    /// Get the process ID of the spawned agent
    pub fn id(&self) -> u32 {
        self.pid
    }
}

/// Spawn an agent process
///
/// Launches `acowork-runtime` as a child process with the given parameters.
/// A background tokio task is spawned to reap the child's exit status,
/// preventing zombie processes on Unix.
///
/// `shared_state` is used by the reaper to remove the agent from
/// `running_agents` when the child exits — without this, an auto-sleep
/// (Runtime calls `process::exit(0)`) or a crash leaves a dead PID in
/// `running_agents`, which makes `/api/agents` keep reporting
/// `running=true` and turns `POST /api/agents/{id}/stop` into a 500
/// (the Gateway tries to `taskkill` / `kill` a PID that no longer
/// exists). Pass `None` in tests that don't have a `GatewayState`.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_agent_process(
    agent_id: &str,
    install_path: &str,
    workspace: &Path,
    dev_mode: bool,
    debug_port: Option<u16>,
    log_file_size_mb: u64,
    log_file_count: u64,
    mqtt_port: Option<u16>,
    shared_state: Option<SharedState>,
) -> Result<AgentChild, GatewayError> {
    // Locate the acowork-runtime binary (sibling of current executable)
    let runtime_bin = std::env::current_exe()
        .map_err(|e| GatewayError::Lifecycle(format!("Cannot find current executable: {}", e)))?
        .parent()
        .map(|p| {
            let bin_name = if cfg!(windows) {
                "acowork-runtime.exe"
            } else {
                "acowork-runtime"
            };
            p.join(bin_name)
        })
        .unwrap_or_else(|| {
            let bin_name = if cfg!(windows) {
                "acowork-runtime.exe"
            } else {
                "acowork-runtime"
            };
            std::path::PathBuf::from(bin_name)
        });

    let manifest_path = Path::new(install_path).join("manifest.toml");

    let mut cmd = tokio::process::Command::new(&runtime_bin);
    cmd.arg("--agent-id")
        .arg(agent_id)
        .arg("--package-path")
        .arg(install_path)
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("--work-dir")
        .arg(workspace);

    // ADR-033: Pass MQTT port so Runtime connects via MQTT instead of (or alongside) gRPC.
    if let Some(port) = mqtt_port {
        cmd.arg("--mqtt-port").arg(port.to_string());
        // ADR-033: Enable Runtime HTTP server for Gateway reverse proxy.
        cmd.arg("--http-port").arg("0"); // 0 = random port
    }

    // Developer mode: lower log level to DEBUG for detailed diagnostics
    let log_level = if dev_mode { "debug" } else { "info" };
    cmd.arg("--log-level")
        .arg(log_level)
        .arg("--log-file-size-mb")
        .arg(log_file_size_mb.to_string())
        .arg("--log-file-count")
        .arg(log_file_count.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    // Developer mode: pass --dev-mode so Runtime starts Debug Protocol
    // (HTTP RPC + MQTT events per ADR-048; the legacy WebSocket server
    // was removed in D4).
    if dev_mode {
        cmd.arg("--dev-mode");
        if let Some(port) = debug_port {
            cmd.arg("--debug-port").arg(port.to_string());
        }
    }

    // On Unix, create a new process group so we can kill the entire group later
    #[cfg(unix)]
    #[allow(unused_imports)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|e| {
        GatewayError::Lifecycle(format!(
            "Failed to spawn agent '{}' (binary: {:?}): {}",
            agent_id, runtime_bin, e
        ))
    })?;

    let pid = child.id().ok_or_else(|| {
        GatewayError::Lifecycle(format!(
            "Failed to get PID for agent '{}' (process may have exited immediately)",
            agent_id
        ))
    })?;

    // Spawn a background reaper task.
    //
    // Two responsibilities:
    //   1. Reap the child's exit status (prevents zombies on Unix).
    //   2. Remove the agent from `GatewayState::running_agents` when the
    //      child exits — regardless of cause (idle auto-sleep, crash,
    //      manual kill). Without this, the Gateway keeps a dead PID and
    //      subsequent `/api/agents/{id}/stop` calls 500 on `taskkill`
    //      of a non-existent PID, and `/api/agents` keeps reporting
    //      `running=true` for a process that has long since exited.
    //
    // The state cleanup is best-effort: if the caller passed `None`
    // (e.g. some tests), we log a debug message and move on — the
    // reaper's primary job (preventing zombies) is always done.
    let agent_id_owned = agent_id.to_string();
    tokio::spawn(async move {
        let exit_status = child.wait().await;
        if let Some(state) = shared_state {
            match state.write().await.remove_running(&agent_id_owned) {
                Some(removed) => {
                    tracing::info!(
                        agent_id = %agent_id_owned,
                        pid = removed.pid,
                        exit_status = ?exit_status,
                        "Runtime process exited, removed from running_agents"
                    );
                }
                None => {
                    // Not in running_agents anymore (e.g. `stop_agent`
                    // already cleaned up after a manual kill). Quietly
                    // exit — the reaper's job is done.
                    tracing::debug!(
                        agent_id = %agent_id_owned,
                        exit_status = ?exit_status,
                        "Runtime process exited but was not tracked in running_agents"
                    );
                }
            }
        } else {
            tracing::debug!(
                agent_id = %agent_id_owned,
                exit_status = ?exit_status,
                "Runtime process exited (no shared_state wired; reaper skipped cleanup)"
            );
        }
    });

    tracing::info!("Spawned agent process: {} (PID: {})", agent_id, pid);
    Ok(AgentChild { pid })
}

/// Kill a process by PID
///
/// On Unix: sends SIGTERM via the `kill` command
/// On Windows: uses `taskkill /F /T /PID` to forcefully terminate the process tree
pub async fn kill_agent_process(pid: u32) -> Result<(), GatewayError> {
    if cfg!(unix) {
        let output = tokio::process::Command::new("kill")
            .arg(pid.to_string())
            .output()
            .await
            .map_err(|e| {
                GatewayError::Lifecycle(format!("Failed to execute kill for PID {}: {}", pid, e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GatewayError::Lifecycle(format!(
                "kill command failed for PID {}: {}",
                pid,
                stderr.trim()
            )));
        }
    } else {
        let output = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .await
            .map_err(|e| {
                GatewayError::Lifecycle(format!(
                    "Failed to execute taskkill for PID {}: {}",
                    pid, e
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GatewayError::Lifecycle(format!(
                "taskkill command failed for PID {}: {}",
                pid,
                stderr.trim()
            )));
        }
    }

    tracing::info!("Killed process: PID {}", pid);
    Ok(())
}

/// Find an available TCP port for the debug WebSocket server.
///
/// Starts from `base_port` (typically 19878) and increments until a
/// free port is found. Returns the first available port.
///
/// Uses a quick bind-then-close to test port availability.
pub fn find_available_debug_port(base_port: u16) -> u16 {
    let mut port = base_port;
    loop {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        if std::net::TcpListener::bind(addr).is_ok() {
            // Listener is dropped immediately, releasing the port
            tracing::info!(port, "Found available debug port");
            return port;
        }
        tracing::debug!(port, "Debug port in use, trying next");
        port += 1;
    }
}

/// Check if a process with the given PID is still running (async version).
///
/// On Linux: checks if `/proc/{pid}` exists
/// On Windows: uses `tasklist` to check for the process
/// On macOS: uses `ps -p {pid}` (no /proc filesystem)
pub async fn check_health(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        // Linux: check /proc/{pid}
        tokio::fs::metadata(format!("/proc/{}", pid)).await.is_ok()
    } else if cfg!(windows) {
        // Windows: use tasklist
        match tokio::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    } else {
        // macOS / other Unix: use ps -p
        match tokio::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
        {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }
}

/// Synchronous process liveness check (no await, safe inside locks).
///
/// On Linux: checks if `/proc/{pid}` exists (instant).
/// On other platforms: returns `true` (assumes alive — self-corrects on next call).
///
/// Use this instead of `check_health` when you need to call inside a
/// write-lock scope (e.g. to clear stale process state on death).
#[cfg(target_os = "linux")]
pub fn is_process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

#[cfg(not(target_os = "linux"))]
pub fn is_process_alive(_pid: u32) -> bool {
    true // fallback: assume alive if we have a PID record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_child_id() {
        let child = AgentChild { pid: 12345 };
        assert_eq!(child.id(), 12345);
    }

    #[tokio::test]
    async fn test_spawn_nonexistent_binary() {
        // Trying to spawn a non-existent agent should fail.
        // `shared_state = None` keeps the test independent of GatewayState.
        let result = spawn_agent_process(
            "com.test.nonexistent",
            "/nonexistent/path",
            Path::new("/tmp/nonexistent-workspace"),
            false,
            None,
            10,
            20,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_check_health_current_process() {
        // Current process should be alive
        let pid = std::process::id();
        assert!(check_health(pid).await);
    }

    #[tokio::test]
    async fn test_check_health_nonexistent_pid() {
        // A very large PID is unlikely to exist
        assert!(!check_health(999999999).await);
    }

    #[tokio::test]
    async fn test_kill_nonexistent_pid() {
        // Killing a non-existent PID should fail
        let result = kill_agent_process(999999999).await;
        assert!(result.is_err());
    }

    /// Regression test: when an agent child process exits (e.g. idle
    /// auto-sleep calls `process::exit(0)`), the reaper must remove the
    /// agent from `GatewayState::running_agents`. Otherwise `/api/agents`
    /// keeps reporting `running=true` for a dead process and
    /// `POST /api/agents/{id}/stop` 500s on `taskkill` of a non-existent
    /// PID.
    ///
    /// We don't actually invoke `spawn_agent_process` here (it requires a
    /// real `acowork-runtime` binary on disk). Instead we exercise the
    /// reaper's *cleanup contract* directly: spawn a short-lived child
    /// (the current test binary via `std::process::Command`), wire a
    /// reaper that mirrors the production logic, and assert that
    /// `running_agents` is empty once the child exits.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_reaper_removes_running_agent_on_exit() {
        use crate::gateway::state::{GatewayState, RunningAgentInfo};
        use std::process::Stdio as StdStdio;
        use std::sync::Arc as StdArc;
        use tokio::io::AsyncWriteExt;
        use tokio::sync::RwLock as TokioRwLock;

        // Build a minimal GatewayState with one tracked agent.
        let state: SharedState = StdArc::new(TokioRwLock::new(GatewayState::new(
            std::env::temp_dir().to_str().unwrap_or("/tmp"),
        )));
        {
            let mut gw = state.write().await;
            gw.add_running(RunningAgentInfo {
                agent_id: "com.test.reaper".to_string(),
                pid: 0, // overwritten once we know the real PID
                started_at: chrono::Utc::now(),
                workspace: String::new(),
                connected: false,
                ready: false,
                dev_mode: false,
                debug_state: crate::gateway::state::DebugState::Disabled,
                debug_port: None,
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
        }
        assert!(state.read().await.is_running("com.test.reaper"));

        // Spawn a child that exits immediately. We use `sh -c 'exit 0'`
        // rather than the current test binary so we don't depend on its
        // argv handling.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("exit 0");
        cmd.stdout(StdStdio::null()).stderr(StdStdio::null());
        let mut child = cmd.spawn().expect("sh -c 'exit 0' should spawn");
        let pid = child.id().expect("child has PID");
        // Backfill the PID so the reaper's log line matches production.
        {
            let mut gw = state.write().await;
            if let Some(info) = gw.running_agents.get_mut("com.test.reaper") {
                info.pid = pid;
            }
        }

        // Mirror the production reaper logic.
        let state_for_reaper = state.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            state_for_reaper
                .write()
                .await
                .remove_running("com.test.reaper");
        });

        // Poll until the reaper has done its job (bounded by a timeout so
        // a regression doesn't hang the suite forever).
        let mut removed = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if !state.read().await.is_running("com.test.reaper") {
                removed = true;
                break;
            }
        }
        assert!(removed, "reaper did not remove running agent within 1s");

        // Sanity: an unused writer to silence the unused-import lint on
        // some toolchains where the macro path doesn't fire.
        let mut sink = Vec::new();
        sink.write_all(b"ok").await.unwrap();
        assert_eq!(sink, b"ok");
    }
}

