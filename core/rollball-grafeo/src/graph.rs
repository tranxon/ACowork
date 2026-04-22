//! LPG graph operations for GrafeoStore.

use grafeo_common::types::{EdgeId, NodeId, Value};
use grafeo_core::graph::Direction;
use grafeo_core::graph::lpg::{Edge, Node};

use crate::error::Result;
use crate::grafeo::GrafeoStore;

impl GrafeoStore {
    /// Create a node with the given label and properties.
    ///
    /// Returns the newly created [`NodeId`].
    pub fn store_node<'a>(
        &self,
        label: &str,
        properties: impl IntoIterator<Item = (&'a str, Value)>,
    ) -> Result<NodeId> {
        let id = self.db.create_node_with_props(&[label], properties);
        Ok(id)
    }

    /// Create an edge between two nodes.
    ///
    /// Returns the newly created [`EdgeId`].
    pub fn store_edge<'a>(
        &self,
        src: NodeId,
        dst: NodeId,
        edge_type: &str,
        properties: impl IntoIterator<Item = (&'a str, Value)>,
    ) -> Result<EdgeId> {
        let id = self
            .db
            .create_edge_with_props(src, dst, edge_type, properties);
        Ok(id)
    }

    /// Get a node by ID.
    ///
    /// Returns `None` if the node does not exist.
    pub fn get_node(&self, node_id: NodeId) -> Option<Node> {
        self.db.get_node(node_id)
    }

    /// Get all edges connected to a node in the given direction.
    ///
    /// Direction can be [`Direction::Outgoing`], [`Direction::Incoming`], or
    /// [`Direction::Both`].
    pub fn get_edges(&self, node_id: NodeId, direction: Direction) -> Vec<Edge> {
        let graph = self.db.graph_store();
        let edge_refs = graph.edges_from(node_id, direction);
        edge_refs
            .into_iter()
            .filter_map(|(_, edge_id)| self.db.get_edge(edge_id))
            .collect()
    }

    /// Update (merge) properties on an existing node.
    ///
    /// Existing properties are overwritten; missing properties are left untouched.
    pub fn update_node<'a>(
        &self,
        node_id: NodeId,
        properties: impl IntoIterator<Item = (&'a str, Value)>,
    ) -> Result<()> {
        for (key, value) in properties {
            self.db.set_node_property(node_id, key, value);
        }
        Ok(())
    }

    /// Delete a node and all of its edges.
    ///
    /// Returns `true` if the node existed and was deleted.
    pub fn delete_node(&self, node_id: NodeId) -> Result<bool> {
        Ok(self.db.delete_node(node_id))
    }
}
