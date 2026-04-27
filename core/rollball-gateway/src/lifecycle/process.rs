//! Process spawn/kill/health-check utilities for agent processes

use std::path::Path;
use std::process::Stdio;
use crate::error::GatewayError;

/// Handle to a spawned agent process
///
/// Stores the PID after spawn. The actual `Child` handle is detached into
/// a background reaper task to prevent zombie processes on Unix.
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
/// Launches `rollball-runtime` as a child process with the given parameters.
/// A background tokio task is spawned to reap the child's exit status,
/// preventing zombie processes on Unix.
pub async fn spawn_agent_process(
    agent_id: &str,
    install_path: &str,
    workspace: &Path,
    gateway_socket: &str,
) -> Result<AgentChild, GatewayError> {
    // Locate the rollball-runtime binary (sibling of current executable)
    let runtime_bin = std::env::current_exe()
        .map_err(|e| GatewayError::Lifecycle(format!("Cannot find current executable: {}", e)))?
        .parent()
        .map(|p| {
            let bin_name = if cfg!(windows) {
                "rollball-runtime.exe"
            } else {
                "rollball-runtime"
            };
            p.join(bin_name)
        })
        .unwrap_or_else(|| {
            let bin_name = if cfg!(windows) {
                "rollball-runtime.exe"
            } else {
                "rollball-runtime"
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
        .arg(workspace)
        .arg("--gateway-socket")
        .arg(gateway_socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

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

    // Spawn a background task to reap the child's exit status (prevents zombies on Unix)
    tokio::spawn(async move {
        let _ = child.wait().await;
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

/// Check if a process with the given PID is still running
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
        // Trying to spawn a non-existent agent should fail
        let result = spawn_agent_process(
            "com.test.nonexistent",
            "/nonexistent/path",
            Path::new("/tmp/nonexistent-workspace"),
            "/tmp/test-socket",
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
}
