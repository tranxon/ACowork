//! Error types for rollball-grafeo

use thiserror::Error;

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
}

pub type Result<T> = std::result::Result<T, GrafeoError>;
