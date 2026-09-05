//! GrafeoMemoryAdapter - implements MemoryQueryService via MemoryAdminService.
//!
//! ADR-040: delegates to the shared `memory_query` module which provides
//! thin wrappers over `dyn MemoryAdminService`.
//!
//! ADR-051 P4: `SharedMemoryStore` is now `Arc<dyn MemoryAdminService>`
//! instead of concrete `Arc<GrafeoStore>`.


use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::http::{memory_query, SharedEmbedDimension, SharedMemoryStore};
use crate::usecases::memory_query::{
    ConsolidationReport, CreateMemoryNodeInput, MemoryNode, MemoryNodeListResponse,
    MemoryNodeQuery, MemoryQueryService, MemoryStats, RebuildReport,
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
            sub_type: query.sub_type.clone(),
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
                sub_type: n.sub_type,
                content: n.content,
                confidence: n.confidence,
                importance: n.importance,
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

        let start = std::time::Instant::now();
        let out = memory_query::trigger_consolidate(store.as_ref(), force, retention_days);
        let duration_ms = start.elapsed().as_millis() as u64;

        let episodes_consolidated =
            out.upgraded + out.kept_pending + out.marked_dormant;
        let knowledge_nodes_generated =
            out.triples_extracted + out.procedural_created;

        let message = if !out.started {
            "Memory store unavailable, consolidation skipped".to_string()
        } else if episodes_consolidated == 0 && knowledge_nodes_generated == 0 {
            "No pending memories to consolidate".to_string()
        } else {
            format!(
                "Consolidated {} episodes ({} upgraded, {} dormant), generated {} knowledge nodes, cleaned {} episodic",
                episodes_consolidated,
                out.upgraded,
                out.marked_dormant,
                knowledge_nodes_generated,
                out.episodic_cleaned,
            )
        };

        Ok(ConsolidationReport {
            started: out.started,
            duration_ms,
            episodes_consolidated,
            knowledge_nodes_generated,
            message,
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

    async fn rebuild_embeddings(
        &self,
        endpoint: &str,
        model_id: &str,
        dimension: usize,
    ) -> Result<RebuildReport> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| crate::error::RuntimeError::Memory("memory store unavailable".into()))?;
        let admin: Arc<dyn acowork_memory::admin::MemoryAdminService> = store;

        // Re-embedding is CPU/IO heavy and bridges async embed into a sync
        // closure, so run the whole migration on a blocking thread (same
        // pattern as the session-task UpdateEmbedConfig migration path).
        let endpoint = endpoint.to_string();
        let model_id = model_id.to_string();
        let stats = tokio::task::spawn_blocking(move || {
            let provider =
                crate::embedding::remote::RemoteEmbeddingProvider::try_with_config_and_timeouts(
                    &endpoint,
                    None,
                    &model_id,
                    dimension,
                    &acowork_core::Timeouts::default(),
                );
            let provider: Arc<dyn crate::embedding::EmbeddingProvider> = match provider {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    return Err(crate::error::RuntimeError::Memory(format!(
                        "failed to build embedding provider for migration: {e}"
                    )));
                }
            };

            let handle = tokio::runtime::Handle::current();
            let provider_for_fn = provider.clone();
            let embed_fn = move |text: &str| -> Option<Vec<f32>> {
                let text_owned = text.to_string();
                match handle.block_on(provider_for_fn.embed(&text_owned)) {
                    Ok(vec) => Some(vec),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Re-embedding failed during migration, skipping node"
                        );
                        None
                    }
                }
            };

            admin
                .migrate_embedding_dimension(&embed_fn, dimension)
                .map_err(|e| crate::error::RuntimeError::Memory(e.to_string()))
        })
        .await
        .map_err(|e| {
            crate::error::RuntimeError::Memory(format!("rebuild task panicked: {e}"))
        })??;

        let message = format!(
            "Rebuilt {} embeddings (scanned {}, skipped {} no-embedding / {} no-content, {} errors)",
            stats.rebuilt,
            stats.total_scanned,
            stats.skipped_no_embedding,
            stats.skipped_no_content,
            stats.errors,
        );
        Ok(RebuildReport {
            total_scanned: stats.total_scanned,
            rebuilt: stats.rebuilt,
            skipped_no_embedding: stats.skipped_no_embedding,
            skipped_no_content: stats.skipped_no_content,
            errors: stats.errors,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use acowork_grafeo::grafeo::GrafeoStore;
    use acowork_grafeo::types::labels;
    use grafeo_common::types::Value;
    use acowork_memory::admin::MemoryAdminService;

    use super::*;

    /// Build an adapter backed by an in-memory GrafeoStore seeded with one
    /// Episodic node (importance only) and one Knowledge node (both fields).
    fn seeded_adapter() -> GrafeoMemoryAdapter {
        let store = GrafeoStore::new_in_memory().expect("in-memory store");
        store
            .store_node(
                labels::EPISODIC,
                [
                    ("role", Value::from("user")),
                    ("content", Value::from("episodic event")),
                    ("importance", Value::from(0.7f64)),
                ],
            )
            .unwrap();
        store
            .store_node(
                labels::KNOWLEDGE,
                [
                    ("subject", Value::from("Rust")),
                    ("content", Value::from("Rust is a systems language")),
                    ("confidence", Value::from(0.7f64)),
                    ("importance", Value::from(0.5f64)),
                ],
            )
            .unwrap();

        let admin: Arc<dyn MemoryAdminService> = Arc::new(store);
        let memory_store: SharedMemoryStore = Arc::new(RwLock::new(Some(admin)));
        let embed_dim: SharedEmbedDimension = Arc::new(RwLock::new(0));
        GrafeoMemoryAdapter::new(memory_store, embed_dim)
    }

    #[tokio::test]
    async fn list_nodes_passes_importance_through_verbatim() {
        let adapter = seeded_adapter();
        let resp = adapter
            .list_nodes(&MemoryNodeQuery {
                page: 1,
                size: 20,
                node_type: String::new(),
                sub_type: String::new(),
                keyword: String::new(),
                time_range: String::new(),
            })
            .await
            .expect("list should succeed");

        let by_type: std::collections::HashMap<&str, &MemoryNode> = resp
            .nodes
            .iter()
            .map(|n| (n.node_type.as_str(), n))
            .collect();

        // Episodic: importance present, confidence absent → 0.0 is the truth.
        let ep = by_type.get("Episodic").expect("episodic node");
        assert_eq!(ep.importance, 0.7, "Episodic importance must pass through");
        assert_eq!(ep.confidence, 0.0, "Episodic has no confidence property");

        // Knowledge: both fields present.
        let kn = by_type.get("Knowledge").expect("knowledge node");
        assert_eq!(kn.confidence, 0.7);
        assert_eq!(kn.importance, 0.5);
    }
}
