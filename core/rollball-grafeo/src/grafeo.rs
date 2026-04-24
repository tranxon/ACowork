//! GrafeoStore — GrafeoDB-backed memory storage engine.

use std::path::Path;
use std::time::Duration;

use grafeo_engine::GrafeoDB;

use crate::types::labels;
use crate::types::{
    ArtifactRef as GrafeoArtifactRef, AutobiographicalNode as GrafeoAutobiographicalNode,
    ContentType as GrafeoContentType, Episode as GrafeoEpisode,
    KnowledgeNode as GrafeoKnowledgeNode, KnowledgeSubType as GrafeoKnowledgeSubType,
    ProceduralNode as GrafeoProceduralNode,
    AutobioCategory as GrafeoAutobioCategory, NodeStatus as GrafeoNodeStatus,
};
use rollball_memory::types::SearchResult;
use rollball_memory::{
    AutobiographicalNode, DecayConfig, DecayScanResult, Episode, KnowledgeNode,
    MemoryQuery, ProceduralNode, PurgeResult, StoreHealth, StoreStats,
};

use crate::error::Result;
use crate::index_config::{HnswConfig, EPISODIC_TEXT_FIELDS, KNOWLEDGE_TEXT_FIELDS, VECTOR_METRIC};

/// Grafeo graph database backed by grafeo-engine.
///
/// # Thread Safety
///
/// `GrafeoStore` is `Send + Sync` because `GrafeoDB` uses interior mutability
/// (likely `RwLock` or atomics) to allow concurrent access from multiple threads.
/// This is safe for use in async Runtime contexts where multiple tokio tasks
/// may call memory operations concurrently.
///
/// # Safety Guarantee
///
/// GrafeoDB's internal synchronization ensures that:
/// - Read operations (search, retrieve) can proceed concurrently
/// - Write operations (store, update) are serialized internally
/// - No data races or undefined behavior can occur
pub struct GrafeoStore {
    /// Underlying GrafeoDB engine instance.
    pub(crate) db: GrafeoDB,
    /// HNSW index configuration used for this store.
    hnsw_config: HnswConfig,
}

// Static assertion: GrafeoStore must be Sync for safe concurrent access.
const _: () = {
    const fn assert_sync<T: Sync>() {}
    assert_sync::<GrafeoStore>();
};

impl GrafeoStore {
    /// Open or create a persistent Grafeo database at the given path.
    ///
    /// Automatically initializes the schema (labels, vector indexes, text indexes).
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_config(path, HnswConfig::default())
    }

    /// Open or create a persistent Grafeo database with custom HNSW config.
    pub fn open_with_config(path: &Path, config: HnswConfig) -> Result<Self> {
        let db = GrafeoDB::open(path)?;
        let store = Self { db, hnsw_config: config };
        store.init_schema()?;
        Ok(store)
    }

    /// Create a new in-memory Grafeo database (useful for tests).
    ///
    /// Automatically initializes the schema.
    pub fn new_in_memory() -> Result<Self> {
        Self::new_in_memory_with_config(HnswConfig::default())
    }

    /// Create a new in-memory Grafeo database with custom HNSW config.
    pub fn new_in_memory_with_config(config: HnswConfig) -> Result<Self> {
        let db = GrafeoDB::new_in_memory();
        let store = Self { db, hnsw_config: config };
        store.init_schema()?;
        Ok(store)
    }

    /// Close the database, flushing all pending writes.
    ///
    /// For persistent databases this ensures everything is safely on disk.
    pub fn close(&self) -> Result<()> {
        self.db.close().map_err(Into::into)
    }

    /// Initialize schema: create HNSW vector indexes and BM25 text indexes.
    ///
    /// Vector indexes are only created for labels that store embeddings
    /// (Episodic, Knowledge, Procedural, Autobiographical).
    /// Text indexes are created for searchable text fields defined in
    /// [`EPISODIC_TEXT_FIELDS`] and [`KNOWLEDGE_TEXT_FIELDS`].
    fn init_schema(&self) -> Result<()> {
        let cfg = &self.hnsw_config;

        // HNSW vector indexes on the "embedding" property.
        for label in [
            labels::EPISODIC,
            labels::KNOWLEDGE,
            labels::PROCEDURAL,
            labels::AUTOBIOGRAPHICAL,
        ] {
            self.db.create_vector_index(
                label,
                "embedding",
                Some(cfg.dim),
                Some(VECTOR_METRIC),
                Some(cfg.m),
                Some(cfg.ef_construction),
                None,
            )?;
        }

        // BM25 text indexes for Episodic fields.
        for field in EPISODIC_TEXT_FIELDS {
            self.db.create_text_index(labels::EPISODIC, field)?;
        }

        // BM25 text indexes for Knowledge fields.
        for field in KNOWLEDGE_TEXT_FIELDS {
            self.db.create_text_index(labels::KNOWLEDGE, field)?;
        }

        Ok(())
    }

    /// Return the HNSW config used by this store.
    pub fn hnsw_config(&self) -> &HnswConfig {
        &self.hnsw_config
    }

    /// Return a reference to the underlying GrafeoDB.
    pub fn db(&self) -> &GrafeoDB {
        &self.db
    }
}

// ============================================================================
// MemoryStore trait implementation
// ============================================================================

use rollball_memory::MemoryStore;

impl MemoryStore for GrafeoStore {
    fn store_episode(&self, episode: &Episode) -> rollball_core::error::Result<()> {
        let grafeo_episode = GrafeoEpisode {
            id: None,
            session_id: episode.session_id.clone(),
            turn_index: episode.turn_index,
            role: episode.role.clone(),
            content: episode.content.clone(),
            content_type: match episode.content_type {
                rollball_memory::ContentType::Informational => GrafeoContentType::Informational,
                rollball_memory::ContentType::Artifact => GrafeoContentType::Artifact,
                rollball_memory::ContentType::Structural => GrafeoContentType::Structural,
            },
            embedding: episode.embedding.clone(),
            timestamp: episode.timestamp,
            consolidated: episode.consolidated,
            metadata: episode.metadata.clone(),
            artifact_refs: episode.artifact_refs.iter().map(|r| GrafeoArtifactRef {
                path: r.path.clone(),
                hash: r.hash.clone(),
                description: r.description.clone(),
                line_range: r.line_range,
            }).collect(),
            importance: episode.importance,
        };
        GrafeoStore::store_episode(self, &grafeo_episode)
            .map(|_| ())
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))
    }

    fn search_episodes(&self, _query: &MemoryQuery) -> rollball_core::error::Result<Vec<SearchResult>> {
        // TODO: implement using episodic/search.rs methods
        Ok(vec![])
    }

    fn mark_consolidated(&self, ids: &[u64]) -> rollball_core::error::Result<()> {
        // TODO: implement using episodic/consolidate.rs
        for id in ids {
            self.mark_episode_consolidated(grafeo_common::NodeId(*id))
                .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;
        }
        Ok(())
    }

    fn cleanup_episodes(&self, _older_than: Duration) -> rollball_core::error::Result<u64> {
        // TODO: implement cleanup logic
        Ok(0)
    }

    fn store_knowledge(&self, node: &KnowledgeNode) -> rollball_core::error::Result<()> {
        let grafeo_node = GrafeoKnowledgeNode {
            id: None,
            subject: node.subject.clone(),
            predicate: node.predicate.clone(),
            object: node.object.clone(),
            sub_type: match node.sub_type {
                rollball_memory::KnowledgeSubType::Fact => GrafeoKnowledgeSubType::Fact,
                rollball_memory::KnowledgeSubType::Preference => GrafeoKnowledgeSubType::Preference,
                rollball_memory::KnowledgeSubType::Relation => GrafeoKnowledgeSubType::Relation,
            },
            confidence: node.confidence,
            source_episode_id: None,
            embedding: node.embedding.clone(),
            status: match node.status {
                rollball_memory::NodeStatus::Active => GrafeoNodeStatus::Active,
                rollball_memory::NodeStatus::Dormant => GrafeoNodeStatus::Dormant,
                rollball_memory::NodeStatus::Pending => GrafeoNodeStatus::Pending,
            },
            created_at: node.created_at,
            updated_at: node.updated_at,
            metadata: node.metadata.clone(),
        };
        GrafeoStore::store_knowledge(self, &grafeo_node)
            .map(|_| ())
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))
    }

    fn store_procedural(&self, node: &ProceduralNode) -> rollball_core::error::Result<()> {
        let grafeo_node = GrafeoProceduralNode {
            id: None,
            name: node.name.clone(),
            trigger_condition: node.trigger_condition.clone(),
            action_pattern: node.action_pattern.clone(),
            success_count: node.success_count,
            fail_count: node.fail_count,
            confidence: node.confidence,
            embedding: node.embedding.clone(),
            status: match node.status {
                rollball_memory::NodeStatus::Active => GrafeoNodeStatus::Active,
                rollball_memory::NodeStatus::Dormant => GrafeoNodeStatus::Dormant,
                rollball_memory::NodeStatus::Pending => GrafeoNodeStatus::Pending,
            },
            created_at: node.created_at,
            updated_at: node.updated_at,
            metadata: node.metadata.clone(),
        };
        GrafeoStore::store_procedural(self, &grafeo_node)
            .map(|_| ())
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))
    }

    fn store_autobiographical(&self, node: &AutobiographicalNode) -> rollball_core::error::Result<()> {
        let grafeo_node = GrafeoAutobiographicalNode {
            id: None,
            category: match node.category {
                rollball_memory::AutobioCategory::Identity => GrafeoAutobioCategory::Identity,
                rollball_memory::AutobioCategory::Capability => GrafeoAutobioCategory::Capability,
                rollball_memory::AutobioCategory::Limitation => GrafeoAutobioCategory::Limitation,
                rollball_memory::AutobioCategory::Preference => GrafeoAutobioCategory::Preference,
                rollball_memory::AutobioCategory::History => GrafeoAutobioCategory::History,
                rollball_memory::AutobioCategory::Relationship => GrafeoAutobioCategory::Relationship,
            },
            key: node.key.clone(),
            value: node.value.clone(),
            confidence: node.confidence,
            source_episode_id: None,
            embedding: node.embedding.clone(),
            status: match node.status {
                rollball_memory::NodeStatus::Active => GrafeoNodeStatus::Active,
                rollball_memory::NodeStatus::Dormant => GrafeoNodeStatus::Dormant,
                rollball_memory::NodeStatus::Pending => GrafeoNodeStatus::Pending,
            },
            created_at: node.created_at,
            updated_at: node.updated_at,
            metadata: node.metadata.clone(),
        };
        GrafeoStore::store_autobiographical(self, &grafeo_node)
            .map(|_| ())
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))
    }

    fn hybrid_search(&self, _query: &MemoryQuery) -> rollball_core::error::Result<Vec<SearchResult>> {
        // TODO: implement using retrieval.rs methods
        Ok(vec![])
    }

    fn graph_expand(&self, _seeds: &[SearchResult], _hops: u8) -> rollball_core::error::Result<Vec<SearchResult>> {
        // TODO: implement using spreading.rs
        Ok(vec![])
    }

    fn run_decay_scan(&self, _config: &DecayConfig) -> rollball_core::error::Result<DecayScanResult> {
        // TODO: implement using forgetting/scan.rs
        Ok(DecayScanResult::default())
    }

    fn reactivate_node(&self, node_id: u64) -> rollball_core::error::Result<()> {
        GrafeoStore::reactivate_node(self, grafeo_common::NodeId(node_id))
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))
    }

    fn purge_expired(&self, _max_dormant_age: Duration) -> rollball_core::error::Result<PurgeResult> {
        // TODO: implement using forgetting/purge.rs
        Ok(PurgeResult::default())
    }

    fn health_check(&self) -> rollball_core::error::Result<StoreHealth> {
        // TODO: implement health check
        Ok(StoreHealth {
            is_healthy: true,
            latency_ms: 0,
            error_count: 0,
            details: None,
        })
    }

    fn stats(&self) -> rollball_core::error::Result<StoreStats> {
        // TODO: implement using stats.rs
        Ok(StoreStats::default())
    }

    fn close(&self) -> rollball_core::error::Result<()> {
        GrafeoStore::close(self)
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))
    }
}
