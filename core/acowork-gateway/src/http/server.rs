//! HTTP server lifecycle management
//!
//! Starts the Axum HTTP server alongside the IPC server in Gateway::run().
//! Handles port conflict auto-increment and pidfile writing.

use std::net::TcpListener as StdTcpListener;
use std::path::Path;
use std::sync::Arc;

use axum;
use tokio::sync::RwLock;

use crate::config::HttpConfig;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::http::auth::HttpAuth;
use crate::http::routes::{self, AppState, BridgeEvent, SessionPendingRequests, SharedSessionMgr};
use crate::ipc::global_push::GlobalResourcePusher;

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
                Err(e) => {
                    tracing::warn!("Failed to remove PID file '{}': {}", self.path.display(), e)
                }
            }
        }
    }
}

/// Check if a port is occupied by another Gateway instance.
///
/// Probes `GET /health` on the given port with a 2-second timeout.
/// If the response contains the Gateway `version` field, it's another
/// Gateway instance. Non-Gateway apps or unreachable ports return `false`.
///
/// This is more reliable than PID-based checks because:
/// - No stale state after crashes
/// - No PID reuse false positives
/// - Direct verification of the service identity
async fn is_gateway_on_port(host: &str, port: u16) -> bool {
    let url = format!("http://{}:{}/health", host, port);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(&url).send().await {
        Ok(resp) => {
            // Check if the response is JSON with a "version" field (Gateway signature)
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                body.get("version").is_some() && body.get("checks").is_some()
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// Remove a stale pidfile from a previous Gateway run.
///
/// This is purely for housekeeping — mutual exclusion is now handled
/// by probing the HTTP port (see `is_gateway_on_port`). If the file
/// exists, it is simply removed; no PID liveness check is performed.
pub fn cleanup_stale_pidfile(data_dir: &Path) {
    let pid_path = data_dir.join("gateway.pid");
    if pid_path.exists() {
        match std::fs::remove_file(&pid_path) {
            Ok(()) => tracing::info!("Removed stale pidfile '{}'", pid_path.display()),
            Err(e) => tracing::warn!("Failed to remove stale pidfile '{}': {}", pid_path.display(), e),
        }
    }
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
    session_pending: Option<SessionPendingRequests>,
    log_reload_handle: Option<crate::LogReloadHandle>,
    pusher: Option<Arc<GlobalResourcePusher>>,
) -> Result<(), GatewayError> {
    if !http_config.enabled {
        tracing::info!("HTTP API disabled by configuration");
        return Ok(());
    }

    // Initialize auth
    let auth = Arc::new(HttpAuth::new(http_config.auth_enabled));
    auth.write_token_file(data_dir)?;

    // Build app state
    let mut app_state = AppState::with_config(
        gateway_state,
        auth,
        session_mgr,
        bridge_tx,
        session_pending,
        log_reload_handle,
    );
    app_state.grpc_session_mgr = grpc_session_mgr;
    app_state.pusher = pusher;
    app_state.cors_enabled = http_config.cors_enabled;

    // Start background LSP pool reaper (evicts idle processes after timeout)
    crate::lsp::LspPool::start_reaper(Arc::clone(&app_state.lsp_pool));

    // Clean up stale pidfile from a previous run (if any). This is purely
    // for housekeeping — mutual exclusion is handled by port probing below.
    cleanup_stale_pidfile(data_dir);

    // Find an available port, probing occupied ports to distinguish between
    // another Gateway instance (fatal) and unrelated applications (skip).
    let (actual_port, std_listener) = find_available_port(
        &http_config.host,
        http_config.port,
        http_config.port_max,
    )
    .await?;

    // Write pidfile for Desktop App discovery
    let _pidfile_guard = write_pidfile(data_dir, actual_port, socket_path)?;

    // Build router
    let app = routes::build_router(app_state);

    // Convert std::net::TcpListener to tokio::net::TcpListener
    // This reuses the already-bound listener — no second bind() call.
    // On Windows, we must set non-blocking mode before conversion.
    std_listener
        .set_nonblocking(true)
        .map_err(|e| GatewayError::Config(format!("Failed to set non-blocking mode: {}", e)))?;
    let listener = tokio::net::TcpListener::from_std(std_listener)
        .map_err(|e| GatewayError::Config(format!("Failed to convert TCP listener: {}", e)))?;

    tracing::info!(
        "HTTP API listening on http://{}:{}",
        http_config.host,
        actual_port
    );

    axum::serve(listener, app)
        .await
        .map_err(|e| GatewayError::Config(format!("HTTP server error: {}", e)))?;

    Ok(())
}

/// Find an available port in the configured range.
///
/// For each occupied port, probes `GET /health` to determine if it's another
/// Gateway instance (fatal error) or an unrelated application (skip to next).
/// Returns the bound `TcpListener` directly so the caller can pass it to
/// `axum::serve` without a second `bind()` call.
async fn find_available_port(
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
                // Port occupied — probe to check if it's another Gateway
                if is_gateway_on_port(host, port).await {
                    return Err(GatewayError::Config(format!(
                        "Another Gateway instance is running on {}:{}",
                        host, port
                    )));
                }
                tracing::warn!(
                    "Port {} occupied by non-Gateway application, trying next",
                    port
                );
            }
        }
    }
    Err(GatewayError::Config(format!(
        "No available port in range {}-{}",
        start_port, max_port
    )))
}

/// Write pidfile for Desktop App discovery.
/// Returns a `PidFileGuard` that will delete the pidfile on Drop.
fn write_pidfile(
    data_dir: &Path,
    http_port: u16,
    socket_path: &str,
) -> Result<PidFileGuard, GatewayError> {
    let pid_file = PidFile {
        pid: std::process::id(),
        http_port,
        socket_path: socket_path.to_string(),
    };
    let content = serde_json::to_string_pretty(&pid_file)
        .map_err(|e| GatewayError::Config(format!("Failed to serialize pidfile: {}", e)))?;
    let pid_path = data_dir.join("gateway.pid");
    std::fs::write(&pid_path, content).map_err(|e| {
        GatewayError::Config(format!(
            "Failed to write pidfile '{}': {}",
            pid_path.display(),
            e
        ))
    })?;
    tracing::info!("PID file written to {}", pid_path.display());
    Ok(PidFileGuard::new(pid_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_find_available_port() {
        // find_available_port returns both the port number and the bound listener
        let result = find_available_port("127.0.0.1", 19876, 19878).await;
        assert!(result.is_ok());
        let (port, listener) = result.unwrap();
        assert!((19876..=19878).contains(&port));
        // Verify the listener is actually bound
        assert!(listener.local_addr().is_ok());
    }

    #[tokio::test]
    async fn test_find_available_port_range_exhausted() {
        // Use an impossible range to test exhaustion
        let result = find_available_port("127.0.0.1", 1, 0).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_write_pidfile() {
        let dir = std::env::temp_dir().join("acowork-test-pidfile");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let _guard = write_pidfile(&dir, 19876, r"\\.\\pipe\\acowork-gateway").unwrap();

        let pid_path = dir.join("gateway.pid");
        assert!(pid_path.exists());

        let content = std::fs::read_to_string(&pid_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["http_port"], 19876);
        assert!(parsed["pid"].as_u64().unwrap() > 0);

        drop(_guard);
        assert!(
            !pid_path.exists(),
            "pidfile should be cleaned up after guard is dropped"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── S5.9 tests ──────────────────────────────────────────────────────────

    #[test]
    fn test_pidfile_guard_cleanup_on_drop() {
        let dir = std::env::temp_dir().join("acowork-test-pidfile-drop");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write pidfile via write_pidfile, which returns a PidFileGuard
        let pid_path = dir.join("gateway.pid");
        {
            let _guard = write_pidfile(&dir, 19876, "/tmp/test.sock").unwrap();
            // pidfile should exist while guard is alive
            assert!(
                pid_path.exists(),
                "pidfile should exist while PidFileGuard is alive"
            );
        }
        // After guard is dropped, pidfile should be cleaned up
        assert!(
            !pid_path.exists(),
            "pidfile should be deleted after PidFileGuard is dropped"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cleanup_stale_pidfile_removes_file() {
        let dir = std::env::temp_dir().join("acowork-test-pidfile-stale");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a stale pidfile
        let pid_file = PidFile {
            pid: 9999999,
            http_port: 19876,
            socket_path: "/tmp/test.sock".to_string(),
        };
        let pid_path = dir.join("gateway.pid");
        let content = serde_json::to_string_pretty(&pid_file).unwrap();
        std::fs::write(&pid_path, &content).unwrap();
        assert!(pid_path.exists(), "stale pidfile should exist before cleanup");

        // cleanup_stale_pidfile should simply remove it
        cleanup_stale_pidfile(&dir);
        assert!(!pid_path.exists(), "stale pidfile should be removed after cleanup");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cleanup_stale_pidfile_no_file() {
        let dir = std::env::temp_dir().join("acowork-test-pidfile-none");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // No pidfile exists — should not panic
        cleanup_stale_pidfile(&dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_is_gateway_on_port_no_server() {
        // A port with nothing listening should return false
        assert!(!is_gateway_on_port("127.0.0.1", 19999).await);
    }
}
