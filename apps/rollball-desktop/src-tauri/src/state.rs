//! Application state shared across Tauri commands

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::gateway_client::GatewayClient;

/// Shared application state
pub struct AppState {
    /// Gateway HTTP client
    pub gateway: Arc<RwLock<GatewayClient>>,
}

impl AppState {
    /// Create a new AppState with default Gateway URL
    pub fn new() -> Self {
        Self {
            gateway: Arc::new(RwLock::new(GatewayClient::new())),
        }
    }
}
