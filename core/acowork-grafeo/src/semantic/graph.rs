//! Graph relationship management for the semantic layer.

use std::collections::{HashSet, VecDeque};

use grafeo_common::types::{EdgeId, NodeId, Value};
use grafeo_core::graph::Direction;

use crate::error::Result;
use crate::grafeo::GrafeoStore;

/// Decay constant for the recency factor (per day).
const EDGE_WEIGHT_LAMBDA: f64 = 0.01;

/// Tuple returned by [`GrafeoStore::get_edges_by_type`].
pub type EdgeInfo = (EdgeId, NodeId, Vec<(String, Value)>);

impl GrafeoStore {
    /// Create an edge between two memory nodes with a type and properties.
    ///
    /// When the caller does not provide an explicit `weight` property, the
    /// edge weight is computed automatically from the average confidence of
    /// both endpoint nodes (design §3.1, G12). This only applies when both
    /// endpoints carry a `confidence` property (i.e. they are memory nodes);
    /// otherwise the edge is left without a `weight` so readers fall back to
    /// the default (`DEFAULT_EDGE_WEIGHT`). An explicit `weight` property is
    /// never overridden (backward compatibility).
    pub fn create_memory_edge(
        &self,
        src: NodeId,
        dst: NodeId,
        edge_type: &str,
        properties: Vec<(String, Value)>,
    ) -> Result<EdgeId> {
        let mut props: Vec<(&str, Value)> = properties
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();

        let has_explicit_weight = props.iter().any(|(k, _)| *k == "weight");
        if !has_explicit_weight {
            // New edges have `days_since = 0.0`; confidence comes from the
            // endpoints (see §3.1). Formula parameters (lambda / cap) come
            // from ADR-062 EdgeWeightQuality; defaults match the hardcoded
            // values (lambda 0.01, cap 0.8).
            if let Some(confidence_avg) = self.edge_confidence_avg(src, dst) {
                let ew = self.quality().edge_weight;
                let weight = compute_edge_weight_with(confidence_avg, 0.0, ew.lambda, ew.cap);
                props.push(("weight", Value::from(f64::from(weight))));
            }
        }

        self.store_edge(src, dst, edge_type, props)
    }

    /// Average confidence of both endpoint nodes, if both carry a
    /// `confidence` property (memory nodes). Returns `None` when either
    /// endpoint is not a memory node (no `confidence` property) so the
    /// caller can fall back to the default edge weight.
    fn edge_confidence_avg(&self, src: NodeId, dst: NodeId) -> Option<f32> {
        let src_node = self.db.get_node(src)?;
        let src_c = src_node
            .get_property("confidence")
            .and_then(|v| v.as_float64())? as f32;
        let dst_node = self.db.get_node(dst)?;
        let dst_c = dst_node
            .get_property("confidence")
            .and_then(|v| v.as_float64())? as f32;
        Some((src_c + dst_c) / 2.0)
    }

    /// Get all outgoing edges of a specific type from a node.
    pub fn get_edges_by_type(&self, node_id: NodeId, edge_type: &str) -> Result<Vec<EdgeInfo>> {
        let graph = self.db.graph_store();
        let edge_refs = graph.edges_from(node_id, Direction::Outgoing);

        let mut results = Vec::new();
        for (dst_id, edge_id) in edge_refs {
            if let Some(edge) = self.db.get_edge(edge_id)
                && edge.edge_type.as_str() == edge_type
            {
                let properties: Vec<(String, Value)> = edge
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();
                results.push((edge_id, dst_id, properties));
            }
        }
        Ok(results)
    }

    /// Get all connected nodes within `max_hops` from `node_id`.
    ///
    /// Returns a list of `(neighbor_id, label, hop_distance)` tuples.
    pub fn get_neighbors(
        &self,
        node_id: NodeId,
        max_hops: u32,
    ) -> Result<Vec<(NodeId, String, u32)>> {
        let mut visited = HashSet::new();
        let mut results = Vec::new();
        let mut queue = VecDeque::new();

        queue.push_back((node_id, 0u32));
        visited.insert(node_id);

        while let Some((current_id, hops)) = queue.pop_front() {
            if hops >= max_hops {
                continue;
            }

            let graph = self.db.graph_store();
            let edge_refs = graph.edges_from(current_id, Direction::Both);

            for (neighbor_id, _edge_id) in edge_refs {
                if !visited.contains(&neighbor_id) {
                    visited.insert(neighbor_id);
                    if let Some(node) = self.db.get_node(neighbor_id) {
                        let label = node
                            .labels
                            .first()
                            .map(|l| l.to_string())
                            .unwrap_or_default();
                        results.push((neighbor_id, label, hops + 1));
                        queue.push_back((neighbor_id, hops + 1));
                    }
                }
            }
        }

        // Exclude the starting node itself (should not appear because it was
        // already in `visited` before exploring edges).
        Ok(results)
    }
}

/// Compute edge weight per design §3.1.
///
/// ```text
/// edge_strength = min(0.8, confidence_avg × exp(-0.01 × days_since))
/// ```
///
/// The 0.8 cap prevents a single high-confidence edge from dominating graph
/// expansion. `confidence_avg` is the average confidence of the two endpoint
/// nodes; `days_since` is days since the edge was created (0.0 for new edges).
///
/// Uses the pre-ADR-062 hardcoded formula parameters (`lambda = 0.01`,
/// `cap = 0.8`). Callers that want to honour an agent's memory quality config
/// should use [`compute_edge_weight_with`].
pub fn compute_edge_weight(confidence_avg: f32, days_since: f64) -> f32 {
    compute_edge_weight_with(confidence_avg, days_since, EDGE_WEIGHT_LAMBDA, 0.8)
}

/// Parameterized edge-weight formula (ADR-062 EdgeWeightQuality).
///
/// Same formula as [`compute_edge_weight`] but with explicit `lambda` (recency
/// decay per day) and `cap` (maximum edge strength) so a store can honour
/// `MemoryQualityConfig.edge_weight` instead of the hardcoded constants.
pub fn compute_edge_weight_with(confidence_avg: f32, days_since: f64, lambda: f64, cap: f32) -> f32 {
    let recency = (-lambda * days_since).exp();
    (f64::from(confidence_avg) * recency).min(f64::from(cap)) as f32
}

/// Calculate edge weight: `min(0.8, confidence * exp(-lambda * days_since))`.
///
/// `lambda` is fixed at `0.01` (half-life ~69 days).
///
/// This is a single-node convenience wrapper over [`compute_edge_weight`]
/// (design §3.1 uses the average confidence of both endpoints). Kept as the
/// single source of truth for the formula lives in [`compute_edge_weight`].
pub fn calculate_edge_weight(confidence: f32, days_since_update: f64) -> f64 {
    f64::from(compute_edge_weight(confidence, days_since_update))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    #[test]
    fn test_create_memory_edge() {
        let store = test_store();
        let src = store
            .store_node("Knowledge", [("subject", Value::from("user"))])
            .unwrap();
        let dst = store
            .store_node("Knowledge", [("object", Value::from("Beijing"))])
            .unwrap();

        let edge_id = store
            .create_memory_edge(
                src,
                dst,
                "REFERENCES",
                vec![("strength".to_string(), Value::from(0.8f64))],
            )
            .unwrap();

        let edge = store.db.get_edge(edge_id).unwrap();
        assert_eq!(edge.src, src);
        assert_eq!(edge.dst, dst);
        assert_eq!(edge.edge_type.as_str(), "REFERENCES");
    }

    #[test]
    fn test_get_edges_by_type() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();
        let c = store
            .store_node("Knowledge", [("k", Value::from("c"))])
            .unwrap();

        store
            .create_memory_edge(a, b, "REFERENCES", vec![])
            .unwrap();
        store
            .create_memory_edge(
                a,
                c,
                "DERIVED_FROM",
                vec![("p".to_string(), Value::from("v"))],
            )
            .unwrap();

        let refs = store.get_edges_by_type(a, "REFERENCES").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].1, b);

        let derived = store.get_edges_by_type(a, "DERIVED_FROM").unwrap();
        assert_eq!(derived.len(), 1);
        assert_eq!(derived[0].2.len(), 1);
        assert_eq!(derived[0].2[0].0, "p");
    }

    #[test]
    fn test_get_neighbors() {
        let store = test_store();
        let n1 = store
            .store_node("Knowledge", [("k", Value::from("1"))])
            .unwrap();
        let n2 = store
            .store_node("Knowledge", [("k", Value::from("2"))])
            .unwrap();
        let n3 = store
            .store_node("Procedural", [("k", Value::from("3"))])
            .unwrap();
        let n4 = store
            .store_node("Knowledge", [("k", Value::from("4"))])
            .unwrap();

        // n1 -> n2 -> n3, and n1 -> n4
        store.create_memory_edge(n1, n2, "R", vec![]).unwrap();
        store.create_memory_edge(n2, n3, "R", vec![]).unwrap();
        store.create_memory_edge(n1, n4, "R", vec![]).unwrap();

        let neighbors = store.get_neighbors(n1, 2).unwrap();
        assert_eq!(neighbors.len(), 3);

        // All should be reachable within 2 hops.
        let ids: HashSet<NodeId> = neighbors.iter().map(|(id, _, _)| *id).collect();
        assert!(ids.contains(&n2));
        assert!(ids.contains(&n3));
        assert!(ids.contains(&n4));

        // n3 is 2 hops away.
        let n3_hop = neighbors.iter().find(|(id, _, _)| *id == n3).unwrap();
        assert_eq!(n3_hop.2, 2);
    }

    #[test]
    fn test_calculate_edge_weight() {
        // Fresh edge (0 days) should have weight == confidence.
        let w0 = calculate_edge_weight(0.8, 0.0);
        assert!((w0 - 0.8).abs() < 1e-6);

        // After ~69 days (half-life) weight should be ~0.4.
        let w69 = calculate_edge_weight(0.8, 69.0);
        assert!((w69 - 0.4).abs() < 0.05);

        // Very old edge should approach zero.
        let w500 = calculate_edge_weight(0.8, 500.0);
        assert!(w500 < 0.01);
    }

    #[test]
    fn test_compute_edge_weight() {
        // (0.8, 0.0) → 0.8 (fresh edge, exactly at the cap).
        let w0 = compute_edge_weight(0.8, 0.0);
        assert!((w0 - 0.8).abs() < 1e-6);

        // (0.5, 69.0) ≈ 0.5 × e^-0.69 ≈ 0.25 (half-life decay).
        let w69 = compute_edge_weight(0.5, 69.0);
        assert!((w69 - 0.25).abs() < 0.05);

        // (1.0, 0.0) → 0.8 (capped at 0.8, prevents dominance).
        let w_cap = compute_edge_weight(1.0, 0.0);
        assert!((w_cap - 0.8).abs() < 1e-6);
    }

    /// Store a Knowledge node carrying a `confidence` property (memory node).
    fn store_confident(store: &GrafeoStore, key: &str, confidence: f64) -> NodeId {
        let id = store
            .store_node("Knowledge", [("k", Value::from(key))])
            .unwrap();
        store
            .db()
            .set_node_property(id, "confidence", Value::from(confidence));
        id
    }

    #[test]
    fn test_create_memory_edge_auto_weight() {
        let store = test_store();
        let a = store_confident(&store, "a", 0.9);
        let b = store_confident(&store, "b", 0.7);

        // No explicit weight: auto-computed from confidence_avg (0.8) and
        // days_since = 0.0 → 0.8 (design §3.1).
        let edge_id = store.create_memory_edge(a, b, "R", vec![]).unwrap();
        let edge = store.db.get_edge(edge_id).unwrap();
        let weight = edge
            .get_property("weight")
            .and_then(|v| v.as_float64())
            .unwrap();
        assert!((weight - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_create_memory_edge_explicit_weight_not_overridden() {
        let store = test_store();
        let a = store_confident(&store, "a", 0.9);
        let b = store_confident(&store, "b", 0.7);

        // Explicit weight is preserved (backward compatibility).
        let edge_id = store
            .create_memory_edge(
                a,
                b,
                "R",
                vec![("weight".to_string(), Value::from(0.3f64))],
            )
            .unwrap();
        let edge = store.db.get_edge(edge_id).unwrap();
        let weight = edge
            .get_property("weight")
            .and_then(|v| v.as_float64())
            .unwrap();
        assert!((weight - 0.3).abs() < 1e-6);
    }

    #[test]
    fn test_create_memory_edge_no_confidence_skips_weight() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();

        // Non-memory nodes (no confidence property): weight is skipped so
        // readers fall back to DEFAULT_EDGE_WEIGHT.
        let edge_id = store.create_memory_edge(a, b, "R", vec![]).unwrap();
        let edge = store.db.get_edge(edge_id).unwrap();
        assert!(edge.get_property("weight").is_none());
    }
}
