//! Associative diffusion retrieval.

use grafeo_common::types::{NodeId, Value};

use crate::error::Result;
use crate::grafeo::GrafeoStore;

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

    /// Graph expansion: traverse from a start node up to `max_hops` away.
    ///
    /// Returns a deduplicated list of reachable [`NodeId`]s (excluding the
    /// start node itself). The `threshold` parameter is reserved for future
    /// score-based pruning and currently has no effect.
    pub fn graph_expand(
        &self,
        start_id: NodeId,
        max_hops: usize,
        _threshold: f32,
    ) -> Result<Vec<NodeId>> {
        let session = self.db.session();
        let gql = format!(
            "MATCH (m)-[r*1..{}]-(other) WHERE id(m) = {} RETURN DISTINCT id(other)",
            max_hops,
            start_id.as_u64()
        );
        let result = session.execute(&gql)?;

        let mut nodes = Vec::new();
        for row in result.rows() {
            if let Some(Value::Int64(id)) = row.first() {
                let node_id = NodeId::new(*id as u64);
                if node_id != start_id {
                    nodes.push(node_id);
                }
            }
        }
        Ok(nodes)
    }
}
