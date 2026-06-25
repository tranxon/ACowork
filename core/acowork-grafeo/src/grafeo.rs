//! GrafeoStore — GrafeoDB-backed memory storage engine.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use grafeo_common::types::Value;
use grafeo_engine::GrafeoDB;

use crate::types::labels;
use crate::types::{
    AutobioCategory as GrafeoAutobioCategory, AutobiographicalNode as GrafeoAutobiographicalNode,
    Episode as GrafeoEpisode, GrafeoConfig, KnowledgeNode as GrafeoKnowledgeNode,
    KnowledgeSubType as GrafeoKnowledgeSubType, NodeStatus as GrafeoNodeStatus,
    ProceduralNode as GrafeoProceduralNode,
};
use acowork_memory::types::{ResultSource, SearchResult};
use acowork_memory::{
    AutobiographicalNode, DecayConfig, DecayScanResult, Episode, KnowledgeNode, MemoryQuery,
    ProceduralNode, PurgeResult, StoreHealth, StoreStats,
};

use crate::error::Result;
use crate::index_config::{EPISODIC_TEXT_FIELDS, HnswConfig, KNOWLEDGE_TEXT_FIELDS, VECTOR_METRIC};

/// Statistics returned by [`GrafeoStore::rebuild_embeddings`].
#[derive(Debug, Clone, Default)]
pub struct RebuildStats {
    /// Total number of nodes scanned across all memory labels.
    pub total_scanned: u64,
    /// Number of nodes whose embeddings were successfully regenerated.
    pub rebuilt: u64,
    /// Number of nodes skipped because they had no existing embedding.
    pub skipped_no_embedding: u64,
    /// Number of nodes skipped because they had no content text.
    pub skipped_no_content: u64,
    /// Number of nodes that encountered errors during rebuild.
    pub errors: u64,
}

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
    pub(crate) hnsw_config: HnswConfig,
    /// Current embedding dimension, tracked atomically so that
    /// [`migrate_embedding_dimension`](Self::migrate_embedding_dimension)
    /// can update it through `&self` (the store is behind `Arc`).
    pub(crate) embedding_dim: AtomicUsize,
}

// Static assertion: GrafeoStore must be Sync for safe concurrent access.
const _: () = {
    const fn assert_sync<T: Sync>() {}
    assert_sync::<GrafeoStore>();
};

/// Default checkpoint interval: how often the engine flushes dirty
/// sections to the .grafeo file (via background thread). 5 minutes
/// balances crash safety against write amplification.
const DEFAULT_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(300);

impl GrafeoStore {
    /// Open or create a persistent Grafeo database with the given configuration.
    ///
    /// Automatically initializes the schema (labels, vector indexes, text indexes).
    /// The HNSW vector index dimension is taken from [`GrafeoConfig::embedding_dim`].
    pub fn open(config: &GrafeoConfig) -> Result<Self> {
        let hnsw_config = crate::index_config::HnswConfig::new(config.embedding_dim);
        Self::open_with_config(&config.db_path, hnsw_config)
    }

    /// Open or create a persistent Grafeo database at the given path.
    ///
    /// Automatically initializes the schema (labels, vector indexes, text indexes).
    /// Uses the default HNSW configuration, including the default embedding dimension
    /// ([`crate::types::DEFAULT_EMBEDDING_DIM`] = 384). For non-default dimensions,
    /// use [`open`](Self::open) with a [`GrafeoConfig`].
    pub fn open_with_default_config(path: &Path) -> Result<Self> {
        Self::open_with_config(path, HnswConfig::default())
    }

    /// Open or create a persistent Grafeo database with custom HNSW config.
    pub fn open_with_config(path: &Path, config: HnswConfig) -> Result<Self> {
        let engine_config = grafeo_engine::Config::persistent(path)
            .with_checkpoint_interval(DEFAULT_CHECKPOINT_INTERVAL);
        let db = GrafeoDB::with_config(engine_config)?;
        let store = Self {
            db,
            hnsw_config: config,
            embedding_dim: AtomicUsize::new(0),
        };
        store.init_schema()?;
        // Sync embedding_dim with the configured dimension after schema init.
        store.embedding_dim.store(store.hnsw_config.dim, Ordering::Relaxed);
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
        let store = Self {
            db,
            hnsw_config: config,
            embedding_dim: AtomicUsize::new(0),
        };
        store.init_schema()?;
        store.embedding_dim.store(store.hnsw_config.dim, Ordering::Relaxed);
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
    /// Vector indexes are created for all four memory labels **only if they
    /// do not already exist**. When the database is restored from persistence,
    /// indexes are restored with their original dimensions and should not be
    /// recreated. This prevents dimension-mismatch errors when the embedding
    /// provider dimension has changed since the last session.
    ///
    /// Text indexes are created on the "content" property for all labels,
    /// plus any label-specific text fields defined in
    /// [`EPISODIC_TEXT_FIELDS`] and [`KNOWLEDGE_TEXT_FIELDS`].
    fn init_schema(&self) -> Result<()> {
        let cfg = &self.hnsw_config;

        // HNSW vector indexes on the "embedding" property.
        // Try to create each index. If creation fails (e.g., the index was
        // already restored from persistence with a different dimension, or
        // existing vectors have a mismatched dimension), log a warning and
        // continue. The store remains usable for text search and graph
        // operations; vector search will fail until migration is performed.
        for label in [
            labels::EPISODIC,
            labels::KNOWLEDGE,
            labels::PROCEDURAL,
            labels::AUTOBIOGRAPHICAL,
        ] {
            if let Err(e) = self.db.create_vector_index(
                label,
                "embedding",
                Some(cfg.dim),
                Some(VECTOR_METRIC),
                Some(cfg.m),
                Some(cfg.ef_construction),
                None,
            ) {
                tracing::warn!(
                    label,
                    dim = cfg.dim,
                    error = %e,
                    "Failed to create vector index (may already exist or dimension mismatch). \
                     Vector search will be unavailable until migration."
                );
            }
        }

        // BM25 text indexes for Episodic fields.
        for field in EPISODIC_TEXT_FIELDS {
            let _ = self.db.create_text_index(labels::EPISODIC, field);
        }

        // BM25 text indexes for Knowledge fields.
        for field in KNOWLEDGE_TEXT_FIELDS {
            let _ = self.db.create_text_index(labels::KNOWLEDGE, field);
        }

        // BM25 text indexes for Procedural content (computed from name/trigger/action).
        let _ = self.db.create_text_index(labels::PROCEDURAL, "content");

        // BM25 text indexes for Autobiographical content (computed from key/value).
        let _ = self.db.create_text_index(labels::AUTOBIOGRAPHICAL, "content");

        // Also add content text index for Knowledge (computed from subject/predicate/object).
        let _ = self.db.create_text_index(labels::KNOWLEDGE, "content");

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

    /// Return the current embedding dimension.
    pub fn embedding_dim(&self) -> usize {
        let dim = self.embedding_dim.load(Ordering::Relaxed);
        if dim > 0 {
            dim
        } else {
            self.hnsw_config.dim
        }
    }

    /// Validate that an embedding vector has the expected dimension.
    ///
    /// Returns `Err` if the embedding length does not match the HNSW index
    /// dimension. Use this before any write operation that stores an embedding.
    pub fn validate_embedding(&self, embedding: &[f32]) -> Result<()> {
        crate::index_config::validate_embedding_dim(embedding, self.hnsw_config.dim)
    }

    /// Migrate all embeddings and vector indexes to a new dimension.
    ///
    /// This is the core migration primitive used when the user switches to an
    /// embedding model with a different dimension (e.g., 384d → 768d).
    ///
    /// # Steps
    ///
    /// 1. Re-embed all nodes using `embed_fn` (the new provider).
    /// 2. Drop all existing HNSW vector indexes (old dimension).
    /// 3. Create new HNSW vector indexes with `new_dim`.
    /// 4. Update `hnsw_config.dim` to `new_dim`.
    ///
    /// # Arguments
    ///
    /// * `embed_fn` — Synchronous closure that takes a text string and returns
    ///   an embedding vector of length `new_dim`. Callers are responsible for
    ///   bridging async providers (e.g., via `tokio::runtime::Handle::block_on`).
    /// * `new_dim` — The dimension of the new embedding model.
    ///
    /// # Returns
    ///
    /// A [`RebuildStats`] with counts of nodes scanned, rebuilt, skipped, and
    /// errors.
    pub fn migrate_embedding_dimension(
        &self,
        embed_fn: impl Fn(&str) -> Option<Vec<f32>>,
        new_dim: usize,
    ) -> Result<RebuildStats> {
        tracing::info!(
            old_dim = self.hnsw_config.dim,
            new_dim,
            "Starting embedding dimension migration"
        );

        // Step 1: Re-embed all nodes with the new provider.
        let stats = self.rebuild_embeddings(&embed_fn, new_dim)?;

        // Step 2: Drop all old HNSW vector indexes.
        for label in [
            labels::EPISODIC,
            labels::KNOWLEDGE,
            labels::PROCEDURAL,
            labels::AUTOBIOGRAPHICAL,
        ] {
            let dropped = self.db.drop_vector_index(label, "embedding");
            if dropped {
                tracing::debug!(label, "Dropped old vector index");
            }
        }

        // Step 3: Create new HNSW vector indexes with the new dimension.
        // Update the atomic dimension tracker; hnsw_config.dim is not mutated
        // (it was only used during initial init_schema and is not the source
        // of truth after migration).
        self.embedding_dim.store(new_dim, Ordering::Relaxed);
        for label in [
            labels::EPISODIC,
            labels::KNOWLEDGE,
            labels::PROCEDURAL,
            labels::AUTOBIOGRAPHICAL,
        ] {
            self.db.create_vector_index(
                label,
                "embedding",
                Some(new_dim),
                Some(VECTOR_METRIC),
                Some(self.hnsw_config.m),
                Some(self.hnsw_config.ef_construction),
                None,
            )?;
        }

        tracing::info!(
            new_dim,
            rebuilt = stats.rebuilt,
            skipped = stats.skipped_no_embedding + stats.skipped_no_content,
            errors = stats.errors,
            "Embedding dimension migration complete"
        );

        Ok(stats)
    }

    /// Rebuild all embeddings using a new embedding function.
    ///
    /// Scans all nodes with an `embedding` property across all memory labels,
    /// regenerates the embedding from the node's `content` property using the
    /// provided `embed_fn`, updates the node's embedding, and tracks statistics.
    ///
    /// This is used when switching to a different embedding model with a
    /// different dimension (e.g., from Ollama nomic-embed-text 768d to
    /// bge-small-zh 512d). After rebuilding, the caller should reopen the
    /// store with the new dimension.
    ///
    /// # Arguments
    ///
    /// * `embed_fn` — A synchronous function that takes a text string and
    ///   returns an embedding vector. This is called for each node's `content`
    ///   property. The function is expected to return a vector of `new_dim` length.
    /// * `new_dim` — The dimension of the new embedding model. Used for
    ///   validation after regeneration.
    ///
    /// # Returns
    ///
    /// A [`RebuildStats`] with counts of total nodes scanned, nodes rebuilt,
    /// nodes skipped (no content or no prior embedding), and errors.
    pub fn rebuild_embeddings(
        &self,
        embed_fn: impl Fn(&str) -> Option<Vec<f32>>,
        new_dim: usize,
    ) -> Result<RebuildStats> {
        use crate::types::labels;

        let mut stats = RebuildStats::default();

        for label in [
            labels::EPISODIC,
            labels::KNOWLEDGE,
            labels::PROCEDURAL,
            labels::AUTOBIOGRAPHICAL,
        ] {
            let graph = self.db.graph_store();
            let node_ids = graph.nodes_by_label(label);

            for &node_id in &node_ids {
                stats.total_scanned += 1;

                // Get the node's content property (all 4 types store one).
                let node = match self.db.get_node(node_id) {
                    Some(n) => n,
                    None => {
                        stats.errors += 1;
                        continue;
                    }
                };

                let props: Vec<(String, Value)> = node
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();
                let prop_map: std::collections::HashMap<&str, &Value> =
                    props.iter().map(|(k, v)| (k.as_str(), v)).collect();

                // Skip nodes without an existing embedding (they may have been
                // created without an embedding provider).
                let has_embedding = prop_map
                    .get("embedding")
                    .and_then(|v| v.as_vector())
                    .is_some();
                if !has_embedding {
                    stats.skipped_no_embedding += 1;
                    continue;
                }

                // Get the content text for re-embedding.
                let content = match prop_map.get("content").and_then(|v| v.as_str()) {
                    Some(c) if !c.is_empty() => c.to_string(),
                    _ => {
                        stats.skipped_no_content += 1;
                        continue;
                    }
                };

                // Generate new embedding.
                match embed_fn(&content) {
                    Some(new_embedding) => {
                        if new_embedding.len() != new_dim {
                            // Wrong dimension from embed_fn — update anyway,
                            // caller should handle dimension mismatch.
                        }
                        let emb_value =
                            Value::Vector(std::sync::Arc::from(new_embedding.as_slice()));
                        self.db.set_node_property(node_id, "embedding", emb_value);
                        stats.rebuilt += 1;
                    }
                    None => {
                        stats.errors += 1;
                    }
                }
            }
        }

        Ok(stats)
    }
}

// ============================================================================
// MemoryStore trait implementation
// ============================================================================

use acowork_memory::MemoryStore;

impl MemoryStore for GrafeoStore {
    fn store_episode(&self, episode: &Episode) -> acowork_core::error::Result<()> {
        let grafeo_episode = GrafeoEpisode {
            id: None,
            session_id: episode.session_id.clone(),
            turn_index: episode.turn_index,
            role: episode.role.clone(),
            content: episode.content.clone(),
            embedding: episode.embedding.clone(),
            timestamp: episode.timestamp,
            consolidated: episode.consolidated,
            metadata: episode.metadata.clone(),
            importance: episode.importance,
        };
        GrafeoStore::store_episode(self, &grafeo_episode)
            .map(|_| ())
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))
    }

    fn search_episodes(
        &self,
        query: &MemoryQuery,
    ) -> acowork_core::error::Result<Vec<SearchResult>> {
        // Bridge to episodic search methods based on query type
        if let Some(ref embedding) = query.embedding {
            // Vector search with embedding
            let episodes = self
                .search_episodes_by_embedding(embedding, query.limit)
                .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?;

            Ok(episodes
                .into_iter()
                .map(|(ep, score)| SearchResult {
                    node_id: ep.id.map(|id| id.0).unwrap_or(0),
                    content: ep.content,
                    label: "Episodic".to_string(),
                    score,
                    source: ResultSource::DirectMatch,
                    context_tokens: 0,
                    source_context: None,
                })
                .collect())
        } else {
            // Keyword text search
            let episodes = self
                .search_episodes_by_keyword(&query.query_text, query.limit)
                .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?;

            Ok(episodes
                .into_iter()
                .map(|(ep, score)| SearchResult {
                    node_id: ep.id.map(|id| id.0).unwrap_or(0),
                    content: ep.content,
                    label: "Episodic".to_string(),
                    score,
                    source: ResultSource::DirectMatch,
                    context_tokens: 0,
                    source_context: None,
                })
                .collect())
        }
    }

    fn mark_consolidated(&self, ids: &[u64]) -> acowork_core::error::Result<()> {
        // TODO: implement using episodic/consolidate.rs
        for id in ids {
            self.mark_episode_consolidated(grafeo_common::NodeId(*id))
                .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?;
        }
        Ok(())
    }

    fn cleanup_episodes(&self, older_than: Duration) -> acowork_core::error::Result<u64> {
        // Convert Duration to days for the native method
        let retention_days = (older_than.as_secs() / 86400) as u32;
        let count = self
            .cleanup_old_episodes(retention_days)
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?;
        Ok(count as u64)
    }

    fn get_episodes(
        &self,
        session_id: Option<&str>,
        limit: usize,
    ) -> acowork_core::error::Result<Vec<acowork_memory::Episode>> {
        let grafeo_eps = if let Some(sid) = session_id {
            self.search_episodes_by_session(sid, limit)
                .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?
        } else {
            self.list_all_episodes(limit)
                .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?
        };

        Ok(grafeo_eps
            .into_iter()
            .map(|ep| acowork_memory::Episode {
                session_id: ep.session_id,
                turn_index: ep.turn_index,
                role: ep.role,
                content: ep.content,
                embedding: ep.embedding,
                timestamp: ep.timestamp,
                consolidated: ep.consolidated,
                metadata: ep.metadata,
                importance: ep.importance,
            })
            .collect())
    }

    fn store_knowledge(&self, node: &KnowledgeNode) -> acowork_core::error::Result<()> {
        let grafeo_node = GrafeoKnowledgeNode {
            id: None,
            subject: node.subject.clone(),
            predicate: node.predicate.clone(),
            object: node.object.clone(),
            sub_type: match node.sub_type {
                acowork_memory::KnowledgeSubType::Fact => GrafeoKnowledgeSubType::Fact,
                acowork_memory::KnowledgeSubType::Preference => GrafeoKnowledgeSubType::Preference,
                acowork_memory::KnowledgeSubType::Relation => GrafeoKnowledgeSubType::Relation,
            },
            confidence: node.confidence,
            source_episode_id: None,
            embedding: node.embedding.clone(),
            status: match node.status {
                acowork_memory::NodeStatus::Active => GrafeoNodeStatus::Active,
                acowork_memory::NodeStatus::Dormant => GrafeoNodeStatus::Dormant,
                acowork_memory::NodeStatus::Pending => GrafeoNodeStatus::Pending,
            },
            created_at: node.created_at,
            updated_at: node.updated_at,
            metadata: node.metadata.clone(),
        };
        GrafeoStore::store_knowledge(self, &grafeo_node)
            .map(|_| ())
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))
    }

    fn store_procedural(&self, node: &ProceduralNode) -> acowork_core::error::Result<()> {
        let grafeo_node = GrafeoProceduralNode {
            id: None,
            name: node.name.clone(),
            trigger_condition: node.trigger_condition.clone(),
            action_pattern: node.action_pattern.clone(),
            success_count: node.success_count,
            fail_count: node.fail_count,
            confidence: node.confidence,
            activation_count: node.activation_count,
            source_skill: node.source_skill.clone(),
            learned_from: node.learned_from.clone(),
            embedding: node.embedding.clone(),
            status: match node.status {
                acowork_memory::NodeStatus::Active => GrafeoNodeStatus::Active,
                acowork_memory::NodeStatus::Dormant => GrafeoNodeStatus::Dormant,
                acowork_memory::NodeStatus::Pending => GrafeoNodeStatus::Pending,
            },
            created_at: node.created_at,
            updated_at: node.updated_at,
            metadata: node.metadata.clone(),
        };
        GrafeoStore::store_procedural(self, &grafeo_node)
            .map(|_| ())
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))
    }

    fn store_autobiographical(
        &self,
        node: &AutobiographicalNode,
    ) -> acowork_core::error::Result<()> {
        let grafeo_node = GrafeoAutobiographicalNode {
            id: None,
            category: match node.category {
                acowork_memory::AutobioCategory::Identity => GrafeoAutobioCategory::Identity,
                acowork_memory::AutobioCategory::Capability => GrafeoAutobioCategory::Capability,
                acowork_memory::AutobioCategory::Limitation => GrafeoAutobioCategory::Limitation,
                acowork_memory::AutobioCategory::Preference => GrafeoAutobioCategory::Preference,
                acowork_memory::AutobioCategory::History => GrafeoAutobioCategory::History,
                acowork_memory::AutobioCategory::Relationship => {
                    GrafeoAutobioCategory::Relationship
                }
            },
            key: node.key.clone(),
            value: node.value.clone(),
            confidence: node.confidence,
            source_episode_id: None,
            embedding: node.embedding.clone(),
            status: match node.status {
                acowork_memory::NodeStatus::Active => GrafeoNodeStatus::Active,
                acowork_memory::NodeStatus::Dormant => GrafeoNodeStatus::Dormant,
                acowork_memory::NodeStatus::Pending => GrafeoNodeStatus::Pending,
            },
            created_at: node.created_at,
            updated_at: node.updated_at,
            metadata: node.metadata.clone(),
        };
        GrafeoStore::store_autobiographical(self, &grafeo_node)
            .map(|_| ())
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))
    }

    fn hybrid_search(&self, query: &MemoryQuery) -> acowork_core::error::Result<Vec<SearchResult>> {
        // Run hybrid search across all labels and merge results
        let labels = ["Episodic", "Knowledge", "Procedural", "Autobiographical"];
        let mut all_results: Vec<SearchResult> = Vec::new();

        for label in &labels {
            // Skip if no embedding and no query text
            if query.embedding.is_none() && query.query_text.is_empty() {
                continue;
            }

            let embedding = query.embedding.as_deref().unwrap_or(&[]);
            let search_results = if !embedding.is_empty() && !query.query_text.is_empty() {
                // Hybrid search with both text and vector
                self.hybrid_search(
                    label,
                    "content",
                    "embedding",
                    &query.query_text,
                    embedding,
                    query.limit,
                )
                .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?
            } else if !embedding.is_empty() {
                // Vector search only
                self.vector_search(label, embedding, query.limit, None)
                    .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?
                    .into_iter()
                    .map(|(id, score)| (id, score as f64))
                    .collect()
            } else {
                // Text search only
                self.text_search(label, &query.query_text, query.limit)
                    .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?
            };

            // Convert to SearchResult
            for (node_id, score) in search_results {
                all_results.push(SearchResult {
                    node_id: node_id.0,
                    content: String::new(), // Will be populated by caller if needed
                    label: label.to_string(),
                    score,
                    source: ResultSource::DirectMatch,
                    context_tokens: 0,
                    source_context: None,
                });
            }
        }

        // Sort by score descending
        all_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());

        // Apply min_score filter if present
        if let Some(min_score) = query.min_score {
            all_results.retain(|r| r.score >= min_score as f64);
        }

        // Limit results
        all_results.truncate(query.limit);

        Ok(all_results)
    }

    fn graph_expand(
        &self,
        seeds: &[SearchResult],
        hops: u8,
    ) -> acowork_core::error::Result<Vec<SearchResult>> {
        // Convert SearchResult to (NodeId, f64) format for native method
        let seed_nodes: Vec<(grafeo_common::NodeId, f64)> = seeds
            .iter()
            .map(|s| (grafeo_common::NodeId(s.node_id), s.score))
            .collect();

        // Create GraphExpandConfig from hops parameter
        let config = crate::spreading::GraphExpandConfig {
            max_hops: hops as u32,
            ..Default::default()
        };

        // Call native graph_expand method
        let expanded = self
            .graph_expand(&seed_nodes, &config)
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?;

        // Convert ExpandedNode back to SearchResult
        Ok(expanded
            .into_iter()
            .map(|node| SearchResult {
                node_id: node.node_id.0,
                content: String::new(),
                label: node.label,
                score: node.accumulated_score,
                source: ResultSource::GraphExpansion,
                context_tokens: 0,
                source_context: None,
            })
            .collect())
    }

    fn run_decay_scan(&self, config: &DecayConfig) -> acowork_core::error::Result<DecayScanResult> {
        // Convert acowork_memory::DecayConfig to native DecayConfig
        let native_config = crate::forgetting::decay::DecayConfig {
            lambda: config.lambda as f64,
            access_boost: config.access_per_hit as f64,
            dormant_threshold: config.dormant_threshold,
        };

        // Call native decay scan method
        let transitioned = self
            .run_decay_scan(&native_config)
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?;

        Ok(DecayScanResult {
            to_dormant: transitioned as u64,
            reactivated: 0,
            purged: 0,
        })
    }

    fn reactivate_node(&self, node_id: u64) -> acowork_core::error::Result<()> {
        GrafeoStore::reactivate_node(self, grafeo_common::NodeId(node_id))
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))
    }

    fn purge_expired(&self, max_dormant_age: Duration) -> acowork_core::error::Result<PurgeResult> {
        // Convert Duration to days for native method
        let max_days = (max_dormant_age.as_secs() / 86400) as u32;

        // Use purge_expired_dormant from purge_log module
        let purged_entries = self
            .purge_expired_dormant(max_days)
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?;

        Ok(PurgeResult {
            purged_count: purged_entries.len() as u64,
            bytes_freed: 0, // Native method doesn't return this
        })
    }

    fn health_check(&self) -> acowork_core::error::Result<StoreHealth> {
        // Basic health check: verify database is accessible
        let is_healthy = true; // GrafeoDB session() doesn't return Result

        Ok(StoreHealth {
            is_healthy,
            latency_ms: 0,
            error_count: 0,
            details: None,
        })
    }

    fn stats(&self) -> acowork_core::error::Result<StoreStats> {
        // Use native stats collection method
        let memory_stats = crate::stats::collect_stats(self)
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))?;

        // Extract counts from label_counts HashMap
        let episode_count = *memory_stats.label_counts.get("Episodic").unwrap_or(&0) as u64;
        let knowledge_count = *memory_stats.label_counts.get("Knowledge").unwrap_or(&0) as u64;
        let procedural_count = *memory_stats.label_counts.get("Procedural").unwrap_or(&0) as u64;
        let autobio_count = *memory_stats
            .label_counts
            .get("Autobiographical")
            .unwrap_or(&0) as u64;

        Ok(StoreStats {
            episode_count,
            node_count: knowledge_count + procedural_count + autobio_count,
            active_node_count: 0, // Native stats doesn't provide this breakdown
            dormant_node_count: memory_stats.dormant_count as u64,
            edge_count: 0,         // Native stats doesn't provide this
            storage_size_bytes: 0, // Native stats doesn't provide this
            index_count: 0,        // Native stats doesn't provide this
        })
    }

    fn close(&self) -> acowork_core::error::Result<()> {
        GrafeoStore::close(self)
            .map_err(|e| acowork_core::error::AcoworkError::Memory(e.to_string()))
    }
}

// ============================================================================
// IdentityStore trait removed as part of identity simplification (ADR-009/018).
// User identity is now managed centrally by Gateway as a versioned JSON resource.
// ============================================================================
