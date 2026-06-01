//! Application state shared across Tauri commands

use std::process::Child;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::gateway_client::GatewayClient;

/// Shared application state
pub struct AppState {
    /// Gateway HTTP client
    pub gateway: Arc<RwLock<GatewayClient>>,
    /// Handle to the locally spawned Gateway process (None when remote mode)
    pub gateway_process: Arc<Mutex<Option<Child>>>,
}

impl AppState {
    /// Create a new AppState with default Gateway URL
    pub fn new() -> Self {
        Self {
            gateway: Arc::new(RwLock::new(GatewayClient::new())),
            gateway_process: Arc::new(Mutex::new(None)),
        }
    }
}
