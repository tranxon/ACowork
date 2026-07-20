//! GrafeoMemoryAdapter — implements MemoryQueryService via GrafeoStore.
//!
//! ADR-040: delegates to the shared `memory_query` module which already
//! provides the business logic; this adapter simply holds the late-bind
//! store reference and dimension.


use std::collections::HashMap;

use async_trait::async_trait;

use crate::error::Result;
use crate::http::{memory_query, SharedEmbedDimension, SharedMemoryStore};
use crate::usecases::memory_query::{
    ConsolidationReport, CreateMemoryNodeInput, MemoryNode, MemoryNodeListResponse,
    MemoryNodeQuery, MemoryQueryService, MemoryStats,
};

pub struct GrafeoMemoryAdapter {
    memory_store: SharedMemoryStore,
    embed_dim: SharedEmbedDimension,
}

impl GrafeoMemoryAdapter {
    pub fn new(memory_store: SharedMemoryStore, embed_dim: SharedEmbedDimension) -> Self {
        Self {
            memory_store,
            embed_dim,
        }
    }
}

#[async_trait]
impl MemoryQueryService for GrafeoMemoryAdapter {
    async fn list_nodes(&self, query: &MemoryNodeQuery) -> Result<MemoryNodeListResponse> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone());
        let params = memory_query::ListNodesParams {
            page: query.page,
            size: query.size,
            node_type: query.node_type.clone(),
            keyword: query.keyword.clone(),
            time_range: query.time_range.clone(),
        };
        let out = memory_query::list_nodes(store.as_ref(), params);
        let dim = self.embed_dim.read().map(|d| *d).unwrap_or(0);

        let nodes: Vec<MemoryNode> = out
            .nodes
            .into_iter()
            .map(|n| MemoryNode {
                node_id: n.node_id,
                node_type: n.node_type,
                content: n.content,
                confidence: n.confidence,
                decay_score: n.decay_score,
                created_at: n.created_at,
                last_accessed_at: n.last_accessed_at,
                access_count: n.access_count,
                status: n.status,
            })
            .collect();

        Ok(MemoryNodeListResponse {
            nodes,
            total: out.total,
            page: out.page,
            size: out.size,
            model_dim: dim,
        })
    }

    async fn get_node(&self, node_id: u64) -> Result<serde_json::Value> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone());
        let out = memory_query::get_node(store.as_ref(), node_id);
        Ok(memory_query::get_output_to_json(&out))
    }

    async fn get_stats(&self) -> Result<MemoryStats> {
        // ADR-040: the service is a thin facade over the shared
        // `memory_query` business logic. The HTTP handler ferries the
        // returned `MemoryStats` straight into the wire format via
        // `serde_json::to_value`, so the field set must stay aligned with
        // `MemoryStatsResponse` in `acowork-gateway::http::memory_api`.
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone());
        let dim = self.embed_dim.read().map(|d| *d).unwrap_or(0);
        Ok(memory_query::get_stats(store.as_ref(), dim))
    }

    async fn consolidate(&self, force: bool, retention_days: u32) -> Result<ConsolidationReport> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone());
        let out = memory_query::trigger_consolidate(store.as_ref(), force, retention_days);

        Ok(ConsolidationReport {
            consolidated: out.episodes_consolidated as usize,
            failed: 0,
            retention_days,
        })
    }

    async fn delete_node(&self, node_id: u64) -> Result<()> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone());
        memory_query::delete_node(store.as_ref(), node_id);
        Ok(())
    }

    async fn create_node(&self, input: &CreateMemoryNodeInput) -> Result<u64> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| crate::error::RuntimeError::Memory("memory store unavailable".into()))?;
        memory_query::create_node(Some(&store), &input.label, &input.properties)
    }

    async fn update_node(
        &self,
        node_id: u64,
        properties: &HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| crate::error::RuntimeError::Memory("memory store unavailable".into()))?;
        memory_query::update_node(Some(&store), node_id, properties)
    }
}
