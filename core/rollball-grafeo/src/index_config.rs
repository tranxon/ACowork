//! HNSW index configuration and embedding validation.
//!
//! Provides [`HnswConfig`] for tuning HNSW vector index parameters and
//! [`validate_embedding_dim`] for checking embedding vector dimensions
//! against the expected index dimension.

use crate::error::{GrafeoError, Result};
use crate::types::DEFAULT_EMBEDDING_DIM;

// ---------------------------------------------------------------------------
// HNSW configuration
// ---------------------------------------------------------------------------

/// Default HNSW M parameter (connections per layer).
pub const HNSW_DEFAULT_M: usize = 16;

/// Default HNSW ef_construction parameter (build-time beam width).
pub const HNSW_DEFAULT_EF_CONSTRUCTION: usize = 100;

/// Default HNSW ef_search parameter (query-time beam width).
pub const HNSW_DEFAULT_EF_SEARCH: usize = 64;

// ---------------------------------------------------------------------------
// BM25 indexed fields
// ---------------------------------------------------------------------------

/// Text fields indexed for Episodic nodes.
pub const EPISODIC_TEXT_FIELDS: &[&str] = &["content"];

/// Text fields indexed for Knowledge nodes.
pub const KNOWLEDGE_TEXT_FIELDS: &[&str] = &["subject", "object"];

/// Distance metric used for all vector indexes.
pub const VECTOR_METRIC: &str = "cosine";

/// HNSW index configuration parameters.
///
/// Controls the trade-off between recall, latency, and memory usage of
/// approximate nearest-neighbor search. These values are passed to
/// [`GrafeoDB::create_vector_index`](grafeo_engine::GrafeoDB::create_vector_index).
#[derive(Debug, Clone)]
pub struct HnswConfig {
    /// Maximum number of bi-directional links per node at layers > 0.
    ///
    /// Higher M = better recall, more memory, slower construction.
    /// Default: 16.
    pub m: usize,

    /// Search beam width during index construction.
    ///
    /// Higher values = better index quality, slower construction.
    /// Default: 100.
    pub ef_construction: usize,

    /// Default search beam width during queries.
    ///
    /// Higher values = better recall, higher latency.
    /// Default: 64.
    pub ef_search: usize,

    /// Expected vector dimensionality.
    ///
    /// Must match the embedding model output.  Default: 384.
    /// Actual dimension depends on the active embedding provider
    /// and is configured via [`GrafeoConfig::embedding_dim`].
    pub dim: usize,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            m: HNSW_DEFAULT_M,
            ef_construction: HNSW_DEFAULT_EF_CONSTRUCTION,
            ef_search: HNSW_DEFAULT_EF_SEARCH,
            dim: DEFAULT_EMBEDDING_DIM,
        }
    }
}

impl HnswConfig {
    /// Create a new config with the given dimension and defaults for all other params.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            ..Self::default()
        }
    }

    /// Set the M (connections per layer) parameter.
    pub fn with_m(mut self, m: usize) -> Self {
        self.m = m;
        self
    }

    /// Set the ef_construction (build-time beam width) parameter.
    pub fn with_ef_construction(mut self, ef_construction: usize) -> Self {
        self.ef_construction = ef_construction;
        self
    }

    /// Set the ef_search (query-time beam width) parameter.
    pub fn with_ef_search(mut self, ef_search: usize) -> Self {
        self.ef_search = ef_search;
        self
    }
}

// ---------------------------------------------------------------------------
// Embedding validation
// ---------------------------------------------------------------------------

/// Validate that an embedding vector has the expected dimension.
///
/// The expected dimension is provided by the caller (typically from the
/// active embedding provider), defaulting to [`DEFAULT_EMBEDDING_DIM`].
/// Returns [`GrafeoError::InvalidDimension`] if the length does not match.
pub fn validate_embedding_dim(embedding: &[f32], expected_dim: usize) -> Result<()> {
    if embedding.len() != expected_dim {
        return Err(GrafeoError::InvalidDimension {
            expected: expected_dim,
            got: embedding.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hnsw_config_defaults() {
        let config = HnswConfig::default();
        assert_eq!(config.m, 16);
        assert_eq!(config.ef_construction, 100);
        assert_eq!(config.ef_search, 64);
        assert_eq!(config.dim, DEFAULT_EMBEDDING_DIM);
    }

    #[test]
    fn test_hnsw_config_builder() {
        let config = HnswConfig::new(768)
            .with_m(32)
            .with_ef_construction(200)
            .with_ef_search(128);
        assert_eq!(config.m, 32);
        assert_eq!(config.ef_construction, 200);
        assert_eq!(config.ef_search, 128);
        assert_eq!(config.dim, 768);
    }

    #[test]
    fn test_validate_embedding_dim_ok() {
        let dim = 384;
        let embedding = vec![0.0f32; dim];
        assert!(validate_embedding_dim(&embedding, dim).is_ok());
    }

    #[test]
    fn test_validate_embedding_dim_wrong() {
        let dim = 384;
        let embedding = vec![0.0f32; 128];
        let err = validate_embedding_dim(&embedding, dim).unwrap_err();
        match err {
            GrafeoError::InvalidDimension { expected, got } => {
                assert_eq!(expected, dim);
                assert_eq!(got, 128);
            }
            other => panic!("expected InvalidDimension, got: {other}"),
        }
    }

    #[test]
    fn test_validate_embedding_dim_empty() {
        let dim = 384;
        let embedding: Vec<f32> = Vec::new();
        let err = validate_embedding_dim(&embedding, dim).unwrap_err();
        match err {
            GrafeoError::InvalidDimension { expected, got } => {
                assert_eq!(expected, dim);
                assert_eq!(got, 0);
            }
            other => panic!("expected InvalidDimension, got: {other}"),
        }
    }
}
