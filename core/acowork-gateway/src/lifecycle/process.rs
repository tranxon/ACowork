//! Process kill/health-check utilities for agent processes.
//!
//! ADR-055 Phase 2b.3: `spawn_agent_process` / `AgentChild` were deleted —
//! Runtime processes are now spawned by the node (`acowork-node`). What
//! remains are the process-signal / liveness helpers still used by the
//! Gateway's own embedded services (embed, LSP relay) and their
//! supervisors.

use crate::error::GatewayError;
use std::process::Stdio;

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
}
