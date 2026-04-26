//! HTTP server lifecycle management
//!
//! Starts the Axum HTTP server alongside the IPC server in Gateway::run().
//! Handles port conflict auto-increment and pidfile writing.

use std::net::TcpListener;
use std::sync::Arc;
use std::path::Path;

use axum;
use tokio::sync::RwLock;

use crate::config::HttpConfig;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::http::auth::HttpAuth;
use crate::http::routes::{self, AppState};

/// PID file content for Desktop App discovery
#[derive(serde::Serialize)]
struct PidFile {
    pid: u32,
    http_port: u16,
    socket_path: String,
}

/// Start the HTTP API server
///
/// Binds to the configured host:port. If port is occupied,
/// auto-increments up to port_max.
pub async fn start_http_server(
    http_config: &HttpConfig,
    gateway_state: Arc<RwLock<GatewayState>>,
    socket_path: &str,
    data_dir: &Path,
) -> Result<(), GatewayError> {
    if !http_config.enabled {
        tracing::info!("HTTP API disabled by configuration");
        return Ok(());
    }

    // Initialize auth
    let auth = Arc::new(HttpAuth::new(http_config.auth_enabled));
    auth.write_token_file(data_dir)?;

    // Build app state
    let app_state = AppState {
        gateway_state,
        auth,
    };

    // Find available port
    let actual_port = find_available_port(
        &http_config.host,
        http_config.port,
        http_config.port_max,
    )?;

    // Write pidfile for Desktop App discovery
    write_pidfile(data_dir, actual_port, socket_path)?;

    // Build router
    let app = routes::build_router(app_state);

    // Bind and serve
    let addr = format!("{}:{}", http_config.host, actual_port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| GatewayError::Config(format!(
            "Failed to bind HTTP server on {}: {}", addr, e
        )))?;

    tracing::info!("HTTP API listening on http://{}", addr);

    axum::serve(listener, app)
        .await
        .map_err(|e| GatewayError::Config(format!("HTTP server error: {}", e)))?;

    Ok(())
}

/// Find an available port in the configured range
fn find_available_port(host: &str, start_port: u16, max_port: u16) -> Result<u16, GatewayError> {
    for port in start_port..=max_port {
        if TcpListener::bind(format!("{}:{}", host, port)).is_ok() {
            tracing::info!("Found available HTTP port: {}", port);
            return Ok(port);
        }
        tracing::warn!("Port {} occupied, trying next", port);
    }
    Err(GatewayError::Config(format!(
        "No available port in range {}-{}", start_port, max_port
    )))
}

/// Write pidfile for Desktop App discovery
fn write_pidfile(data_dir: &Path, http_port: u16, socket_path: &str) -> Result<(), GatewayError> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_available_port() {
        // Port 0 is always available (OS auto-assigns)
        let port = find_available_port("127.0.0.1", 19876, 19878);
        assert!(port.is_ok());
    }

    #[test]
    fn test_find_available_port_range_exhausted() {
        // Use an impossible range to test exhaustion
        let port = find_available_port("127.0.0.1", 1, 0);
        assert!(port.is_err());
    }

    #[test]
    fn test_write_pidfile() {
        let dir = std::env::temp_dir().join("rollball-test-pidfile");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_pidfile(&dir, 19876, r"\\.\pipe\rollball-gateway").unwrap();

        let pid_path = dir.join("gateway.pid");
        assert!(pid_path.exists());

        let content = std::fs::read_to_string(&pid_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["http_port"], 19876);
        assert!(parsed["pid"].as_u64().unwrap() > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
