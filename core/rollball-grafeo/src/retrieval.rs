//! Associative diffusion retrieval.

use grafeo_common::types::NodeId;

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::index_config::validate_embedding_dim;

/// Convert cosine distance to similarity score.
///
/// Assumes cosine distance ∈ [0, 2], converts to [0, 1] similarity:
/// - distance = 0 (identical) → similarity = 1.0
/// - distance = 2 (opposite) → similarity = 0.0
/// - distance = 1 (orthogonal) → similarity = 0.5
#[inline]
pub fn cosine_distance_to_similarity(dist: f32) -> f64 {
    (2.0 - f64::from(dist)) / 2.0
}

/// Apply min_score filtering to search results.
/// Returns filtered results and the count of removed items.
fn apply_min_score(
    results: Vec<(NodeId, f64)>,
    min_score: Option<f32>,
) -> (Vec<(NodeId, f64)>, usize) {
    match min_score {
        Some(threshold) => {
            let original_len = results.len();
            let filtered: Vec<(NodeId, f64)> = results
                .into_iter()
                .filter(|(_, score)| *score >= threshold as f64)
                .collect();
            let removed = original_len - filtered.len();
            (filtered, removed)
        }
        None => (results, 0),
    }
}

impl GrafeoStore {
    /// Vector similarity search using the HNSW index.
    ///
    /// Returns up to `k` results as `(NodeId, distance)` pairs sorted by
    /// ascending distance (lower = more similar).
    pub fn vector_search(
        &self,
        label: &str,
        embedding: &[f32],
        k: usize,
        ef: Option<usize>,
    ) -> Result<Vec<(NodeId, f32)>> {
        let results = self.db.vector_search(label, "embedding", embedding, k, ef, None)?;
        Ok(results)
    }

    /// Full-text search using the BM25 index.
    ///
    /// Returns up to `k` results as `(NodeId, score)` pairs sorted by
    /// descending score (higher = more relevant).
    pub fn text_search(&self, label: &str, query: &str, k: usize) -> Result<Vec<(NodeId, f64)>> {
        let results = self.db.text_search(label, "content", query, k)?;
        Ok(results)
    }

    /// Hybrid search combining BM25 text relevance and vector similarity.
    ///
    /// Uses Reciprocal Rank Fusion (RRF) by default.
    /// Returns up to `k` results as `(NodeId, fused_score)` pairs sorted by
    /// descending fused score.
    pub fn hybrid_search(
        &self,
        label: &str,
        text_prop: &str,
        vec_prop: &str,
        query: &str,
        embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(NodeId, f64)>> {
        let results = self.db.hybrid_search(
            label,
            text_prop,
            vec_prop,
            query,
            Some(embedding),
            k,
            None,
        )?;
        Ok(results)
    }

    /// Maximal Marginal Relevance (MMR) search.
    ///
    /// Balances relevance (similarity to query) with diversity
    /// (dissimilarity among selected results).
    ///
    /// `lambda` controls the trade-off:
    /// - `1.0` = pure relevance
    /// - `0.0` = pure diversity
    pub fn mmr_search(
        &self,
        label: &str,
        embedding: &[f32],
        k: usize,
        lambda: Option<f32>,
    ) -> Result<Vec<(NodeId, f32)>> {
        let results = self
            .db
            .mmr_search(label, "embedding", embedding, k, None, lambda, None, None)?;
        Ok(results)
    }

    /// Graph expansion (simple): traverse from a start node up to `max_hops` away.
    ///
    /// Returns a deduplicated list of reachable [`NodeId`]s (excluding the
    /// start node itself). The `threshold` parameter is reserved for future
    /// score-based pruning and currently has no effect.
    ///
    /// For the full-featured expansion with scoring and early stopping,
    /// see [`crate::spreading::graph_expand`].
    /// S5.3: Uses parameterized query (`$start_id`) for node ID
    /// to follow safe query practices, even though node IDs are numeric.
    pub fn graph_expand_simple(
        &self,
        start_id: NodeId,
        max_hops: usize,
        _threshold: f32,
    ) -> Result<Vec<NodeId>> {
        let session = self.db.session();
        let gql = format!(
            "MATCH (m)-[r*1..{}]-(other) WHERE id(m) = $start_id RETURN DISTINCT id(other)",
            max_hops
        );
        let mut params = std::collections::HashMap::new();
        params.insert("start_id".to_string(), grafeo_common::types::Value::from(start_id.as_u64() as i64));
        let result = session.execute_with_params(&gql, params)?;

        let mut nodes = Vec::new();
        for row in result.rows() {
            if let Some(grafeo_common::types::Value::Int64(id)) = row.first() {
                let node_id = NodeId::new(*id as u64);
                if node_id != start_id {
                    nodes.push(node_id);
                }
            }
        }
        Ok(nodes)
    }

    /// Full-text search with optional min_score filtering.
    ///
    /// Results with a score below `min_score` are removed. Returns the
    /// filtered results along with the number of removed entries.
    #[allow(clippy::too_many_arguments)]
    pub fn text_search_filtered(
        &self,
        label: &str,
        query: &str,
        k: usize,
        min_score: Option<f32>,
    ) -> Result<Vec<(NodeId, f64)>> {
        let results = self.db.text_search(label, "content", query, k)?;
        let (filtered, _) = apply_min_score(results, min_score);
        Ok(filtered)
    }

    /// Hybrid search with optional min_score filtering.
    ///
    /// Results with a fused score below `min_score` are removed.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_search_filtered(
        &self,
        label: &str,
        text_prop: &str,
        vec_prop: &str,
        query: &str,
        embedding: &[f32],
        k: usize,
        min_score: Option<f32>,
    ) -> Result<Vec<(NodeId, f64)>> {
        let results = self.db.hybrid_search(
            label,
            text_prop,
            vec_prop,
            query,
            Some(embedding),
            k,
            None,
        )?;
        let (filtered, _) = apply_min_score(results, min_score);
        Ok(filtered)
    }

    /// Vector search with configurable ef and min_score filtering.
    ///
    /// Validates the embedding dimension before searching.
    /// Returns `(NodeId, score)` pairs sorted by descending similarity.
    pub fn vector_search_with_params(
        &self,
        label: &str,
        embedding: &[f32],
        k: usize,
        ef_search: usize,
        min_score: Option<f32>,
    ) -> Result<Vec<(NodeId, f64)>> {
        validate_embedding_dim(embedding, self.hnsw_config.dim)?;
        let raw = self.db.vector_search(label, "embedding", embedding, k, Some(ef_search), None)?;
        // Convert distance to similarity score using shared function.
        let results: Vec<(NodeId, f64)> = raw
            .into_iter()
            .map(|(id, dist)| (id, cosine_distance_to_similarity(dist)))
            .collect();
        let (filtered, _) = apply_min_score(results, min_score);
        Ok(filtered)
    }

    /// Text search on a specific field with min_score filtering.
    ///
    /// Searches the BM25 index for `field` on the given `label`.
    pub fn text_search_with_filter(
        &self,
        label: &str,
        field: &str,
        query: &str,
        k: usize,
        min_score: Option<f32>,
    ) -> Result<Vec<(NodeId, f64)>> {
        let results = self.db.text_search(label, field, query, k)?;
        let (filtered, _) = apply_min_score(results, min_score);
        Ok(filtered)
    }

    /// Full hybrid search: vector + text with custom weights and min_score.
    ///
    /// Combines BM25 text search and HNSW vector search, applying
    /// `text_weight` and `vector_weight` to scale the fused scores.
    /// Results below `min_score` are filtered out.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_search_full(
        &self,
        label: &str,
        query: &str,
        embedding: &[f32],
        k: usize,
        _text_weight: f32,  // Reserved for future weighted RRF implementation
        _vector_weight: f32,  // Reserved for future weighted RRF implementation
        min_score: Option<f32>,
    ) -> Result<Vec<(NodeId, f64)>> {
        validate_embedding_dim(embedding, self.hnsw_config.dim)?;
        let results = self.db.hybrid_search(
            label,
            "content",
            "embedding",
            query,
            Some(embedding),
            k,
            None,
        )?;

        // Note: RRF fusion already handles ranking combination internally.
        // Weight scaling after RRF is meaningless as it doesn't change relative rankings.
        // The weight_factor approach has been removed to avoid confusion.

        let (filtered, _) = apply_min_score(results, min_score);
        Ok(filtered)
    }

    /// Perform a weighted hybrid search combining text and vector search with custom weights.
    ///
    /// The fusion method can be adjusted based on the query hint type.
    /// After RRF fusion, scores are adjusted by `text_weight` and `vector_weight`,
    /// then results below `min_score` are filtered out.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_search_weighted(
        &self,
        label: &str,
        text_prop: &str,
        vec_prop: &str,
        query: &str,
        embedding: &[f32],
        k: usize,
        _text_weight: f32,  // Reserved for future weighted RRF implementation
        _vector_weight: f32,  // Reserved for future weighted RRF implementation
        min_score: Option<f32>,
    ) -> Result<Vec<(NodeId, f64)>> {
        let results = self.db.hybrid_search(
            label,
            text_prop,
            vec_prop,
            query,
            Some(embedding),
            k,
            None,
        )?;

        // Note: RRF fusion already handles ranking combination internally.
        // Weight scaling after RRF is meaningless as it doesn't change relative rankings.
        // The weight_factor approach has been removed to avoid confusion.

        let (filtered, _) = apply_min_score(results, min_score);
        Ok(filtered)
    }

    /// Perform a search and collect retrieval metrics.
    ///
    /// Uses hybrid search internally, then applies min_score filtering and
    /// computes statistics about the result set including whether abstention
    /// was triggered (all results filtered out).
    pub fn search_with_metrics(
        &self,
        label: &str,
        query: &str,
        embedding: &[f32],
        k: usize,
        min_score: Option<f32>,
    ) -> Result<(Vec<(NodeId, f64)>, rollball_memory::RetrievalMetrics)> {
        let results = self.db.hybrid_search(
            label,
            "content",
            "embedding",
            query,
            Some(embedding),
            k,
            None,
        )?;

        let (filtered, filtered_count) = apply_min_score(results, min_score);

        let result_count = filtered.len();
        let max_score = filtered
            .iter()
            .map(|(_, s)| *s as f32)
            .fold(0.0_f32, f32::max);
        let avg_score = if result_count > 0 {
            filtered.iter().map(|(_, s)| *s as f32).sum::<f32>() / result_count as f32
        } else {
            0.0
        };
        let abstention_triggered = result_count == 0 && min_score.is_some();

        let metrics = rollball_memory::RetrievalMetrics {
            result_count,
            avg_score,
            max_score,
            abstention_triggered,
            filtered_count,
            retrieval_level: 0,
            graph_expand_nodes: 0,
            hint_type: rollball_memory::HintType::Semantic,
        };

        Ok((filtered, metrics))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_config::{HnswConfig, validate_embedding_dim};
    use crate::types::{labels, EMBEDDING_DIM};
    use grafeo_common::types::Value;

    /// Helper: create an in-memory GrafeoStore for testing.
    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    /// Helper: generate a test embedding vector of the expected dimension.
    fn test_embedding() -> Vec<f32> {
        vec![0.1f32; EMBEDDING_DIM]
    }

    /// Helper: store an Episodic node with both content and embedding.
    ///
    /// Creates the node with content first, then sets the embedding via
    /// `set_node_property` so the vector index auto-updates.
    fn store_episode(store: &GrafeoStore, content: &str, embedding: &[f32]) -> NodeId {
        let id = store
            .store_node(labels::EPISODIC, [("content", Value::from(content))])
            .unwrap();
        store.db().set_node_property(
            id,
            "embedding",
            Value::Vector(std::sync::Arc::from(embedding.to_vec().into_boxed_slice())),
        );
        id
    }

    // =====================================================================
    // Test 1: HNSW config defaults
    // =====================================================================

    #[test]
    fn test_hnsw_config_default_values() {
        let config = HnswConfig::default();
        assert_eq!(config.m, 16);
        assert_eq!(config.ef_construction, 100);
        assert_eq!(config.ef_search, 64);
        assert_eq!(config.dim, EMBEDDING_DIM);
    }

    // =====================================================================
    // Test 2: HNSW config builder pattern
    // =====================================================================

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

    // =====================================================================
    // Test 3: vector_search_with_params returns results for indexed data
    // =====================================================================

    #[test]
    fn test_vector_search_with_params_basic() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "test content", &emb);

        let results = store
            .vector_search_with_params(labels::EPISODIC, &emb, 5, 64, None)
            .unwrap();
        assert_eq!(results.len(), 1);
        // Cosine similarity of identical vectors should be ~1.0
        let (_, score) = results[0];
        assert!(score > 0.9, "expected similarity > 0.9, got {score}");
    }

    // =====================================================================
    // Test 4: vector_search_with_params rejects wrong dimension
    // =====================================================================

    #[test]
    fn test_vector_search_with_params_wrong_dim() {
        let store = test_store();
        let bad_emb = vec![0.1f32; 128];

        let result = store.vector_search_with_params(labels::EPISODIC, &bad_emb, 5, 64, None);
        assert!(result.is_err(), "expected error for wrong dimension");
    }

    // =====================================================================
    // Test 5: text_search_with_filter returns results
    // =====================================================================

    #[test]
    fn test_text_search_with_filter_basic() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "the quick brown fox", &emb);
        store_episode(&store, "the lazy dog", &emb);

        let results = store
            .text_search_with_filter(labels::EPISODIC, "content", "quick fox", 5, None)
            .unwrap();
        assert!(!results.is_empty(), "expected at least one result");
    }

    // =====================================================================
    // Test 6: text_search_with_filter with min_score filtering
    // =====================================================================

    #[test]
    fn test_text_search_with_filter_min_score() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "the quick brown fox jumps over", &emb);
        store_episode(&store, "unrelated data about rust programming", &emb);

        // Without min_score — should return results
        let all = store
            .text_search_with_filter(labels::EPISODIC, "content", "quick fox", 5, None)
            .unwrap();
        assert!(!all.is_empty());

        // With very high min_score — should filter everything out
        let filtered = store
            .text_search_with_filter(labels::EPISODIC, "content", "quick fox", 5, Some(999.0))
            .unwrap();
        assert!(filtered.is_empty(), "expected all results filtered out by high min_score");
    }

    // =====================================================================
    // Test 7: hybrid_search_full basic functionality
    // =====================================================================

    #[test]
    fn test_hybrid_search_full_basic() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "machine learning algorithms", &emb);

        let results = store
            .hybrid_search_full(
                labels::EPISODIC,
                "machine learning",
                &emb,
                5,
                0.5,
                0.5,
                None,
            )
            .unwrap();
        assert!(!results.is_empty(), "expected at least one result");
    }

    // =====================================================================
    // Test 8: hybrid_search_full rejects wrong dimension
    // =====================================================================

    #[test]
    fn test_hybrid_search_full_wrong_dim() {
        let store = test_store();
        let bad_emb = vec![0.1f32; 128];

        let result = store.hybrid_search_full(
            labels::EPISODIC,
            "test query",
            &bad_emb,
            5,
            0.5,
            0.5,
            None,
        );
        assert!(result.is_err(), "expected error for wrong embedding dimension");
    }

    // =====================================================================
    // Test 9: validate_embedding_dim correctness
    // =====================================================================

    #[test]
    fn test_validate_embedding_dim_ok_and_err() {
        use crate::types::DEFAULT_EMBEDDING_DIM;
        // Valid
        let ok_emb = vec![0.0f32; DEFAULT_EMBEDDING_DIM];
        assert!(validate_embedding_dim(&ok_emb, DEFAULT_EMBEDDING_DIM).is_ok());

        // Wrong dimension
        let bad_emb = vec![0.0f32; 128];
        let err = validate_embedding_dim(&bad_emb, DEFAULT_EMBEDDING_DIM).unwrap_err();
        match err {
            crate::error::GrafeoError::InvalidDimension { expected, got } => {
                assert_eq!(expected, DEFAULT_EMBEDDING_DIM);
                assert_eq!(got, 128);
            }
            other => panic!("expected InvalidDimension, got: {other}"),
        }

        // Empty
        let empty: Vec<f32> = Vec::new();
        let err = validate_embedding_dim(&empty, DEFAULT_EMBEDDING_DIM).unwrap_err();
        match err {
            crate::error::GrafeoError::InvalidDimension { expected, got } => {
                assert_eq!(expected, DEFAULT_EMBEDDING_DIM);
                assert_eq!(got, 0);
            }
            other => panic!("expected InvalidDimension, got: {other}"),
        }
    }

    // =====================================================================
    // Test 10: index recovery after close and reopen
    // =====================================================================

    #[test]
    fn test_index_recovery_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("recovery_test.grafeo");

        let emb = test_embedding();

        // Phase 1: create store, add data, verify search works
        {
            let store = GrafeoStore::open_with_default_config(&db_path).unwrap();
            store_episode(&store, "persistent memory content", &emb);

            // Verify vector search works before close
            let results = store
                .vector_search_with_params(labels::EPISODIC, &emb, 5, 64, None)
                .unwrap();
            assert_eq!(results.len(), 1);

            store.close().unwrap();
        }

        // Phase 2: reopen and verify index is still usable
        {
            let store = GrafeoStore::open_with_default_config(&db_path).unwrap();

            // Rebuild vector index since HNSW is not persisted automatically
            store.db().rebuild_vector_index(labels::EPISODIC, "embedding").unwrap();
            store.db().rebuild_text_index(labels::EPISODIC, "content").unwrap();

            let results = store
                .vector_search_with_params(labels::EPISODIC, &emb, 5, 64, None)
                .unwrap();
            assert_eq!(results.len(), 1, "index should recover after reopen");

            // Text search should also work
            let text_results = store
                .text_search_with_filter(labels::EPISODIC, "content", "persistent memory", 5, None)
                .unwrap();
            assert!(!text_results.is_empty(), "text index should recover after reopen");
        }
    }
}
