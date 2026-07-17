//! Application state shared across Tauri commands
//!
//! Holds shared, mutable application state accessible from any Tauri command.

use std::process::Child;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::gateway_client::GatewayClient;
use crate::mqtt_client::{MqttStatus, SharedDesktopMqttClient};

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

/// Shared application state
pub struct AppState {
    /// Gateway HTTP client. `base_url` reflects the active configuration:
    ///   - Local mode  → `acowork_core::defaults::GATEWAY_HTTP_URL`
    ///   - Remote mode → user-configured URL
    pub gateway: Arc<RwLock<GatewayClient>>,
    /// Active deployment mode. Set by `set_gateway_config` (called from frontend).
    pub gateway_mode: Arc<RwLock<GatewayMode>>,
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
    pub mqtt_client: Arc<Mutex<Option<SharedDesktopMqttClient>>>,

    /// ADR-036: last observed MQTT broker connection status.
    ///
    /// `None`  — the poll task has not yet observed any state transition
    ///           (initial state, before first ConnAck / Disconnect).
    /// `Some`  — the most recent transition observed by the poll task.
    ///
    /// This three-valued slot is the source of truth.  `get_mqtt_status`
    /// returns it synchronously so the frontend can recover the initial
    /// state without racing the `mqtt-status` Tauri event.
    pub last_mqtt_status: Arc<RwLock<Option<MqttStatus>>>,
}

impl AppState {
    /// Create a new AppState. Initial defaults:
    ///   - mode = Local (matches the pre-bug UX where Rust spawned a local
    ///     gateway immediately; the frontend must call `set_gateway_config`
    ///     on startup to switch to Remote if needed)
    ///   - base_url = acowork_core::defaults::GATEWAY_HTTP_URL
    pub fn new() -> Self {
        Self {
            gateway: Arc::new(RwLock::new(GatewayClient::new())),
            gateway_mode: Arc::new(RwLock::new(GatewayMode::Local)),
            gateway_process: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "windows")]
            gateway_job: Arc::new(Mutex::new(None)),
            mqtt_client: Arc::new(Mutex::new(None)),
            // ADR-036: starts as `None` (= "unknown / not yet observed").
            // The poll task will overwrite this with the first ConnAck
            // (or the first failed poll → Disconnected).  Distinguishing
            // "unknown" from "disconnected" lets the frontend avoid
            // flashing the disconnected banner on cold start.
            last_mqtt_status: Arc::new(RwLock::new(None)),
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
