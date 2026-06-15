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
    pub fn create_memory_edge(
        &self,
        src: NodeId,
        dst: NodeId,
        edge_type: &str,
        properties: Vec<(String, Value)>,
    ) -> Result<EdgeId> {
        let props: Vec<(&str, Value)> = properties
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        self.store_edge(src, dst, edge_type, props)
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

/// Calculate edge weight: `confidence * exp(-lambda * days_since_update)`.
///
/// `lambda` is fixed at `0.01` (half-life ~69 days).
pub fn calculate_edge_weight(confidence: f32, days_since_update: f64) -> f64 {
    let recency_factor = (-EDGE_WEIGHT_LAMBDA * days_since_update).exp();
    f64::from(confidence) * recency_factor
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
}
