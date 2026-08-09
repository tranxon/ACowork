//! Error types for acowork-grafeo

use thiserror::Error;

use acowork_core::error::AcoworkError;

#[derive(Debug, Error)]
pub enum GrafeoError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Database error: {0}")]
    Database(#[from] grafeo_common::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Memory error: {0}")]
    Memory(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Invalid embedding dimension: expected {expected}, got {got}")]
    InvalidDimension {
        /// Expected dimension (EMBEDDING_DIM).
        expected: usize,
        /// Actual dimension provided.
        got: usize,
    },
}

impl From<AcoworkError> for GrafeoError {
    fn from(e: AcoworkError) -> Self {
        GrafeoError::Memory(e.to_string())
    }
}

impl From<String> for GrafeoError {
    fn from(s: String) -> Self {
        GrafeoError::Parse(s)
    }
}

pub type Result<T> = std::result::Result<T, GrafeoError>;
