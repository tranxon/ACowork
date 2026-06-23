//! Error types for acowork-runtime
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Core error: {0}")]
    Core(#[from] acowork_core::AcoworkError),

    #[error("Provider error: {0}")]
    Provider(acowork_core::providers::ProviderError),

    #[error("Stream error: {0}")]
    StreamError(acowork_core::providers::StreamError),

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
    Manifest(#[from] acowork_core::manifest::ManifestError),

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

impl RuntimeError {
    /// Extract user-friendly error info as `(user_message, detail, error_type)`.
    ///
    /// - `user_message`: short, readable summary for default frontend display
    /// - `detail`: raw error string for the expandable "Details" section
    /// - `error_type`: stringified `ProviderErrorType` for conditional rendering
    pub fn error_info(&self) -> (String, String, String) {
        match self {
            RuntimeError::Provider(pe) => {
                let user_message = if pe.user_message.is_empty() {
                    pe.message.clone()
                } else {
                    pe.user_message.clone()
                };
                (
                    user_message,
                    pe.message.clone(),
                    format!("{:?}", pe.error_type),
                )
            }
            RuntimeError::Core(acowork_core::AcoworkError::Provider(pe)) => {
                let user_message = if pe.user_message.is_empty() {
                    pe.message.clone()
                } else {
                    pe.user_message.clone()
                };
                (
                    user_message,
                    pe.message.clone(),
                    format!("{:?}", pe.error_type),
                )
            }
            RuntimeError::StreamError(se) => {
                let user_message = acowork_core::ProviderError::compute_user_message(
                    &se.error_type,
                    None,
                );
                (
                    user_message,
                    se.message.clone(),
                    format!("{:?}", se.error_type),
                )
            }
            RuntimeError::ContextOverflow(msg) => {
                (
                    "Context too long. History compressed.".to_string(),
                    msg.clone(),
                    "ContextOverflow".to_string(),
                )
            }
            RuntimeError::BudgetExceeded(msg) => {
                (
                    "Budget exceeded.".to_string(),
                    msg.clone(),
                    "BudgetExceeded".to_string(),
                )
            }
            _ => {
                let detail = self.to_string();
                (
                    "Unexpected error. See details.".to_string(),
                    detail,
                    "Unknown".to_string(),
                )
            }
        }
    }
}
