//! HTTP server lifecycle management
//!
//! Starts the Axum HTTP server alongside the IPC server in Gateway::run().
//! Handles port conflict auto-increment and pidfile writing.

use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::path::Path;

use axum;
use tokio::sync::RwLock;

use crate::config::HttpConfig;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::http::auth::HttpAuth;
use crate::http::routes::{self, AppState, SharedSessionMgr, BridgeEvent};

/// PID file content for Desktop App discovery
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PidFile {
    pub pid: u32,
    pub http_port: u16,
    pub socket_path: String,
}

/// RAII guard for the pidfile — deletes the file on Drop.
///
/// Ensures cleanup on both normal shutdown and panic-induced exits.
pub struct PidFileGuard {
    path: std::path::PathBuf,
}

impl PidFileGuard {
    /// Create a new guard that will delete the file at `path` on drop.
    fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => tracing::info!("PID file cleaned up: {}", self.path.display()),
                Err(e) => tracing::warn!("Failed to remove PID file '{}': {}", self.path.display(), e),
            }
        }
    }
}

/// Clean up stale pidfile from a previous Gateway run.
///
/// If the file exists but the recorded PID is no longer running,
/// the stale file is deleted. If the PID is still alive (another
/// Gateway instance), returns an error.
pub fn cleanup_stale_pidfile(data_dir: &Path) -> Result<(), GatewayError> {
    let pid_path = data_dir.join("gateway.pid");
    if !pid_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&pid_path)
        .map_err(|e| GatewayError::Config(format!(
            "Failed to read pidfile '{}': {}", pid_path.display(), e
        )))?;

    let parsed: PidFile = serde_json::from_str(&content)
        .map_err(|e| GatewayError::Config(format!(
            "Failed to parse pidfile '{}': {}", pid_path.display(), e
        )))?;

    // Check if the recorded process is still alive
    if is_pid_alive(parsed.pid) {
        return Err(GatewayError::Config(format!(
            "Another Gateway instance is running (PID {})", parsed.pid
        )));
    }

    // Stale pidfile — process no longer exists
    tracing::warn!(
        "Removing stale pidfile '{}' (old PID {} no longer running)",
        pid_path.display(),
        parsed.pid
    );
    std::fs::remove_file(&pid_path)
        .map_err(|e| GatewayError::Config(format!(
            "Failed to remove stale pidfile '{}': {}", pid_path.display(), e
        )))?;

    Ok(())
}

/// Check if a process with the given PID is still alive.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    // Signal 0 doesn't kill the process; it just checks existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Check if a process with the given PID is still alive (non-Unix fallback).
#[cfg(not(unix))]
fn is_pid_alive(pid: u32) -> bool {
    // On Windows, try to open the process handle.
    // If we can't, the process is likely not running.
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

/// Start the HTTP API server
///
/// Binds to the configured host:port. If port is occupied,
/// auto-increments up to port_max.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_http_server(
    http_config: &HttpConfig,
    gateway_state: Arc<RwLock<GatewayState>>,
    socket_path: &str,
    data_dir: &Path,
    session_mgr: Option<SharedSessionMgr>,
    grpc_session_mgr: Option<crate::grpc::SharedGrpcSessionMgr>,
    bridge_tx: Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    models_cache: crate::http::models_api::ModelsCache,
    session_pending: Option<crate::http::routes::SessionPendingRequests>,
    log_reload_handle: Option<crate::LogReloadHandle>,
) -> Result<(), GatewayError> {
    if !http_config.enabled {
        tracing::info!("HTTP API disabled by configuration");
        return Ok(());
    }

    // Initialize auth
    let auth = Arc::new(HttpAuth::new(http_config.auth_enabled));
    auth.write_token_file(data_dir)?;

    // Build app state
    let mut app_state = AppState::with_models_cache(
        gateway_state,
        auth,
        session_mgr,
        bridge_tx,
        models_cache,
        session_pending,
        log_reload_handle,
    );
    app_state.grpc_session_mgr = grpc_session_mgr;
    app_state.cors_enabled = http_config.cors_enabled;

    // S5.9: Clean up stale pidfile from a previous Gateway run before writing a new one.
    // If the previous process is still alive, this returns an error (prevents dual Gateway).
    cleanup_stale_pidfile(data_dir)?;

    // P1-3 fix: Find available port and return the bound listener
    // to eliminate the TOCTOU race between checking and binding.
    let (actual_port, std_listener) = find_available_port(
        &http_config.host,
        http_config.port,
        http_config.port_max,
    )?;

    // Write pidfile for Desktop App discovery
    let _pidfile_guard = write_pidfile(data_dir, actual_port, socket_path)?;

    // Build router
    let app = routes::build_router(app_state);

    // Convert std::net::TcpListener to tokio::net::TcpListener
    // This reuses the already-bound listener — no second bind() call.
    // On Windows, we must set non-blocking mode before conversion.
    std_listener.set_nonblocking(true)
        .map_err(|e| GatewayError::Config(format!(
            "Failed to set non-blocking mode: {}", e
        )))?;
    let listener = tokio::net::TcpListener::from_std(std_listener)
        .map_err(|e| GatewayError::Config(format!(
            "Failed to convert TCP listener: {}", e
        )))?;

    tracing::info!("HTTP API listening on http://{}:{}", http_config.host, actual_port);

    axum::serve(listener, app)
        .await
        .map_err(|e| GatewayError::Config(format!("HTTP server error: {}", e)))?;

    Ok(())
}

/// Find an available port in the configured range.
///
/// P1-3 fix: Returns the bound `TcpListener` directly so the caller
/// can pass it to `axum::serve` without a second `bind()` call,
/// eliminating the TOCTOU race condition between port-check and bind.
fn find_available_port(
    host: &str,
    start_port: u16,
    max_port: u16,
) -> Result<(u16, StdTcpListener), GatewayError> {
    for port in start_port..=max_port {
        match StdTcpListener::bind(format!("{}:{}", host, port)) {
            Ok(listener) => {
                tracing::info!("Found available HTTP port: {}", port);
                return Ok((port, listener));
            }
            Err(_) => {
                tracing::warn!("Port {} occupied, trying next", port);
            }
        }
    }
    Err(GatewayError::Config(format!(
        "No available port in range {}-{}", start_port, max_port
    )))
}

/// Write pidfile for Desktop App discovery.
/// Returns a `PidFileGuard` that will delete the pidfile on Drop.
fn write_pidfile(data_dir: &Path, http_port: u16, socket_path: &str) -> Result<PidFileGuard, GatewayError> {
    let pid_file = PidFile {
        pid: std::process::id(),
        http_port,
        socket_path: socket_path.to_string(),
    };
    let content = serde_json::to_string_pretty(&pid_file)
        .map_err(|e| GatewayError::Config(format!("Failed to serialize pidfile: {}", e)))?;
    let pid_path = data_dir.join("gateway.pid");
    std::fs::write(&pid_path, content)
        .map_err(|e| GatewayError::Config(format!(
            "Failed to write pidfile '{}': {}", pid_path.display(), e
        )))?;
    tracing::info!("PID file written to {}", pid_path.display());
    Ok(PidFileGuard::new(pid_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_available_port() {
        // find_available_port returns both the port number and the bound listener
        let result = find_available_port("127.0.0.1", 19876, 19878);
        assert!(result.is_ok());
        let (port, listener) = result.unwrap();
        assert!((19876..=19878).contains(&port));
        // Verify the listener is actually bound
        assert!(listener.local_addr().is_ok());
    }

    #[test]
    fn test_find_available_port_range_exhausted() {
        // Use an impossible range to test exhaustion
        let result = find_available_port("127.0.0.1", 1, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_write_pidfile() {
        let dir = std::env::temp_dir().join("rollball-test-pidfile");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let _guard = write_pidfile(&dir, 19876, r"\\.\\pipe\\rollball-gateway").unwrap();

        let pid_path = dir.join("gateway.pid");
        assert!(pid_path.exists());

        let content = std::fs::read_to_string(&pid_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["http_port"], 19876);
        assert!(parsed["pid"].as_u64().unwrap() > 0);

        drop(_guard);
        assert!(!pid_path.exists(), "pidfile should be cleaned up after guard is dropped");

        let _ = std::fs::remove_dir_all(&dir);
    }
    
    // ── S5.9 tests ──────────────────────────────────────────────────────────
    
    #[test]
    fn test_pidfile_guard_cleanup_on_drop() {
        let dir = std::env::temp_dir().join("rollball-test-pidfile-drop");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
    
        // Write pidfile via write_pidfile, which returns a PidFileGuard
        let pid_path = dir.join("gateway.pid");
        {
            let _guard = write_pidfile(&dir, 19876, "/tmp/test.sock").unwrap();
            // pidfile should exist while guard is alive
            assert!(pid_path.exists(), "pidfile should exist while PidFileGuard is alive");
        }
        // After guard is dropped, pidfile should be cleaned up
        assert!(!pid_path.exists(), "pidfile should be deleted after PidFileGuard is dropped");
    
        let _ = std::fs::remove_dir_all(&dir);
    }
    
    #[test]
    fn test_cleanup_stale_pidfile_removes_dead_process() {
        let dir = std::env::temp_dir().join("rollball-test-pidfile-stale");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
    
        // Write a pidfile referencing a PID that doesn't exist
        let stale_pid: u32 = 9999999; // Extremely unlikely to be a running process
        let pid_file = PidFile {
            pid: stale_pid,
            http_port: 19876,
            socket_path: "/tmp/test.sock".to_string(),
        };
        let pid_path = dir.join("gateway.pid");
        let content = serde_json::to_string_pretty(&pid_file).unwrap();
        std::fs::write(&pid_path, &content).unwrap();
        assert!(pid_path.exists(), "stale pidfile should exist before cleanup");
    
        // cleanup_stale_pidfile should detect the dead process and remove the file
        let result = cleanup_stale_pidfile(&dir);
        assert!(result.is_ok(), "cleanup_stale_pidfile should succeed for dead process: {:?}", result);
        assert!(!pid_path.exists(), "stale pidfile should be removed after cleanup");
    
        let _ = std::fs::remove_dir_all(&dir);
    }
    
    #[test]
    fn test_cleanup_stale_pidfile_rejects_live_process() {
        let dir = std::env::temp_dir().join("rollball-test-pidfile-live");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
    
        // Write a pidfile referencing our own PID (which is definitely alive)
        let live_pid: u32 = std::process::id();
        let pid_file = PidFile {
            pid: live_pid,
            http_port: 19876,
            socket_path: "/tmp/test.sock".to_string(),
        };
        let pid_path = dir.join("gateway.pid");
        let content = serde_json::to_string_pretty(&pid_file).unwrap();
        std::fs::write(&pid_path, &content).unwrap();
    
        // cleanup_stale_pidfile should refuse because the process is alive
        let result = cleanup_stale_pidfile(&dir);
        assert!(result.is_err(), "cleanup_stale_pidfile should reject a live Gateway process");
        assert!(pid_path.exists(), "pidfile should NOT be removed when process is alive");
    
        let _ = std::fs::remove_dir_all(&dir);
    }
    
    #[test]
    fn test_cleanup_stale_pidfile_no_file() {
        let dir = std::env::temp_dir().join("rollball-test-pidfile-none");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
    
        // No pidfile exists — should succeed without doing anything
        let result = cleanup_stale_pidfile(&dir);
        assert!(result.is_ok(), "cleanup_stale_pidfile should succeed when no pidfile exists");
    
        let _ = std::fs::remove_dir_all(&dir);
    }
}
