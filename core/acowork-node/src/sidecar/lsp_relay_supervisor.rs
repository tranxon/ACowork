//! LSP Relay process supervisor — monitors the node-local LSP Relay
//! child process via SSE, detects failures, and restarts with
//! exponential backoff (ADR-055 §6.7 node-local sidecar).
//!
//! Migrated from the Gateway's `lifecycle/lsp_relay_supervisor.rs`
//! (the tested template — SSE heartbeat, crash recovery) with these
//! changes:
//! - `SharedState` re-based onto [`crate::state::SharedNodeState`];
//! - the parent-health probe retargeted from the Gateway `/health` to
//!   the Node `/health` (the relay CLI arg keeps its historical name);
//! - every ready/unavailable transition now PUBLISHES the retained
//!   `acowork/nodes/{node_id}/lsps` (AvailableLsps) + sidecar status
//!   topic — the Gateway version left this as `TODO(sidecar-mqtt)`.
//!
//! See ADR-019 for the supervisor design rationale.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::time::sleep;

use acowork_core::health::supervisor_defaults;
use acowork_core::supervisor::{
    HeartbeatStatus, HeartbeatWatchdog, RestartHistory, SseFrame, backoff_with_jitter,
    parse_sse_frame,
};

use crate::state::SharedNodeState;

use super::lsp_relay::{
    check_lsp_relay_health, kill_lsp_relay, spawn_lsp_relay,
};

/// Shared HTTP client for LSP Relay supervisor REST calls
/// (short-lived `/health`, `/events` probe, …). Reusing a single
/// `reqwest::Client` gives us connection pooling across the
/// supervisor's many probe calls.
pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("Failed to build LSP Relay supervisor HTTP client")
    })
}

/// Shared HTTP client for the LSP Relay `/events` SSE stream.
///
/// SSE connections are held open for hours/days, so this client
/// deliberately sets only `connect_timeout` (for the initial TCP
/// handshake) and leaves the per-request timeout unset — the
/// heartbeat watchdog is the liveness bound, not reqwest.
pub(crate) fn sse_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("Failed to build LSP Relay SSE HTTP client")
    })
}

/// Connect / reconnect backoff bounds. The SSE client itself is
/// built via `expect` in `sse_http_client`, so this constant is
/// only referenced by the unit tests below (asserted for sanity).
/// `#[allow(dead_code)]` suppresses the lib-only dead-code warning.
#[allow(dead_code)]
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Configuration for the LSP Relay supervisor (node-local, ADR-055 §6.7).
#[derive(Clone)]
pub struct LspRelaySupervisorConfig {
    /// Node data directory (for the relay log file).
    pub data_dir: std::path::PathBuf,
    /// Port the relay listens on.
    pub port: u16,
    /// Node health URL passed to the relay as `--gateway-health-url`
    /// (self-exit watchdog target — the parent is now the Node).
    pub health_url: String,
    /// This node's id — the retained `lsps` topic is per-node.
    pub node_id: String,
    /// Advertise host used to build the published endpoint
    /// (`http://{advertise_host}:{port}` — ADR-055 §6.3, so remote
    /// Runtimes and the Desktop reach the relay on THIS node).
    pub advertise_host: String,
}

/// Publish the retained node-local LSP state through the process-wide
/// dispatcher (ADR-055 §6.7): `acowork/nodes/{node_id}/lsps`
/// (AvailableLsps) + sidecar status topic. Called on every
/// ready/unavailable transition. Best-effort — a failed publish is
/// logged, never fatal (the next transition or node re-bootstrap
/// re-asserts it).
async fn publish_lsps_state(cfg: &LspRelaySupervisorConfig, ready: bool) {
    let _ = crate::control::dispatcher::publish_lsps_state(
        cfg.node_id.clone(),
        cfg.advertise_host.clone(),
        cfg.port,
        ready,
    )
    .await;
}

/// Full node-local sidecar lifecycle: attach-or-spawn the relay
/// (migrated from the Gateway's startup block), reap the child, and
/// supervise via SSE. Runs until the relay is intentionally removed or
/// the node process exits.
pub fn start_lsp_relay_supervisor(cfg: LspRelaySupervisorConfig, state: SharedNodeState) {
    let port = cfg.port;
    tokio::spawn(async move {
        // ── Initial attach-or-spawn (ADR-055 §6.7, §6.11 local node) ──
        {
            if let Some(health) = check_lsp_relay_health(port).await {
                let relay_state = super::lsp_relay::attach_existing_lsp_relay(port, Some(health));
                let relay_ready = relay_state.ready;
                tracing::info!(
                    port = relay_state.port,
                    ready = relay_ready,
                    "Reusing existing LSP Relay process"
                );
                state.write().await.lsp_relay_process = Some(relay_state);
                publish_lsps_state(&cfg, relay_ready).await;
            } else {
                match spawn_lsp_relay(&cfg.data_dir, port, &cfg.health_url).await {
                    Ok((relay_state, child)) => {
                        let child_pid = relay_state.pid;
                        tracing::info!(
                            pid = relay_state.pid,
                            port = relay_state.port,
                            "LSP Relay process spawned"
                        );
                        state.write().await.lsp_relay_process = Some(relay_state);
                        let state_for_reaper = state.clone();
                        // Reaper: clear the state only if the PID is
                        // still ours (same contract as the Gateway).
                        tokio::spawn(async move {
                            let mut child = child;
                            let exit_status = child.wait().await;
                            tracing::warn!(
                                pid = child_pid,
                                exit_status = ?exit_status,
                                "LSP Relay process exited"
                            );
                            let mut s = state_for_reaper.write().await;
                            let still_ours = s
                                .lsp_relay_process
                                .as_ref()
                                .map(|eps| eps.pid == child_pid)
                                .unwrap_or(false);
                            if still_ours {
                                s.lsp_relay_process = None;
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to spawn LSP Relay (codebase tooling will be unavailable)"
                        );
                        // Clear any stale retained state from a previous
                        // run so consumers see "unavailable".
                        publish_lsps_state(&cfg, false).await;
                    }
                }
            }
        }
        run_supervisor(cfg, state, port).await;
    });
}

/// Long-running supervisor. Monitors the LSP Relay child via SSE; on
/// heartbeat timeout or connection failure, restarts the relay with
/// exponential backoff. Gives up after MAX_RESTART_ATTEMPTS recent
/// failures.
async fn run_supervisor(
    cfg: LspRelaySupervisorConfig,
    state: SharedNodeState,
    port: u16,
) {
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
        let exit_reason = match run_monitor_session(&cfg, &state, port, &mut in_startup_grace).await {
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
            let mut s = state.write().await;
            s.lsp_relay_process = None;
        }
        // ADR-055 §6.7: the retained per-node lsps topic must reflect
        // the unavailable state (the Gateway version left this as
        // TODO(sidecar-mqtt)).
        publish_lsps_state(&cfg, false).await;

        let attempts = history.record(supervisor_defaults::RESTART_WINDOW);
        if attempts as u32 > supervisor_defaults::MAX_RESTART_ATTEMPTS {
            tracing::error!(attempts, "LSP Relay restart limit exceeded; giving up and clearing node LSP Relay state");
            {
                let mut s = state.write().await;
                s.lsp_relay_process = None;
            }
            publish_lsps_state(&cfg, false).await;
            return;
        }

        let backoff = backoff_with_jitter(
            attempts as u32,
            supervisor_defaults::RESTART_BACKOFF_MIN,
            supervisor_defaults::RESTART_BACKOFF_MAX,
        );
        tracing::info!(attempt = attempts, ?backoff, "Restarting LSP Relay process");
        sleep(backoff).await;

        match spawn_lsp_relay(&cfg.data_dir, port, &cfg.health_url).await {
            Ok((new_state, child)) => {
                tracing::info!(pid = new_state.pid, port = new_state.port, attempt = attempts, "LSP Relay restarted");
                {
                    let mut s = state.write().await;
                    s.lsp_relay_process = Some(new_state.clone());
                }
                publish_lsps_state(&cfg, false).await;
                let new_child_pid = new_state.pid;
                let state_for_reaper = state.clone();
                tokio::spawn(async move {
                    let mut child = child;
                    let _ = child.wait().await;
                    tracing::warn!(pid = new_child_pid, "LSP Relay (respawned) exited");
                    let mut s = state_for_reaper.write().await;
                    let still_ours = s
                        .lsp_relay_process
                        .as_ref()
                        .map(|eps| eps.pid == new_child_pid)
                        .unwrap_or(false);
                    if still_ours {
                        s.lsp_relay_process = None;
                    }
                });
            }
            Err(e) => {
                if let Some(health) = check_lsp_relay_health(port).await {
                    let attached = super::lsp_relay::attach_existing_lsp_relay(port, Some(health));
                    let attached_ready = attached.ready;
                    tracing::info!(port, ready = attached_ready, "Reusing existing LSP Relay after restart failure");
                    {
                        let mut s = state.write().await;
                        s.lsp_relay_process = Some(attached);
                    }
                    publish_lsps_state(&cfg, attached_ready).await;
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
    cfg: &LspRelaySupervisorConfig,
    state: &SharedNodeState,
    port: u16,
    in_startup_grace: &mut bool,
) -> MonitorExit {
    let url = format!("http://127.0.0.1:{port}/events");
    tracing::info!(%url, "Connecting to LSP Relay SSE event stream");

    // SSE is a long-lived connection (hours/days). Use only a connect
    // timeout for the TCP handshake; the per-connection total-timeout
    // would kill the stream after 30s and falsely trigger a restart.
    // Liveness is enforced by the heartbeat watchdog at the app level.
    let client = sse_http_client();

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

    // Mark the LSP Relay as ready and publish the retained per-node lsps
    // topic. Without this, a freshly spawned relay would remain
    // permanently "not ready" because spawn_lsp_relay() initializes
    // ready=false and nothing ever promoted it (the exact gap the
    // Gateway's GET /api/lsp/endpoint hit — see the migration notes).
    {
        let mut s = state.write().await;
        if let Some(ref mut eps) = s.lsp_relay_process
            && !eps.ready
        {
            eps.ready = true;
            tracing::info!("LSP Relay marked as ready (port: {})", eps.port);
            drop(s);
            // ADR-055 §6.7: ready transition → publish AvailableLsps
            // with the advertise-host endpoint.
            publish_lsps_state(cfg, true).await;
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
                let s = state.read().await;
                if s.lsp_relay_process.is_none() {
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
async fn lsp_relay_alive(state: &SharedNodeState, port: u16) -> bool {
    let pid = state.read().await.lsp_relay_process.as_ref().map(|e| e.pid);
    match pid {
        Some(0) => check_lsp_relay_health(port).await.is_some(),
        Some(pid) => crate::process::spawn::check_health(pid).await,
        None => false,
    }
}

/// Try once to connect to /events and confirm it returns 2xx.
async fn try_connect_events(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/events");
    let client = http_client();
    match client
        .get(&url)
        .header("Accept", "text/event-stream")
        .timeout(Duration::from_secs(2))
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
    use crate::state::NodeState;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn test_config() -> LspRelaySupervisorConfig {
        LspRelaySupervisorConfig {
            data_dir: std::path::PathBuf::from("/tmp/acowork-node"),
            port: 19878,
            health_url: "http://127.0.0.1:19900/health".to_string(),
            node_id: "local".to_string(),
            advertise_host: "127.0.0.1".to_string(),
        }
    }

    #[test]
    fn test_supervisor_config_construction() {
        let cfg = test_config();
        assert_eq!(cfg.port, 19878);
        assert_eq!(cfg.health_url, "http://127.0.0.1:19900/health");
        assert_eq!(cfg.node_id, "local");
    }

    #[test]
    fn test_supervisor_config_minimal() {
        let cfg = LspRelaySupervisorConfig {
            data_dir: std::path::PathBuf::from("/tmp"),
            port: 0,
            health_url: "http://127.0.0.1:19900/health".to_string(),
            node_id: "gpu-1".to_string(),
            advertise_host: "192.168.1.10".to_string(),
        };
        assert_eq!(cfg.port, 0);
        assert_eq!(cfg.advertise_host, "192.168.1.10");
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
        let state: SharedNodeState = Arc::new(RwLock::new(NodeState::new(16)));
        let result = lsp_relay_alive(&state, 1).await;
        assert!(!result, "Should be false when no relay process state");
    }
}
