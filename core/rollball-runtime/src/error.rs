//! Error types for rollball-runtime

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Core error: {0}")]
    Core(#[from] rollball_core::RollballError),

    #[error("Provider error: {0}")]
    Provider(rollball_core::providers::ProviderError),

    #[error("Stream error: {0}")]
    StreamError(rollball_core::providers::StreamError),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("Package error: {0}")]
    Package(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Budget exceeded: {0}")]
    BudgetExceeded(String),

    #[error("Loop detected: {0}")]
    LoopDetected(String),

    #[error("Context overflow: {0}")]
    ContextOverflow(String),

    #[error("Manifest error: {0}")]
    Manifest(#[from] rollball_core::manifest::ManifestError),

    #[error("Sign error: {0}")]
    Sign(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Tool timeout: {0}")]
    ToolTimeout(String),

    #[error("WASM error: {0}")]
    Wasm(String),

    #[error("WASM fuel exhausted: {0}")]
    WasmFuelExhausted(String),

    #[error("WASM memory limit exceeded: {0}")]
    WasmMemoryLimit(String),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;
