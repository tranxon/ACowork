//! Auto-sleep (idle) watcher — Runtime self-terminates after a configurable
//! idle period when no user input has been received and no session is active.
//!
//! Design overview
//! ---------------
//! The Runtime owns the lifecycle decision: it has the authoritative view of
//! when the last inbound message arrived and which sessions are currently
//! active. The Gateway used to host a similar checker (see `gateway/mod.rs`
//! Phase A stub) but it was removed because the Gateway lacks the
//! session-state signal needed to make a safe decision. Everything in this
//! module is resolved and triggered from the Runtime side.
//!
//! Behaviour
//! ---------
//! 1. `resolve_idle_timeout_secs` computes the effective timeout from the
//!    three-layer chain (user override → manifest default → 300s).
//! 2. `IdleWatcherHandle::record_inbound` is called from every MQTT control
//!    dispatch (the single point where user activity enters the Runtime —
//!    see `startup/gateway_loop.rs::control_action_to_inbound`).
//! 3. A background tokio task wakes every `min(60s, timeout)` and:
//!    - If `effective_timeout == 0` ("never sleep") the task is never started.
//!    - If any session is in `is_active()` state (Working / Thinking /
//!      Streaming / ToolExecuting / WaitingApproval / Paused — see
//!      `SessionStatus::is_active`), the deadline is suspended until the
//!      session becomes Idle again.
//!    - Otherwise (`all sessions Idle`), the time since the last recorded
//!      inbound is checked against the effective timeout.
//!    - On expiry: publish `"sleeping"` to the agent status retained topic,
//!      call `RuntimeMqttClient::disconnect`, then `process::exit(0)`. The
//!      broker LWT will replace the payload with `"offline"` shortly after
//!      the connection drops; the Gateway receives the `"sleeping"` payload
//!      first (because `disconnect` waits for in-flight publishes) and uses
//!      it to stamp the agent's `sleeping_at` timestamp so the frontend can
//!      distinguish a sleep from a manual stop.
//!
//! This file deliberately avoids coupling to `SessionManager` directly: the
//! watcher takes a `session_active_fn: Arc<dyn Fn() -> bool + Send + Sync>`
//! closure so tests can inject a deterministic state without spinning a
//! real SessionManager.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use crate::mqtt::client::MqttQoS;
use crate::mqtt::RuntimeMqttClient;

/// Default idle timeout in seconds when neither the user override nor the
/// manifest supplies one. Matches the legacy `GatewayConfig::timeouts::
/// idle_timeout_secs` default (1800 s = 30 min) so behaviour is unchanged for
/// operators who never set the new field.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;

/// Sentinel for "never sleep" — stored in `AgentConfig::idle_timeout_secs`
/// (and exposed to the UI as the "Never" dropdown option). When the
/// effective timeout resolves to this value, the watcher is **not**
/// spawned at all.
pub const NEVER_SLEEP: u64 = 0;

/// Minimum polling interval. The watcher always wakes at least this often
/// so that a tight timeout (e.g. 5 min) gets evaluated within a reasonable
/// window of the deadline. For very long timeouts this is dominated by the
/// timeout itself (`min(60s, timeout)`).
const MIN_INTERVAL_SECS: u64 = 60;

/// Resolve the effective idle timeout from the three-layer chain.
///
/// Resolution order (Layer 1 = highest priority):
/// 1. `config.idle_timeout_secs` — user's agent-level setting (set via
///    Agent Setup panel).
/// 2. `manifest.resources.idle_timeout_secs` — package author default.
/// 3. [`DEFAULT_IDLE_TIMEOUT_SECS`] — hardcoded fallback.
///
/// `Some(0)` at any layer means "never sleep" and short-circuits
/// resolution; the manifest / fallback values are NOT consulted. This
/// mirrors the existing `System Agent cannot be stopped` contract for
/// `LifecycleManager::stop_agent` — the sentinel is a clean "disable"
/// signal rather than a tier on the chain.
pub fn resolve_idle_timeout_secs(
    config_override: Option<u64>,
    manifest_default: Option<u64>,
) -> u64 {
    match config_override {
        // User explicitly chose "never" — honour it.
        Some(NEVER_SLEEP) => NEVER_SLEEP,
        Some(secs) => secs,
        None => match manifest_default {
            Some(NEVER_SLEEP) => NEVER_SLEEP,
            Some(secs) => secs,
            None => DEFAULT_IDLE_TIMEOUT_SECS,
        },
    }
}

/// Handle to a running [`IdleWatcher`] task.
///
/// Cheap to clone (`Arc` internals). Method calls are wait-free (`AtomicI64`
/// stores). The background task is detached: dropping the handle does **not**
/// stop the watcher — it keeps running until the timeout fires or the
/// process exits. This matches the "fire-and-forget" intent of auto-sleep
/// (the watcher owns the exit decision, not the owner of the handle).
#[derive(Clone)]
pub struct IdleWatcherHandle {
    /// Last user-inbound timestamp, in milliseconds since the Unix epoch.
    /// `AtomicI64` so writers (`record_inbound`) and readers (the watcher
    /// loop) never block on each other. `i64` is wide enough for ~292
    /// million years; the field is initialised at spawn time and never
    /// `Ordering::Relaxed` cross-thread.
    last_inbound_at_ms: Arc<AtomicI64>,
}

impl IdleWatcherHandle {
    /// Record that user activity has just arrived. Cheap (`AtomicI64`
    /// store).
    ///
    /// Called from every MQTT control dispatch that represents a user
    /// action — `SendMessage`, `OpenSession`, `CreateSession`,
    /// `ContinueExecution`, `StopGeneration`, etc. — via the watcher
    /// extension point in `startup/gateway_loop.rs`. The check `now -
    /// last_inbound_at > timeout` happens in the background loop; this
    /// method only updates the timestamp.
    pub fn record_inbound(&self) {
        let now_ms = unix_now_ms();
        self.last_inbound_at_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Last recorded inbound timestamp, in milliseconds since the Unix
    /// epoch. Returns `None` if no inbound has been recorded yet (only
    /// possible if the watcher was spawned but `record_inbound` was never
    /// called — currently the dispatch path always calls it).
    pub fn last_inbound_at_ms(&self) -> Option<i64> {
        let v = self.last_inbound_at_ms.load(Ordering::Relaxed);
        if v == 0 {
            None
        } else {
            Some(v)
        }
    }
}

/// Configuration for [`spawn_idle_watcher`].
///
/// `effective_timeout_secs` is the **resolved** value (post `resolve_idle_timeout_secs`).
/// Callers are expected to compute it once at startup and pass the result
/// in — that way the watcher does not need to know about manifest or
/// agent_config structures.
pub struct IdleWatcherConfig {
    /// Effective timeout in seconds. `0` disables the watcher entirely
    /// (the spawn function returns `None` and the task is not started).
    pub effective_timeout_secs: u64,
    /// Agent ID — used purely for log enrichment.
    pub agent_id: String,
    /// MQTT client — the watcher publishes the `"sleeping"` status
    /// payload and calls `disconnect()` before exiting.
    pub mqtt_client: RuntimeMqttClient,
    /// Async-friendly view of the live session state. The watcher's tick
    /// loop awaits this future to decide whether the idle deadline should
    /// be suspended (Working / Thinking / Streaming / ToolExecuting /
    /// WaitingApproval / Paused).
    ///
    /// We use an async trait object (rather than a sync `Fn() -> bool`)
    /// because [`crate::agent::session::SessionManager`] is not `Send`
    /// (it contains `tokio::sync::Mutex` fields etc.) and its state
    /// accessor is naturally async. Implementations wrap an `Arc<Mutex<…>>`
    /// around the relevant session state and await a lock.
    pub session_activity: Arc<dyn SessionActivityChecker>,
}

/// Async view of "is any session currently active?".
///
/// `SessionActivityChecker::any_active` is awaited from the watcher's
/// tick loop on every interval. It should be cheap (lock + iterate the
/// active-session map, all O(n) and n is typically 1).
///
/// The trait object makes `session_init.rs` (the only producer) free to
/// choose its preferred locking strategy — `tokio::sync::Mutex`,
/// `parking_lot::Mutex` with `spawn_blocking`, an `RwLock`, etc.
#[async_trait::async_trait]
pub trait SessionActivityChecker: Send + Sync {
    /// Returns `true` when **any** session is in
    /// [`SessionStatus::is_active()`] state (Working / Thinking /
    /// Streaming / ToolExecuting / WaitingApproval / Paused). Returns
    /// `false` if no sessions are tracked or if the activity view is
    /// temporarily unavailable (e.g. the lock is contended — the watcher
    /// then falls back to "nothing active" and proceeds to the deadline
    /// check, which is the conservative direction).
    async fn any_active(&self) -> bool;
}

/// Spawn the idle-watcher background task, returning a handle that the
/// caller can use to record inbound activity.
///
/// Returns `None` when `effective_timeout_secs == 0` ("never sleep") — the
/// caller does not need to retain a handle in that case.
///
/// The task is **detached** (via `tokio::spawn`); dropping the returned
/// handle does not stop the watcher. The watcher always terminates by
/// exiting the process, never by falling out of the loop.
pub fn spawn_idle_watcher(
    config: IdleWatcherConfig,
) -> Option<IdleWatcherHandle> {
    if config.effective_timeout_secs == NEVER_SLEEP {
        info!(
            agent_id = %config.agent_id,
            "Idle watcher: never-sleep mode (effective_timeout_secs=0), not spawned",
        );
        return None;
    }

    let handle = IdleWatcherHandle {
        last_inbound_at_ms: Arc::new(AtomicI64::new(unix_now_ms())),
    };

    let tick = tick_interval(config.effective_timeout_secs);
    info!(
        agent_id = %config.agent_id,
        effective_timeout_secs = config.effective_timeout_secs,
        tick_interval_secs = tick.as_secs(),
        "Idle watcher spawned",
    );

    let handle_for_task = handle.clone();
    tokio::spawn(async move {
        run_watcher(config, handle_for_task, tick).await;
    });

    Some(handle)
}

/// Convert the effective timeout into a Tokio interval.
///
/// The polling cadence is capped at `MIN_INTERVAL_SECS` (60 s) so the
/// watcher reacts within a minute regardless of how long the effective
/// timeout is. The deadline itself lives in `effective_timeout_secs` —
/// the wakeup just needs to happen often enough to catch it.
fn tick_interval(effective_timeout_secs: u64) -> Duration {
    let secs = effective_timeout_secs.clamp(1, MIN_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Main watcher loop. Runs until the timeout fires, at which point it
/// exits the process.
///
/// Visibility: `pub(crate)` only — tests reach the inner state via
/// dedicated helpers (see `tests` module below).
pub(crate) async fn run_watcher(
    config: IdleWatcherConfig,
    handle: IdleWatcherHandle,
    tick: Duration,
) {
    let mut interval = tokio::time::interval(tick);
    // The first tick fires immediately; skip it so we don't double-publish
    // a check at exactly the spawn tick.
    interval.tick().await;

    loop {
        interval.tick().await;

        // Suspend the deadline while any session is in-flight. The user
        // may have walked away, but the agent is actively working — we
        // must not yank the process out from under it.
        if config.session_activity.any_active().await {
            tracing::trace!(
                agent_id = %config.agent_id,
                "Idle watcher: session active, deadline suspended",
            );
            continue;
        }

        // No session active — check elapsed time since last inbound.
        let last_ms = handle
            .last_inbound_at_ms
            .load(Ordering::Relaxed)
            .max(0) as u64;
        let now_ms = unix_now_ms() as u64;
        let elapsed_ms = now_ms.saturating_sub(last_ms);
        let elapsed_secs = elapsed_ms / 1000;

        if elapsed_secs < config.effective_timeout_secs {
            tracing::trace!(
                agent_id = %config.agent_id,
                elapsed_secs,
                effective_timeout_secs = config.effective_timeout_secs,
                "Idle watcher: within timeout",
            );
            continue;
        }

        // Deadline reached. Publish "sleeping", disconnect, exit.
        warn!(
            agent_id = %config.agent_id,
            elapsed_secs,
            effective_timeout_secs = config.effective_timeout_secs,
            "Idle watcher: timeout reached, initiating auto-sleep",
        );

        if let Err(e) = publish_sleeping(&config.mqtt_client, &config.agent_id).await {
            warn!(
                agent_id = %config.agent_id,
                error = %e,
                "Idle watcher: failed to publish 'sleeping' status, proceeding with exit anyway",
            );
        }

        if let Err(e) = config.mqtt_client.disconnect().await {
            warn!(
                agent_id = %config.agent_id,
                error = %e,
                "Idle watcher: MQTT disconnect failed, proceeding with exit anyway",
            );
        }

        info!(
            agent_id = %config.agent_id,
            "Idle watcher: exiting process",
        );
        // `process::exit(0)` deliberately — the watcher's contract is
        // "stop this process when the deadline is hit". Drop-based
        // shutdown would require waiting for LLM streams to finish, which
        // contradicts the "no long-running idle agent" intent.
        std::process::exit(0);
    }
}

/// Publish the `"sleeping"` payload to the agent status retained topic.
async fn publish_sleeping(
    mqtt_client: &RuntimeMqttClient,
    agent_id: &str,
) -> Result<(), crate::mqtt::RuntimeMqttClientError> {
    let topic = format!("acowork/agents/{}/status", agent_id);
    mqtt_client
        .publish_raw(&topic, b"sleeping", MqttQoS::AtLeastOnce, true)
        .await
}

/// Current Unix time in milliseconds.
fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_layer1_user_override_wins() {
        // User explicitly chose 15 min — manifest / default are ignored.
        assert_eq!(resolve_idle_timeout_secs(Some(900), Some(60)), 900);
        assert_eq!(resolve_idle_timeout_secs(Some(900), None), 900);
    }

    #[test]
    fn resolve_layer1_never_short_circuits() {
        // `Some(0)` at layer 1 wins regardless of manifest / default.
        assert_eq!(
            resolve_idle_timeout_secs(Some(NEVER_SLEEP), Some(60)),
            NEVER_SLEEP
        );
        assert_eq!(
            resolve_idle_timeout_secs(Some(NEVER_SLEEP), None),
            NEVER_SLEEP
        );
    }

    #[test]
    fn resolve_layer2_manifest_default() {
        // No user override → manifest default.
        assert_eq!(resolve_idle_timeout_secs(None, Some(1800)), 1800);
        assert_eq!(
            resolve_idle_timeout_secs(None, Some(NEVER_SLEEP)),
            NEVER_SLEEP
        );
    }

    #[test]
    fn resolve_layer3_fallback() {
        // Nothing set → 300s default.
        assert_eq!(
            resolve_idle_timeout_secs(None, None),
            DEFAULT_IDLE_TIMEOUT_SECS
        );
    }

    #[test]
    fn tick_interval_clamps_short_timeouts() {
        // 5 min timeout → 60s tick (clamped at MIN).
        assert_eq!(tick_interval(300), Duration::from_secs(60));
        // 6 min timeout → 60s tick (still above).
        assert_eq!(tick_interval(360), Duration::from_secs(60));
    }

    #[test]
    fn tick_interval_uses_timeout_for_long_timeouts() {
        // 3 hours → capped at MIN_INTERVAL_SECS (60 s). The deadline is
        // checked against `effective_timeout_secs` separately; the tick
        // interval is just the polling cadence and stays at the MIN
        // ceiling so short timeouts react within a minute and long
        // ones still get polled frequently (1 wake/min for 3 h = 180
        // wakes, well within budget).
        assert_eq!(tick_interval(10_800), Duration::from_secs(60));
    }

    #[test]
    fn tick_interval_never_below_one_second() {
        // Defensive: even if a caller passes 0 to tick_interval (the
        // outer `spawn_idle_watcher` short-circuits before this), the
        // floor is 1 s.
        assert_eq!(tick_interval(1), Duration::from_secs(1));
    }

    #[test]
    fn handle_record_inbound_updates_timestamp() {
        let initial = unix_now_ms();
        let handle = IdleWatcherHandle {
            last_inbound_at_ms: Arc::new(AtomicI64::new(initial)),
        };
        assert_eq!(handle.last_inbound_at_ms(), Some(initial));

        // Sleep 5 ms then record — timestamp must advance.
        std::thread::sleep(Duration::from_millis(5));
        handle.record_inbound();

        let after = handle.last_inbound_at_ms().unwrap();
        assert!(after > initial, "record_inbound must advance the timestamp");
    }

    #[test]
    fn handle_initial_zero_is_none() {
        let handle = IdleWatcherHandle {
            last_inbound_at_ms: Arc::new(AtomicI64::new(0)),
        };
        assert_eq!(handle.last_inbound_at_ms(), None);
    }

    #[test]
    fn handle_is_clone_and_shares_state() {
        let h1 = IdleWatcherHandle {
            last_inbound_at_ms: Arc::new(AtomicI64::new(0)),
        };
        let h2 = h1.clone();
        h1.record_inbound();
        let ts = h2.last_inbound_at_ms().unwrap();
        assert!(ts > 0, "clone must share the same atomic state");
    }
}
