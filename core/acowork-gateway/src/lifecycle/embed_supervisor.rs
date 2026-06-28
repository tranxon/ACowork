//! Embed process supervisor — monitors the embed child process via SSE,
//! detects failures, and restarts with exponential backoff.
//!
//! ## Why SSE instead of polling
//!
//! Earlier the gateway polled `GET /health` for the embed's loaded model.
//! That had two problems: a 2-second poll lag for state changes, and
//! nothing distinguished "process alive but no model yet" from "process
//! stuck". With the embed's `/events` SSE stream we get push-style state
//! transitions AND a 2-second heartbeat in one connection.
//!
//! ## Failure detection
//!
//! Two layers:
//!   1. `child.wait()` returning — process crashed or was killed.
//!   2. No heartbeat received for `HEARTBEAT_TIMEOUT` (10s) — process
//!      alive but stuck in a deadlock / GC pause / ONNX hang.
//!
//! Both trigger the same restart path.
//!
//! ## Restart policy
//!
//! Exponential backoff: 1s, 2s, 4s, 8s, ... capped at 60s. After
//! `MAX_RESTART_ATTEMPTS` consecutive failures within
//! `RESTART_WINDOW` (5 minutes) the supervisor gives up and surfaces
//! the failure to the gateway, which falls back to remote embedding
//! providers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::RwLock;
use tokio::time::sleep;

use acowork_core::health::supervisor_defaults;
use acowork_core::supervisor::{
    HeartbeatStatus, HeartbeatWatchdog, RestartHistory, SseFrame, backoff_with_jitter,
    parse_sse_frame,
};

use crate::gateway::state::GatewayState;
use crate::ipc::global_push::GlobalResourcePusher;

use super::embed::spawn_embed_process;

/// Shared gateway state handle. Same as `ipc::server::SharedState`
/// but re-declared here to avoid a cycle (lifecycle shouldn't import
/// from ipc::server, and ipc::server shouldn't import lifecycle).
pub type SharedState = Arc<RwLock<GatewayState>>;

/// Connect / reconnect backoff bounds.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Configuration passed in from the gateway when starting the supervisor.
/// Holds the same args used to spawn the initial embed instance so we
/// can re-spawn on failure with identical settings.
#[derive(Clone)]
pub struct EmbedSupervisorConfig {
    pub data_dir: std::path::PathBuf,
    pub models_dir: std::path::PathBuf,
    pub port: u16,
    pub hf_mirrors: Vec<String>,
    pub onnx_variant: String,
    pub model_id: Option<String>,
}

/// Parsed `state` event payload from the embed SSE stream.
///
/// We only care about the active model ID and dimension. The gateway
/// stores these in `EmbedProcessState` and pushes them to running
/// agents via `RuntimeConfigUpdate.embed_config_json`.
#[derive(Debug, Deserialize)]
struct StateEvent {
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    dimension: Option<u64>,
}

/// Parsed wrapper of the SSE `state` event line:
/// `{"seq": N, "state": {...}}`
#[derive(Debug, Deserialize)]
struct StateEventEnvelope {
    state: StateEvent,
}

/// Shared state handle for the supervisor and the HTTP layer. Re-exports
/// the same `SharedState` type used elsewhere in the gateway.
pub type SharedEmbedState = SharedState;

/// Spawn the supervisor task. Must be called from an async context,
/// AFTER the embed child has been spawned. The supervisor connects to
/// `http://127.0.0.1:{port}/events` and:
///   - updates shared state on every `state` event
///   - watches heartbeats (2s cadence, 10s timeout = stuck)
///   - restarts the embed process on confirmed failure, with
///     exponential backoff and a 5-attempts/5-min cap
///
/// On reaching the restart cap, the supervisor clears the shared state
/// and returns — the gateway's HTTP API will then fall back to remote
/// embedding providers.
pub fn start_embed_supervisor(
    cfg: EmbedSupervisorConfig,
    state: SharedEmbedState,
    pusher: Option<Arc<GlobalResourcePusher>>,
) {
    let port = cfg.port;
    tokio::spawn(async move {
        run_supervisor(cfg, state, pusher, port).await;
    });
}

/// Long-running supervisor. Monitors the embed child via SSE; on
/// heartbeat timeout or connection failure, restarts the embed with
/// exponential backoff. Gives up after `MAX_RESTART_ATTEMPTS` recent
/// failures.
async fn run_supervisor(
    cfg: EmbedSupervisorConfig,
    state: SharedEmbedState,
    pusher: Option<Arc<GlobalResourcePusher>>,
    port: u16,
) {
    let mut history = RestartHistory::new();
    // We start in a "startup grace" window during which connection
    // failures do NOT count as restart attempts. The freshly spawned
    // embed may take a few seconds to bind port 18080, load the ORT
    // library, and load the recommended model — all of which can exceed
    // the naive "first reconnect is immediate" policy. Once the
    // supervisor successfully connects once, it transitions to the
    // normal mode where any disconnection IS a restart trigger.
    let mut in_startup_grace = true;

    // Wait for the initial embed to bind and start serving /events.
    // Don't count failures during this period against the restart budget.
    {
        let deadline = Instant::now() + supervisor_defaults::STARTUP_GRACE;
        loop {
            if try_connect_events(port).await {
                tracing::info!("Initial embed is serving /events");
                break;
            }
            // If the reaper cleared state, the process died during boot —
            // fall through to the normal restart path immediately.
            if state.read().await.embed_process.is_none() {
                tracing::warn!("Initial embed died during startup grace; entering restart loop");
                in_startup_grace = false;
                break;
            }
            if Instant::now() >= deadline {
                if embed_process_alive(&state, port).await {
                    tracing::warn!(
                        "Initial embed has not bound /events within {:?}, but process is still running; continuing startup wait",
                        supervisor_defaults::STARTUP_GRACE
                    );
                    sleep(supervisor_defaults::STARTUP_POLL).await;
                    continue;
                }
                tracing::warn!(
                    "Initial embed did not respond within {:?} and process is not alive; entering restart loop",
                    supervisor_defaults::STARTUP_GRACE
                );
                in_startup_grace = false;
                break;
            }
            sleep(supervisor_defaults::STARTUP_POLL).await;
        }
    }

    loop {
        let exit_reason =
            match run_monitor_session(&cfg, &state, &pusher, port, &mut in_startup_grace).await {
                MonitorExit::Clean => {
                    tracing::info!("Embed monitor session ended cleanly");
                    return;
                }
                exit @ MonitorExit::HeartbeatTimeout { .. } | exit @ MonitorExit::ConnectionLost => exit,
            };

        // During startup grace, failures don't count — the embed is
        // probably just slow to boot.
        if in_startup_grace {
            tracing::warn!("Embed monitor session ended during startup grace — retrying shortly");
            sleep(supervisor_defaults::STARTUP_POLL).await;
            continue;
        }

        // Best-effort: is the embed actually dead, or just the SSE
        // connection that broke?
        let embed_alive = try_connect_events(port).await;

        match exit_reason {
            MonitorExit::HeartbeatTimeout { elapsed_secs } => {
                // SSE heartbeats stopped, but the embed may still be healthy —
                // the watchdog itself could have been starved (e.g. by a
                // blocking LSP install on the same tokio runtime). Before
                // killing, probe /health to confirm the embed is truly stuck.
                let health = super::embed::check_embed_health(port).await;
                if health.is_some() {
                    tracing::warn!(
                        elapsed_secs,
                        "Embed heartbeat timeout, but /health probe succeeded — \
                         likely watchdog starvation, not embed stuck. Reconnecting without kill."
                    );
                    continue;
                }
                // /health probe also failed — embed is truly stuck.
                tracing::warn!(
                    elapsed_secs,
                    "Embed heartbeat timeout — /health probe also failed, killing stuck process"
                );
                if embed_alive {
                    let pid = state.read().await.embed_process.as_ref().map(|e| e.pid);
                    if let Some(p) = pid
                        && p != 0
                    {
                        let _ = super::embed::kill_embed_process(p).await;
                    }
                }
            }
            MonitorExit::ConnectionLost => {
                if embed_alive {
                    // Transient network glitch or the SSE-timeout bug.
                    // Embed is fine — just reconnect.
                    tracing::info!(
                        "Embed /events connection lost but server is responding; reconnecting"
                    );
                    continue;
                }
                // Embed process died. Fall through to restart.
            }
            MonitorExit::Clean => unreachable!(),
        }

        if embed_process_alive(&state, port).await {
            tracing::info!(
                "Embed HTTP is not ready yet, but process is still alive; waiting instead of restarting"
            );
            sleep(supervisor_defaults::STARTUP_POLL).await;
            continue;
        }

        {
            let mut gw = state.write().await;
            gw.embed_process = None;
        }

        let attempts = history.record(supervisor_defaults::RESTART_WINDOW);
        if attempts as u32 > supervisor_defaults::MAX_RESTART_ATTEMPTS {
            tracing::error!(
                attempts,
                "Embed restart limit exceeded; giving up and clearing gateway embed state"
            );
            {
                let mut gw = state.write().await;
                gw.embed_process = None;
            }
            return;
        }

        let backoff = backoff_with_jitter(
            attempts as u32,
            supervisor_defaults::RESTART_BACKOFF_MIN,
            supervisor_defaults::RESTART_BACKOFF_MAX,
        );
        tracing::info!(attempt = attempts, ?backoff, "Restarting embed process");
        sleep(backoff).await;

        match spawn_embed_process(
            &cfg.data_dir,
            &cfg.models_dir,
            port,
            &cfg.hf_mirrors,
            &cfg.onnx_variant,
            cfg.model_id.as_deref(),
        )
        .await
        {
            Ok((new_state, child)) => {
                tracing::info!(
                    pid = new_state.pid,
                    port = new_state.port,
                    attempt = attempts,
                    "Embed restarted"
                );
                // Update shared state with the new child. The reaper in
                // gateway::mod.rs is PID-aware so the old child's exit
                // won't clobber this. (Same reaper is also installed for
                // the new child via the one in the `if let Some(child)`
                // block above — but that's only the initial spawn path;
                // subsequent respawns get their own reaper here.)
                {
                    let mut gw = state.write().await;
                    gw.embed_process = Some(new_state.clone());
                }

                // Spawn a PID-aware reaper for the new child. The reaper
                // clears `embed_process` only if its own PID still owns
                // the slot, so a previous child's late exit won't clobber
                // the new state.
                let new_child_pid = new_state.pid;
                let state_for_reaper = state.clone();
                tokio::spawn(async move {
                    let mut child = child; // wait() needs &mut
                    let _ = child.wait().await;
                    tracing::warn!(pid = new_child_pid, "Embed (respawned) exited");
                    let mut gw = state_for_reaper.write().await;
                    let still_ours = gw
                        .embed_process
                        .as_ref()
                        .map(|eps| eps.pid == new_child_pid)
                        .unwrap_or(false);
                    if still_ours {
                        gw.embed_process = None;
                    }
                });
                // Push the (now-empty or new-model) embed config to
                // agents so they can refresh local embedding caches.
                if let Some(p) = &pusher {
                    p.push_embedding_config().await;
                }
            }
            Err(e) => {
                if let Some(health) = super::embed::check_embed_health(port).await {
                    let attached = super::embed::attach_existing_embed_process(port, Some(health));
                    tracing::info!(
                        port,
                        ready = attached.ready,
                        "Reusing existing embed after restart failure"
                    );
                    {
                        let mut gw = state.write().await;
                        gw.embed_process = Some(attached);
                    }
                    if let Some(p) = &pusher {
                        p.push_embedding_config().await;
                    }
                } else {
                    tracing::error!(error = %e, "Failed to restart embed process");
                }
            }
        }

        // After a restart, give the new embed child a short grace
        // window to boot before we attempt to connect. This is the
        // same logic as the initial startup grace, but inlined here
        // so it runs after every restart, not just the first one.
        {
            let deadline = Instant::now() + supervisor_defaults::STARTUP_GRACE;
            loop {
                if try_connect_events(port).await {
                    tracing::info!("Restarted embed is serving /events");
                    break;
                }
                if state.read().await.embed_process.is_none() {
                    tracing::warn!("Restarted embed died during grace window");
                    break;
                }
                if Instant::now() >= deadline {
                    if embed_process_alive(&state, port).await {
                        tracing::warn!(
                            "Restarted embed has not bound /events within {:?}, but process is still running; continuing startup wait",
                            supervisor_defaults::STARTUP_GRACE
                        );
                        sleep(supervisor_defaults::STARTUP_POLL).await;
                        continue;
                    }
                    tracing::warn!(
                        "Restarted embed did not respond within {:?} and process is not alive; entering monitor anyway",
                        supervisor_defaults::STARTUP_GRACE
                    );
                    break;
                }
                sleep(supervisor_defaults::STARTUP_POLL).await;
            }
        }
    }
}

enum MonitorExit {
    /// Process ended naturally (monitor session saw process go away).
    Clean,
    /// Heartbeat stream went silent for > HEARTBEAT_TIMEOUT.
    HeartbeatTimeout {
        /// How long since the last heartbeat was received.
        elapsed_secs: u64,
    },
    /// SSE connection lost for non-timeout reasons (network, restart, etc.).
    ConnectionLost,
}

/// Run one SSE session: connect to /events, parse events, update shared
/// state. Returns when the connection ends or heartbeat times out.
/// `in_startup_grace` is set to false once a connection is established,
/// signaling to the caller that any subsequent disconnection is a real
/// restart trigger (not part of the initial boot window).
async fn run_monitor_session(
    _cfg: &EmbedSupervisorConfig,
    state: &SharedEmbedState,
    pusher: &Option<Arc<GlobalResourcePusher>>,
    port: u16,
    in_startup_grace: &mut bool,
) -> MonitorExit {
    let url = format!("http://127.0.0.1:{port}/events");
    tracing::info!(%url, "Connecting to embed SSE event stream");

    // SSE is a long-lived connection (hours/days). Use only a connect
    // timeout for the TCP handshake; the per-connection total-timeout
    // would kill the stream after 30s and falsely trigger a restart.
    // Liveness is enforced by the heartbeat watchdog at the app level.
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "Failed to build HTTP client for SSE");
            // No point retrying immediately — this is a config-level error.
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
            tracing::warn!(error = %e, "Failed to connect to embed /events");
            return MonitorExit::ConnectionLost;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "Embed /events returned non-2xx");
        return MonitorExit::ConnectionLost;
    }

    // We've established a connection. From here on, the supervisor
    // is in normal (non-grace) mode — any disconnection is a real
    // restart trigger. The caller is expected to clear this flag.
    *in_startup_grace = false;

    // Bootstrap the loaded-model state from the embed's /health
    // endpoint. This catches the case where the embed was already
    // serving before we connected (e.g. the supervisor's monitor
    // session was restarted but the embed was fine the whole time).
    // We only do this on session start; later state transitions
    // arrive via the SSE stream.
    if let Err(e) = bootstrap_state_from_health(port, state, pusher).await {
        tracing::warn!(error = %e, "Initial /health bootstrap failed; continuing with SSE only");
    }

    // Verify the model can actually produce embeddings, not just respond
    // to HTTP. A model that loaded but is stuck in ONNX will pass /health
    // but fail inference. This is a non-fatal check — we log a warning
    // and continue; agents have fallback to text search.
    if !probe_inference(port).await {
        tracing::warn!("Embed inference probe failed — model may not be fully loaded");
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    // Heartbeat watchdog — fires when no heartbeat for too long.
    let mut watchdog = HeartbeatWatchdog::new(
        Duration::from_secs(2),
        supervisor_defaults::HEARTBEAT_TIMEOUT,
    );

    loop {
        tokio::select! {
            // Periodic check: did the heartbeat go stale?
            status = watchdog.tick() => {
                match status {
                    HeartbeatStatus::Ok => {}
                    HeartbeatStatus::Timeout { elapsed_secs } => {
                        tracing::warn!(
                            elapsed_secs,
                            "Embed heartbeat timeout"
                        );
                        return MonitorExit::HeartbeatTimeout { elapsed_secs };
                    }
                }
                // Also check whether the shared state was cleared by
                // the reaper (process died). If so, end the session
                // cleanly.
                let gw = state.read().await;
                if gw.embed_process.is_none() {
                    return MonitorExit::Clean;
                }
            }

            // Read next chunk from the SSE stream.
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        // Parse complete SSE frames (terminated by \n\n).
                        while let Some(idx) = buffer.find("\n\n") {
                            let frame: String = buffer.drain(..idx + 2).collect();
                            match parse_sse_frame(&frame) {
                                Some(SseFrame::Heartbeat) => {
                                    watchdog.beat();
                                }
                                Some(SseFrame::State(raw_json)) => {
                                    watchdog.beat();
                                    if let Ok(env) = serde_json::from_str::<StateEventEnvelope>(&raw_json) {
                                        apply_state_event(state, pusher, env.state).await;
                                    }
                                }
                                Some(SseFrame::Comment(_)) | None => {
                                    // SSE comment (e.g., "lagged:3") or
                                    // unparseable frame — ignore.
                                }
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

/// Bootstrap the gateway's view of the loaded model from the embed's
/// `/health` endpoint. Used once at the start of each monitor session
/// to recover state when the supervisor's connection lagged behind the
/// embed's startup (e.g. the embed was already ready when the
/// supervisor's SSE connect finally went through, so the `Ready` SSE
/// state event was already published and lost).
async fn bootstrap_state_from_health(
    port: u16,
    state: &SharedEmbedState,
    pusher: &Option<Arc<GlobalResourcePusher>>,
) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/health");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = client
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let status = body.get("status").and_then(|s| s.as_str()).unwrap_or("");
    let (model_id, dimension) = if status == "ready" {
        let id = body
            .get("model")
            .and_then(|m| m.get("id"))
            .and_then(|s| s.as_str())
            .map(String::from);
        let dim = body
            .get("model")
            .and_then(|m| m.get("dimension"))
            .and_then(|d| d.as_u64())
            .map(|d| d as usize);
        (id, dim)
    } else {
        (None, None)
    };

    let (changed, applied) = {
        let mut gw = state.write().await;
        let Some(eps) = gw.embed_process.as_mut() else {
            return Err("no embed state".to_string());
        };
        let changed = eps.active_model_id != model_id || eps.active_dimension != dimension;
        eps.active_model_id = model_id;
        eps.active_dimension = dimension;
        if eps.active_model_id.is_some() {
            eps.ready = true;
        }
        (changed, (eps.active_model_id.clone(), eps.active_dimension))
    };

    if changed {
        tracing::info!(
            model_id = ?applied.0,
            dimension = ?applied.1,
            "Embed state bootstrapped from /health"
        );
        if let Some(p) = pusher {
            p.push_embedding_config().await;
        }
    }
    Ok(())
}

/// Probe the embed's actual inference capability by sending a lightweight
/// embedding request. Unlike /health (which only checks HTTP liveness),
/// this verifies the ONNX model is loaded and can produce embeddings.
///
/// Returns `true` if the embed returns a valid embedding vector.
async fn probe_inference(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let body = serde_json::json!({"input": "health check"});
    match client.post(&url).json(&body).send().await {
        Ok(resp) => {
            if !resp.status().is_success() {
                return false;
            }
            // Verify the response contains a valid embedding.
            match resp.json::<serde_json::Value>().await {
                Ok(json) => {
                    json.get("data")
                        .and_then(|d| d.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|item| item.get("embedding"))
                        .and_then(|emb| emb.as_array())
                        .map(|arr| !arr.is_empty())
                        .unwrap_or(false)
                }
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}

/// Try once to connect to `http://127.0.0.1:{port}/events` and confirm
/// it returns a 2xx status. Used during the startup grace window to
/// wait for the freshly-spawned embed to begin serving without
/// consuming the restart budget.
async fn embed_process_alive(state: &SharedEmbedState, port: u16) -> bool {
    let pid = state.read().await.embed_process.as_ref().map(|e| e.pid);
    match pid {
        Some(0) => super::embed::check_embed_health(port).await.is_some(),
        Some(pid) => crate::lifecycle::process::check_health(pid).await,
        None => false,
    }
}

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

/// Apply a parsed `state` event to the shared gateway state and push
/// the change to running agents so they pick up the new model.
async fn apply_state_event(
    state: &SharedEmbedState,
    pusher: &Option<Arc<GlobalResourcePusher>>,
    event: StateEvent,
) {
    // Snapshot the new values for the lock, plus the old values for change
    // detection, in a single short-lived critical section.
    let (new_model_id, new_dimension, changed) = {
        let mut gw = state.write().await;
        let Some(eps) = gw.embed_process.as_mut() else {
            return;
        };
        let new_model_id = event.model_id.filter(|s| !s.is_empty());
        let new_dimension = event.dimension.map(|d| d as usize);
        let changed = eps.active_model_id != new_model_id || eps.active_dimension != new_dimension;
        eps.active_model_id = new_model_id.clone();
        eps.active_dimension = new_dimension;
        if eps.active_model_id.is_some() {
            eps.ready = true;
        }
        (new_model_id, new_dimension, changed)
    };

    if changed {
        tracing::info!(
            model_id = ?new_model_id,
            dimension = ?new_dimension,
            "Embed state updated from SSE"
        );
        if let Some(p) = pusher {
            p.push_embedding_config().await;
        }
    }
}
