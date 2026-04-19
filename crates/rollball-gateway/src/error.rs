//! Error types for rollball-gateway

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Core error: {0}")]
    Core(#[from] rollball_core::RollballError),

    #[error("Sign error: {0}")]
    Sign(String),

    #[error("Vault error: {0}")]
    Vault(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("Lifecycle error: {0}")]
    Lifecycle(String),

    #[error("Package error: {0}")]
    Package(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    #[error("Agent already running: {0}")]
    AgentAlreadyRunning(String),

    #[error("Agent not running: {0}")]
    AgentNotRunning(String),

    #[error("Signature verification failed: {0}")]
    SignatureFailed(String),
}

pub type Result<T> = std::result::Result<T, GatewayError>;
