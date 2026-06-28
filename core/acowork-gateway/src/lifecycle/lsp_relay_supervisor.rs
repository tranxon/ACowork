//! LSP Relay process supervisor — monitors the LSP Relay child process
//! via SSE, detects failures, and restarts with exponential backoff.
//!
//! This is the LSP Relay counterpart of `embed_supervisor.rs`. It follows
//! the same architecture: connect to the relay's `/events` SSE stream,
//! watch heartbeats (2s cadence, 10s timeout), and restart on failure
//! with exponential backoff (5 attempts / 5 min cap).
//!
//! See ADR-019 for the full design rationale.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::RwLock;
use tokio::time::sleep;

use acowork_core::health::supervisor_defaults;
use acowork_core::supervisor::{
    HeartbeatStatus, HeartbeatWatchdog, RestartHistory, SseFrame, backoff_with_jitter,
    parse_sse_frame,
};

use crate::gateway::state::GatewayState;

use super::lsp_relay::{
    check_lsp_relay_health, kill_lsp_relay, spawn_lsp_relay,
};

/// Shared gateway state handle.
pub type SharedState = Arc<RwLock<GatewayState>>;

/// Connect / reconnect backoff bounds.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Configuration for the LSP Relay supervisor.
#[derive(Clone)]
pub struct LspRelaySupervisorConfig {
    pub data_dir: std::path::PathBuf,
    pub port: u16,
    pub gateway_health_url: String,
}

/// Spawn the supervisor task. Must be called from an async context,
/// AFTER the LSP Relay child has been spawned.
pub fn start_lsp_relay_supervisor(cfg: LspRelaySupervisorConfig, state: SharedState) {
    let port = cfg.port;
    tokio::spawn(async move {
        run_supervisor(cfg, state, port).await;
    });
}

/// Long-running supervisor. Monitors the LSP Relay child via SSE; on
/// heartbeat timeout or connection failure, restarts the relay with
/// exponential backoff. Gives up after MAX_RESTART_ATTEMPTS recent failures.
async fn run_supervisor(cfg: LspRelaySupervisorConfig, state: SharedState, port: u16) {
    let mut history = RestartHistory::new();
    let mut in_startup_grace = true;

    // Wait for the initial LSP Relay to bind and start serving /events.
    {
        let deadline = Instant::now() + supervisor_defaults::STARTUP_GRACE;
        loop {
            if try_connect_events(port).await {
                tracing::info!("Initial LSP Relay is serving /events");
                break;
            }
            if state.read().await.lsp_relay_process.is_none() {
                tracing::warn!("Initial LSP Relay died during startup grace; entering restart loop");
                in_startup_grace = false;
                break;
            }
            if Instant::now() >= deadline {
                if lsp_relay_alive(&state, port).await {
                    tracing::warn!("Initial LSP Relay has not bound /events within {:?}, but process is still running; continuing startup wait", supervisor_defaults::STARTUP_GRACE);
                    sleep(supervisor_defaults::STARTUP_POLL).await;
                    continue;
                }
                tracing::warn!("Initial LSP Relay did not respond within {:?} and process is not alive; entering restart loop", supervisor_defaults::STARTUP_GRACE);
                in_startup_grace = false;
                break;
            }
            sleep(supervisor_defaults::STARTUP_POLL).await;
        }
    }

    loop {
        let exit_reason = match run_monitor_session(&state, port, &mut in_startup_grace).await {
            MonitorExit::Clean => {
                tracing::info!("LSP Relay monitor session ended cleanly");
                return;
            }
            exit @ MonitorExit::HeartbeatTimeout { .. } | exit @ MonitorExit::ConnectionLost => exit,
        };

        if in_startup_grace {
            tracing::warn!("LSP Relay monitor session ended during startup grace — retrying shortly");
            sleep(supervisor_defaults::STARTUP_POLL).await;
            continue;
        }

        let relay_alive = try_connect_events(port).await;

        match exit_reason {
            MonitorExit::HeartbeatTimeout { elapsed_secs } => {
                let health = check_lsp_relay_health(port).await;
                if health.is_some() {
                    tracing::warn!(elapsed_secs, "LSP Relay heartbeat timeout, but /health probe succeeded — likely watchdog starvation, not relay stuck. Reconnecting without kill.");
                    continue;
                }
                tracing::warn!(elapsed_secs, "LSP Relay heartbeat timeout — /health probe also failed, killing stuck process");
                if relay_alive {
                    let pid = state.read().await.lsp_relay_process.as_ref().map(|e| e.pid);
                    if let Some(p) = pid
                        && p != 0
                    {
                        let _ = kill_lsp_relay(p).await;
                    }
                }
            }
            MonitorExit::ConnectionLost => {
                if relay_alive {
                    tracing::info!("LSP Relay /events connection lost but server is responding; reconnecting");
                    continue;
                }
            }
            MonitorExit::Clean => unreachable!(),
        }

        if lsp_relay_alive(&state, port).await {
            tracing::info!("LSP Relay HTTP is not ready yet, but process is still alive; waiting instead of restarting");
            sleep(supervisor_defaults::STARTUP_POLL).await;
            continue;
        }

        {
            let mut gw = state.write().await;
            gw.lsp_relay_process = None;
        }

        let attempts = history.record(supervisor_defaults::RESTART_WINDOW);
        if attempts as u32 > supervisor_defaults::MAX_RESTART_ATTEMPTS {
            tracing::error!(attempts, "LSP Relay restart limit exceeded; giving up and clearing gateway LSP Relay state");
            {
                let mut gw = state.write().await;
                gw.lsp_relay_process = None;
            }
            return;
        }

        let backoff = backoff_with_jitter(
            attempts as u32,
            supervisor_defaults::RESTART_BACKOFF_MIN,
            supervisor_defaults::RESTART_BACKOFF_MAX,
        );
        tracing::info!(attempt = attempts, ?backoff, "Restarting LSP Relay process");
        sleep(backoff).await;

        match spawn_lsp_relay(&cfg.data_dir, port, &cfg.gateway_health_url).await {
            Ok((new_state, child)) => {
                tracing::info!(pid = new_state.pid, port = new_state.port, attempt = attempts, "LSP Relay restarted");
                {
                    let mut gw = state.write().await;
                    gw.lsp_relay_process = Some(new_state.clone());
                }
                let new_child_pid = new_state.pid;
                let state_for_reaper = state.clone();
                tokio::spawn(async move {
                    let mut child = child;
                    let _ = child.wait().await;
                    tracing::warn!(pid = new_child_pid, "LSP Relay (respawned) exited");
                    let mut gw = state_for_reaper.write().await;
                    let still_ours = gw.lsp_relay_process.as_ref().map(|eps| eps.pid == new_child_pid).unwrap_or(false);
                    if still_ours {
                        gw.lsp_relay_process = None;
                    }
                });
            }
            Err(e) => {
                if let Some(health) = check_lsp_relay_health(port).await {
                    let attached = super::lsp_relay::attach_existing_lsp_relay(port, Some(health));
                    tracing::info!(port, ready = attached.ready, "Reusing existing LSP Relay after restart failure");
                    {
                        let mut gw = state.write().await;
                        gw.lsp_relay_process = Some(attached);
                    }
                } else {
                    tracing::error!(error = %e, "Failed to restart LSP Relay process");
                }
            }
        }

        // After a restart, give the new relay a short grace window to boot.
        {
            let deadline = Instant::now() + supervisor_defaults::STARTUP_GRACE;
            loop {
                if try_connect_events(port).await {
                    tracing::info!("Restarted LSP Relay is serving /events");
                    break;
                }
                if state.read().await.lsp_relay_process.is_none() {
                    tracing::warn!("Restarted LSP Relay died during grace window");
                    break;
                }
                if Instant::now() >= deadline {
                    if lsp_relay_alive(&state, port).await {
                        sleep(supervisor_defaults::STARTUP_POLL).await;
                        continue;
                    }
                    break;
                }
                sleep(supervisor_defaults::STARTUP_POLL).await;
            }
        }
    }
}

enum MonitorExit {
    Clean,
    HeartbeatTimeout { elapsed_secs: u64 },
    ConnectionLost,
}

/// Run one SSE session: connect to /events, parse events, update shared
/// state. Returns when the connection ends or heartbeat times out.
async fn run_monitor_session(
    state: &SharedState,
    port: u16,
    in_startup_grace: &mut bool,
) -> MonitorExit {
    let url = format!("http://127.0.0.1:{port}/events");
    tracing::info!(%url, "Connecting to LSP Relay SSE event stream");

    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "Failed to build HTTP client for SSE");
            sleep(RECONNECT_MAX).await;
            return MonitorExit::ConnectionLost;
        }
    };

    let resp = match client
        .get(&url)
        .header("Accept", "text/event-stream")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to connect to LSP Relay /events");
            return MonitorExit::ConnectionLost;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "LSP Relay /events returned non-2xx");
        return MonitorExit::ConnectionLost;
    }

    *in_startup_grace = false;

    // Mark the LSP Relay as ready so Gateway's GET /api/lsp/endpoint returns
    // available: true. Without this, a freshly spawned relay would remain
    // permanently "not ready" because spawn_lsp_relay() initializes ready=false
    // and nothing ever promoted it — see analysis report for details.
    {
        let mut gw = state.write().await;
        if let Some(ref mut eps) = gw.lsp_relay_process
            && !eps.ready
        {
            eps.ready = true;
            tracing::info!("LSP Relay marked as ready (port: {})", eps.port);
        }
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    let mut watchdog = HeartbeatWatchdog::new(
        Duration::from_secs(2),
        supervisor_defaults::HEARTBEAT_TIMEOUT,
    );

    loop {
        tokio::select! {
            status = watchdog.tick() => {
                match status {
                    HeartbeatStatus::Ok => {}
                    HeartbeatStatus::Timeout { elapsed_secs } => {
                        tracing::warn!(elapsed_secs, "LSP Relay heartbeat timeout");
                        return MonitorExit::HeartbeatTimeout { elapsed_secs };
                    }
                }
                let gw = state.read().await;
                if gw.lsp_relay_process.is_none() {
                    return MonitorExit::Clean;
                }
            }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(idx) = buffer.find("\n\n") {
                            let frame: String = buffer.drain(..idx + 2).collect();
                            match parse_sse_frame(&frame) {
                                Some(SseFrame::Heartbeat) => {
                                    watchdog.beat();
                                }
                                Some(SseFrame::State(_raw_json)) => {
                                    watchdog.beat();
                                    // LSP Relay state events are informational
                                    // (Starting, Ready, Error). Unlike embed,
                                    // there is no model state to track — the
                                    // relay is either serving or not.
                                }
                                Some(SseFrame::Comment(_)) | None => {}
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "SSE stream read error");
                        return MonitorExit::ConnectionLost;
                    }
                    None => {
                        tracing::info!("SSE stream closed by peer");
                        return MonitorExit::ConnectionLost;
                    }
                }
            }
        }
    }
}

/// Check if the LSP Relay process is alive (by PID or health check).
async fn lsp_relay_alive(state: &SharedState, port: u16) -> bool {
    let pid = state.read().await.lsp_relay_process.as_ref().map(|e| e.pid);
    match pid {
        Some(0) => check_lsp_relay_health(port).await.is_some(),
        Some(pid) => crate::lifecycle::process::check_health(pid).await,
        None => false,
    }
}

/// Try once to connect to /events and confirm it returns 2xx.
async fn try_connect_events(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/events");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client
        .get(&url)
        .header("Accept", "text/event-stream")
        .send()
        .await
    {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supervisor_config_construction() {
        let cfg = LspRelaySupervisorConfig {
            data_dir: std::path::PathBuf::from("/tmp/acowork"),
            port: 19878,
            gateway_health_url: "http://127.0.0.1:19876/health".to_string(),
        };
        assert_eq!(cfg.port, 19878);
        assert_eq!(cfg.gateway_health_url, "http://127.0.0.1:19876/health");
    }

    #[test]
    fn test_supervisor_config_minimal() {
        let cfg = LspRelaySupervisorConfig {
            data_dir: std::path::PathBuf::from("/tmp"),
            port: 0,
            gateway_health_url: "http://127.0.0.1:19876/health".to_string(),
        };
        assert_eq!(cfg.port, 0);
    }

    #[test]
    fn test_reconnect_max_is_reasonable() {
        // Should be at least 5 seconds and at most 60 seconds
        assert!(RECONNECT_MAX >= Duration::from_secs(5));
        assert!(RECONNECT_MAX <= Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_try_connect_events_unreachable() {
        let result = try_connect_events(1).await;
        assert!(!result, "Port 1 should be unreachable");
    }

    #[tokio::test]
    async fn test_lsp_relay_alive_no_state() {
        let state: SharedState = Arc::new(RwLock::new(GatewayState::new("/tmp")));
        let result = lsp_relay_alive(&state, 1).await;
        assert!(!result, "Should be false when no relay process state");
    }
}
