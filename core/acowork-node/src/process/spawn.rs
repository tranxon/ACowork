//! Runtime process table + spawn/kill/reap + health probe (ADR-055
//! §6.20 — migrated from gateway `lifecycle/process.rs` + `manager.rs`).
//!
//! Re-based from `GatewayError` / `SharedState` to
//! [`crate::error::NodeError`] / [`crate::state::SharedNodeState`].
//! Sibling-binary location, process-group isolation, reaper and health
//! semantics are unchanged from the Gateway implementation.
//!
//! Re-adopt (§6.19) reconciles the local process table against the
//! broker's retained `acowork/agents/{id}/status` and the machine's
//! process list after a Node restart — landed in `manager.rs`.

use std::path::Path;
use std::process::Stdio;

use crate::error::{NodeError, Result};
use crate::state::SharedNodeState;

/// Handle to a spawned agent process.
///
/// Stores the PID after spawn. The actual `Child` handle is detached
/// into a background reaper task that — in addition to reaping the
/// zombie — removes the agent from the node process table when the
/// child exits, regardless of cause (idle auto-sleep, crash, kill).
pub struct AgentChild {
    pid: u32,
}

impl AgentChild {
    /// Get the process ID of the spawned agent.
    pub fn id(&self) -> u32 {
        self.pid
    }
}

/// Spawn an agent Runtime process.
///
/// `shared_state` is used by the reaper to remove the agent from the
/// node process table when the child exits. Pass `None` in tests that
/// don't have a `NodeState`.
///
/// `gateway_host` is forwarded to the Runtime as `--gateway-host`
/// (ADR-055 L3-5): the node's own MQTT connection target, so a remote
/// node's Runtimes reach the same broker.
///
/// `node_id` is forwarded as `--node-id` (ADR-055 §6.7, Phase 4): the
/// Runtime subscribes to `acowork/nodes/{node_id}/lsps` so it can
/// register the `codebase` tool when this node's LSP relay is ready.
///
/// `http_port` is the loopback HTTP port the Node allocated for this
/// Runtime (ADR-055 §6.4); `http_advertise_endpoint` is the Node
/// reverse-proxy base URL (`http://{advertise_host}:{proxy_port}`)
/// injected so the Runtime publishes `{base}/agents/{id}` as its
/// retained `http_endpoint` (§6.3). When `None` (standalone/test), the
/// Runtime falls back to the direct loopback endpoint.
///
/// `node_token` (Phase 5a): when the node holds a Gateway-issued
/// token it is injected as `--mqtt-username agent:{id}` /
/// `--mqtt-password {token}` so spawned Runtimes authenticate against
/// the broker's `agent:{id}` CONNECT rule (§6.8).
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
    gateway_host: &str,
    node_id: &str,
    http_port: u16,
    http_advertise_endpoint: Option<&str>,
    node_token: Option<&str>,
    shared_state: Option<SharedNodeState>,
) -> Result<AgentChild> {
    // Locate the acowork-runtime binary (sibling of current executable)
    let runtime_bin = std::env::current_exe()
        .map_err(|e| NodeError::Lifecycle(format!("Cannot find current executable: {}", e)))?
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

    // ADR-055 L3-5: forward the node's Gateway broker host so the
    // Runtime connects to the same broker the node is enrolled with.
    cmd.arg("--gateway-host").arg(gateway_host);

    // ADR-055 §6.7 (Phase 4): forward the hosting node id so the Runtime
    // subscribes to this node's LSP relay topic (`acowork/nodes/{id}/lsps`)
    // and registers the `codebase` tool when the relay is ready.
    cmd.arg("--node-id").arg(node_id);

    // ADR-055 Phase 5a §6.8: when the node holds a Gateway-issued
    // token, Runtimes connect to the broker with it — `agent:{id}`
    // CONNECT usernames are accepted with the spawning node's token.
    if let Some(token) = node_token {
        cmd.arg("--mqtt-username")
            .arg(format!("agent:{agent_id}"))
            .arg("--mqtt-password")
            .arg(token);
    }

    // ADR-033: Pass MQTT port so Runtime connects via MQTT.
    if let Some(port) = mqtt_port {
        cmd.arg("--mqtt-port").arg(port.to_string());
        // ADR-055 §6.4: the Node allocates a concrete loopback port so
        // its reverse proxy can route `/agents/{id}/*` here.
        cmd.arg("--http-port").arg(http_port.to_string());
    }

    // ADR-055 §6.3: inject the Node reverse-proxy base URL so the
    // Runtime publishes `{base}/agents/{id}` as its retained
    // `http_endpoint` (the Gateway then proxies through the Node).
    if let Some(base) = http_advertise_endpoint {
        cmd.arg("--http-advertise-endpoint").arg(base);
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

    // Developer mode: pass --dev-mode so Runtime starts Debug Protocol.
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
        NodeError::Lifecycle(format!(
            "Failed to spawn agent '{}' (binary: {:?}): {}",
            agent_id, runtime_bin, e
        ))
    })?;

    let pid = child.id().ok_or_else(|| {
        NodeError::Lifecycle(format!(
            "Failed to get PID for agent '{}' (process may have exited immediately)",
            agent_id
        ))
    })?;

    // Spawn a background reaper task: reap the child and remove the
    // agent from the node process table on exit.
    let agent_id_owned = agent_id.to_string();
    tokio::spawn(async move {
        let exit_status = child.wait().await;
        if let Some(state) = shared_state {
            match state.write().await.remove_agent(&agent_id_owned) {
                Some(removed) => {
                    tracing::info!(
                        agent_id = %agent_id_owned,
                        pid = removed.pid,
                        exit_status = ?exit_status,
                        "Runtime process exited, removed from node process table"
                    );
                }
                None => {
                    tracing::debug!(
                        agent_id = %agent_id_owned,
                        exit_status = ?exit_status,
                        "Runtime process exited but was not tracked in node process table"
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

/// Kill a process by PID.
///
/// On Unix: sends SIGTERM via the `kill` command.
/// On Windows: uses `taskkill /F /T /PID` to forcefully terminate the
/// process tree.
pub async fn kill_agent_process(pid: u32) -> Result<()> {
    if cfg!(unix) {
        let output = tokio::process::Command::new("kill")
            .arg(pid.to_string())
            .output()
            .await
            .map_err(|e| {
                NodeError::Lifecycle(format!("Failed to execute kill for PID {}: {}", pid, e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NodeError::Lifecycle(format!(
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
                NodeError::Lifecycle(format!(
                    "Failed to execute taskkill for PID {}: {}",
                    pid, e
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NodeError::Lifecycle(format!(
                "taskkill command failed for PID {}: {}",
                pid,
                stderr.trim()
            )));
        }
    }

    tracing::info!("Killed process: PID {}", pid);
    Ok(())
}

/// Find an available TCP port starting from `base_port`.
pub fn find_available_debug_port(base_port: u16) -> u16 {
    let mut port = base_port;
    loop {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        if std::net::TcpListener::bind(addr).is_ok() {
            tracing::info!(port, "Found available debug port");
            return port;
        }
        tracing::debug!(port, "Debug port in use, trying next");
        port += 1;
    }
}

/// Find an available loopback HTTP port for a Runtime, starting from
/// [`acowork_core::node::NODE_HTTP_PORT_BASE`] (ADR-055 §6.4). The Node
/// allocates a concrete port (rather than `--http-port 0`) so its
/// reverse proxy has a stable `{agent_id} → port` mapping.
pub fn find_available_http_port() -> u16 {
    find_available_debug_port(acowork_core::node::NODE_HTTP_PORT_BASE)
}

/// Check if a process with the given PID is still running (async).
///
/// Linux: `/proc/{pid}`; Windows: `tasklist`; macOS/other: `ps -p`.
pub async fn check_health(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        tokio::fs::metadata(format!("/proc/{}", pid)).await.is_ok()
    } else if cfg!(windows) {
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
/// Linux: checks `/proc/{pid}` (instant). Other platforms: returns
/// `true` (assumes alive — self-corrects on next call).
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
        // Spawning a non-existent agent should fail (binary won't be found).
        let result = spawn_agent_process(
            "com.test.nonexistent",
            "/nonexistent/path",
            Path::new("/tmp/nonexistent-workspace"),
            false,
            None,
            10,
            20,
            None,
            "127.0.0.1",
            "test-node",
            acowork_core::node::NODE_HTTP_PORT_BASE,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_check_health_current_process() {
        let pid = std::process::id();
        assert!(check_health(pid).await);
    }

    #[tokio::test]
    async fn test_check_health_nonexistent_pid() {
        assert!(!check_health(999999999).await);
    }

    #[tokio::test]
    async fn test_kill_nonexistent_pid() {
        let result = kill_agent_process(999999999).await;
        assert!(result.is_err());
    }
}
