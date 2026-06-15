//! Application state shared across Tauri commands

use std::process::Child;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::gateway_client::GatewayClient;

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
        }
    }
}
