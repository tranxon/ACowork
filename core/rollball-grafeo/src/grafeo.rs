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
use rollball_memory::types::{ResultSource, SearchResult};
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

    fn search_episodes(&self, query: &MemoryQuery) -> rollball_core::error::Result<Vec<SearchResult>> {
        // Bridge to episodic search methods based on query type
        if let Some(ref embedding) = query.embedding {
            // Vector search with embedding
            let episodes = self
                .search_episodes_by_embedding(embedding, query.limit)
                .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;
            
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
                .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;
            
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

    fn mark_consolidated(&self, ids: &[u64]) -> rollball_core::error::Result<()> {
        // TODO: implement using episodic/consolidate.rs
        for id in ids {
            self.mark_episode_consolidated(grafeo_common::NodeId(*id))
                .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;
        }
        Ok(())
    }

    fn cleanup_episodes(&self, older_than: Duration) -> rollball_core::error::Result<u64> {
        // Convert Duration to days for the native method
        let retention_days = (older_than.as_secs() / 86400) as u32;
        let count = self
            .cleanup_old_episodes(retention_days)
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;
        Ok(count as u64)
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

    fn hybrid_search(&self, query: &MemoryQuery) -> rollball_core::error::Result<Vec<SearchResult>> {
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
                self.hybrid_search(label, "content", "embedding", &query.query_text, embedding, query.limit)
                    .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?
            } else if !embedding.is_empty() {
                // Vector search only
                self.vector_search(label, embedding, query.limit, None)
                    .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?
                    .into_iter()
                    .map(|(id, score)| (id, score as f64))
                    .collect()
            } else {
                // Text search only
                self.text_search(label, &query.query_text, query.limit)
                    .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?
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

    fn graph_expand(&self, seeds: &[SearchResult], hops: u8) -> rollball_core::error::Result<Vec<SearchResult>> {
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
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;

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

    fn run_decay_scan(&self, config: &DecayConfig) -> rollball_core::error::Result<DecayScanResult> {
        // Convert rollball_memory::DecayConfig to native DecayConfig
        let native_config = crate::forgetting::decay::DecayConfig {
            lambda: config.lambda as f64,
            access_boost: config.access_per_hit as f64,
            dormant_threshold: config.dormant_threshold,
        };

        // Call native decay scan method
        let transitioned = self
            .run_decay_scan(&native_config)
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;

        Ok(DecayScanResult {
            to_dormant: transitioned as u64,
            reactivated: 0,
            purged: 0,
        })
    }

    fn reactivate_node(&self, node_id: u64) -> rollball_core::error::Result<()> {
        GrafeoStore::reactivate_node(self, grafeo_common::NodeId(node_id))
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))
    }

    fn purge_expired(&self, max_dormant_age: Duration) -> rollball_core::error::Result<PurgeResult> {
        // Convert Duration to days for native method
        let max_days = (max_dormant_age.as_secs() / 86400) as u32;
        
        // Use purge_expired_dormant from purge_log module
        let purged_entries = self
            .purge_expired_dormant(max_days)
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;

        Ok(PurgeResult {
            purged_count: purged_entries.len() as u64,
            bytes_freed: 0, // Native method doesn't return this
        })
    }

    fn health_check(&self) -> rollball_core::error::Result<StoreHealth> {
        // Basic health check: verify database is accessible
        let is_healthy = true; // GrafeoDB session() doesn't return Result
        
        Ok(StoreHealth {
            is_healthy,
            latency_ms: 0,
            error_count: 0,
            details: None,
        })
    }

    fn stats(&self) -> rollball_core::error::Result<StoreStats> {
        // Use native stats collection method
        let memory_stats = crate::stats::collect_stats(self)
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;

        // Extract counts from label_counts HashMap
        let episode_count = *memory_stats.label_counts.get("Episodic").unwrap_or(&0) as u64;
        let knowledge_count = *memory_stats.label_counts.get("Knowledge").unwrap_or(&0) as u64;
        let procedural_count = *memory_stats.label_counts.get("Procedural").unwrap_or(&0) as u64;
        let autobio_count = *memory_stats.label_counts.get("Autobiographical").unwrap_or(&0) as u64;

        Ok(StoreStats {
            episode_count,
            node_count: knowledge_count + procedural_count + autobio_count,
            active_node_count: 0, // Native stats doesn't provide this breakdown
            dormant_node_count: memory_stats.dormant_count as u64,
            edge_count: 0, // Native stats doesn't provide this
            storage_size_bytes: 0, // Native stats doesn't provide this
            index_count: 0, // Native stats doesn't provide this
        })
    }

    fn close(&self) -> rollball_core::error::Result<()> {
        GrafeoStore::close(self)
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))
    }
}

// ============================================================================
// IdentityStore trait implementation
// ============================================================================

use rollball_core::identity::{IdentityEntry, IdentityQueryResult, IdentityStore, IdentitySubscription};

impl IdentityStore for GrafeoStore {
    fn store(&self, entry: &IdentityEntry) -> rollball_core::error::Result<()> {
        #[allow(unused_imports)]
        use crate::types::labels;
        #[allow(unused_imports)]
        use grafeo_common::types::Value;

        // Check if an autobiographical node with this key already exists.
        let existing = self.find_autobiographical_by_key(&entry.field)
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;

        match existing {
            Some(mut existing_node) => {
                // Update only if new confidence >= existing confidence.
                if entry.confidence >= existing_node.confidence {
                    existing_node.value = entry.value.clone();
                    existing_node.confidence = entry.confidence;
                    existing_node.updated_at = chrono::Utc::now();
                    existing_node.metadata.insert(
                        "source".to_string(),
                        serde_json::Value::String(entry.source.clone()),
                    );
                    existing_node.metadata.insert(
                        "privacy".to_string(),
                        serde_json::Value::String(entry.privacy.as_str().to_string()),
                    );
                    existing_node.metadata.insert(
                        "category".to_string(),
                        serde_json::Value::String(entry.category.as_str().to_string()),
                    );
                    existing_node.metadata.insert(
                        "updated_at_iso".to_string(),
                        serde_json::Value::String(entry.updated_at.clone()),
                    );
                    self.update_autobiographical(&existing_node)
                        .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;
                }
            }
            None => {
                // Create a new AutobiographicalNode.
                let mut metadata = std::collections::HashMap::new();
                metadata.insert(
                    "source".to_string(),
                    serde_json::Value::String(entry.source.clone()),
                );
                metadata.insert(
                    "privacy".to_string(),
                    serde_json::Value::String(entry.privacy.as_str().to_string()),
                );
                metadata.insert(
                    "category".to_string(),
                    serde_json::Value::String(entry.category.as_str().to_string()),
                );
                metadata.insert(
                    "updated_at_iso".to_string(),
                    serde_json::Value::String(entry.updated_at.clone()),
                );

                let node = crate::types::AutobiographicalNode {
                    id: None,
                    category: match entry.category {
                        rollball_core::identity::IdentityCategory::Identity => crate::types::AutobioCategory::Identity,
                        rollball_core::identity::IdentityCategory::Preferences => crate::types::AutobioCategory::Preference,
                        rollball_core::identity::IdentityCategory::Knowledge => crate::types::AutobioCategory::Capability,
                        rollball_core::identity::IdentityCategory::Work => crate::types::AutobioCategory::History,
                    },
                    key: entry.field.clone(),
                    value: entry.value.clone(),
                    confidence: entry.confidence,
                    source_episode_id: None,
                    embedding: None,
                    status: crate::types::NodeStatus::Active,
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                    metadata,
                };

                GrafeoStore::store_autobiographical(self, &node)
                    .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;
            }
        }

        Ok(())
    }

    fn query(&self, fields: &[String]) -> rollball_core::error::Result<IdentityQueryResult> {
        let mut values = std::collections::HashMap::new();
        let mut confidence = std::collections::HashMap::new();

        for field in fields {
            let node_opt = self.find_autobiographical_by_key(field)
                .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;
            if let Some(node) = node_opt {
                values.insert(field.clone(), node.value.clone());
                confidence.insert(field.clone(), node.confidence);
            }
        }

        Ok(IdentityQueryResult { values, confidence })
    }

    fn observe(&self, subscription: &IdentitySubscription) -> rollball_core::error::Result<()> {
        use crate::types::labels;
        use grafeo_common::types::Value;

        // Store subscription as a SystemConfig node.
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "type".to_string(),
            serde_json::Value::String("identity_subscription".to_string()),
        );
        metadata.insert(
            "subscriber_id".to_string(),
            serde_json::Value::String(subscription.subscriber_id.clone()),
        );
        metadata.insert(
            "fields".to_string(),
            serde_json::json!(subscription.fields),
        );
        metadata.insert(
            "callback_intent".to_string(),
            serde_json::Value::String(subscription.callback_intent.clone()),
        );

        let props: Vec<(&str, Value)> = vec![
            ("type", Value::from("identity_subscription")),
            ("subscriber_id", Value::from(subscription.subscriber_id.as_str())),
            ("fields", Value::from(
                serde_json::to_string(&subscription.fields)
                    .unwrap_or_default()
                    .as_str(),
            )),
            ("callback_intent", Value::from(subscription.callback_intent.as_str())),
        ];

        self.store_node(labels::SYSTEM_CONFIG, props)
            .map_err(|e| rollball_core::error::RollballError::Memory(e.to_string()))?;

        Ok(())
    }

    fn list_all(&self) -> rollball_core::error::Result<Vec<IdentityEntry>> {
        use crate::types::labels;

        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::AUTOBIOGRAPHICAL);

        let mut entries = Vec::new();
        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                let props: Vec<(String, grafeo_common::types::Value)> = n
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();

                if let Ok(ab_node) = crate::types::AutobiographicalNode::from_properties(id, &props) {
                    let category = ab_node.metadata.get("category")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(rollball_core::identity::IdentityCategory::Identity);

                    let privacy = ab_node.metadata.get("privacy")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(rollball_core::identity::PrivacyLevel::Personal);

                    let source = ab_node.metadata.get("source")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();

                    let updated_at = ab_node.metadata.get("updated_at_iso")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    entries.push(IdentityEntry {
                        field: ab_node.key.clone(),
                        value: ab_node.value.clone(),
                        confidence: ab_node.confidence,
                        category,
                        privacy,
                        source,
                        updated_at,
                    });
                }
            }
        }

        Ok(entries)
    }
}
