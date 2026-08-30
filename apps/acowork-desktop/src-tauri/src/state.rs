//! Application state shared across Tauri commands
//!
//! Holds shared, mutable application state accessible from any Tauri command.
//!
//! ## MQTT connection status (ADR-036 / ADR-039)
//!
//! MQTT connection state is intentionally NOT stored here.  The single
//! source of truth lives in `DesktopMqttClient::session_state` (a
//! `tokio::sync::watch` channel updated synchronously by the poll task's
//! `on_status` callback).  `get_mqtt_status` reads it directly via
//! `session_state().current()`, which means:
//!
//! - No cache to keep in sync with the watch channel
//! - No `tokio::spawn` indirection that could race the frontend's
//!   snapshot read after a webview reload
//! - One well-defined path for status transitions
//!
//! The `on_status` callback also emits a `mqtt-status` Tauri event for
//! real-time updates, but the event is best-effort: the frontend's
//! `initMqttListener` always calls `get_mqtt_status` to fetch the
//! authoritative snapshot, then subscribes for subsequent updates.

use std::process::Child;
use std::sync::Arc;
use tokio::sync::Mutex;

use serde::{Deserialize, Serialize};

use acowork_core::mqtt_proto::{BootstrapPhase, BootstrapState};

use crate::gateway_client::GatewayClient;
use crate::mqtt_client::SharedDesktopMqttClient;

#[cfg(target_os = "windows")]
use crate::win_job::JobHandle;

/// Gateway deployment mode, mirrors frontend `GatewayMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayMode {
    /// Local mode: Desktop App spawns a child Gateway process on the
    /// global default host:port (see `acowork_core::defaults::GATEWAY_HTTP_URL`).
    Local,
    /// Remote mode: Desktop App connects to a pre-existing Gateway at
    /// a user-configured URL (e.g. a Gateway running in WSL).
    Remote,
}

impl GatewayMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "remote" => GatewayMode::Remote,
            _ => GatewayMode::Local,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            GatewayMode::Local => "local",
            GatewayMode::Remote => "remote",
        }
    }
}

/// Proto-name of a bootstrap phase, e.g. `BOOTING`, `SHUTTING_DOWN`.
///
/// Mirrors the Gateway's `/api/bootstrap` projection (`http/bootstrap_api.rs`
/// `phase_name`): SCREAMING_SNAKE_CASE without the `BOOTSTRAP_PHASE_` prefix
/// that `BootstrapPhase::as_str_name()` produces.
fn phase_name(phase: BootstrapPhase) -> &'static str {
    match phase {
        BootstrapPhase::Unspecified => "UNSPECIFIED",
        BootstrapPhase::Booting => "BOOTING",
        BootstrapPhase::Ready => "READY",
        BootstrapPhase::Degraded => "DEGRADED",
        BootstrapPhase::Failed => "FAILED",
        BootstrapPhase::ShuttingDown => "SHUTTING_DOWN",
    }
}

/// Latest bootstrap snapshot, mirrored from the Gateway's retained
/// `acowork/global/bootstrap` MQTT topic / `GET /api/bootstrap` projection.
///
/// Field-for-field the wire-level `BootstrapState` proto (ADR-059 §5.4.4).
/// `phase` is the SCREAMING_SNAKE_CASE name of the proto enum, matching the
/// Gateway's HTTP JSON projection exactly so Desktop and Gateway agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapStateView {
    pub protocol_version: u32,
    pub instance_id: String,
    pub version: u64,
    pub phase: String,
    pub phase_detail: String,
    pub issued_at_ms: u64,
}

impl BootstrapStateView {
    /// Convert a decoded `BootstrapState` proto (MQTT retained payload).
    pub fn from_proto(proto: &BootstrapState) -> Self {
        Self {
            protocol_version: proto.protocol_version,
            instance_id: proto.instance_id.clone(),
            version: proto.version,
            phase: phase_name(BootstrapPhase::try_from(proto.phase).unwrap_or(BootstrapPhase::Unspecified))
                .to_string(),
            phase_detail: proto.phase_detail.clone(),
            issued_at_ms: proto.issued_at_ms,
        }
    }
}

/// Shared application state
pub struct AppState {
    /// Gateway HTTP client. `base_url` reflects the active configuration:
    ///   - Local mode  → `acowork_core::defaults::GATEWAY_HTTP_URL`
    ///   - Remote mode → user-configured URL
    pub gateway: Arc<tokio::sync::RwLock<GatewayClient>>,
    /// Active deployment mode. Set by `set_gateway_config` (called from frontend).
    pub gateway_mode: Arc<tokio::sync::RwLock<GatewayMode>>,
    /// Handle to the locally spawned Gateway process (None in remote mode
    /// or before `init_local_gateway` is called).
    pub gateway_process: Arc<Mutex<Option<Child>>>,
    /// Windows Job Object that automatically kills the Gateway process tree
    /// when the desktop app exits (any exit path: Ctrl+C, crash, kill).
    /// On non-Windows platforms this field does not exist.
    #[cfg(target_os = "windows")]
    pub gateway_job: Arc<Mutex<Option<JobHandle>>>,

    /// ADR-033 Phase 3: Desktop MQTT client for real-time events.
    /// Connected after the Gateway is confirmed healthy. None until
    /// `connect_mqtt` is called from the frontend.
    ///
    /// Connection state lives inside the client (`session_state()`); see
    /// module docs.
    pub mqtt_client: Arc<Mutex<Option<SharedDesktopMqttClient>>>,

    /// ADR-059: latest Gateway bootstrap snapshot.
    ///
    /// Updated by the MQTT `bootstrap_handler` on every push of the
    /// retained `acowork/global/bootstrap` topic (including re-delivery on
    /// reconnect). `get_bootstrap()` falls back to a one-shot HTTP
    /// `GET /api/bootstrap` when this is `None` (e.g. MQTT not yet
    /// connected) and caches the result here.
    pub bootstrap_state: Arc<tokio::sync::RwLock<Option<BootstrapStateView>>>,
}

impl AppState {
    /// Create a new AppState. Initial defaults:
    ///   - mode = Local (matches the pre-bug UX where Rust spawned a local
    ///     gateway immediately; the frontend must call `set_gateway_config`
    ///     on startup to switch to Remote if needed)
    ///   - base_url = acowork_core::defaults::GATEWAY_HTTP_URL
    pub fn new() -> Self {
        Self {
            gateway: Arc::new(tokio::sync::RwLock::new(GatewayClient::new())),
            gateway_mode: Arc::new(tokio::sync::RwLock::new(GatewayMode::Local)),
            gateway_process: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "windows")]
            gateway_job: Arc::new(Mutex::new(None)),
            mqtt_client: Arc::new(Mutex::new(None)),
            bootstrap_state: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }
}

/// Last-resort cleanup: kill the local Gateway process tree when AppState is
/// dropped (e.g. on Ctrl+C termination, OS shutdown, or forced exit).
///
/// This is a safety net — the tray "quit" handler and `RunEvent::Exit` handler
/// in lib.rs normally kill the Gateway before Drop fires in normal shutdown.
/// But on abrupt termination (Ctrl+C in dev mode, `taskkill` of the Tauri
/// process), Rust's stack unwind will run this Drop and prevent orphaned
/// Gateway / Runtime / Embed processes from lingering.
impl Drop for AppState {
    fn drop(&mut self) {
        // Only try to lock if the mutex isn't poisoned. During unwind from
        // a panic, the mutex may be poisoned.
        if let Ok(mut proc) = self.gateway_process.try_lock()
            && let Some(mut child) = proc.take()
        {
            let pid = child.id();
            tracing::info!(pid = pid, "AppState dropped, killing Gateway process tree");
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .output();
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-INT", &pid.to_string()])
                    .output();
            }
            let _ = child.wait();
        }
        // On Windows, drop the Job Object handle so KILL_ON_JOB_CLOSE fires.
        // On abrupt exit (Ctrl+C) this Drop may not run, but the OS closes
        // all handles anyway, triggering the same cleanup.
        #[cfg(target_os = "windows")]
        {
            if let Ok(mut job) = self.gateway_job.try_lock() {
                *job = None;
            }
        }
    }
}
