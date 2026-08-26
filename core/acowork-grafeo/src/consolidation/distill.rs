//! Distillation landing pipeline helpers (ADR-057 P0).
//!
//! The actual [`MemoryProvider`] trait override lives in `provider_impl.rs`
//! (it has to share the existing `impl MemoryProvider for GrafeoStore` block).
//! This module only provides the embedding helper used by the landing
//! pipeline plus unit tests for the landing semantics via the public
//! provider method.
//!
//! Pipeline summary (full code in `provider_impl::ingest_distilled_triples`):
//! 1. Store the episode (always succeeds first; episode is never lost).
//! 2. For each `Triple`: compute embedding → land via `process_memory_store`
//!    (object-aware dedup > 0.95, conflict detection + `conflict_group_id`,
//!    Active ≥ 0.85 / Pending dispatch) → create the
//!    `Episodic -[SOURCED_FROM]-> Knowledge` edge (D9).
//!
//! Embedding degradation policy (D1): when no `EmbeddingProvider` is
//! available — or the provider errors — the helper returns `None` and the
//! node is stored WITHOUT a vector. No fake/hash vectors are written to the
//! semantic layer: a meaningless vector would pollute both semantic search
//! and future conflict detection. The deterministic hash embedding remains
//! available for ProceduralNode creation paths only (see
//! `acowork_memory::procedural_embedding_fallback`), where the write
//! contract requires a non-empty vector.

use acowork_core::EmbeddingProvider;

use crate::grafeo::GrafeoStore;

impl GrafeoStore {
    /// Compute a text embedding via the supplied provider.
    ///
    /// Returns `None` when no provider is configured or the provider errors
    /// (degradation is logged). `None` means "store without a vector" — it
    /// never means "store a fake vector".
    pub async fn compute_text_embedding(
        text: &str,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Option<Vec<f32>> {
        match embedding_provider {
            Some(prov) => match prov.embed(text).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        provider = prov.name(),
                        "embedding generation failed during distillation landing; storing without vector"
                    );
                    None
                }
            },
            None => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_memory::provider::IngestResult;
    use acowork_memory::types::{DistilledEpisode, KnowledgeSubType};
    use acowork_memory::MemoryProvider;

    /// Test embedding provider keyed on `subject + predicate` ONLY.
    ///
    /// Vectors for the same (subject, predicate) are identical (cosine 1.0)
    /// regardless of object; different (subject, predicate) pairs hash to
    /// unrelated vectors. This makes dedup / knowledge-update / conflict
    /// scenarios deterministic without a real model:
    /// - same (s, p, o)        → cosine 1.0, object matches → duplicate
    /// - same (s, p), diff o   → cosine 1.0, object differs → conflict path (conflict_group_id)
    /// - different (s, p)      → unrelated → new node, no conflict
    struct SpKeyedEmbedding;

    fn sp_hash(text: &str) -> Vec<f32> {
        // Deterministic per-text hash vector (same algorithm family as the
        // procedural fallback, dimension matches HnswConfig default).
        acowork_memory::manager::procedural_embedding_fallback(text)
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for SpKeyedEmbedding {
        fn name(&self) -> &str {
            "sp-keyed-test"
        }

        async fn embed(&self, text: &str) -> Result<Vec<f32>, acowork_core::EmbeddingError> {
            // Key on the first two words: "subject predicate object".
            let key = text.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            Ok(sp_hash(&key))
        }

        async fn embed_batch(
            &self,
            texts: &[&str],
        ) -> Result<Vec<Vec<f32>>, acowork_core::EmbeddingError> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }

        fn dimension(&self) -> usize {
            384
        }

        async fn is_available(&self) -> bool {
            true
        }
    }

    fn triple(s: &str, p: &str, o: &str, c: f32, t: KnowledgeSubType) -> acowork_memory::Triple {
        acowork_memory::Triple {
            subject: s.to_string(),
            predicate: p.to_string(),
            object: o.to_string(),
            confidence: c,
            sub_type: t,
        }
    }

    fn episode_with(triples: Vec<acowork_memory::Triple>) -> DistilledEpisode {
        DistilledEpisode {
            session_id: "sess-1".to_string(),
            summary: "summary text".to_string(),
            source_session_id: "sess-1".to_string(),
            consolidated: false,
            triples,
        }
    }

    #[tokio::test]
    async fn ingest_distilled_triples_creates_episode_and_knowledge_with_sourced_from_edge() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let ep = episode_with(vec![triple(
            "User",
            "requested",
            "context compaction fix",
            0.95,
            KnowledgeSubType::Fact,
        )]);

        let result = MemoryProvider::ingest_distilled_triples(&store, &ep, None)
            .await
            .expect("ingest should succeed");
        // NodeId may legally be 0 (GrafeoDB starts IDs at 0); verify the episode
        // is actually stored by resolving the NodeId back to a node.
        assert!(
            store
                .get_node(grafeo_common::types::NodeId(result.episode_id))
                .is_some(),
            "episode NodeId must resolve to a stored node"
        );
        assert_eq!(result.knowledge_ids.len(), 1);
        assert_eq!(result.conflicts_detected, 0);

        let edges_from_episode = store.db.graph_store();
        let neighbors = edges_from_episode.edges_from(
            grafeo_common::types::NodeId(result.episode_id),
            grafeo_core::graph::Direction::Both,
        );
        assert!(
            !neighbors.is_empty(),
            "at least one edge (SOURCED_FROM) must be present"
        );
    }

    #[tokio::test]
    async fn ingest_distilled_dedups_repeated_triple() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let t = triple("User", "likes", "coffee", 0.92, KnowledgeSubType::Preference);

        let first = MemoryProvider::ingest_distilled_triples(
            &store,
            &episode_with(vec![t.clone()]),
            Some(&SpKeyedEmbedding),
        )
        .await
        .expect("first ingest succeeds");
        assert_eq!(first.knowledge_ids.len(), 1);

        let second = MemoryProvider::ingest_distilled_triples(
            &store,
            &episode_with(vec![t]),
            Some(&SpKeyedEmbedding),
        )
        .await
        .expect("second ingest succeeds");
        assert_eq!(
            second.knowledge_ids.len(),
            0,
            "duplicate triple should be skipped"
        );
    }

    // ── ADR-057 §7.1: knowledge update (object change) ──────────────────
    //
    // Same (subject, predicate) with a DIFFERENT object must NOT be treated
    // as a duplicate and silently dropped. It must land as a new node and be
    // routed through conflict detection: both nodes end up sharing a
    // `conflict_group_id` (Ambiguous pending arbitration) and the ingest
    // result reports the conflict.

    #[tokio::test]
    async fn ingest_distilled_object_change_is_conflict_not_dedup() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let provider = SpKeyedEmbedding;

        let first = MemoryProvider::ingest_distilled_triples(
            &store,
            &episode_with(vec![triple(
                "User",
                "lives in",
                "Beijing",
                0.9,
                KnowledgeSubType::Fact,
            )]),
            Some(&provider),
        )
        .await
        .unwrap();
        assert_eq!(first.knowledge_ids.len(), 1);
        assert_eq!(first.conflicts_detected, 0);

        // Same (subject, predicate), different object → knowledge update.
        let second = MemoryProvider::ingest_distilled_triples(
            &store,
            &episode_with(vec![triple(
                "User",
                "lives in",
                "Shanghai",
                0.95,
                KnowledgeSubType::Fact,
            )]),
            Some(&provider),
        )
        .await
        .unwrap();

        assert_eq!(
            second.knowledge_ids.len(),
            1,
            "object change must NOT be deduplicated away"
        );
        assert!(
            second.conflicts_detected >= 1,
            "object change must be reported as a conflict"
        );

        // Both nodes carry the same conflict_group_id (Ambiguous until
        // LLM arbitration), per the instant pipeline semantics.
        for nid in first
            .knowledge_ids
            .iter()
            .chain(second.knowledge_ids.iter())
        {
            let node = store
                .get_node(grafeo_common::types::NodeId(*nid))
                .expect("knowledge node exists");
            let group = node
                .get_property("metadata")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|m| m.get("conflict_group_id").cloned());
            assert!(
                group.is_some(),
                "node {nid} must be tagged with conflict_group_id"
            );
        }
    }

    // ── Within-batch dedup: identical triples inside ONE episode ─────────

    #[tokio::test]
    async fn ingest_distilled_dedups_within_single_episode() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let t = triple("User", "prefers", "tea", 0.9, KnowledgeSubType::Preference);
        // The same triple twice inside one DistilledEpisode.
        let ep = episode_with(vec![t.clone(), t]);
        let result = MemoryProvider::ingest_distilled_triples(
            &store,
            &ep,
            Some(&SpKeyedEmbedding),
        )
        .await
        .unwrap();
        assert_eq!(
            result.knowledge_ids.len(),
            1,
            "duplicate triple inside one episode must land exactly once"
        );
    }

    #[tokio::test]
    async fn ingest_distilled_dispatches_low_confidence_to_pending() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let ep = episode_with(vec![triple(
            "User",
            "might",
            "prefer tea",
            0.6,
            KnowledgeSubType::Preference,
        )]);
        let result = MemoryProvider::ingest_distilled_triples(&store, &ep, None)
            .await
            .unwrap();
        assert_eq!(result.knowledge_ids.len(), 1);
        // Pending status is a confidence dispatch, NOT a conflict — the
        // metric must stay 0 (conflicts are conflict_resolutions only).
        assert_eq!(result.conflicts_detected, 0);

        let nid = grafeo_common::types::NodeId(result.knowledge_ids[0]);
        let node = store.db.get_node(nid).unwrap();
        let status = node
            .get_property("status")
            .and_then(grafeo_common::types::Value::as_str)
            .unwrap();
        assert_eq!(status, "Pending");
    }

    #[tokio::test]
    async fn ingest_distilled_high_confidence_goes_active() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let ep = episode_with(vec![triple(
            "User",
            "lives in",
            "Beijing",
            0.95,
            KnowledgeSubType::Fact,
        )]);
        let result = MemoryProvider::ingest_distilled_triples(&store, &ep, None)
            .await
            .unwrap();
        let nid = grafeo_common::types::NodeId(result.knowledge_ids[0]);
        let node = store.db.get_node(nid).unwrap();
        let status = node
            .get_property("status")
            .and_then(grafeo_common::types::Value::as_str)
            .unwrap();
        assert_eq!(status, "Active");
        let source_id = node
            .get_property("source_episode_id")
            .and_then(grafeo_common::types::Value::as_int64)
            .unwrap();
        assert_eq!(source_id as u64, result.episode_id);
    }

    #[tokio::test]
    async fn ingest_distilled_empty_triples_returns_zero_knowledge() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let ep = episode_with(vec![]);
        let result: IngestResult = MemoryProvider::ingest_distilled_triples(&store, &ep, None)
            .await
            .unwrap();
        assert_eq!(result.knowledge_ids.len(), 0);
        assert_eq!(result.conflicts_detected, 0);
        // NodeId may legally be 0 — verify the episode resolves to a stored node.
        assert!(
            store
                .get_node(grafeo_common::types::NodeId(result.episode_id))
                .is_some(),
            "episode must be stored even when the input has no triples"
        );
    }

    // ── ADR-057 C8: cross-layer diffusion via SOURCED_FROM edges ─────────
    //
    // Verifies the explicit acceptance criterion from the ADR: a freshly
    // distilled `Episodic` node MUST be reachable from the resulting
    // `Knowledge` node through a `SOURCED_FROM` edge in both directions, so
    // graph expansion / spreading activation can hop layers during recall.

    #[tokio::test]
    async fn cross_layer_diffusion_episode_knowledge_via_sourced_from_edge() {
        use crate::types::edge_types;
        use grafeo_common::types::NodeId;

        let store = GrafeoStore::new_in_memory().unwrap();
        let triples = vec![
            triple("User", "prefers", "Rust", 0.92, KnowledgeSubType::Preference),
            triple("User", "works on", "ADR-057", 0.88, KnowledgeSubType::Fact),
        ];
        let triples_len = triples.len();
        let result = MemoryProvider::ingest_distilled_triples(
            &store,
            &episode_with(triples),
            None,
        )
        .await
        .expect("ingest succeeds");
        assert_eq!(result.knowledge_ids.len(), 2);
        let episode_id = NodeId(result.episode_id);

        // 1. Episode has outgoing SOURCED_FROM edges to every knowledge node.
        let outgoing = store.get_edges(episode_id, grafeo_core::graph::Direction::Outgoing);
        let outgoing_types: Vec<&str> = outgoing.iter().map(|e| &*e.edge_type).collect();
        assert_eq!(
            outgoing.len(),
            triples_len,
            "each triple must produce one SOURCED_FROM edge from the episode"
        );
        assert!(
            outgoing_types.iter().all(|t| *t == edge_types::SOURCED_FROM),
            "all outgoing edges from the episode must be SOURCED_FROM, got {outgoing_types:?}"
        );

        // 2. Each knowledge node has an incoming SOURCED_FROM edge back to
        //    the episode (reverse traversal).
        for kn_id in &result.knowledge_ids {
            let kn = NodeId(*kn_id);
            let incoming = store.get_edges(kn, grafeo_core::graph::Direction::Incoming);
            assert_eq!(
                incoming.len(),
                1,
                "each knowledge node must have exactly one incoming edge"
            );
            assert_eq!(&*incoming[0].edge_type, edge_types::SOURCED_FROM);
            assert_eq!(
                incoming[0].dst, kn,
                "edge must terminate at the knowledge node (not originate)"
            );
            assert_eq!(incoming[0].src, episode_id);
        }

        // 3. Source metadata is preserved on the knowledge node so that
        //    downstream consolidation can follow the link back.
        let first_kn = NodeId(result.knowledge_ids[0]);
        let kn_node = store.get_node(first_kn).expect("knowledge node exists");
        let source_id = kn_node
            .get_property("source_episode_id")
            .and_then(grafeo_common::types::Value::as_int64)
            .expect("source_episode_id is set");
        assert_eq!(source_id as u64, result.episode_id);
    }

    // ── ADR-057 C8 (e2e): graph_expand actually reaches knowledge ────────
    //
    // Beyond asserting edges exist, run the real `graph_expand` BFS from the
    // episode seed and require the knowledge nodes to be reachable — this is
    // the acceptance criterion the ADR demands for cross-layer diffusion.

    #[tokio::test]
    async fn cross_layer_diffusion_graph_expand_reaches_knowledge() {
        use crate::spreading::GraphExpandConfig;
        use grafeo_common::types::NodeId;

        let store = GrafeoStore::new_in_memory().unwrap();
        let result = MemoryProvider::ingest_distilled_triples(
            &store,
            &episode_with(vec![
                triple("User", "prefers", "Rust", 0.92, KnowledgeSubType::Preference),
                triple("User", "works on", "ADR-057", 0.88, KnowledgeSubType::Fact),
            ]),
            None,
        )
        .await
        .expect("ingest succeeds");
        assert_eq!(result.knowledge_ids.len(), 2);

        let seeds = vec![(NodeId(result.episode_id), 1.0f64)];
        let config = GraphExpandConfig::default();
        let expanded = store.graph_expand(&seeds, &config).expect("expand succeeds");

        let reached: Vec<u64> = expanded.iter().map(|n| n.node_id.as_u64()).collect();
        for kn in &result.knowledge_ids {
            assert!(
                reached.contains(kn),
                "graph_expand from the episode seed must reach knowledge node {kn}, reached: {reached:?}"
            );
        }
    }

    #[tokio::test]
    async fn cross_layer_diffusion_repeated_distillation_does_not_orphan_episodes() {
        use grafeo_common::types::NodeId;

        let store = GrafeoStore::new_in_memory().unwrap();

        // Two distillations with different triples so they are NOT deduped.
        let first = MemoryProvider::ingest_distilled_triples(
            &store,
            &episode_with(vec![triple(
                "User",
                "uses",
                "neovim",
                0.9,
                KnowledgeSubType::Fact,
            )]),
            None,
        )
        .await
        .unwrap();
        let second = MemoryProvider::ingest_distilled_triples(
            &store,
            &episode_with(vec![triple(
                "User",
                "drinks",
                "coffee",
                0.9,
                KnowledgeSubType::Preference,
            )]),
            None,
        )
        .await
        .unwrap();

        assert_ne!(first.episode_id, second.episode_id);
        assert_eq!(first.knowledge_ids.len(), 1);
        assert_eq!(second.knowledge_ids.len(), 1);

        // Every episode must have its own non-empty outgoing edge set; no
        // orphan episodes (D9 acceptance criterion).
        for ep_id in [first.episode_id, second.episode_id] {
            let edges = store.get_edges(NodeId(ep_id), grafeo_core::graph::Direction::Outgoing);
            assert!(
                !edges.is_empty(),
                "episode {ep_id} must have at least one SOURCED_FROM edge"
            );
        }
    }
}
